#[macro_use]
extern crate log;

use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::{stream, StreamExt, TryStreamExt};
use indicatif::ProgressBar;
use mac6::Mac;
use reqwest::Client;
use sqlx::{query, query_file, query_scalar, PgPool};

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
        let mut todo = query_scalar!("select mac from wifi_todo order by cell limit 4096")
            .fetch_all(&pool)
            .await?;
        if todo.is_empty() {
            query_file!("queries/wifi_cell_todo.sql")
                .execute(&pool)
                .await?;
            todo = query_scalar!("select mac from wifi_todo order by cell limit 4096")
                .fetch_all(&pool)
                .await?;
        }

        if todo.is_empty() {
            info!("nothing to do");
            break;
        }

        let todo = BTreeSet::from_iter(todo);

        // queue requests
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

        let mut crawled_cells = BTreeSet::new();
        let mut seen_cells = BTreeSet::new();
        for (mac, loc) in locs {
            if let Some(loc) = loc {
                let cell = loc.cell();
                query!(
                    "insert into wifi_seen (
                        mac, cell, lat, lon
                    ) values (
                        $1, $2, $3, $4
                    ) on conflict (mac) do update set cell = $2, lat = $3, lon = $4",
                    mac,
                    cell,
                    loc.lat,
                    loc.lon,
                )
                .execute(&mut *tx)
                .await?;

                seen_cells.insert(cell);
                if todo.contains(&mac) {
                    crawled_cells.insert(cell);
                }
            } else {
                query!("update wifi_seen set gone = true where mac = $1", mac,)
                    .execute(&mut *tx)
                    .await?;
            }
            pb.inc(1);

            query!("delete from wifi_todo where mac = $1", mac)
                .execute(&mut *tx)
                .await?;
        }
        pb.finish_and_clear();

        for cell in seen_cells {
            query!(
                "insert into wifi_cell (cell) values ($1) on conflict (cell) do nothing",
                cell
            )
            .execute(&mut *tx)
            .await?;
        }
        for cell in crawled_cells {
            query!(
                "update wifi_cell set last_crawled = current_timestamp where cell = $1",
                cell
            )
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
    }

    Ok(())
}
