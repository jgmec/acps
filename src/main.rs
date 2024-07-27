use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::File,
    io::Write,
    str::FromStr,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::{stream, StreamExt, TryStreamExt};
use h3c::H3c;
use h3o::{CellIndex, LatLng, Resolution};
use macn::Mac;
use reqwest::Client;
use rusqlite::{Connection, OptionalExtension};

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
    Brute { start: String, count: u64 },
    Dump,
    Explore,
    ExploreQueue,
    Query { mac: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut conn = Connection::open("wtfps.db")?;
    conn.pragma_update(None, "journal_mode", "wal")?;
    conn.pragma_update(None, "synchronous", "off")?;
    conn.execute_batch(include_str!("../db.sql"))?;

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

            let tx = conn.transaction()?;
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
        Command::Dump => {
            let f = File::create("wtfps.bin.zst")?;
            let mut e = zstd::Encoder::new(f, 9)?;
            for (c, res) in conn
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
        Command::Explore => explore(conn).await?,
        Command::ExploreQueue => {
            println!("lat,lon");
            for cell in conn
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
            }
        }
    }

    Ok(())
}

fn commit(
    conn: &mut Connection,
    cache: &mut BTreeSet<Mac>,
    locs: BTreeMap<Mac, Option<Location>>,
) -> Result<()> {
    let tx = conn.transaction()?;

    let total_macs = locs.len();
    let mut new_macs = 0;
    let mut failed = 0;
    let mut to_explore = HashMap::new();
    let mut to_cache = BTreeSet::new();
    for (mac, loc) in locs {
        let macn = u64::from(mac);

        if let Some(loc) = loc {
            let cell = loc.cell();
            let h3c = H3c::from(cell);
            let celln = u64::from(cell);

            let mut existing = cache.contains(&mac);

            if !existing {
                for h3c in h3c.search_area() {
                    if tx
                        .prepare_cached("select 1 from wifi where h3c = ? and mac = ?")?
                        .query_row((h3c.0, macn), |row| Ok(1))
                        .optional()?
                        .is_some()
                    {
                        existing = true;
                        break;
                    }
                }
            };
            to_cache.insert(mac);

            if !existing {
                tx.prepare_cached(
                    "insert into wifi (h3c, mac, lat, lon) values (?, ?, ?, ?) on conflict do nothing"
                )?.execute(
                    (h3c.0, macn, loc.lat, loc.lon),
                )?;

                new_macs += 1;
                to_explore.insert(celln, macn);
            }
        } else {
            failed += 1;
            tx.prepare_cached("insert into wifi_fail (mac) values (?)")?
                .execute((macn,))?;
        }

        tx.prepare_cached("delete from wifi_explore where mac = ?")?
            .execute((macn,))?;
        tx.prepare_cached("delete from wifi_brute where mac = ?")?
            .execute((macn,))?;
    }

    for (celln, macn) in to_explore {
        tx.prepare_cached(
            "insert into wifi_explore (cell, mac) values (?, ?) on conflict do nothing",
        )?
        .execute((celln, macn))?;
        let loc = LatLng::from(CellIndex::try_from(celln)?);
        println!("{{\"latitude\":{},\"longitude\":{}}}", loc.lat(), loc.lng());
    }

    // approx 1GB of RAM
    if cache.len() > 32_000_000 {
        *cache = to_cache;
    } else {
        cache.extend(to_cache);
    }

    tx.commit()?;

    eprintln!(
        "{total_macs} total, {new_macs} new, {failed} failed, {} cached",
        cache.len()
    );
    Ok(())
}

async fn explore(mut conn: Connection) -> Result<()> {
    let client = Client::new();
    let mut cache = BTreeSet::new();

    loop {
        let mut todo = BTreeSet::new();
        for q in [
            "select mac from wifi_brute limit 16384",
            // "select mac from wifi_explore order by cell limit 16384",
        ] {
            for mac in conn.prepare(q)?.query_map((), |row| {
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
        eprintln!("-> {} requests", requests.len());
        let mut requests = stream::iter(requests).buffer_unordered(128);
        let mut locs = BTreeMap::new();
        while let Some(response) = requests.try_next().await? {
            for (mac, loc) in response {
                locs.insert(mac, loc);
            }
        }
        eprint!("<- ");
        commit(&mut conn, &mut cache, locs)?;
    }

    Ok(())
}
