use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    str::FromStr,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::{stream, StreamExt, TryStreamExt};
use h3o::{CellIndex, LatLng};
use mac_address::MacAddress;
use reqwest::Client;
use sqlx::{query, query_scalar, MySqlPool};

mod fetch;
use crate::fetch::fetch;

pub mod protobuf {
    include!(concat!(env!("OUT_DIR"), "/apple.rs"));
}

#[derive(Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    Explore,
    ExploreQueue,
    Query { mac: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let pool = MySqlPool::connect(&dotenvy::var("DATABASE_URL")?).await?;
    match cli.command {
        Command::Explore => explore(pool).await?,
        Command::ExploreQueue => {
            let cells = query_scalar!("select cell from wifi_explore")
                .fetch_all(&pool)
                .await?;
            println!("lat,lon");
            for cell in cells {
                let cell = CellIndex::try_from(cell)?;
                let loc = LatLng::from(cell);
                let (lat, lon) = (loc.lat(), loc.lng());
                println!("{lat},{lon}");
            }
        }
        Command::Query { mac } => {
            let wifis = fetch(Client::new(), mac).await?;
            let wifi = wifis.first();
            if let Some(wifi) = wifi {
                let loc = wifi.location.unwrap();
                let (lat, lon) = (coord(loc.latitude.unwrap()), coord(loc.longitude.unwrap()));
                println!(
                    "https://www.openstreetmap.org/search?query={lat}%2F{lon}#map=12/{lat}/{lon}"
                )
            }
            dbg!(wifis.len());
        }
    }

    Ok(())
}

async fn explore(pool: MySqlPool) -> Result<()> {
    let mut known_cache = BTreeSet::new();
    let mut todo = BTreeMap::new();
    {
        // https://github.com/launchbadge/sqlx/issues/3199
        let q = query!("select cell, cast(mac as binary) as mac from wifi_explore")
            .fetch_all(&pool)
            .await?;
        for rec in q {
            let cell = rec.cell;
            let mac = MacAddress::new(rec.mac.unwrap().try_into().unwrap());
            todo.insert(cell, mac);
        }
    }

    let client = Client::new();
    loop {
        if todo.is_empty() {
            break;
        }

        // start requests
        let mut requests = Vec::new();
        let mut cells_requested = Vec::new();
        while let Some((cell, mac)) = todo.pop_first() {
            requests.push(fetch(client.clone(), mac.to_string()));
            cells_requested.push(cell);
            if requests.len() == 1024 {
                break;
            }
        }

        // collect responses
        let mut requests = stream::iter(requests).buffer_unordered(64);
        let mut wifis = HashMap::new();
        while let Some(response) = requests.try_next().await? {
            // println!("{}", response.len());
            for wifi in response {
                let mut mac = String::new();
                for part in wifi.bssid.split(':') {
                    if part.len() == 1 {
                        mac.push('0');
                    }
                    mac.push_str(&part);
                    mac.push(':');
                }
                mac.pop();
                let mac = MacAddress::from_str(&mac)?;
                wifis.insert(mac, wifi.location.unwrap());
            }
        }

        // write to db
        eprintln!("db");
        let mut tx = pool.begin().await?;
        for cell in cells_requested {
            query!("delete from wifi_explore where cell = ?", cell)
                .execute(&mut *tx)
                .await?;
        }
        let received = wifis.len();
        let mut new = 0;
        for (mac, loc) in wifis {
            let known = if known_cache.contains(&mac) {
                true
            } else {
                known_cache.insert(mac);
                query_scalar!("select count(*) from wifi where mac = ?", &mac.bytes()[..])
                    .fetch_one(&mut *tx)
                    .await?
                    > 0
            };

            if !known {
                new += 1;
                let (lat, lon) = (coord(loc.latitude.unwrap()), coord(loc.longitude.unwrap()));
                println!("{{\"latitude\":{lat},\"longitude\":{lon}}}");
                query!(
                    "insert into wifi (mac, lat, lon, acc, alt, alt_acc) values (?, ?, ?, ?, ?, ?)",
                    &mac.bytes()[..],
                    lat,
                    lon,
                    loc.horizontal_accuracy.unwrap(),
                    loc.altitude.unwrap(),
                    loc.vertical_accuracy.unwrap_or(0),
                )
                .execute(&mut *tx)
                .await?;

                let cell = LatLng::new(lat, lon)?.to_cell(h3o::Resolution::Ten);
                let cell = u64::from(cell);
                if !todo.contains_key(&cell) {
                    todo.insert(cell, mac);
                    query!(
                        "insert ignore into wifi_explore (cell, mac) values (?, ?)",
                        cell,
                        &mac.bytes()[..],
                    )
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        eprintln!(
            "received {received}, {new} new, {} known cached, {} cells todo",
            known_cache.len(),
            todo.len()
        );
        tx.commit().await?;
    }

    Ok(())
}

#[inline(always)]
pub fn coord(coord: i64) -> f64 {
    coord as f64 * 1e-8
}
