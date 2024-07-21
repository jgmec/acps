use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    str::FromStr,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::{stream, StreamExt, TryStreamExt};
use h3o::{CellIndex, LatLng, Resolution};
use mac_address::MacAddress;
use macn::Mac;
use reqwest::Client;
use rusqlite::Connection;

mod fetch;
use crate::fetch::{fetch, Location};

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
    Brute { base: String },
    Coverage,
    Explore,
    ExploreQueue,
    Query { mac: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut conn = Connection::open("wtfps.db")?;
    conn.execute_batch(&include_str!("../db.sql"))?;

    match cli.command {
        // Command::Brute { base } => {
        //     let mut tx = pool.begin().await?;
        //     for e in 0..=255u8 {
        //         for f in 0..=255u8 {
        //             let mac = format!("{base}:{f:02x}:{e:02x}");
        //             let mac = MacAddress::from_str(&mac)?;
        //             query!(
        //             "insert ignore into wifi_explore (cell, mac) values (FLOOR((RAND() * 10000000000000)), ?)",
        //             &mac.bytes()[..]
        //         )
        //             .execute(&mut *tx)
        //             .await?;
        //         }
        //     }
        //     tx.commit().await?;
        // }
        // Command::Coverage => {
        //     let mut cells = BTreeMap::new();
        //     query!("select lat, lon from wifi")
        //         .fetch(&pool)
        //         .try_for_each(|x| {
        //             let loc = LatLng::new(x.lat, x.lon).unwrap();
        //             let cell = loc.to_cell(Resolution::Ten);
        //             if let Some(x) = cells.get_mut(&cell) {
        //                 *x += 1;
        //             } else {
        //                 cells.insert(cell, 1usize);
        //             }

        //             async { Ok(()) } // :)
        //         })
        //         .await?;

        //     println!("lat,lon,count");
        //     for (cell, count) in cells {
        //         let loc = LatLng::from(cell);
        //         println!("{},{},{count}", loc.lat(), loc.lng());
        //     }
        // }
        Command::Explore => explore(conn).await?,
        // Command::ExploreQueue => {
        //     let cells = query_scalar!("select cell from wifi_explore")
        //         .fetch_all(&pool)
        //         .await?;
        //     println!("lat,lon");
        //     for cell in cells {
        //         let cell = CellIndex::try_from(cell)?;
        //         let loc = LatLng::from(cell);
        //         let (lat, lon) = (loc.lat(), loc.lng());
        //         println!("{lat},{lon}");
        //     }
        // }
        Command::Query { mac } => {
            let wifis = fetch(Client::new(), mac).await?;
            if let Some((_, loc)) = wifis.first() {
                if let Some(Location { lat, lon }) = loc {
                    println!(
                        "https://www.openstreetmap.org/search?query={lat}%2F{lon}#map=12/{lat}/{lon}"
                    )
                }
            }

            dbg!(wifis.len());
            let mut locs = BTreeMap::new();
            for (mac, loc) in wifis {
                if let Some(loc) = loc {
                    locs.insert(mac, loc);
                }
            }
            commit(&mut conn, &mut BTreeSet::new(), locs)?;
        }
        _ => todo!(),
    }

    Ok(())
}

fn commit(
    conn: &mut Connection,
    cache: &mut BTreeSet<Mac>,
    locs: BTreeMap<Mac, Location>,
) -> Result<()> {
    let tx = conn.transaction()?;

    let total_macs = locs.len();
    let mut new_macs = 0;
    let mut to_explore = BTreeMap::new();
    for (mac, loc) in locs {
        let celln = loc.celln();
        let macn = u64::from(mac);

        let existing = if cache.contains(&mac) {
            true
        } else {
            tx.query_row("select count(*) from wifi where mac = ?", (macn,), |row| {
                let x: usize = row.get(0)?;
                Ok(x == 1)
            })?
        };
        cache.insert(mac);

        if existing {
            tx.execute("delete from wifi_explore where mac = ?", (macn,))?;
        } else {
            println!("{{\"latitude\":{},\"longitude\":{}}}", loc.lat, loc.lon);
            tx.execute(
                "insert into wifi (mac, cell, lat, lon) values (?, ?, ?, ?) on conflict do nothing",
                (macn, celln, loc.lat, loc.lon),
            )?;

            new_macs += 1;
            to_explore.insert(celln, macn);
        }
    }
    eprintln!("{total_macs} total, {new_macs} new, {} cached", cache.len());

    for (celln, macn) in to_explore {
        tx.execute(
            "insert into wifi_explore (cell, mac) values (?, ?) on conflict do nothing",
            (celln, macn),
        )?;
    }

    tx.commit()?;

    Ok(())
}

async fn explore(mut conn: Connection) -> Result<()> {
    let client = Client::new();
    let mut cache = BTreeSet::new();

    loop {
        let mut todo = Vec::new();
        for mac in conn
            .prepare("select mac from wifi_explore")?
            .query_map((), |row| {
                let mac: u64 = row.get(0)?;
                Ok(Mac::from(mac))
            })?
        {
            todo.push(mac?);
        }

        if todo.is_empty() {
            break;
        }

        // start requests
        let mut requests = Vec::new();
        while let Some(mac) = todo.pop() {
            requests.push(fetch(client.clone(), mac.to_string()));
        }
        eprintln!("-> {} requests", requests.len());
        let mut requests = stream::iter(requests).buffer_unordered(64);
        let mut locs = BTreeMap::new();
        while let Some(response) = requests.try_next().await? {
            for (mac, loc) in response {
                if let Some(loc) = loc {
                    locs.insert(mac, loc);
                }
            }
        }
        eprintln!("<-");
        commit(&mut conn, &mut cache, locs)?;
    }

    Ok(())
}
