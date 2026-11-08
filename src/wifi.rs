//! WiFi exploration: querying MACs from the `todo` queue or from uncrawled areas, and
//! storing the networks Apple returns.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use futures::{stream, StreamExt, TryStreamExt};
use h3o::Resolution;
use mac_address::MacAddress;
use serde_json::json;
use sqlx::{query, query_scalar, PgPool};

use crate::fetch::{AppleWPSClient, Location};

/// WiFi exploration: query the `todo` queue, or with `crawl` one random known network per
/// uncrawled area. With `ignore_extra`, only the queried networks are stored, not the
/// nearby ones Apple returns with them.
pub(crate) async fn explore_wifi(client: &AppleWPSClient, pool: &PgPool, crawl: bool, ignore_extra: bool) -> Result<()> {
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

pub(crate) async fn commit(
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
