#[macro_use]
extern crate log;

use std::string::String;
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures::{stream, StreamExt, TryStreamExt};
use h3o::{LatLng, Resolution};
use mac6::Mac;
use mac_address::MacAddress;
use serde_json::json;

use sqlx::{postgres::PgPoolOptions, query, query_scalar, PgPool};


mod api;
mod coverage;
mod fetch;
mod lookup;
mod raw;
use crate::fetch::{AppleWPSClient, Location};
use crate::protobuf::CellTower;

pub mod protobuf {
    include!(concat!(env!("OUT_DIR"), "/apple.rs"));
}

const CONCURRENT_PARTS: usize = 8;
const SENTINEL_CELL_ID: u32 = u32::MAX;
//const CONCURRENT_CELLS_PER_PART: usize = 8;

#[derive(Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Command,
    #[arg(long)]
    china: bool,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    Explore {
        #[clap(subcommand)]
        source: ExploreSource,
    },
    Query {
        mac: String,
    },
    Cell {
        mcc: u32,
        mnc: u32,
        tacid: u32,
        cellid: u32,
        /// print every field of Apple's raw reply, including ones wtfps doesn't decode
        #[arg(long)]
        raw: bool,
    },
    Oui {
        oui: String,
    },
    /// Write an HTML map estimating the coverage of a cell. Uses the database; a cell
    /// that isn't crawled yet is fetched from Apple (with its site's sibling sectors) and
    /// saved first
    Heatmap {
        mcc: i32,
        mnc: i32,
        tac_id: i32,
        cell_id: i64,
        /// include same-operator cells within this distance
        #[arg(long, default_value_t = DEFAULT_RADIUS_KM)]
        radius_km: f64,
        /// write GeoJSON (the cell, its accuracy circle, the nearby cells and the modelled
        /// coverage as H3 hexagons) instead of HTML
        #[arg(long)]
        geojson: bool,
        #[command(flatten)]
        model: ModelArgs,
        /// output file [default: cell-<mcc>-<mnc>-<tac>-<cell>.html or .geojson]
        #[arg(long)]
        out: Option<String>,
        /// build from Apple's replies only, without a database (nothing is saved)
        #[arg(long, conflicts_with = "no_fetch")]
        online: bool,
        /// only use the database: don't ask Apple for a cell that isn't crawled yet
        #[arg(long)]
        no_fetch: bool,
    },
    /// Apply pending database migrations and exit
    Migrate,
    /// Serve the HTTP API (see src/api.rs)
    Serve {
        #[arg(long, default_value = "0.0.0.0:8090")]
        listen: String,
    },
}

/// Coverage model settings for `heatmap --geojson`; the HTML map has sliders for these
#[derive(clap::Args, Debug, Clone)]
struct ModelArgs {
    /// path-loss exponent: about 3.4-3.7 for masts of 50-15 m (Okumura-Hata's distance slope)
    #[arg(long, default_value_t = 3.5)]
    path_loss: f64,
    /// random signal variation from buildings and terrain, in dB
    #[arg(long, default_value_t = 8.0)]
    sigma_db: f64,
    /// how much each cell's position is blurred by its accuracy (0 = exact)
    #[arg(long, default_value_t = 0.0)]
    acc_weight: f64,
    /// distance at which signal fades out [default: Okumura-Hata's radius for the cell's band
    /// and environment, or 4x the typical spacing of competing cells without a known band]
    #[arg(long)]
    range_km: Option<f64>,
    /// compete with cells on all frequencies, not just the cell's own EARFCN
    #[arg(long)]
    all_frequencies: bool,
    /// experimental: fade out at the cell's own reach from Apple's undocumented field 12
    /// instead of one shared max range (--range-km); falls back to the range without it
    #[arg(long)]
    use_unknown_12: bool,
    /// H3 resolution of the coverage hexagons (10: ~0.015 km², 11: ~0.002 km²)
    /// [default: 10, coarser for large rural areas]
    #[arg(long, value_parser = clap::value_parser!(u8).range(7..=12))]
    hex_res: Option<u8>,
    /// how signal depends on frequency and how far a cell reaches by default: hata
    /// (Okumura-Hata / COST-231) or simple (distance only, range 4x the cell spacing)
    #[arg(long, value_enum, default_value_t = coverage::Propagation::Hata)]
    propagation: coverage::Propagation,
    /// experimental: give sectors a wedge-shaped antenna pattern, pointing the way their
    /// position is offset from their site's centre (a rough estimate)
    #[arg(long)]
    directional: bool,
    /// write smooth likelihood contour bands instead of hexagons
    #[arg(long)]
    contours: bool,
    /// likelihood levels where the bands start, ascending, between 0 and 1
    #[arg(long, value_delimiter = ',', default_values_t = [0.1, 0.25, 0.5, 0.75, 0.9])]
    contour_levels: Vec<f64>,
    /// spacing of the grid the contours are traced on, in metres
    #[arg(long, default_value_t = 20.0)]
    contour_step_m: f64,
}

#[derive(Subcommand, Debug, Clone)]
enum ExploreSource {
    Crawl,
    Todo {
        #[arg(long)]
        ignore_extra: bool,
    },
    Cell {
        /// don't guess cell IDs near known sites once the area crawl runs out
        #[arg(long)]
        no_probe: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let client = AppleWPSClient::new(cli.china);

    match cli.command {
        Command::Explore { source } => {
            let pool = PgPoolOptions::new()
                .max_connections((CONCURRENT_PARTS * 2) as _)
                .connect(&database_url()?)
                .await?;
            sqlx::migrate!().run(&pool).await?;

            match source {
                ExploreSource::Cell { no_probe } => {
                    explore_cells(&client, &pool, !no_probe).await?
                }

                ExploreSource::Crawl => explore_wifi(&client, &pool, true, false).await?,
                ExploreSource::Todo { ignore_extra } => {
                    explore_wifi(&client, &pool, false, ignore_extra).await?
                }
            }
        }
        Command::Query { mac } => {
            let wifis = client
                .query_wifi(
                    Mac::from_str(&mac)
                        .map_err(|_| anyhow::anyhow!("not a MAC address: {mac}"))?
                        .0
                        .into(),
                )
                .await?;
            if let Some((_, Some(loc))) = wifis.first() {
                println!("{}", osm(loc));
                println!("{} networks in reply", wifis.len());
            } else {
                println!("not found")
            }
        }

        Command::Cell { mcc, mnc, tacid, cellid, raw } =>{
            let request = CellTower{
                mcc,
                mnc,
                cell_id: cellid,
                tac_id: tacid,
                location: None,
                uarfcn: None,
                pid: None,
            };
            if raw {
                print!("{}", raw::summarize(&client.query_cell_raw(request).await?));
                return Ok(());
            }
            println!("cellid: {cellid}");

            let cells = client.query_cell(request).await?;
            if let Some((_, Some(loc))) = cells.first() {
                println!("{}", osm(loc));
                println!("{} entries in reply", cells.len());
            } else {
                println!("not found")
            }
            println!(
                "{:<12} {:>4} {:>7} {:>7}  {:<14} {:>4} {:>7}  location",
                "cell_id", "mnc", "tac", "earfcn", "band", "pci", "acc_m"
            );
            for (cell, loc) in cells {
                let dash = || "-".to_string();
                let earfcn = radio_value(cell.uarfcn);
                let band = earfcn.and_then(lte_band).map_or_else(dash, |(b, mhz)| format!("{b} ({mhz} MHz)"));
                let acc = cell.location.and_then(|l| l.horizontal_accuracy);
                let id = if cell.cell_id == SENTINEL_CELL_ID { "area est.".into() } else { cell.cell_id.to_string() };
                println!(
                    "{id:<12} {:>4} {:>7} {:>7}  {band:<14} {:>4} {:>7}  {}",
                    cell.mnc,
                    cell.tac_id,
                    earfcn.map_or_else(dash, |e| e.to_string()),
                    radio_value(cell.pid).map_or_else(dash, |p| p.to_string()),
                    acc.map_or_else(dash, |a| a.to_string()),
                    loc.as_ref().map_or_else(|| "not found".to_string(), osm),
                );
            }

        }

        Command::Heatmap { mcc, mnc, tac_id, cell_id, radius_km, geojson, model, out, online, no_fetch } => {
            let key = (mcc, mnc, tac_id, cell_id);
            let pool = if online { None } else { Some(lazy_pool(1)?) };
            let pool = pool.as_ref().ok_or("database not used");
            let (rows, source) = lookup::heatmap_rows(&client, pool, key, radius_km, !no_fetch).await?;
            match &source {
                lookup::Source::Database => {}
                lookup::Source::Fetched(found) => {
                    println!("cell wasn't crawled yet: fetched it from Apple and saved {found} cells")
                }
                lookup::Source::Online(reason) => {
                    println!("fetched {} cells of this operator from Apple; not saved ({reason})", rows.len())
                }
            }
            let extension = if geojson { "geojson" } else { "html" };
            let path =
                out.unwrap_or_else(|| format!("cell-{mcc}-{mnc}-{tac_id}-{cell_id}.{extension}"));
            let (document, neighbours) = heatmap_document(key, rows, &model, geojson, &source)?;
            std::fs::write(&path, document)?;
            println!("wrote {path} ({neighbours} nearby cells)");
        }

        Command::Serve { listen } => {
            // starts even when the database is down: requests then fall back to Apple, and
            // the schema is brought up to date once it can be reached
            let pool = lazy_pool(CONCURRENT_PARTS as u32)?;
            let schema_ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            tokio::spawn(migrate_when_reachable(pool.clone(), schema_ready.clone()));
            api::serve(client, pool, schema_ready, &listen).await?;
        }

        Command::Migrate => {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&database_url()?)
                .await?;
            sqlx::migrate!().run(&pool).await?;
            println!("database schema is up to date");
        }

        Command::Oui { oui } => {
            for i in 0..256u64.pow(3) {
                println!("{oui}{i:06x}");
            }
        }
    }

    Ok(())
}

/// WiFi exploration: query the `todo` queue, or with `crawl` one random known network per
/// uncrawled area. With `ignore_extra`, only the queried networks are stored, not the
/// nearby ones Apple returns with them.
async fn explore_wifi(client: &AppleWPSClient, pool: &PgPool, crawl: bool, ignore_extra: bool) -> Result<()> {
    loop {
        let mut crawling = BTreeSet::new();
        let todo = if crawl {
            let mut todo = Vec::new();
            let mut tx = pool.begin().await?;
            let areas = query_scalar!(
                "select area from crawl where last_crawled is null order by area limit 256"
            )
            .fetch_all(&mut *tx)
            .await?;
            if areas.is_empty() {
                return Ok(());
            }

            for area in areas {
                let mac = query_scalar!(
                    "select mac from wifi where crawl_area = $1 order by random() limit 1",
                    area
                )
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(mac) = mac {
                    crawling.insert(area);
                    todo.push(mac);
                } else {
                    query!("delete from crawl where area = $1", area)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            tx.commit().await?;
            todo
        } else {
            let todo = query_scalar!("select mac from todo limit 256")
                .fetch_all(pool)
                .await?;
            if todo.is_empty() {
                return Ok(());
            }
            todo
        };

        let mut requests = Vec::new();
        for mac in &todo {
            requests.push(client.query_wifi(*mac));
        }

        info!("making {} requests...", requests.len());
        let mut requests = stream::iter(requests).buffer_unordered(256);
        let mut locs = BTreeMap::new();
        while let Some(response) = requests.try_next().await? {
            for (mac, loc) in response {
                if ignore_extra && !todo.contains(&mac) {
                    continue;
                }
                locs.insert(mac, loc);
            }
        }
        commit(pool.clone(), crawling, locs).await?;
    }
}

async fn commit(
    pool: PgPool,
    crawling: BTreeSet<i64>,
    locs: BTreeMap<MacAddress, Option<Location>>,
) -> Result<()> {
    let mut tx = pool.begin().await?;

    info!(
        "{} macs received, {} not found",
        locs.values().filter(|x| x.is_some()).count(),
        locs.values().filter(|x| x.is_none()).count()
    );

    let mut check_areas = BTreeSet::new();
    for loc in locs.values().flatten() {
        check_areas.insert(loc.cell(Resolution::Nine));
    }
    for area in check_areas.clone() {
        check_areas.extend(area.edges().map(|x| x.destination()))
    }
    let mut known = BTreeSet::new();
    for area in check_areas {
        known.extend(
            query_scalar!(
                "select mac from wifi where check_area = $1",
                u64::from(area) as i64
            )
            .fetch_all(&mut *tx)
            .await?,
        );
    }

    for (mac, loc) in locs {
        query!("delete from todo where mac = $1", mac)
            .execute(&mut *tx)
            .await?;

        if let Some(loc) = loc {
            if known.contains(&mac) {
                continue;
            }

            let check_area = u64::from(loc.cell(Resolution::Nine)) as i64;
            let crawl_area = u64::from(loc.cell(Resolution::Eleven)) as i64;
            let Location { lat, lon, .. } = loc;
            query!("insert into wifi (mac, geom, check_area, crawl_area) values ($1, ST_SetSRID(ST_MakePoint($2, $3), 4326), $4, $5) on conflict do nothing", mac, lon, lat, check_area, crawl_area)
                .execute(&mut *tx)
                .await?;
            query!(
                "insert into crawl (area) values ($1) on conflict do nothing",
                crawl_area
            )
            .execute(&mut *tx)
            .await?;

            println!("{}", json!({"lat": lat, "lon": lon}));
        } else {
            query!("delete from wifi where mac = $1", mac)
                .execute(&mut *tx)
                .await?;
        }
    }

    for area in crawling {
        query!(
            "update crawl set last_crawled = current_timestamp where area = $1",
            area
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

type CellKey = (i32, i32, i32, i64);
type CellArea = (i32, i32, i64);

/// One H3 area (~5 km²) per operator is crawled; a single cell request returns
/// its neighbours, so querying every discovered cell would mostly return known cells.
const CELL_CRAWL_RES: Resolution = Resolution::Seven;
const CELL_BATCH: i64 = 256;

fn cell_key(cell: &CellTower) -> CellKey {
    (
        cell.mcc as i32,
        cell.mnc as i32,
        cell.tac_id as i32,
        cell.cell_id as i64,
    )
}

/// LTE band and its nominal frequency in MHz for a downlink EARFCN (3GPP TS 36.101);
/// same table as the HTML map's
fn lte_band(earfcn: i32) -> Option<(u32, u32)> {
    coverage::LTE_BANDS.iter().find(|b| (b.0..=b.1).contains(&earfcn)).map(|b| (b.2, b.3))
}

/// EARFCN / PCI as reported by Apple; u32::MAX (-1) means unknown
fn radio_value(value: Option<u32>) -> Option<i32> {
    value.filter(|&v| v != u32::MAX).and_then(|v| i32::try_from(v).ok())
}

fn cell_area(lat: f64, lon: f64) -> Result<i64> {
    Ok(u64::from(LatLng::new(lat, lon)?.to_cell(CELL_CRAWL_RES)) as i64)
}

type CellReply = Vec<(CellTower, Option<Location>)>;

/// Query cells concurrently; each key is returned with its own result.
async fn query_cells(client: &AppleWPSClient, keys: &[CellKey]) -> Vec<(CellKey, Result<CellReply>)> {
    let requests: Vec<_> = keys
        .iter()
        .map(|&key @ (mcc, mnc, tac_id, cell_id)| {
            let request = (|| {
                Ok(CellTower {
                    mcc: u32::try_from(mcc)?,
                    mnc: u32::try_from(mnc)?,
                    tac_id: u32::try_from(tac_id)?,
                    cell_id: u32::try_from(cell_id)?,
                    location: None,
                    uarfcn: None,
                    pid: None,
                })
            })();
            async move {
                let result = match request {
                    Ok(cell) => client.query_cell(cell).await,
                    Err(e) => Err(e),
                };
                (key, result)
            }
        })
        .collect();

    stream::iter(requests)
        .buffer_unordered(CONCURRENT_PARTS)
        .collect()
        .await
}

async fn explore_cells(client: &AppleWPSClient, pool: &PgPool, probe: bool) -> Result<()> {
    backfill_cell_areas(pool).await?;

    loop {
        // user-seeded cells first, then one random known cell from each uncrawled area
        let mut todo: Vec<CellKey> =
            sqlx::query_as("select mcc, mnc, tac_id, cell_id from cell_todo limit $1")
                .bind(CELL_BATCH)
                .fetch_all(pool)
                .await?;
        let mut picked_areas = BTreeMap::new();

        if todo.is_empty() {
            let picked: Vec<(i32, i32, i64, Option<i32>, Option<i64>)> = sqlx::query_as(
                "select a.mcc, a.mnc, a.area, c.tac_id, c.cell_id \
                 from (select mcc, mnc, area from cell_crawl where last_crawled is null limit $1) a \
                 left join lateral ( \
                     select tac_id, cell_id from cell \
                     where cell.mcc = a.mcc and cell.mnc = a.mnc and cell.crawl_area = a.area \
                     order by random() limit 1 \
                 ) c on true",
            )
            .bind(CELL_BATCH)
            .fetch_all(pool)
            .await?;
            if picked.is_empty() {
                // Apple returns the same fixed group of ~100 cells for every cell in it,
                // so the area crawl can't leave a group; guessing nearby cell IDs can
                if probe && probe_cells(client, pool).await? {
                    continue;
                }
                break;
            }

            for (mcc, mnc, area, tac_id, cell_id) in picked {
                if let (Some(tac_id), Some(cell_id)) = (tac_id, cell_id) {
                    let key = (mcc, mnc, tac_id, cell_id);
                    todo.push(key);
                    picked_areas.insert(key, (mcc, mnc, area));
                } else {
                    sqlx::query("delete from cell_crawl where mcc = $1 and mnc = $2 and area = $3")
                        .bind(mcc)
                        .bind(mnc)
                        .bind(area)
                        .execute(pool)
                        .await?;
                }
            }
        }

        info!("making {} cell requests...", todo.len());
        let mut queried = Vec::new();
        let mut results = Vec::new();
        for (key, resp) in query_cells(client, &todo).await {
            match resp {
                Ok(resp) => {
                    queried.push(key);
                    results.extend(resp);
                }
                // failed cells stay queued and are retried in a later batch
                Err(e) => warn!("cell {key:?} failed: {e:#}"),
            }
        }

        if queried.is_empty() && !todo.is_empty() {
            anyhow::bail!("all {} cell requests in batch failed", todo.len());
        }

        let crawled = queried
            .iter()
            .filter_map(|key| picked_areas.get(key).copied())
            .collect();
        commit_cells(pool, &queried, crawled, results).await?;
    }

    Ok(())
}

/// A found cell's location with any EARFCN / PCI seen for it.
type FoundCell = (Location, Option<i32>, Option<i32>);

/// Cells from one or more replies: those with a location (merged when a cell appears in
/// several replies) and those without one. Tracking-area estimates are dropped.
fn merge_found(
    results: Vec<(CellTower, Option<Location>)>,
) -> (BTreeMap<CellKey, FoundCell>, BTreeSet<CellKey>) {
    let mut found: BTreeMap<CellKey, FoundCell> = BTreeMap::new();
    let mut not_found = BTreeSet::new();
    for (cell, loc) in results {
        // Apple includes entries with cell_id u32::MAX (-1): per-TAC location estimates
        // (~150km accuracy), not real cells, so don't store or crawl them.
        if cell.cell_id == SENTINEL_CELL_ID {
            continue;
        }
        match loc {
            Some(loc) => {
                // the same cell can appear in several replies; keep any radio details seen
                let (earfcn, pci) = (radio_value(cell.uarfcn), radio_value(cell.pid));
                let entry = found.entry(cell_key(&cell)).or_insert((loc, None, None));
                entry.1 = earfcn.or(entry.1);
                entry.2 = pci.or(entry.2);
            }
            None => {
                not_found.insert(cell_key(&cell));
            }
        }
    }
    not_found.retain(|key| !found.contains_key(key));
    (found, not_found)
}

async fn commit_cells(
    pool: &PgPool,
    queried: &[CellKey],
    mut crawled: BTreeSet<CellArea>,
    results: Vec<(CellTower, Option<Location>)>,
) -> Result<()> {
    let (found, mut not_found) = merge_found(results);
    // a queried cell missing from its own response is also not found
    not_found.extend(queried.iter().copied());
    not_found.retain(|key| !found.contains_key(key));

    // seeded cells mark their own area as crawled, too
    let mut areas = BTreeSet::new();
    let (mut mccs, mut mncs, mut tacs, mut cell_ids) = (vec![], vec![], vec![], vec![]);
    let (mut lats, mut lons, mut accs, mut cell_areas) = (vec![], vec![], vec![], vec![]);
    let (mut earfcns, mut pcis) = (vec![], vec![]);
    let (mut unknown_11s, mut unknown_12s) = (vec![], vec![]);
    let (mut altitudes, mut vertical_accs) = (vec![], vec![]);
    for (&key @ (mcc, mnc, tac_id, cell_id), (loc, earfcn, pci)) in &found {
        let area = cell_area(loc.lat, loc.lon)?;
        areas.insert((mcc, mnc, area));
        if queried.contains(&key) {
            crawled.insert((mcc, mnc, area));
        }
        mccs.push(mcc);
        mncs.push(mnc);
        tacs.push(tac_id);
        cell_ids.push(cell_id);
        lats.push(loc.lat);
        lons.push(loc.lon);
        accs.push(loc.acc);
        cell_areas.push(area);
        earfcns.push(*earfcn);
        pcis.push(*pci);
        unknown_11s.push(loc.unknown_11);
        unknown_12s.push(loc.unknown_12);
        altitudes.push(loc.alt);
        vertical_accs.push(loc.alt_acc);
    }
    areas.extend(crawled.iter().copied());

    info!(
        "{} cells with location, {} not found, {} areas crawled",
        found.len(),
        not_found.len(),
        crawled.len(),
    );

    let mut tx = pool.begin().await?;

    // cells stored before areas were tracked get their area and accuracy on the next sighting;
    // known cells keep their position but pick up EARFCN / PCI and the undocumented fields
    // when a reply includes them. New cells get crawled_at from the column default; it's
    // refreshed at most hourly, because the same cells come back in many replies and
    // rewriting every row each batch would be wasteful
    sqlx::query(
        "insert into cell (mcc, mnc, tac_id, cell_id, geom, acc, crawl_area, earfcn, pci, \
                           unknown_11, unknown_12, altitude, vertical_acc) \
         select mcc, mnc, tac_id, cell_id, ST_SetSRID(ST_MakePoint(lon, lat), 4326), acc, area, \
                earfcn, pci, unknown_11, unknown_12, altitude, vertical_acc \
         from unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[], \
                     $5::float8[], $6::float8[], $7::bigint[], $8::bigint[], \
                     $9::integer[], $10::integer[], $11::bigint[], $12::bigint[], \
                     $13::bigint[], $14::bigint[]) \
             as t(mcc, mnc, tac_id, cell_id, lat, lon, acc, area, earfcn, pci, \
                  unknown_11, unknown_12, altitude, vertical_acc) \
         on conflict (mcc, mnc, tac_id, cell_id) do update set \
             geom = case when cell.crawl_area is null then excluded.geom else cell.geom end, \
             acc = case when cell.crawl_area is null then excluded.acc else cell.acc end, \
             crawl_area = coalesce(cell.crawl_area, excluded.crawl_area), \
             earfcn = coalesce(excluded.earfcn, cell.earfcn), \
             pci = coalesce(excluded.pci, cell.pci), \
             unknown_11 = coalesce(excluded.unknown_11, cell.unknown_11), \
             unknown_12 = coalesce(excluded.unknown_12, cell.unknown_12), \
             altitude = coalesce(excluded.altitude, cell.altitude), \
             vertical_acc = coalesce(excluded.vertical_acc, cell.vertical_acc), \
             crawled_at = current_timestamp \
         where cell.crawl_area is null \
            or cell.crawled_at is null or cell.crawled_at < current_timestamp - interval '1 hour' \
            or excluded.earfcn is distinct from cell.earfcn and excluded.earfcn is not null \
            or excluded.pci is distinct from cell.pci and excluded.pci is not null \
            or excluded.unknown_11 is distinct from cell.unknown_11 and excluded.unknown_11 is not null \
            or excluded.unknown_12 is distinct from cell.unknown_12 and excluded.unknown_12 is not null \
            or excluded.altitude is distinct from cell.altitude and excluded.altitude is not null \
            or excluded.vertical_acc is distinct from cell.vertical_acc and excluded.vertical_acc is not null",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&tacs)
    .bind(&cell_ids)
    .bind(&lats)
    .bind(&lons)
    .bind(&accs)
    .bind(&cell_areas)
    .bind(&earfcns)
    .bind(&pcis)
    .bind(&unknown_11s)
    .bind(&unknown_12s)
    .bind(&altitudes)
    .bind(&vertical_accs)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "insert into cell_site (mcc, mnc, site) \
         select distinct mcc, mnc, cell_id >> 8 \
         from unnest($1::integer[], $2::integer[], $3::bigint[]) as t(mcc, mnc, cell_id) \
         on conflict do nothing",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&cell_ids)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "delete from cell_not_found n \
         using unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[]) \
             as t(mcc, mnc, tac_id, cell_id) \
         where (n.mcc, n.mnc, n.tac_id, n.cell_id) = (t.mcc, t.mnc, t.tac_id, t.cell_id)",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&tacs)
    .bind(&cell_ids)
    .execute(&mut *tx)
    .await?;

    let (mut mccs, mut mncs, mut tacs, mut cell_ids) = (vec![], vec![], vec![], vec![]);
    for &(mcc, mnc, tac_id, cell_id) in &not_found {
        mccs.push(mcc);
        mncs.push(mnc);
        tacs.push(tac_id);
        cell_ids.push(cell_id);
    }
    sqlx::query(
        "insert into cell_not_found (mcc, mnc, tac_id, cell_id) \
         select * from unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[]) \
         on conflict (mcc, mnc, tac_id, cell_id) do update set checked_at = current_timestamp",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&tacs)
    .bind(&cell_ids)
    .execute(&mut *tx)
    .await?;

    let (mut mccs, mut mncs, mut area_ids, mut is_crawled) = (vec![], vec![], vec![], vec![]);
    for &key @ (mcc, mnc, area) in &areas {
        mccs.push(mcc);
        mncs.push(mnc);
        area_ids.push(area);
        is_crawled.push(crawled.contains(&key));
    }
    sqlx::query(
        "insert into cell_crawl (mcc, mnc, area, last_crawled) \
         select mcc, mnc, area, case when crawled then current_timestamp end \
         from unnest($1::integer[], $2::integer[], $3::bigint[], $4::bool[]) \
             as t(mcc, mnc, area, crawled) \
         on conflict (mcc, mnc, area) do update set last_crawled = excluded.last_crawled \
             where cell_crawl.last_crawled is null and excluded.last_crawled is not null",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&area_ids)
    .bind(&is_crawled)
    .execute(&mut *tx)
    .await?;

    let (mut mccs, mut mncs, mut tacs, mut cell_ids) = (vec![], vec![], vec![], vec![]);
    for &(mcc, mnc, tac_id, cell_id) in queried {
        mccs.push(mcc);
        mncs.push(mnc);
        tacs.push(tac_id);
        cell_ids.push(cell_id);
    }
    sqlx::query(
        "delete from cell_todo c \
         using unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[]) \
             as t(mcc, mnc, tac_id, cell_id) \
         where (c.mcc, c.mnc, c.tac_id, c.cell_id) = (t.mcc, t.mnc, t.tac_id, t.cell_id)",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&tacs)
    .bind(&cell_ids)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Known sites probed per round; each yields up to 4 neighbour sites x 2 sectors x its TACs.
const PROBE_SITES: i64 = 32;
const PROBE_SITE_OFFSETS: [i64; 4] = [-2, -1, 1, 2];
const PROBE_SECTORS: usize = 2;

fn key_columns(keys: &[CellKey]) -> (Vec<i32>, Vec<i32>, Vec<i32>, Vec<i64>) {
    let (mut mccs, mut mncs, mut tacs, mut cell_ids) = (vec![], vec![], vec![], vec![]);
    for &(mcc, mnc, tac_id, cell_id) in keys {
        mccs.push(mcc);
        mncs.push(mnc);
        tacs.push(tac_id);
        cell_ids.push(cell_id);
    }
    (mccs, mncs, tacs, cell_ids)
}

/// Guess cells at site numbers next to a batch of unprobed known sites (an LTE cell id
/// is site * 256 + sector) and store whatever the hits return. Returns false once every
/// known site has been probed.
async fn probe_cells(client: &AppleWPSClient, pool: &PgPool) -> Result<bool> {
    let sites: Vec<(i32, i32, i64)> =
        sqlx::query_as("select mcc, mnc, site from cell_site where probed_at is null limit $1")
            .bind(PROBE_SITES)
            .fetch_all(pool)
            .await?;
    if sites.is_empty() {
        return Ok(false);
    }

    // some operators number their sites in tens
    let mut steps = BTreeMap::new();
    for &(mcc, mnc, _) in &sites {
        if steps.contains_key(&(mcc, mnc)) {
            continue;
        }
        let (total, tens): (i64, i64) = sqlx::query_as(
            "select count(*), count(*) filter (where site % 10 = 0) \
             from (select site from cell_site where mcc = $1 and mnc = $2 limit 1000) s",
        )
        .bind(mcc)
        .bind(mnc)
        .fetch_one(pool)
        .await?;
        let step = if total >= 10 && tens * 10 >= total * 8 { 10 } else { 1 };
        steps.insert((mcc, mnc), step);
    }

    // guesses reuse the source site's TACs and sectors; remember the source of each guess
    let mut guesses = BTreeMap::new();
    for &source @ (mcc, mnc, site) in &sites {
        let cells: Vec<(i32, i64)> = sqlx::query_as(
            "select tac_id, cell_id from cell \
             where mcc = $1 and mnc = $2 and cell_id between $3 and $4",
        )
        .bind(mcc)
        .bind(mnc)
        .bind(site << 8)
        .bind((site << 8) | 255)
        .fetch_all(pool)
        .await?;
        let tacs: BTreeSet<i32> = cells.iter().map(|&(tac_id, _)| tac_id).collect();
        let sectors: BTreeSet<i64> = cells.iter().map(|&(_, cell_id)| cell_id & 255).collect();

        for offset in PROBE_SITE_OFFSETS {
            let neighbour = site + offset * steps[&(mcc, mnc)];
            if neighbour < 0 {
                continue;
            }
            for &tac_id in &tacs {
                for &sector in sectors.iter().take(PROBE_SECTORS) {
                    guesses
                        .entry((mcc, mnc, tac_id, (neighbour << 8) | sector))
                        .or_insert(source);
                }
            }
        }
    }

    // skip sites we already know and guesses tried before
    let keys: Vec<CellKey> = guesses.keys().copied().collect();
    let (mccs, mncs, tacs, cell_ids) = key_columns(&keys);
    let skip: Vec<CellKey> = sqlx::query_as(
        "select t.mcc, t.mnc, t.tac_id, t.cell_id \
         from unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[]) \
             as t(mcc, mnc, tac_id, cell_id) \
         where exists (select 1 from cell_site s \
                       where (s.mcc, s.mnc, s.site) = (t.mcc, t.mnc, t.cell_id >> 8)) \
            or exists (select 1 from cell_probe p \
                       where (p.mcc, p.mnc, p.tac_id, p.cell_id) = (t.mcc, t.mnc, t.tac_id, t.cell_id))",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&tacs)
    .bind(&cell_ids)
    .fetch_all(pool)
    .await?;
    for key in skip {
        guesses.remove(&key);
    }

    let keys: Vec<CellKey> = guesses.keys().copied().collect();
    info!("probing {} guessed cells near {} sites...", keys.len(), sites.len());
    let mut probed = Vec::new();
    let mut failed_sites = BTreeSet::new();
    let mut results = Vec::new();
    for (key, resp) in query_cells(client, &keys).await {
        match resp {
            Ok(resp) => {
                let hit = resp.iter().any(|(cell, loc)| cell_key(cell) == key && loc.is_some());
                probed.push((key, hit));
                // most guesses miss; keep those out of cell_not_found
                results.extend(
                    resp.into_iter()
                        .filter(|(cell, loc)| loc.is_some() || cell_key(cell) != key),
                );
            }
            // the source site stays unprobed, so its guesses are retried in a later round
            Err(e) => {
                warn!("probe {key:?} failed: {e:#}");
                failed_sites.insert(guesses[&key]);
            }
        }
    }
    if probed.is_empty() && !keys.is_empty() {
        anyhow::bail!("all {} probe requests failed", keys.len());
    }
    info!(
        "{} of {} guesses found",
        probed.iter().filter(|(_, hit)| *hit).count(),
        probed.len()
    );

    commit_cells(pool, &[], BTreeSet::new(), results).await?;

    let mut tx = pool.begin().await?;
    let probed_keys: Vec<CellKey> = probed.iter().map(|&(key, _)| key).collect();
    let hits: Vec<bool> = probed.iter().map(|&(_, hit)| hit).collect();
    let (mccs, mncs, tacs, cell_ids) = key_columns(&probed_keys);
    sqlx::query(
        "insert into cell_probe (mcc, mnc, tac_id, cell_id, found) \
         select * from unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[], $5::bool[]) \
         on conflict do nothing",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&tacs)
    .bind(&cell_ids)
    .bind(&hits)
    .execute(&mut *tx)
    .await?;

    let (mut mccs, mut mncs, mut site_ids) = (vec![], vec![], vec![]);
    for &(mcc, mnc, site) in sites.iter().filter(|s| !failed_sites.contains(*s)) {
        mccs.push(mcc);
        mncs.push(mnc);
        site_ids.push(site);
    }
    sqlx::query(
        "update cell_site s set probed_at = current_timestamp \
         from unnest($1::integer[], $2::integer[], $3::bigint[]) as t(mcc, mnc, site) \
         where (s.mcc, s.mnc, s.site) = (t.mcc, t.mnc, t.site)",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&site_ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(true)
}

/// Cells stored before area-based crawling have no crawl_area; compute it and
/// queue their areas so the crawl covers them.
async fn backfill_cell_areas(pool: &PgPool) -> Result<()> {
    loop {
        let rows: Vec<(i32, i32, i32, i64, f64, f64)> = sqlx::query_as(
            "select mcc, mnc, tac_id, cell_id, ST_Y(geom::geometry), ST_X(geom::geometry) \
             from cell where crawl_area is null limit 10000",
        )
        .fetch_all(pool)
        .await?;
        if rows.is_empty() {
            return Ok(());
        }

        let (mut mccs, mut mncs, mut tacs, mut cell_ids, mut areas) =
            (vec![], vec![], vec![], vec![], vec![]);
        for (mcc, mnc, tac_id, cell_id, lat, lon) in &rows {
            mccs.push(*mcc);
            mncs.push(*mnc);
            tacs.push(*tac_id);
            cell_ids.push(*cell_id);
            areas.push(cell_area(*lat, *lon)?);
        }

        let mut tx = pool.begin().await?;
        sqlx::query(
            "update cell c set crawl_area = t.area \
             from unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[], $5::bigint[]) \
                 as t(mcc, mnc, tac_id, cell_id, area) \
             where (c.mcc, c.mnc, c.tac_id, c.cell_id) = (t.mcc, t.mnc, t.tac_id, t.cell_id)",
        )
        .bind(&mccs)
        .bind(&mncs)
        .bind(&tacs)
        .bind(&cell_ids)
        .bind(&areas)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "insert into cell_crawl (mcc, mnc, area) \
             select distinct * from unnest($1::integer[], $2::integer[], $3::bigint[]) \
             on conflict do nothing",
        )
        .bind(&mccs)
        .bind(&mncs)
        .bind(&areas)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        info!("backfilled crawl areas for {} cells", rows.len());
    }
}

const HEATMAP_TEMPLATE: &str = include_str!("heatmap.html");
/// same-operator cells within this distance are included in a heatmap (the nearest
/// HEATMAP_MAX_CELLS of them, so towns stay as dense as before and rural cells, which reach
/// tens of km, still see their competitors)
const DEFAULT_RADIUS_KM: f64 = 50.0;

/// How long to wait for a database connection before treating it as unreachable.
const DB_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A pool that connects on first use, so a database outage doesn't stop the command or
/// server from starting; each use waits at most DB_CONNECT_TIMEOUT for a connection.
fn lazy_pool(max_connections: u32) -> Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(DB_CONNECT_TIMEOUT)
        .connect_lazy(&database_url()?)?)
}

/// Apply pending migrations, retrying every 10 s until the database can be reached, then
/// set `ready`. Until then the API treats the database as unavailable: on a new database the
/// tables don't exist yet.
async fn migrate_when_reachable(pool: PgPool, ready: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    loop {
        match sqlx::migrate!().run(&pool).await {
            Ok(()) => {
                ready.store(true, std::sync::atomic::Ordering::Relaxed);
                return info!("database schema is up to date");
            }
            Err(e) => {
                warn!("can't apply migrations yet ({e}); retrying in 10 s");
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
    }
}

/// The coverage model's default settings (as with no command-line options).
fn default_model() -> ModelArgs {
    #[derive(Parser)]
    struct Defaults {
        #[command(flatten)]
        model: ModelArgs,
    }
    Defaults::parse_from(["wtfps"]).model
}
/// the page models every cell for every pixel, so keep the nearest ones only
const HEATMAP_MAX_CELLS: i64 = 800;

#[derive(sqlx::FromRow, Clone)]
struct NearbyCell {
    tac_id: i32,
    cell_id: i64,
    lat: f64,
    lon: f64,
    acc: Option<i64>,
    earfcn: Option<i32>,
    pci: Option<i32>,
    distance_m: f64,
    /// direction from the chosen cell, degrees clockwise from north
    bearing_deg: Option<f64>,
    unknown_12: Option<i64>,
    /// ISO 8601 UTC; None for cells crawled before crawl times were recorded
    crawled_at: Option<String>,
    /// estimated antenna direction, degrees clockwise from north
    #[sqlx(skip)]
    azimuth_deg: Option<f64>,
    generated: String,
}

/// The cell (if crawled) and the nearest crawled cells of its operator, nearest first.
async fn nearby_from_db(pool: &PgPool, key: CellKey, radius_km: f64) -> Result<Vec<NearbyCell>> {
    let (mcc, mnc, tac_id, cell_id) = key;
    Ok(sqlx::query_as(
        "with t as (select geom from cell \
                    where mcc = $1 and mnc = $2 and tac_id = $3 and cell_id = $4) \
         select c.tac_id, c.cell_id, ST_Y(c.geom::geometry) as lat, ST_X(c.geom::geometry) as lon, \
                c.acc, c.earfcn, c.pci, c.unknown_12, ST_Distance(c.geom, t.geom) as distance_m, \
                to_char(c.crawled_at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as crawled_at, \
                degrees(ST_Azimuth(t.geom, c.geom)) as bearing_deg, \
                to_char(now(), 'YYYY-MM-DD HH24:MI TZ') as generated \
         from cell c, t \
         where c.mcc = $1 and c.mnc = $2 and ST_DWithin(c.geom, t.geom, $5) \
         order by ST_Distance(c.geom, t.geom), c.cell_id \
         limit $6",
    )
    .bind(mcc)
    .bind(mnc)
    .bind(tac_id)
    .bind(cell_id)
    .bind(radius_km * 1000.0)
    .bind(HEATMAP_MAX_CELLS)
    .fetch_all(pool)
    .await?)
}

/// The HTML coverage map, or with `geojson` the GeoJSON, for the cell `key` from `rows`
/// (the cell and its operator's nearby cells, nearest first). Returns the document and the
/// number of nearby cells in it.
fn heatmap_document(
    key: CellKey,
    mut rows: Vec<NearbyCell>,
    model: &ModelArgs,
    geojson: bool,
    source: &lookup::Source,
) -> Result<(String, usize)> {
    let (mcc, mnc, tac_id, cell_id) = key;

    // estimated from all of the operator's nearby cells, every frequency: a site's sectors
    // use several bands
    let sites: Vec<_> = rows.iter().map(|c| (c.cell_id >> 8, c.pci, c.lat, c.lon)).collect();
    for (c, azimuth) in rows.iter_mut().zip(coverage::estimate_azimuths(&sites)) {
        c.azimuth_deg = azimuth.map(|a| a.round());
    }

    let cell_json = |c: &NearbyCell| {
        json!({
            "mcc": mcc, "mnc": mnc, "tac_id": c.tac_id, "cell_id": c.cell_id,
            "lat": c.lat, "lon": c.lon, "acc": c.acc, "earfcn": c.earfcn, "pci": c.pci,
            "unknown_12": c.unknown_12, "crawled_at": c.crawled_at, "azimuth_deg": c.azimuth_deg,
        })
    };
    let is_target = |c: &&NearbyCell| (c.tac_id, c.cell_id) == (tac_id, cell_id);
    let Some(target) = rows.iter().find(is_target) else {
        anyhow::bail!("no location for cell {mcc}/{mnc}/{tac_id}/{cell_id}");
    };
    let nearby: Vec<&NearbyCell> = rows.iter().filter(|c| !is_target(c)).collect();

    if geojson {
        let accuracy = target.acc.filter(|&a| a > 0).map(|a| accuracy_circle(target.lat, target.lon, a as f64));
        let coverage = model_coverage(target, &nearby, model)?;
        let mut geojson = cell_geojson(mcc, mnc, target, &nearby, accuracy, &coverage)?;
        // where the data came from, and whether it was saved
        if let Some(cell) = geojson["features"]
            .as_array_mut()
            .and_then(|f| f.iter_mut().find(|f| f["properties"]["role"] == "cell"))
        {
            cell["properties"]["source"] = json!(source.name());
            cell["properties"]["saved"] = json!(source.saved());
        }
        return Ok((geojson.to_string(), nearby.len()));
    }

    let data = json!({
        "target": cell_json(target),
        "cells": nearby.iter().map(|c| cell_json(c)).collect::<Vec<_>>(),
        "generated": target.generated,
        "note": source.note(),
        // the map's initial propagation model; it can be switched on the page
        "propagation": model.propagation.name(),
    });

    // the JSON goes inside a <script> element
    let data = data.to_string().replace("</", "<\\/");
    let html = HEATMAP_TEMPLATE.replacen("/*DATA*/null", &data, 1);
    Ok((html, nearby.len()))
}

/// A circle of `radius_m` around a point as a GeoJSON polygon geometry (64 sides).
fn accuracy_circle(lat: f64, lon: f64, radius_m: f64) -> serde_json::Value {
    let m_lon = 111_320.0 * lat.to_radians().cos();
    let ring: Vec<[f64; 2]> = (0..=64)
        .map(|i| {
            let a = std::f64::consts::TAU * f64::from(i % 64) / 64.0;
            let round = |v: f64| (v * 1e7).round() / 1e7;
            [round(lon + radius_m * a.cos() / m_lon), round(lat + radius_m * a.sin() / 111_320.0)]
        })
        .collect();
    json!({"type": "Polygon", "coordinates": [ring]})
}

/// GeoJSON FeatureCollection: the cell (role "cell"), its accuracy circle ("accuracy")
/// and nearby cells of the same operator ("nearby") with distance and bearing from it.
fn cell_geojson(
    mcc: i32,
    mnc: i32,
    target: &NearbyCell,
    nearby: &[&NearbyCell],
    accuracy: Option<serde_json::Value>,
    coverage: &ModelledCoverage,
) -> Result<serde_json::Value> {
    let properties = |c: &NearbyCell, role: &str| {
        let mut p = json!({
            "role": role,
            "mcc": mcc, "mnc": mnc, "tac_id": c.tac_id, "cell_id": c.cell_id,
            "site": c.cell_id >> 8, "sector": c.cell_id & 255,
            "acc_m": c.acc, "earfcn": c.earfcn, "pci": c.pci, "unknown_12": c.unknown_12,
            "crawled_at": c.crawled_at, "azimuth_deg": c.azimuth_deg,
        });
        if role == "nearby" {
            p["distance_m"] = json!(c.distance_m.round());
            p["bearing_deg"] = json!(c.bearing_deg.map(|b| b.round()));
            p["same_frequency"] = match (c.earfcn, target.earfcn) {
                (Some(a), Some(b)) => json!(a == b),
                _ => serde_json::Value::Null,
            };
        }
        p
    };
    let point = |c: &NearbyCell, role: &str| {
        json!({
            "type": "Feature",
            "geometry": {"type": "Point", "coordinates": [c.lon, c.lat]},
            "properties": properties(c, role),
        })
    };

    let mut cell = point(target, "cell");
    let m = &coverage.model;
    cell["properties"]["coverage"] = json!({
        "area_km2": (coverage.result.area_km2 * 1000.0).round() / 1000.0,
        "reach_m": coverage.result.reach_m.round(),
        "competing_cells": coverage.competitors.len() - 1,
        "same_frequency_only": coverage.same_frequency_only,
        "range_m": coverage.result.range_m.round(),
        "range_basis": coverage.result.range_basis,
        "propagation": m.propagation.name(),
        "reach_from_unknown_12": m.use_unknown_12,
        "directional": m.directional,
        "target_reach_m": coverage.result.target_reach_m.map(f64::round),
        "path_loss": m.path_loss,
        "sigma_db": m.sigma_db,
        "acc_weight": m.acc_weight,
        "hex_res": u8::from(coverage.result.resolution),
        "contour_levels": m.contours.then_some(&m.contour_levels),
    });
    // drawn in order: coverage underneath, then the accuracy circle, nearby cells, the cell
    let mut features = vec![];
    for band in &coverage.bands {
        let round = |v: f64| (v * 1e7).round() / 1e7;
        let polygons: Vec<Vec<Vec<[f64; 2]>>> = band
            .geometry()
            .0
            .iter()
            .map(|p| {
                std::iter::once(p.exterior())
                    .chain(p.interiors())
                    .map(|ring| ring.coords().map(|c| [round(c.x), round(c.y)]).collect())
                    .collect()
            })
            .collect();
        let (low, high) = (band.min_v(), band.max_v().min(1.0));
        let middle = (low + high) / 2.0;
        features.push(json!({
            "type": "Feature",
            "geometry": {"type": "MultiPolygon", "coordinates": polygons},
            "properties": {
                "role": "contour",
                "likelihood_min": low,
                "likelihood_max": high,
                "label": format!("{:.0}–{:.0}%", low * 100.0, high * 100.0),
                "fill": ramp_color(middle),
                "fill-opacity": heat_opacity(middle),
                "stroke-width": 0,
                "stroke-opacity": 0,
            },
        }));
    }
    for hex in coverage.result.hexes.iter().filter(|_| !m.contours) {
        let mut ring: Vec<[f64; 2]> = hex.index.boundary().iter().map(|ll| [ll.lng(), ll.lat()]).collect();
        ring.push(ring[0]);
        let best = coverage.competitors[hex.best];
        features.push(json!({
            "type": "Feature",
            "geometry": {"type": "Polygon", "coordinates": [ring]},
            "properties": {
                "role": "coverage",
                "h3": hex.index.to_string(),
                "likelihood": (hex.likelihood * 1000.0).round() / 1000.0,
                "signal_db": (hex.signal_db * 10.0).round() / 10.0,
                "most_likely_cell_id": best.cell_id,
                "most_likely_tac_id": best.tac_id,
                "most_likely_likelihood": (hex.best_likelihood * 1000.0).round() / 1000.0,
                "fill": ramp_color(hex.likelihood),
                "fill-opacity": heat_opacity(hex.likelihood),
                "stroke-width": 0,
                "stroke-opacity": 0,
            },
        }));
    }
    if let Some(circle) = accuracy {
        features.push(json!({
            "type": "Feature",
            "geometry": circle,
            "properties": {
                "role": "accuracy", "cell_id": target.cell_id, "radius_m": target.acc,
                "stroke": STYLE_TARGET, "stroke-width": 1.5, "stroke-opacity": 1, "fill-opacity": 0,
            },
        }));
    }
    for c in nearby {
        let mut feature = point(c, "nearby");
        let other_frequency = coverage.same_frequency_only && c.earfcn != target.earfcn;
        let style = feature["properties"].as_object_mut().unwrap();
        style.insert("marker-color".into(), json!(if other_frequency { STYLE_MUTED } else { STYLE_CELL }));
        style.insert("marker-size".into(), json!("small"));
        features.push(feature);
    }
    if let (true, Some(azimuth)) = (m.directional, target.azimuth_deg) {
        // a line from the cell in its estimated antenna direction, a quarter of the range it
        // fades out at (at least 300 m), as on the HTML map
        let fade = coverage.result.target_reach_m.unwrap_or(coverage.result.range_m);
        let length = (fade / 4.0).max(300.0);
        let rad = azimuth.to_radians();
        let end = [
            target.lon + length * rad.sin() / (111_320.0 * target.lat.to_radians().cos()),
            target.lat + length * rad.cos() / 111_320.0,
        ];
        features.push(json!({
            "type": "Feature",
            "geometry": {"type": "LineString", "coordinates": [[target.lon, target.lat], end]},
            "properties": {
                "role": "antenna", "cell_id": target.cell_id, "azimuth_deg": azimuth,
                "stroke": STYLE_TARGET, "stroke-width": 2, "stroke-opacity": 1,
            },
        }));
    }
    let style = cell["properties"].as_object_mut().unwrap();
    style.insert("marker-color".into(), json!(STYLE_TARGET));
    style.insert("marker-size".into(), json!("medium"));
    features.push(cell);
    Ok(json!({"type": "FeatureCollection", "features": features}))
}

// Styling uses the simplestyle properties (fill, fill-opacity, marker-color, ...) that
// geojson.io and GitHub's map preview apply automatically; colours match the HTML map.
const STYLE_TARGET: &str = "#eb6834";
const STYLE_CELL: &str = "#52514e";
const STYLE_MUTED: &str = "#898781";
const HEAT_RAMP: [[u8; 3]; 13] = [
    [0xcd, 0xe2, 0xfb], [0xb7, 0xd3, 0xf6], [0x9e, 0xc5, 0xf4], [0x86, 0xb6, 0xef],
    [0x6d, 0xa7, 0xec], [0x55, 0x98, 0xe7], [0x39, 0x87, 0xe5], [0x2a, 0x78, 0xd6],
    [0x25, 0x6a, 0xbf], [0x1c, 0x5c, 0xab], [0x18, 0x4f, 0x95], [0x10, 0x42, 0x81],
    [0x0d, 0x36, 0x6b],
];

/// light-to-dark blue for a value in 0..=1, interpolated between ramp steps
fn ramp_color(value: f64) -> String {
    let f = value.clamp(0.0, 1.0) * (HEAT_RAMP.len() - 1) as f64;
    let i = (f.floor() as usize).min(HEAT_RAMP.len() - 2);
    let u = f - i as f64;
    let mix = |c: usize| (HEAT_RAMP[i][c] as f64 * (1.0 - u) + HEAT_RAMP[i + 1][c] as f64 * u).round() as u8;
    format!("#{:02x}{:02x}{:02x}", mix(0), mix(1), mix(2))
}

/// weak values fade into the basemap, as on the HTML map: from nothing at 2% up to 0.265
/// at 10%, then 0.2 + 0.65 v, at most 0.8
fn heat_opacity(value: f64) -> f64 {
    if value < 0.02 {
        return 0.0;
    }
    let opacity = if value < 0.1 { 2.65 * value } else { (0.2 + 0.65 * value.min(1.0)).min(0.8) };
    (opacity * 100.0).round() / 100.0
}

struct ModelledCoverage<'a> {
    model: ModelArgs,
    /// the target first, then the cells it competes with
    competitors: Vec<&'a NearbyCell>,
    same_frequency_only: bool,
    result: coverage::Coverage,
    /// likelihood bands, with --contours
    bands: Vec<contour::Band>,
}

/// Run the coverage model; unless asked otherwise, the cell only competes with cells known
/// to be on its own EARFCN, because operators run several frequency layers over one area.
fn model_coverage<'a>(
    target: &'a NearbyCell,
    nearby: &[&'a NearbyCell],
    model: &ModelArgs,
) -> Result<ModelledCoverage<'a>> {
    let same_frequency_only = !model.all_frequencies && target.earfcn.is_some();
    let mut competitors = vec![target];
    competitors.extend(
        nearby
            .iter()
            .filter(|c| !same_frequency_only || c.earfcn == target.earfcn),
    );
    let cells: Vec<coverage::ModelCell> = competitors
        .iter()
        .map(|c| coverage::ModelCell {
            lat: c.lat,
            lon: c.lon,
            acc: c.acc,
            reach: c.unknown_12,
            azimuth_deg: c.azimuth_deg,
            earfcn: c.earfcn,
        })
        .collect();
    let params = coverage::Params {
        path_loss: model.path_loss,
        sigma_db: model.sigma_db,
        acc_weight: model.acc_weight,
        range_m: model.range_km.map(|km| km * 1000.0),
        use_reach: model.use_unknown_12,
        directional: model.directional,
        propagation: model.propagation,
        resolution: model.hex_res.map(Resolution::try_from).transpose()?,
    };
    let levels = &model.contour_levels;
    if model.contours
        && (levels.is_empty()
            || levels.iter().any(|l| !(0.0..1.0).contains(l) || *l == 0.0)
            || levels.windows(2).any(|w| w[0] >= w[1]))
    {
        anyhow::bail!("--contour-levels must be ascending values between 0 and 1, e.g. 0.1,0.5,0.9");
    }
    let bands = if model.contours {
        coverage::likelihood_bands(&cells, &params, levels, model.contour_step_m.max(1.0))?
    } else {
        vec![]
    };
    Ok(ModelledCoverage {
        model: model.clone(),
        same_frequency_only,
        result: coverage::compute(&cells, &params)?,
        competitors,
        bands,
    })
}

fn database_url() -> Result<String> {
    dotenvy::var("DATABASE_URL").context(
        "DATABASE_URL is not set; export it or put it in a .env file in the current directory, \
         e.g. DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/wtfps",
    )
}

fn osm(Location { lat, lon, .. }: &Location) -> String {
    format!("https://www.openstreetmap.org/search?query={lat}%2F{lon}#map=16/{lat}/{lon}")
}
