#[macro_use]
extern crate log;

use std::string::String;
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::{stream, StreamExt, TryStreamExt};
use h3o::{LatLng, Resolution};
use mac6::Mac;
use mac_address::MacAddress;
use serde_json::json;

use sqlx::{postgres::PgPoolOptions, query, query_scalar, PgPool};


mod fetch;
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
        mcc: String,
        mnc: String,
        tacid: String,
        cellid: String,
    },
    Oui {
        oui: String,
    },
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
                .connect(&dotenvy::var("DATABASE_URL")?)
                .await?;
            sqlx::migrate!().run(&pool).await?;

            match source {
                ExploreSource::Cell { no_probe } => {
                    explore_cells(&client, &pool, !no_probe).await?
                }

                _ => loop {
                    let mut crawling = BTreeSet::new();
                    let todo = match source {
                        ExploreSource::Crawl => {
                            let mut todo = Vec::new();
                            let mut tx = pool.begin().await?;
                            let areas = query_scalar!(
                                "select area from crawl where last_crawled is null order by area limit 256"
                            )
                            .fetch_all(&mut *tx)
                            .await?;
                            if areas.len() == 0 {
                                break;
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
                        }
                        ExploreSource::Todo { .. } => {
                            let todo = query_scalar!("select mac from todo limit 256")
                                .fetch_all(&pool)
                                .await?;
                            if todo.len() == 0 {
                                break;
                            }
                            todo
                        }
                        ExploreSource::Cell { .. } => unreachable!(),
                    };

                    let mut requests = Vec::new();
                    for mac in &todo {
                        requests.push(client.query_wifi(*mac));
                    }

                    let ignore_extra = match source {
                        ExploreSource::Todo { ignore_extra } => ignore_extra,
                        _ => false,
                    };

                    info!("making {} requests...", requests.len());
                    let mut requests = stream::iter(requests).buffer_unordered(256);
                    let mut locs = BTreeMap::new();
                    while let Some(response) = requests.try_next().await? {
                        for (mac, loc) in response {
                            if ignore_extra {
                                if !todo.contains(&mac) {
                                    continue;
                                }
                            }
                            locs.insert(mac, loc);
                        }
                    }
                    commit(pool.clone(), crawling, locs).await?;
                },
            }
        }
        Command::Query { mac } => {
            let wifis = client
                .query_wifi(Mac::from_str(&mac).unwrap().0.into())
                .await?;
            if let Some((_, Some(loc))) = wifis.first() {
                println!("{}", osm(loc));
                dbg!(wifis.len());
            } else {
                println!("not found")
            }
        }

        Command::Cell { mcc, mnc, tacid, cellid } =>{
            println!("cellid: {cellid}");

            let cells = client
                .query_cell(CellTower{
                    mcc: (&mcc).parse().unwrap(),
                    mnc: (&mnc).parse().unwrap(),
                    cell_id: (&cellid).parse().unwrap(),
                    tac_id: (&tacid).parse().unwrap(),
                    location: None,
                    uarfcn: None,
                    pid: None,
                })
                .await?;
            if let Some((_, Some(loc))) = cells.first() {
                println!("{}", osm(loc));
                dbg!(cells.len());
            } else {
                println!("not found")
            }
            for (cell, loc) in cells {
                print!("cellid {}", cell.cell_id);
                if let Some(raw_loc) = cell.location {
                    print!("\t\thorizontal_accuracy {:?}", raw_loc.horizontal_accuracy);
                }
                match loc {
                    Some(l) => println!("\t\tlocation {}", osm(&l)),
                    None => println!("\t\tnot found"),
                }
            }

        }

        Command::Oui { oui } => {
            for i in 0..256u64.pow(3) {
                println!("{oui}{i:06x}");
            }
        }
    }

    Ok(())
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

async fn commit_cells(
    pool: &PgPool,
    queried: &[CellKey],
    mut crawled: BTreeSet<CellArea>,
    results: Vec<(CellTower, Option<Location>)>,
) -> Result<()> {
    let mut found = BTreeMap::new();
    let mut not_found = BTreeSet::new();
    for (cell, loc) in results {
        // Apple includes entries with cell_id u32::MAX (-1): per-TAC location estimates
        // (~150km accuracy), not real cells, so don't store or crawl them.
        if cell.cell_id == SENTINEL_CELL_ID {
            continue;
        }
        match loc {
            Some(loc) => {
                found.insert(cell_key(&cell), loc);
            }
            None => {
                not_found.insert(cell_key(&cell));
            }
        }
    }
    // a queried cell missing from its own response is also not found
    not_found.extend(queried.iter().copied());
    not_found.retain(|key| !found.contains_key(key));

    // seeded cells mark their own area as crawled, too
    let mut areas = BTreeSet::new();
    let (mut mccs, mut mncs, mut tacs, mut cell_ids) = (vec![], vec![], vec![], vec![]);
    let (mut lats, mut lons, mut accs, mut cell_areas) = (vec![], vec![], vec![], vec![]);
    for (&key @ (mcc, mnc, tac_id, cell_id), loc) in &found {
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
    }
    areas.extend(crawled.iter().copied());

    info!(
        "{} cells with location, {} not found, {} areas crawled",
        found.len(),
        not_found.len(),
        crawled.len(),
    );

    let mut tx = pool.begin().await?;

    // cells stored before areas were tracked get their area and accuracy on the next sighting
    sqlx::query(
        "insert into cell (mcc, mnc, tac_id, cell_id, geom, acc, crawl_area) \
         select mcc, mnc, tac_id, cell_id, ST_SetSRID(ST_MakePoint(lon, lat), 4326), acc, area \
         from unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[], \
                     $5::float8[], $6::float8[], $7::bigint[], $8::bigint[]) \
             as t(mcc, mnc, tac_id, cell_id, lat, lon, acc, area) \
         on conflict (mcc, mnc, tac_id, cell_id) do update \
             set geom = excluded.geom, acc = excluded.acc, crawl_area = excluded.crawl_area \
             where cell.crawl_area is null",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&tacs)
    .bind(&cell_ids)
    .bind(&lats)
    .bind(&lons)
    .bind(&accs)
    .bind(&cell_areas)
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

fn osm(Location { lat, lon, .. }: &Location) -> String {
    format!("https://www.openstreetmap.org/search?query={lat}%2F{lon}#map=16/{lat}/{lon}")
}
