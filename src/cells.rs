//! Cell exploration: querying cells from the seed queue, from uncrawled areas and by
//! probing guessed cell IDs near known sites, and storing every cell Apple returns.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use futures::{stream, StreamExt};
use h3o::{LatLng, Resolution};
use sqlx::PgPool;

use crate::fetch::{AppleWPSClient, Location};
use crate::protobuf::CellTower;

pub(crate) const CONCURRENT_PARTS: usize = 8;

pub(crate) const SENTINEL_CELL_ID: u32 = u32::MAX;

pub(crate) type CellKey = (i32, i32, i32, i64);

pub(crate) type CellArea = (i32, i32, i64);

/// One H3 area (~5 km²) per operator is crawled; a single cell request returns
/// its neighbours, so querying every discovered cell would mostly return known cells.
pub(crate) const CELL_CRAWL_RES: Resolution = Resolution::Seven;

pub(crate) const CELL_BATCH: i64 = 256;

pub(crate) fn cell_key(cell: &CellTower) -> CellKey {
    (
        cell.mcc as i32,
        cell.mnc as i32,
        cell.tac_id as i32,
        cell.cell_id as i64,
    )
}

/// EARFCN / PCI as reported by Apple; u32::MAX (-1) means unknown
pub(crate) fn radio_value(value: Option<u32>) -> Option<i32> {
    value.filter(|&v| v != u32::MAX).and_then(|v| i32::try_from(v).ok())
}

pub(crate) fn cell_area(lat: f64, lon: f64) -> Result<i64> {
    Ok(u64::from(LatLng::new(lat, lon)?.to_cell(CELL_CRAWL_RES)) as i64)
}

pub(crate) type CellReply = Vec<(CellTower, Option<Location>)>;

/// Query cells concurrently; each key is returned with its own result.
pub(crate) async fn query_cells(client: &AppleWPSClient, keys: &[CellKey]) -> Vec<(CellKey, Result<CellReply>)> {
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

pub(crate) async fn explore_cells(client: &AppleWPSClient, pool: &PgPool, probe: bool) -> Result<()> {
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
            // (mcc, mnc, area, and the TAC and cell ID of a random known cell in it)
            type Picked = (i32, i32, i64, Option<i32>, Option<i64>);
            let picked: Vec<Picked> = sqlx::query_as(
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
pub(crate) type FoundCell = (Location, Option<i32>, Option<i32>);

/// Cells from one or more replies: those with a location (merged when a cell appears in
/// several replies) and those without one. Tracking-area estimates are dropped.
pub(crate) fn merge_found(
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

pub(crate) async fn commit_cells(
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

/// Known sites probed per round; each yields up to 4 neighbour sites x 2 sectors x its TACs,
/// plus its own missing sector numbers next to the known ones x its TACs.
pub(crate) const PROBE_SITES: i64 = 32;

pub(crate) const PROBE_SITE_OFFSETS: [i64; 4] = [-2, -1, 1, 2];

pub(crate) const PROBE_SECTORS: usize = 2;

/// Sector numbers tried around a site's known ones. Apple's replies often lack some of a
/// site's sectors; operators number them consecutively (1, 2, 3 or 11, 12, 13), so the
/// nearest numbers usually hit.
pub(crate) const SIBLING_OFFSETS: [i64; 4] = [-2, -1, 1, 2];

/// Sector numbers (0-255) within SIBLING_OFFSETS of a known one that aren't known themselves.
pub(crate) fn sibling_sectors(known: &BTreeSet<i64>) -> BTreeSet<i64> {
    known
        .iter()
        .flat_map(|sector| SIBLING_OFFSETS.iter().map(move |offset| sector + offset))
        .filter(|s| (0..=255).contains(s) && !known.contains(s))
        .collect()
}

/// Step between an operator's site numbers: 10 if (of at least 10 sites sampled) 80% or
/// more are multiples of 10, else 1.
pub(crate) fn site_step(sampled: i64, multiples_of_ten: i64) -> i64 {
    if sampled >= 10 && multiples_of_ten * 10 >= sampled * 8 { 10 } else { 1 }
}

/// Site numbers guessed around a known site.
pub(crate) fn neighbour_sites(site: i64, step: i64) -> impl Iterator<Item = i64> {
    PROBE_SITE_OFFSETS.iter().map(move |offset| site + offset * step).filter(|&s| s >= 0)
}

/// What a probe guess is: a sector of a neighbouring, unknown site number, or a missing
/// sector of a known site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Guess {
    NeighbourSite,
    Sibling,
}

/// Probe guesses for one known site from its cells (TAC, cell ID): neighbouring site
/// numbers with its first sectors (if `neighbours`), and its own missing sectors next to the
/// known ones (if `siblings`), each in every TAC the site uses.
pub(crate) fn site_guesses(
    site: i64,
    cells: &[(i32, i64)],
    step: i64,
    neighbours: bool,
    siblings: bool,
) -> Vec<(i32, i64, Guess)> {
    let tacs: BTreeSet<i32> = cells.iter().map(|&(tac_id, _)| tac_id).collect();
    let sectors: BTreeSet<i64> = cells.iter().map(|&(_, cell_id)| cell_id & 255).collect();
    let mut guesses = vec![];
    for &tac_id in &tacs {
        if neighbours {
            for neighbour in neighbour_sites(site, step) {
                for &sector in sectors.iter().take(PROBE_SECTORS) {
                    guesses.push((tac_id, (neighbour << 8) | sector, Guess::NeighbourSite));
                }
            }
        }
        if siblings {
            for sector in sibling_sectors(&sectors) {
                guesses.push((tac_id, (site << 8) | sector, Guess::Sibling));
            }
        }
    }
    guesses
}

pub(crate) fn key_columns(keys: &[CellKey]) -> (Vec<i32>, Vec<i32>, Vec<i32>, Vec<i64>) {
    let (mut mccs, mut mncs, mut tacs, mut cell_ids) = (vec![], vec![], vec![], vec![]);
    for &(mcc, mnc, tac_id, cell_id) in keys {
        mccs.push(mcc);
        mncs.push(mnc);
        tacs.push(tac_id);
        cell_ids.push(cell_id);
    }
    (mccs, mncs, tacs, cell_ids)
}

/// Guess cells for a batch of known sites (an LTE cell id is site * 256 + sector): at site
/// numbers next to each site, and the site's own missing sector numbers next to its known
/// ones; store whatever the hits return. Returns false once every known site has been
/// probed both ways.
pub(crate) async fn probe_cells(client: &AppleWPSClient, pool: &PgPool) -> Result<bool> {
    // (mcc, mnc, site, neighbour sites still to guess, own sectors still to guess)
    let sites: Vec<(i32, i32, i64, bool, bool)> = sqlx::query_as(
        "select mcc, mnc, site, probed_at is null, siblings_probed_at is null from cell_site \
         where probed_at is null or siblings_probed_at is null limit $1",
    )
    .bind(PROBE_SITES)
    .fetch_all(pool)
    .await?;
    if sites.is_empty() {
        return Ok(false);
    }

    // some operators number their sites in tens
    let mut steps = BTreeMap::new();
    for &(mcc, mnc, ..) in &sites {
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
        steps.insert((mcc, mnc), site_step(total, tens));
    }

    // guesses reuse the source site's TACs and sectors; remember the source of each guess
    let mut guesses = BTreeMap::new();
    for &(mcc, mnc, site, neighbours, siblings) in &sites {
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
        for (tac_id, cell_id, kind) in site_guesses(site, &cells, steps[&(mcc, mnc)], neighbours, siblings) {
            guesses.entry((mcc, mnc, tac_id, cell_id)).or_insert(((mcc, mnc, site), kind));
        }
    }

    // skip guesses tried before; at neighbouring site numbers, sites we already know; for
    // a site's own sectors, cells we already know (in any TAC)
    let keys: Vec<CellKey> = guesses.keys().copied().collect();
    let is_sibling: Vec<bool> = guesses.values().map(|&(_, kind)| kind == Guess::Sibling).collect();
    let (mccs, mncs, tacs, cell_ids) = key_columns(&keys);
    let skip: Vec<CellKey> = sqlx::query_as(
        "select t.mcc, t.mnc, t.tac_id, t.cell_id \
         from unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[], $5::bool[]) \
             as t(mcc, mnc, tac_id, cell_id, sibling) \
         where (not t.sibling and exists (select 1 from cell_site s \
                       where (s.mcc, s.mnc, s.site) = (t.mcc, t.mnc, t.cell_id >> 8))) \
            or (t.sibling and exists (select 1 from cell c \
                       where (c.mcc, c.mnc, c.cell_id) = (t.mcc, t.mnc, t.cell_id))) \
            or exists (select 1 from cell_probe p \
                       where (p.mcc, p.mnc, p.tac_id, p.cell_id) = (t.mcc, t.mnc, t.tac_id, t.cell_id))",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&tacs)
    .bind(&cell_ids)
    .bind(&is_sibling)
    .fetch_all(pool)
    .await?;
    for key in skip {
        guesses.remove(&key);
    }

    let keys: Vec<CellKey> = guesses.keys().copied().collect();
    let siblings = guesses.values().filter(|&&(_, kind)| kind == Guess::Sibling).count();
    info!(
        "probing {} guessed cells near {} sites ({} of them missing sectors of known sites)...",
        keys.len(),
        sites.len(),
        siblings
    );
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
                failed_sites.insert(guesses[&key].0);
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
    for &(mcc, mnc, site, ..) in sites.iter().filter(|s| !failed_sites.contains(&(s.0, s.1, s.2))) {
        mccs.push(mcc);
        mncs.push(mnc);
        site_ids.push(site);
    }
    sqlx::query(
        "update cell_site s set probed_at = coalesce(s.probed_at, current_timestamp), \
             siblings_probed_at = coalesce(s.siblings_probed_at, current_timestamp) \
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
pub(crate) async fn backfill_cell_areas(pool: &PgPool) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf;

    fn tower(mnc: u32, tac_id: u32, cell_id: u32, earfcn: Option<u32>) -> CellTower {
        CellTower { mcc: 284, mnc, tac_id, cell_id, location: None, uarfcn: earfcn, pid: None }
    }

    fn location(lat: f64) -> Option<Location> {
        Some(Location { lat, lon: 23.3, acc: 500, alt: None, alt_acc: None, unknown_11: None, unknown_12: None })
    }

    #[test]
    fn radio_value_treats_minus_one_as_unknown() {
        assert_eq!(radio_value(Some(3350)), Some(3350));
        assert_eq!(radio_value(Some(u32::MAX)), None);
        assert_eq!(radio_value(None), None);
        // too large for the database column
        assert_eq!(radio_value(Some(u32::MAX - 1)), None);
    }

    #[test]
    fn merge_found_drops_area_estimates_and_merges_repeats() {
        let results = vec![
            (tower(5, 106, SENTINEL_CELL_ID, None), location(42.6)),
            (tower(5, 106, 451645, None), location(42.7)),
            // the same cell in another reply, now with its EARFCN
            (tower(5, 106, 451645, Some(3350)), location(42.8)),
            // not found in one reply, found in another: counts as found
            (tower(5, 106, 451646, None), None),
            (tower(5, 106, 451646, None), location(42.9)),
            (tower(5, 106, 451647, None), None),
        ];
        let (found, not_found) = merge_found(results);
        assert_eq!(found.len(), 2);
        let (loc, earfcn, pci) = &found[&(284, 5, 106, 451645)];
        // the first location is kept, radio details from any sighting
        assert_eq!((loc.lat, *earfcn, *pci), (42.7, Some(3350), None));
        assert!(found.contains_key(&(284, 5, 106, 451646)));
        assert_eq!(not_found.into_iter().collect::<Vec<_>>(), vec![(284, 5, 106, 451647)]);
    }

    #[test]
    fn cell_key_widens_without_sign_changes() {
        let key = cell_key(&protobuf::CellTower { cell_id: u32::MAX - 1, ..tower(5, 106, 0, None) });
        assert_eq!(key, (284, 5, 106, i64::from(u32::MAX - 1)));
    }

    #[test]
    fn cell_area_is_an_h3_resolution_7_cell() {
        let area = cell_area(42.6977, 23.3219).unwrap();
        let index = h3o::CellIndex::try_from(area as u64).unwrap();
        assert_eq!(index.resolution(), CELL_CRAWL_RES);
        // nearby points share it, far ones don't
        assert_eq!(cell_area(42.6978, 23.3220).unwrap(), area);
        assert_ne!(cell_area(42.15, 24.75).unwrap(), area);
        assert!(cell_area(f64::NAN, 0.0).is_err());
    }

    #[test]
    fn sibling_sectors_fill_gaps_and_edges() {
        let known: BTreeSet<i64> = [11, 12, 13].into();
        assert_eq!(sibling_sectors(&known), [9, 10, 14, 15].into());
        // only 0-255
        assert_eq!(sibling_sectors(&[0].into()), [1, 2].into());
        assert_eq!(sibling_sectors(&[255].into()), [253, 254].into());
        // a gap between known sectors is filled once
        assert_eq!(sibling_sectors(&[1, 3].into()), [0, 2, 4, 5].into());
        assert!(sibling_sectors(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn site_step_detects_numbering_in_tens() {
        assert_eq!(site_step(100, 85), 10);
        assert_eq!(site_step(100, 50), 1);
        // too few sites to tell
        assert_eq!(site_step(5, 5), 1);
        assert_eq!(neighbour_sites(1764, 1).collect::<Vec<_>>(), [1762, 1763, 1765, 1766]);
        assert_eq!(neighbour_sites(10, 10).collect::<Vec<_>>(), [0, 20, 30]);
    }

    #[test]
    fn site_guesses_cover_neighbours_and_siblings() {
        // site 1764 with sectors 1 and 2 in TAC 106, and sector 1 again in TAC 107
        let cells = [(106, (1764 << 8) | 1), (106, (1764 << 8) | 2), (107, (1764 << 8) | 1)];
        let guesses = site_guesses(1764, &cells, 1, true, true);
        let siblings: BTreeSet<(i32, i64)> = guesses
            .iter()
            .filter(|g| g.2 == Guess::Sibling)
            .map(|&(tac, cell, _)| (tac, cell & 255))
            .collect();
        // sectors 0, 3 and 4 in both TACs
        assert_eq!(siblings, [(106, 0), (106, 3), (106, 4), (107, 0), (107, 3), (107, 4)].into());
        assert!(guesses.iter().filter(|g| g.2 == Guess::Sibling).all(|g| g.1 >> 8 == 1764));

        let neighbours: Vec<_> = guesses.iter().filter(|g| g.2 == Guess::NeighbourSite).collect();
        // 4 sites x 2 sectors x 2 TACs
        assert_eq!(neighbours.len(), 16);
        assert!(neighbours.iter().all(|g| g.1 >> 8 != 1764 && [1, 2].contains(&(g.1 & 255))));

        assert!(site_guesses(1764, &cells, 1, false, true).iter().all(|g| g.2 == Guess::Sibling));
        assert!(site_guesses(1764, &cells, 1, true, false).iter().all(|g| g.2 == Guess::NeighbourSite));
        assert!(site_guesses(1764, &[], 1, true, true).is_empty());
    }
}
