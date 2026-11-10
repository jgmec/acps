//! Getting a cell's heatmap input when it may not be crawled yet: from the database,
//! from Apple (and then saved), or straight from Apple's replies without a database.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use sqlx::PgPool;

use crate::fetch::AppleWPSClient;
use crate::cells::{cell_key, commit_cells, merge_found, query_cells, sibling_sectors, CellKey, CellReply, Radio, SENTINEL_CELL_ID};
use crate::gsm;
use crate::heatmap::{nearby_from_db, NearbyCell, HEATMAP_MAX_CELLS};

/// The cell isn't in the database (with fetching off), or Apple has no location for it.
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotFound {}

/// The cell and its site's neighbouring sector numbers (see `sibling_sectors`). Apple's
/// reply for a cell often lacks its own site's other sectors, which antenna directions need.
fn with_siblings(key: CellKey) -> Vec<CellKey> {
    let (mcc, mnc, tac_id, cell_id) = key;
    let site = cell_id >> 8;
    let mut keys = vec![key];
    keys.extend(sibling_sectors(&[cell_id & 255].into()).into_iter().map(|s| (mcc, mnc, tac_id, (site << 8) | s)));
    keys
}

/// A 5G cell and its neighbouring cell numbers (±1, ±2). How a 5G cell ID splits into site
/// and sector differs per operator, but a site's cells are usually numbered consecutively.
fn with_nr_neighbours(key: CellKey) -> Vec<CellKey> {
    let (mcc, mnc, tac_id, cell_id) = key;
    let mut keys = vec![key];
    keys.extend([-2, -1, 1, 2].into_iter().map(|d| cell_id + d).filter(|&id| id >= 0).map(|id| (mcc, mnc, tac_id, id)));
    keys
}

/// Ask Apple for an LTE or 5G cell and its neighbouring cell numbers (LTE: its site's
/// sibling sectors). Fails if the request for the cell itself fails; guesses that fail or
/// don't exist are dropped.
pub async fn fetch_with_siblings(client: &AppleWPSClient, radio: Radio, key: CellKey) -> Result<CellReply> {
    let (mcc, mnc, tac_id, cell_id) = key;
    let keys = if radio == Radio::Nr { with_nr_neighbours(key) } else { with_siblings(key) };

    let mut results = vec![];
    for (asked, reply) in query_cells(client, radio, &keys).await {
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

/// A GSM / UMTS cell and the blocks of cell IDs around it (see `gsm::blocks_around`): Apple's
/// GSM replies contain only the asked cells, so its neighbours have to be asked for too. Fails
/// if the request with the cell itself fails. Returns the replies and the blocks scanned.
pub async fn fetch_gsm_with_blocks(client: &AppleWPSClient, key: CellKey) -> Result<(CellReply, Vec<gsm::GsmBlock>)> {
    let (mcc, mnc, lac, cell_id) = key;
    let blocks = gsm::blocks_around(&key);
    let mut keys = vec![key];
    keys.extend(blocks.iter().flat_map(gsm::block_keys).filter(|k| *k != key));
    let (results, failed) = gsm::fetch_gsm(client, &keys)
        .await
        .map_err(|e| e.context(format!("asking Apple for GSM cell {mcc}/{mnc}/{lac}/{cell_id}")))?;
    if failed.contains(&key) {
        anyhow::bail!("asking Apple for GSM cell {mcc}/{mnc}/{lac}/{cell_id} failed");
    }
    let blocks = blocks.into_iter().filter(|b| gsm::block_keys(b).iter().all(|k| !failed.contains(k))).collect();
    Ok((results, blocks))
}

/// Ask Apple for the cell and the cells it needs for a heatmap: for LTE its site's sibling
/// sectors and for 5G its neighbouring cell numbers (the replies bring nearby cells), for GSM
/// the blocks of IDs around it.
async fn fetch_for_heatmap(client: &AppleWPSClient, key: CellKey, radio: Radio) -> Result<(CellReply, Vec<gsm::GsmBlock>)> {
    match radio {
        Radio::Lte | Radio::Nr => Ok((fetch_with_siblings(client, radio, key).await?, vec![])),
        Radio::Gsm => fetch_gsm_with_blocks(client, key).await,
    }
}

/// How the heatmap input was obtained, for the caller to report.
pub enum Source {
    Database,
    /// fetched from Apple and saved; the number of cells with a location in the replies
    Fetched(usize),
    /// straight from Apple's replies, nothing saved; why the database wasn't used
    Online(&'static str),
}

impl Source {
    /// for the X-Wtfps-Source header and the GeoJSON `source` property
    pub fn name(&self) -> &'static str {
        match self {
            Source::Database => "database",
            Source::Fetched(_) => "fetched",
            Source::Online(_) => "online",
        }
    }

    pub fn saved(&self) -> bool {
        !matches!(self, Source::Online(_))
    }

    /// shown on the map when its cells weren't saved
    pub fn note(&self) -> Option<String> {
        match self {
            Source::Online(reason) => Some(format!("from Apple, not saved: {reason}")),
            _ => None,
        }
    }
}

/// The heatmap input for `key`: from the database, fetching and saving the cell if it isn't
/// crawled yet (see `nearby_or_fetch`). Without a database (`pool` is an error saying why
/// not), or when it can't be reached, straight from Apple's replies, saving nothing.
pub async fn heatmap_rows(
    client: &AppleWPSClient,
    pool: std::result::Result<&PgPool, &'static str>,
    key: CellKey,
    radio: Radio,
    radius_km: f64,
    fetch: bool,
) -> Result<(Vec<NearbyCell>, Source)> {
    let reason = match pool {
        Err(reason) => reason,
        Ok(pool) => match nearby_or_fetch(client, pool, key, radio, radius_km, fetch).await {
            Err(e) if fetch && database_unreachable(&e) => {
                warn!("database unreachable ({e:#}): answering from Apple without saving");
                "database unavailable"
            }
            other => return other,
        },
    };
    let (results, _) = fetch_for_heatmap(client, key, radio).await?;
    let rows = nearby_from_replies(key, radius_km, HEATMAP_MAX_CELLS as usize, results)?;
    Ok((rows, Source::Online(reason)))
}

/// Whether a database error means it couldn't be reached at all (down, refused, timed
/// out), as opposed to a problem with a query, which shouldn't be hidden by a fallback.
fn database_unreachable(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<sqlx::Error>(),
        Some(sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) | sqlx::Error::Tls(_))
    )
}

/// The cell and its nearby cells (of its radio type) from the database. If the cell isn't
/// crawled yet and `fetch` is set, ask Apple for it and the cells around it (see
/// `fetch_for_heatmap`), save every cell in the replies (as the crawler would; nothing is
/// removed) and read again.
pub async fn nearby_or_fetch(
    client: &AppleWPSClient,
    pool: &PgPool,
    key: CellKey,
    radio: Radio,
    radius_km: f64,
    fetch: bool,
) -> Result<(Vec<NearbyCell>, Source)> {
    let (mcc, mnc, tac_id, cell_id) = key;
    let is_target = |c: &NearbyCell| (c.tac_id, c.cell_id) == (tac_id, cell_id);
    let rows = nearby_from_db(pool, key, radio, radius_km).await?;
    if rows.iter().any(is_target) {
        return Ok((rows, Source::Database));
    }
    if !fetch {
        return Err(NotFound(format!(
            "cell {mcc}/{mnc}/{tac_id}/{cell_id} is not in the database (and fetching from Apple is off)"
        ))
        .into());
    }

    let (results, blocks) = fetch_for_heatmap(client, key, radio).await?;
    // replies overlap, so count each cell once
    let found = results
        .iter()
        .filter(|(c, loc)| loc.is_some() && c.cell_id != SENTINEL_CELL_ID)
        .map(|(c, _)| cell_key(c))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    // the cell counts as queried: its area is marked crawled, or it's recorded as not found
    match radio {
        Radio::Lte | Radio::Nr => commit_cells(pool, radio, &[key], Default::default(), results).await?,
        Radio::Gsm => gsm::commit_gsm_cells(pool, &[key], &blocks, results).await?,
    }

    let rows = nearby_from_db(pool, key, radio, radius_km).await?;
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
    results: CellReply,
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
    utc_strings(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64)
}

/// `utc_now` for a time in seconds since 1970-01-01 UTC.
fn utc_strings(secs: i64) -> (String, String) {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fetch::Location;
    use crate::protobuf::CellTower;

    const KEY: CellKey = (284, 5, 106, 451645);

    fn reply(mnc: u32, cell_id: u32, lat: f64, lon: f64) -> (crate::cells::ReplyCell, Option<Location>) {
        let tower = CellTower { mcc: 284, mnc, tac_id: 106, cell_id, location: None, uarfcn: Some(3350), pid: Some(250) }.into();
        let loc = Location { lat, lon, acc: 800, alt: None, alt_acc: None, unknown_11: None, unknown_12: Some(4000) };
        (tower, Some(loc))
    }

    /// the test cell and three neighbours 1, 2 and 60 km north, one of another operator
    pub(crate) fn replies() -> CellReply {
        let km = 1.0 / 111.195;
        vec![
            reply(5, 451646, 42.7 + 2.0 * km, 23.3),
            reply(5, 451645, 42.7, 23.3),
            reply(5, 451647, 42.7 + km, 23.3),
            reply(5, 451648, 42.7 + 60.0 * km, 23.3),
            reply(1, 451649, 42.7 + km, 23.3),
            (reply(5, u32::MAX, 0.0, 0.0).0, reply(5, 0, 42.7, 23.3).1),
        ]
    }

    #[test]
    fn neighbours_of_a_5g_cell() {
        let key = (232, 1, 17008, 17_248_763_905);
        let ids: Vec<i64> = with_nr_neighbours(key).iter().map(|k| k.3).collect();
        assert_eq!(ids, [17_248_763_905, 17_248_763_903, 17_248_763_904, 17_248_763_906, 17_248_763_907]);
        assert_eq!(with_nr_neighbours((232, 1, 1, 1)).iter().map(|k| k.3).collect::<Vec<_>>(), [1, 0, 2, 3]);
    }

    #[test]
    fn siblings_of_a_cell() {
        assert_eq!(
            with_siblings(KEY),
            [KEY, (284, 5, 106, 451643), (284, 5, 106, 451644), (284, 5, 106, 451646), (284, 5, 106, 451647)]
        );
        // sector 0 of site 1764: no negative sectors, and no step into the previous site
        let first = (284, 5, 106, 1764 << 8);
        assert_eq!(with_siblings(first), [first, (284, 5, 106, (1764 << 8) | 1), (284, 5, 106, (1764 << 8) | 2)]);
    }

    #[test]
    fn nearby_from_replies_filters_and_sorts() {
        let rows = nearby_from_replies(KEY, 50.0, 800, replies()).unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.cell_id).collect();
        // same operator, within 50 km, nearest first; no area estimate
        assert_eq!(ids, [451645, 451647, 451646]);
        assert_eq!(rows[0].distance_m, 0.0);
        assert_eq!(rows[0].bearing_deg, None);
        assert!((rows[1].distance_m - 1000.0).abs() < 5.0, "{}", rows[1].distance_m);
        assert!(rows[1].bearing_deg.unwrap() < 0.1);
        assert_eq!((rows[0].earfcn, rows[0].pci, rows[0].unknown_12), (Some(3350), Some(250), Some(4000)));

        let rows = nearby_from_replies(KEY, 50.0, 2, replies()).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn nearby_from_replies_without_the_cell_is_not_found() {
        let replies: Vec<_> = replies().into_iter().filter(|(c, _)| c.cell_id != 451645).collect();
        let Err(err) = nearby_from_replies(KEY, 50.0, 800, replies) else { panic!("found") };
        assert!(err.downcast_ref::<NotFound>().is_some(), "{err:#}");
    }

    #[test]
    fn great_circle_distance_and_bearing() {
        // Sofia to Plovdiv: about 132 km, east-south-east
        let d = distance_m(42.6977, 23.3219, 42.1354, 24.7453);
        assert!((d - 132_000.0).abs() < 1500.0, "{d}");
        let b = bearing_deg(42.6977, 23.3219, 42.1354, 24.7453);
        assert!((110.0..125.0).contains(&b), "{b}");
        assert!((bearing_deg(0.0, 0.0, 0.0, 1.0) - 90.0).abs() < 1e-9);
        assert!((bearing_deg(0.0, 0.0, -1.0, 0.0) - 180.0).abs() < 1e-9);
        assert!((bearing_deg(0.0, 0.0, 0.0, -1.0) - 270.0).abs() < 1e-9);
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc_strings(0).0, "1970-01-01T00:00:00Z");
        assert_eq!(utc_strings(951_782_400).0, "2000-02-29T00:00:00Z");
        assert_eq!(utc_strings(1_700_000_000), ("2023-11-14T22:13:20Z".into(), "2023-11-14 22:13 UTC".into()));
        assert_eq!(utc_strings(4_102_444_799).0, "2099-12-31T23:59:59Z");
    }

    #[test]
    fn source_names() {
        assert_eq!((Source::Database.name(), Source::Database.saved(), Source::Database.note()), ("database", true, None));
        assert_eq!((Source::Fetched(3).name(), Source::Fetched(3).saved()), ("fetched", true));
        let online = Source::Online("database unavailable");
        assert_eq!((online.name(), online.saved()), ("online", false));
        assert_eq!(online.note().unwrap(), "from Apple, not saved: database unavailable");
    }

    #[test]
    fn only_connection_failures_count_as_unreachable() {
        assert!(database_unreachable(&sqlx::Error::PoolTimedOut.into()));
        assert!(database_unreachable(&sqlx::Error::Io(std::io::ErrorKind::ConnectionRefused.into()).into()));
        assert!(!database_unreachable(&sqlx::Error::RowNotFound.into()));
        assert!(!database_unreachable(&anyhow::anyhow!("something else")));
    }
}
