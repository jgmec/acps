//! 2G GSM and 3G UMTS cells (Apple's request and reply field 1). They're stored in `cell` with
//! radio 'gsm', the location area code (LAC) in `tac_id`; a 3G cell ID is the full 28-bit one
//! (RNC x 65536 + CID).
//!
//! Apple's GSM replies contain only the asked cells: no nearby cells and no tracking-area
//! estimates, so the LTE crawl (following replies) finds nothing new. Instead the crawler asks
//! for whole blocks of GSM_BLOCK consecutive cell IDs, in one request each, around every known
//! GSM cell (its block and the two next to it, in its LAC). Blocks are recorded once scanned,
//! and the scan spreads as cells turn up in new blocks.

use std::collections::BTreeSet;

use anyhow::Result;
use futures::{stream, StreamExt};
use sqlx::PgPool;

use crate::cells::{cell_area, key_columns, merge_found, CellKey, CellReply, CONCURRENT_PARTS};
use crate::fetch::AppleWPSClient;
use crate::protobuf::GsmCellTower;

/// Cell IDs per block, and per request: Apple answers 500 GSM cells in one request as it
/// answers them one by one.
pub(crate) const GSM_BLOCK: i64 = 500;

/// Blocks scanned per crawler round (one request each).
const GSM_BLOCKS_PER_ROUND: i64 = 32;

/// Seeded GSM cells asked per crawler round.
const GSM_SEEDS_PER_ROUND: i64 = 2048;

/// A scanned block: (mcc, mnc, LAC, block number).
pub(crate) type GsmBlock = (i32, i32, i32, i64);

/// Ask Apple for GSM cells, GSM_BLOCK per request, CONCURRENT_PARTS requests at a time. Each
/// chunk of keys comes back with its own result.
pub(crate) async fn query_gsm_cells(client: &AppleWPSClient, keys: &[CellKey]) -> Vec<(Vec<CellKey>, Result<CellReply>)> {
    let requests: Vec<_> = keys
        .chunks(GSM_BLOCK as usize)
        .map(|chunk| async move {
            let cells: Result<Vec<GsmCellTower>> = chunk
                .iter()
                .map(|&(mcc, mnc, lac, cell_id)| {
                    Ok(GsmCellTower {
                        mcc: u32::try_from(mcc)?,
                        mnc: u32::try_from(mnc)?,
                        cell_id: u32::try_from(cell_id)?,
                        lac: u32::try_from(lac)?,
                        location: None,
                        unknown_11: None,
                        unknown_12: None,
                    })
                })
                .collect();
            let result = match cells {
                Ok(cells) => client
                    .query_gsm_cells(cells)
                    .await
                    .map(|reply| reply.into_iter().map(|(cell, loc)| (cell.into(), loc)).collect()),
                Err(e) => Err(e),
            };
            (chunk.to_vec(), result)
        })
        .collect();
    stream::iter(requests).buffer_unordered(CONCURRENT_PARTS).collect().await
}

/// The cell IDs of a block, in one LAC.
pub(crate) fn block_keys(&(mcc, mnc, lac, block): &GsmBlock) -> Vec<CellKey> {
    (block * GSM_BLOCK..(block + 1) * GSM_BLOCK).map(|cell_id| (mcc, mnc, lac, cell_id)).collect()
}

/// A cell's own block and the two next to it.
pub(crate) fn blocks_around(&(mcc, mnc, lac, cell_id): &CellKey) -> Vec<GsmBlock> {
    let block = cell_id / GSM_BLOCK;
    (block - 1..=block + 1).filter(|&b| b >= 0).map(|b| (mcc, mnc, lac, b)).collect()
}

/// Save what Apple returned for GSM requests: found cells into `cell` (radio 'gsm'); `seeds`
/// (asked one by one: seeds and heatmap lookups) missing from the replies into
/// `cell_not_found`, and out of `cell_todo`; `blocks` as scanned, with how many cells each held.
/// Cells in scanned blocks that Apple doesn't know aren't recorded: most IDs are unused.
pub(crate) async fn commit_gsm_cells(
    pool: &PgPool,
    seeds: &[CellKey],
    blocks: &[GsmBlock],
    results: CellReply,
) -> Result<()> {
    let (found, _) = merge_found(results);
    let not_found: Vec<CellKey> = seeds.iter().filter(|key| !found.contains_key(key)).copied().collect();

    let (mut mccs, mut mncs, mut lacs, mut cell_ids) = (vec![], vec![], vec![], vec![]);
    let (mut lats, mut lons, mut accs, mut areas) = (vec![], vec![], vec![], vec![]);
    let (mut unknown_11s, mut unknown_12s) = (vec![], vec![]);
    for (&(mcc, mnc, lac, cell_id), (loc, _, _)) in &found {
        mccs.push(mcc);
        mncs.push(mnc);
        lacs.push(lac);
        cell_ids.push(cell_id);
        lats.push(loc.lat);
        lons.push(loc.lon);
        accs.push(loc.acc);
        // set so the LTE area backfill leaves GSM cells alone; GSM areas aren't crawled
        areas.push(cell_area(loc.lat, loc.lon)?);
        unknown_11s.push(loc.unknown_11);
        unknown_12s.push(loc.unknown_12);
    }
    info!("{} GSM cells with location, {} not found, {} blocks scanned", found.len(), not_found.len(), blocks.len());

    let mut tx = pool.begin().await?;
    // as for LTE, a known cell keeps its position; crawled_at is refreshed at most hourly
    sqlx::query(
        "insert into cell (mcc, mnc, tac_id, cell_id, radio, geom, acc, crawl_area, unknown_11, unknown_12) \
         select mcc, mnc, lac, cell_id, 'gsm', ST_SetSRID(ST_MakePoint(lon, lat), 4326), acc, area, \
                unknown_11, unknown_12 \
         from unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[], $5::float8[], $6::float8[], \
                     $7::bigint[], $8::bigint[], $9::bigint[], $10::bigint[]) \
             as t(mcc, mnc, lac, cell_id, lat, lon, acc, area, unknown_11, unknown_12) \
         on conflict (mcc, mnc, tac_id, cell_id, radio) do update set \
             unknown_11 = coalesce(excluded.unknown_11, cell.unknown_11), \
             unknown_12 = coalesce(excluded.unknown_12, cell.unknown_12), \
             crawled_at = current_timestamp \
         where cell.crawled_at is null or cell.crawled_at < current_timestamp - interval '1 hour'",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&lacs)
    .bind(&cell_ids)
    .bind(&lats)
    .bind(&lons)
    .bind(&accs)
    .bind(&areas)
    .bind(&unknown_11s)
    .bind(&unknown_12s)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "delete from cell_not_found n \
         using unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[]) as t(mcc, mnc, lac, cell_id) \
         where (n.mcc, n.mnc, n.tac_id, n.cell_id) = (t.mcc, t.mnc, t.lac, t.cell_id) and n.radio = 'gsm'",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&lacs)
    .bind(&cell_ids)
    .execute(&mut *tx)
    .await?;

    let (mccs, mncs, lacs, cell_ids) = key_columns(&not_found);
    sqlx::query(
        "insert into cell_not_found (mcc, mnc, tac_id, cell_id, radio) \
         select *, 'gsm' from unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[]) \
         on conflict (mcc, mnc, tac_id, cell_id, radio) do update set checked_at = current_timestamp",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&lacs)
    .bind(&cell_ids)
    .execute(&mut *tx)
    .await?;

    let (mccs, mncs, lacs, cell_ids) = key_columns(seeds);
    sqlx::query(
        "delete from cell_todo c \
         using unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[]) as t(mcc, mnc, lac, cell_id) \
         where (c.mcc, c.mnc, c.tac_id, c.cell_id) = (t.mcc, t.mnc, t.lac, t.cell_id) and c.radio = 'gsm'",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&lacs)
    .bind(&cell_ids)
    .execute(&mut *tx)
    .await?;

    let (mut mccs, mut mncs, mut lacs, mut block_ids, mut counts) = (vec![], vec![], vec![], vec![], vec![]);
    for block @ &(mcc, mnc, lac, b) in blocks {
        mccs.push(mcc);
        mncs.push(mnc);
        lacs.push(lac);
        block_ids.push(b);
        let ids: BTreeSet<CellKey> = block_keys(block).into_iter().collect();
        counts.push(found.keys().filter(|key| ids.contains(key)).count() as i32);
    }
    sqlx::query(
        "insert into cell_gsm_block (mcc, mnc, lac, block, found) \
         select * from unnest($1::integer[], $2::integer[], $3::integer[], $4::bigint[], $5::integer[]) \
         on conflict (mcc, mnc, lac, block) do update set found = excluded.found, scanned_at = current_timestamp",
    )
    .bind(&mccs)
    .bind(&mncs)
    .bind(&lacs)
    .bind(&block_ids)
    .bind(&counts)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Ask Apple for GSM cells (in requests of GSM_BLOCK): what it returned, and the keys whose
/// request failed. Fails only if every request failed.
pub(crate) async fn fetch_gsm(client: &AppleWPSClient, keys: &[CellKey]) -> Result<(CellReply, BTreeSet<CellKey>)> {
    let mut results = vec![];
    let mut failed = BTreeSet::new();
    for (chunk, reply) in query_gsm_cells(client, keys).await {
        match reply {
            Ok(reply) => results.extend(reply),
            Err(e) => {
                warn!("GSM request for {} cells failed: {e:#}", chunk.len());
                failed.extend(chunk);
            }
        }
    }
    if !keys.is_empty() && failed.len() == keys.len() {
        anyhow::bail!("all GSM requests failed");
    }
    Ok((results, failed))
}

/// Ask Apple for seeded cells and whole blocks, and save the cells found. Seeds and blocks
/// whose request failed aren't recorded, so they're tried again later.
pub(crate) async fn scan_blocks(client: &AppleWPSClient, pool: &PgPool, seeds: &[CellKey], blocks: &[GsmBlock]) -> Result<()> {
    let mut keys: Vec<CellKey> = seeds.to_vec();
    keys.extend(blocks.iter().flat_map(block_keys));
    let (results, failed) = fetch_gsm(client, &keys).await?;
    let seeds: Vec<CellKey> = seeds.iter().filter(|k| !failed.contains(k)).copied().collect();
    let blocks: Vec<GsmBlock> =
        blocks.iter().filter(|b| block_keys(b).iter().all(|k| !failed.contains(k))).copied().collect();
    commit_gsm_cells(pool, &seeds, &blocks, results).await
}

/// One round of GSM crawling: seeded GSM cells (`cell_todo` rows with radio 'gsm'), else,
/// with `probe`, unscanned blocks around known GSM cells. Returns false when there's
/// nothing to do.
pub(crate) async fn gsm_step(client: &AppleWPSClient, pool: &PgPool, probe: bool) -> Result<bool> {
    let seeds: Vec<CellKey> =
        sqlx::query_as("select mcc, mnc, tac_id, cell_id from cell_todo where radio = 'gsm' limit $1")
            .bind(GSM_SEEDS_PER_ROUND)
            .fetch_all(pool)
            .await?;
    if !seeds.is_empty() {
        info!("asking for {} seeded GSM cells...", seeds.len());
        scan_blocks(client, pool, &seeds, &[]).await?;
        return Ok(true);
    }
    if !probe {
        return Ok(false);
    }

    let blocks: Vec<GsmBlock> = sqlx::query_as(
        "select distinct c.mcc, c.mnc, c.tac_id, b.block \
         from cell c cross join lateral (values (c.cell_id / $1 - 1), (c.cell_id / $1), (c.cell_id / $1 + 1)) as b(block) \
         where c.radio = 'gsm' and b.block >= 0 \
           and not exists (select 1 from cell_gsm_block g \
                           where (g.mcc, g.mnc, g.lac, g.block) = (c.mcc, c.mnc, c.tac_id, b.block)) \
         limit $2",
    )
    .bind(GSM_BLOCK)
    .bind(GSM_BLOCKS_PER_ROUND)
    .fetch_all(pool)
    .await?;
    if blocks.is_empty() {
        return Ok(false);
    }
    info!("scanning {} blocks of {GSM_BLOCK} GSM cell IDs...", blocks.len());
    scan_blocks(client, pool, &[], &blocks).await?;
    Ok(true)
}
