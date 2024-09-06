#[macro_use]
extern crate log;

use std::{collections::BTreeMap, str::FromStr};

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::{stream, StreamExt, TryStreamExt};
use indicatif::ProgressBar;
use mac6::Mac;
use reqwest::Client;
use sqlx::{query, query_scalar, PgPool};

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
    Explore,
    Query { mac: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let pool = PgPool::connect(&dotenvy::var("DATABASE_URL")?).await?;
    sqlx::migrate!().run(&pool).await?;

    match cli.command {
        Command::Explore => explore(pool).await?,
        Command::Query { mac } => {
            let wifis = fetch(Client::new(), Mac::from_str(&mac).unwrap().to_string()).await?;
            if let Some((_, Some(Location { lat, lon, .. }))) = wifis.first() {
                println!(
                    "https://www.openstreetmap.org/search?query={lat}%2F{lon}#map=12/{lat}/{lon}"
                );
                dbg!(wifis.len());
            } else {
                println!("not found")
            }
        }
    }

    Ok(())
}

async fn explore(pool: PgPool) -> Result<()> {
    let client = Client::new();
    loop {
        let todo = query_scalar!(
            "select mac from wifi where last_crawled is null order by cell asc limit 16384"
        )
        .fetch_all(&pool)
        .await?;
        if todo.is_empty() {
            break;
        }

        // start requests
        let mut requests = Vec::new();
        for mac in &todo {
            requests.push(fetch(client.clone(), mac.to_string()));
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

        let mut tx = pool.begin().await?;
        let pb = ProgressBar::new(locs.len() as u64);
        for (mac, loc) in locs {
            if let Some(loc) = loc {
                query!(
                    "insert into wifi (
                        mac, cell, lat, lon, acc, alt, alt_acc, first_seen, last_seen
                    ) values (
                        $1, $2, $3, $4, $5, $6, $7, current_timestamp, current_timestamp
                    ) on conflict (mac) do update set
                    cell = $2, lat = $3, lon = $4, acc = $5, alt = $6, alt_acc = $7, first_seen = coalesce(wifi.first_seen, current_timestamp), last_seen = current_timestamp
                    ",
                    mac,
                    loc.cell(),
                    loc.lat,
                    loc.lon,
                    loc.acc,
                    loc.alt,
                    loc.alt_acc,
                ).execute(&mut *tx).await?;
            } else {
                // last_seen isn't changed as this is as last_crawled will be updated anyway
                query!(
                    "update wifi set cell = null, lat = null, lon = null, acc = null, alt = null, alt_acc = null where mac = $1",
                    mac,
                ).execute(&mut *tx).await?;
            }
            pb.inc(1);
        }
        pb.finish_and_clear();
        let pb = ProgressBar::new(todo.len() as u64);
        for mac in todo {
            query!(
                "update wifi set last_crawled = current_timestamp where mac = $1",
                mac
            )
            .execute(&mut *tx)
            .await?;
            pb.inc(1);
        }
        pb.finish_and_clear();
        let pb = ProgressBar::new_spinner();
        tx.commit().await?;
        pb.finish_and_clear();
    }

    Ok(())
}
