use std::{collections::BTreeMap, str::FromStr};

use anyhow::Result;
use clap::{Parser, Subcommand};
use h3o::LatLng;
use mac_address::MacAddress;
use query::query;
use rusqlite::Connection;

mod query;

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
    Query { mac: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let conn = Connection::open("./wtfps.db")?;
    conn.execute_batch(include_str!("../db.sql"))?;

    match cli.command {
        Command::Explore => explore(conn)?,
        Command::Query { mac } => {
            let wifis = query(mac)?;
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

fn explore(mut conn: Connection) -> Result<()> {
    let mut todo = BTreeMap::new();
    {
        let mut q = conn.prepare("select cell, mac from wifi_explore")?;
        for row in q.query_map((), |x| {
            let cell: u64 = x.get(0)?;
            let mac: [u8; 6] = x.get(1)?;
            let mac = MacAddress::new(mac);
            Ok((cell, mac))
        })? {
            let (cell, mac) = row?;
            todo.insert(cell, mac);
        }
    }

    while let Some((cell, mac)) = todo.pop_first() {
        eprintln!("fetching {mac}");
        let wifis = query(mac.to_string())?;
        let mut new = 0;
        let tx = conn.transaction()?;
        for wifi in wifis {
            let mut mac2 = String::new();
            for part in wifi.bssid.split(':') {
                if part.len() == 1 {
                    mac2.push('0');
                }
                mac2.push_str(&part);
                mac2.push(':');
            }
            mac2.pop();

            let mac = MacAddress::from_str(&mac2)?;
            let known = tx.query_row(
                "select count(*) from wifi where mac = ?",
                (mac.bytes(),),
                |x| {
                    let count: usize = x.get(0)?;
                    return Ok(count == 1);
                },
            )?;

            if !known {
                new += 1;
                let loc = wifi.location.unwrap();
                let (lat, lon) = (coord(loc.latitude.unwrap()), coord(loc.longitude.unwrap()));
                tx.execute(
                    "insert into wifi (mac, lat, lon, acc, alt, alt_acc) values (?, ?, ?, ?, ?, ?)",
                    (
                        mac.bytes(),
                        lat,
                        lon,
                        loc.horizontal_accuracy.unwrap(),
                        loc.altitude.unwrap(),
                        loc.vertical_accuracy.unwrap_or(0),
                    ),
                )?;

                let cell = LatLng::new(lat, lon)?.to_cell(h3o::Resolution::Ten);
                let cell = u64::from(cell);
                if !todo.contains_key(&cell) {
                    todo.insert(cell, mac);
                    tx.execute(
                        "insert into wifi_explore (cell, mac) values (?, ?) on conflict do nothing",
                        (cell, mac.bytes()),
                    )?;
                }
            }
        }

        eprintln!("found {new}");
        tx.execute("delete from wifi_explore where cell = ?", (cell,))?;
        tx.commit()?;
    }

    Ok(())
}

#[inline(always)]
pub fn coord(coord: i64) -> f64 {
    coord as f64 * 1e-8
}
