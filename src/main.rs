#[macro_use]
extern crate log;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Write,
    str::FromStr,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use db::WtfDb;
use fetch::request;
use futures::{stream, StreamExt, TryStreamExt};
use h3o::{CellIndex, LatLng, Resolution};
use macn::Mac;
use protobuf::{AppleWLoc, CellTower};
use reqwest::Client;
use rusqlite::Connection;

mod fetch;
use crate::fetch::{fetch, Location};
mod db;

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
    Brute {
        start: String,
        count: u64,
    },
    Coverage,
    Dump,
    Explore,
    ExploreQueue,
    Query {
        mac: String,
    },
    QueryCell {
        country: u32,
        network: u32,
        cell: u32,
        area: u32,
    },
    Seed,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let mut db = WtfDb::open()?;

    match cli.command {
        Command::Brute { start, count } => {
            let start = u64::from(Mac::from_str(&start).unwrap());
            let end = start + count;

            // let mut existing = BTreeSet::new();
            // for q in [
            //     "select mac from wifi where mac between ? and ?",
            //     "select mac from wifi_explore where mac between ? and ?",
            //     "select mac from wifi_fail where mac between ? and ?",
            // ] {
            //     for mac in conn.prepare(q)?.query_map((start, end), |row| {
            //         let mac: u64 = row.get(0)?;
            //         Ok(mac)
            //     })? {
            //         existing.insert(mac?);
            //     }
            // }

            let tx = db.main.transaction()?;
            for mac in start..=end {
                // if !existing.contains(&mac) {
                tx.execute(
                    "insert into wifi_brute (mac) values (?) on conflict do nothing",
                    (mac,),
                )?;
                // }
            }
            tx.commit()?;
        }
        Command::Coverage => {
            let mut cells = BTreeMap::new();
            for i in 0..=65535 {
                if let Some(shard) = db.shard_if_exists(i)? {
                    for res in shard
                        .prepare("select lat, lon from wifi")?
                        .query_map((), |row| {
                            let loc = Location {
                                lat: row.get(0)?,
                                lon: row.get(1)?,
                            };
                            Ok(loc.cell())
                        })?
                    {
                        let cell = res?;
                        if let Some(count) = cells.get_mut(&cell) {
                            *count += 1;
                        } else {
                            cells.insert(cell, 1usize);
                        }
                    }
                    info!("loaded shard {i}, {} total", cells.len());
                }
            }
            for (cell, count) in cells {
                let pos = LatLng::from(cell);
                let (lat, lon) = (pos.lat(), pos.lng());
                println!("{lat},{lon},{count}");
            }
        }
        Command::Dump => {
            let f = File::create("wtfps.bin.zst")?;
            let mut e = zstd::Encoder::new(f, 9)?;
            for (c, res) in db
                .main
                .prepare_cached("select mac, lat, lon from wifi order by mac")?
                .query_map((), |row| {
                    let mac: u64 = row.get(0)?;

                    let loc = LatLng::new(row.get(1)?, row.get(2)?).unwrap();
                    let cell = loc.to_cell(Resolution::Fifteen);
                    Ok((mac, cell))
                })?
                .enumerate()
            {
                let (mac, cell) = res?;
                e.write_all(&mac.to_be_bytes())?;
                e.write_all(&u64::from(cell).to_be_bytes())?;

                if (c % 1_000_000) == 0 {
                    println!("{c}");
                }
                if c == 650_000_000 {
                    break;
                }
            }
            e.finish()?;
        }
        Command::Explore => explore(db).await?,
        Command::ExploreQueue => {
            println!("lat,lon");
            for cell in db
                .main
                .prepare("select cell from wifi_explore")?
                .query_map((), |row| {
                    let cell: u64 = row.get(0)?;
                    Ok(cell)
                })?
            {
                let cell = CellIndex::try_from(cell?)?;
                let loc = LatLng::from(cell);
                let (lat, lon) = (loc.lat(), loc.lng());
                println!("{lat},{lon}");
            }
        }
        Command::Query { mac } => {
            let wifis = fetch(Client::new(), Mac::from_str(&mac).unwrap().to_string()).await?;
            if let Some((_, Some(Location { lat, lon }))) = wifis.first() {
                println!(
                    "https://www.openstreetmap.org/search?query={lat}%2F{lon}#map=12/{lat}/{lon}"
                );
                dbg!(wifis.len());
            } else {
                println!("not found")
            }
        }
        Command::Seed => {
            let seeds = Connection::open("seeds.db")?;

            let mut locs = BTreeMap::new();
            for res in seeds
                .prepare("select bssid, lat, lon from seeds")?
                .query_map((), |row| {
                    let bssid: u64 = row.get(0)?;
                    let lat: f64 = row.get(1)?;
                    let lon: f64 = row.get(2)?;
                    Ok((bssid, lat, lon))
                })?
            {
                let (bssid, lat, lon) = res?;
                let mac = Mac::from_str(&format!("{bssid:012x}")).unwrap();
                locs.insert(mac, Some(Location { lat, lon }));
            }
            db.commit(locs)?;
        }
        Command::QueryCell {
            country,
            network,
            cell,
            area,
        } => {
            let x = request(
                Client::new(),
                AppleWLoc {
                    cell_tower_request: Some(CellTower {
                        mcc: country,
                        mnc: network,
                        cell_id: cell,
                        tac_id: area,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await?;
            println!("{x:#?}")
        }
    }

    Ok(())
}

async fn explore(mut db: WtfDb) -> Result<()> {
    db.truncate()?;

    let client = Client::new();
    loop {
        let mut todo = BTreeSet::new();
        for q in [
            "select mac from wifi_brute limit 16384",
            "select mac from wifi_explore order by cell limit 16384",
        ] {
            for mac in db.main.prepare(q)?.query_map((), |row| {
                let mac: u64 = row.get(0)?;
                Ok(Mac::from(mac))
            })? {
                todo.insert(mac?);
            }
        }

        if todo.is_empty() {
            break;
        }

        // start requests
        let mut requests = Vec::new();
        while let Some(mac) = todo.pop_first() {
            requests.push(fetch(client.clone(), mac.to_string()));
            if requests.len() == 16384 {
                break;
            }
        }
        info!("making {} requests", requests.len());
        let mut requests = stream::iter(requests).buffer_unordered(256);
        let mut locs = BTreeMap::new();
        while let Some(response) = requests.try_next().await? {
            for (mac, loc) in response {
                locs.insert(mac, loc);
            }
        }
        info!("received {} macs", locs.len());
        db.commit(locs)?;
    }

    Ok(())
}
