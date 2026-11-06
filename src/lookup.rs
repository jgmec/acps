//! Getting a cell's heatmap input when it may not be crawled yet: from the database,
//! from Apple (and then saved), or straight from Apple's replies without a database.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use sqlx::PgPool;

use crate::fetch::{AppleWPSClient, Location};
use crate::protobuf::CellTower;
use crate::{cell_key, commit_cells, merge_found, nearby_from_db, query_cells, CellKey, NearbyCell};

/// Sector numbers tried around the cell's own on its site. Apple's reply for a cell often
/// lacks its own site's other sectors, which antenna directions need; operators number a
/// site's sectors consecutively (1, 2, 3 or 61, 62, 63), so the nearest numbers usually hit.
const SIBLING_OFFSETS: [i64; 4] = [-2, -1, 1, 2];

/// The cell isn't in the database (with fetching off), or Apple has no location for it.
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotFound {}

/// Ask Apple for the cell and for its site's neighbouring sector numbers. Fails if the
/// request for the cell itself fails; guesses that fail or don't exist are dropped.
pub async fn fetch_with_siblings(
    client: &AppleWPSClient,
    key: CellKey,
) -> Result<Vec<(CellTower, Option<Location>)>> {
    let (mcc, mnc, tac_id, cell_id) = key;
    let (site, sector) = (cell_id >> 8, cell_id & 255);
    let mut keys = vec![key];
    keys.extend(
        SIBLING_OFFSETS
            .iter()
            .map(|offset| sector + offset)
            .filter(|s| (0..=255).contains(s))
            .map(|s| (mcc, mnc, tac_id, (site << 8) | s)),
    );

    let mut results = vec![];
    for (asked, reply) in query_cells(client, &keys).await {
        match reply {
            // a guessed sibling that doesn't exist comes back as its own not-found entry;
            // drop it so it isn't recorded as a real cell Apple doesn't know
            Ok(reply) => results.extend(
                reply
                    .into_iter()
                    .filter(|(cell, loc)| asked == key || loc.is_some() || cell_key(cell) != asked),
            ),
            Err(e) if asked == key => {
                return Err(e.context(format!("asking Apple for cell {mcc}/{mnc}/{tac_id}/{cell_id}")));
            }
            Err(e) => warn!("sibling sector {asked:?} failed: {e:#}"),
        }
    }
    Ok(results)
}

/// How the heatmap input was obtained, for the caller to report.
pub enum Source {
    Database,
    /// fetched from Apple and saved; the number of cells with a location in the replies
    Fetched(usize),
}

/// The cell and its nearby cells from the database. If the cell isn't crawled yet and
/// `fetch` is set, ask Apple for it and its sibling sectors, save every cell in the
/// replies (as the crawler would; nothing is removed) and read again.
pub async fn nearby_or_fetch(
    client: &AppleWPSClient,
    pool: &PgPool,
    key: CellKey,
    radius_km: f64,
    fetch: bool,
) -> Result<(Vec<NearbyCell>, Source)> {
    let (mcc, mnc, tac_id, cell_id) = key;
    let is_target = |c: &NearbyCell| (c.tac_id, c.cell_id) == (tac_id, cell_id);
    let rows = nearby_from_db(pool, key, radius_km).await?;
    if rows.iter().any(is_target) {
        return Ok((rows, Source::Database));
    }
    if !fetch {
        return Err(NotFound(format!(
            "cell {mcc}/{mnc}/{tac_id}/{cell_id} is not in the database (and fetching from Apple is off)"
        ))
        .into());
    }

    let results = fetch_with_siblings(client, key).await?;
    // replies overlap, so count each cell once
    let found = results
        .iter()
        .filter(|(c, loc)| loc.is_some() && c.cell_id != u32::MAX)
        .map(|(c, _)| cell_key(c))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    // the cell counts as queried: its area is marked crawled, or it's recorded as not found
    commit_cells(pool, &[key], Default::default(), results).await?;

    let rows = nearby_from_db(pool, key, radius_km).await?;
    if !rows.iter().any(is_target) {
        return Err(NotFound(format!(
            "Apple has no location for cell {mcc}/{mnc}/{tac_id}/{cell_id} (recorded in cell_not_found)"
        ))
        .into());
    }
    Ok((rows, Source::Fetched(found)))
}

/// The cell and its nearby cells straight from Apple's replies, without a database:
/// same operator, within `radius_km`, nearest first, at most `max_cells`.
pub fn nearby_from_replies(
    key: CellKey,
    radius_km: f64,
    max_cells: usize,
    results: Vec<(CellTower, Option<Location>)>,
) -> Result<Vec<NearbyCell>> {
    let (mcc, mnc, tac_id, cell_id) = key;
    let (found, _) = merge_found(results);
    let Some((target, ..)) = found.get(&key) else {
        return Err(NotFound(format!("Apple has no location for cell {mcc}/{mnc}/{tac_id}/{cell_id}")).into());
    };
    let (target_lat, target_lon) = (target.lat, target.lon);
    let (crawled_at, generated) = utc_now();

    let mut rows: Vec<NearbyCell> = found
        .iter()
        .filter(|((c_mcc, c_mnc, ..), _)| (*c_mcc, *c_mnc) == (mcc, mnc))
        .filter_map(|(&(_, _, tac_id, cell_id), (loc, earfcn, pci))| {
            let distance_m = distance_m(target_lat, target_lon, loc.lat, loc.lon);
            (distance_m <= radius_km * 1000.0).then(|| NearbyCell {
                tac_id,
                cell_id,
                lat: loc.lat,
                lon: loc.lon,
                acc: Some(loc.acc),
                earfcn: *earfcn,
                pci: *pci,
                distance_m,
                bearing_deg: (distance_m > 0.0)
                    .then(|| bearing_deg(target_lat, target_lon, loc.lat, loc.lon)),
                unknown_12: loc.unknown_12,
                crawled_at: Some(crawled_at.clone()),
                azimuth_deg: None,
                generated: generated.clone(),
            })
        })
        .collect();
    rows.sort_by(|a, b| a.distance_m.total_cmp(&b.distance_m).then(a.cell_id.cmp(&b.cell_id)));
    rows.truncate(max_cells);
    Ok(rows)
}

/// Great-circle distance in metres.
fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * a.sqrt().asin()
}

/// Initial bearing from the first point to the second, degrees clockwise from north.
fn bearing_deg(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2, dl) = (lat1.to_radians(), lat2.to_radians(), (lon2 - lon1).to_radians());
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// Now in UTC as ISO 8601 ("2026-11-06T09:30:00Z") and as shown on the map
/// ("2026-11-06 09:30 UTC").
fn utc_now() -> (String, String) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s) = (rest / 3600, rest % 3600 / 60, rest % 60);
    (
        format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z"),
        format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02} UTC"),
    )
}
