#[macro_use]
extern crate log;

use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::{stream, StreamExt, TryStreamExt};
use h3o::CellIndex;
use indicatif::{ProgressBar, ProgressDrawTarget};
use mac6::Mac;
use reqwest::Client;
use sqlx::{postgres::PgPoolOptions, query, query_scalar, PgPool};
use tokio::task::JoinSet;

mod fetch;
use crate::fetch::{fetch, Location};

pub mod protobuf {
    include!(concat!(env!("OUT_DIR"), "/apple.rs"));
}

const CONCURRENT_PARTS: usize = 16;
const CONCURRENT_CELLS_PER_PART: usize = 8;

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

    match cli.command {
        Command::Explore => {
            let client = Client::new();
            let pool = PgPoolOptions::new()
                .max_connections((CONCURRENT_PARTS * 2) as _)
                .connect(dbg!(&dotenvy::var("DATABASE_URL")?))
                .await?;
            sqlx::migrate!().run(&pool).await?;

            let mut parts_exploring = BTreeSet::new();
            let mut tasks = JoinSet::new();
            let pb = ProgressBar::new_spinner();
            pb.set_draw_target(ProgressDrawTarget::hidden());

            tasks.spawn(explore(pb.clone(), client.clone(), pool.clone(), 0));
            loop {
                if tasks.len() < CONCURRENT_PARTS {
                    for part in query_scalar!("select distinct part from cell where last_crawled is null order by part limit $1", CONCURRENT_PARTS as i64).fetch_all(&pool).await? {
                        if tasks.len() < CONCURRENT_PARTS {
                            if parts_exploring.insert(part) {
                                tasks.spawn(explore(pb.clone(), client.clone(),pool.clone(), part));
                            }
                        } else {
                            break;
                        }
                    }
                }

                info!("exploring {} parts at {:.0}/s", tasks.len(), pb.per_sec());

                if let Some(task) = tasks.join_next().await {
                    let part = task??;
                    parts_exploring.remove(&part);
                } else {
                    break;
                }
            }
            pb.finish_and_clear();
        }
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

async fn explore(pb: ProgressBar, client: Client, pool: PgPool, part: i64) -> Result<i64> {
    loop {
        let mut tx = pool.begin().await?;
        let mut todo = BTreeSet::new();
        if part == 0 {
            todo.extend(
                query_scalar!("select mac from todo limit 64")
                    .fetch_all(&pool)
                    .await?,
            );
        } else {
            for cell in query_scalar!(
                "select cell from cell where last_crawled is null and part = $1 limit $2 for update",
                part,
                CONCURRENT_CELLS_PER_PART as i64
            )
            .fetch_all(&mut *tx)
            .await?
            {
                let mac = query_scalar!(
                    "select mac from wifi where part = $1 and cell = $2 and not gone order by random() limit 1",
                    part,
                    cell
                )
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(mac) = mac {
                    todo.insert(mac);
                } else {
                    info!("deleting cell {cell:012x}");
                    query!("delete from cell where part = $1 and cell = $2", part, cell)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        };
        if todo.is_empty() {
            tx.commit().await?;
            return Ok(part);
        }

        // queue requests
        let mut requests = Vec::new();
        for mac in &todo {
            requests.push(fetch(client.clone(), mac.to_string()));
        }

        let mut requests = stream::iter(requests).buffer_unordered(256);
        let mut locs = BTreeMap::new();
        while let Some(response) = requests.try_next().await? {
            for (mac, loc) in response {
                locs.insert(mac, loc);
            }
        }
        // info!("received {} macs for part {part:012x}", locs.len());

        let mut crawled_cells = BTreeSet::new();
        let mut seen_cells = BTreeSet::new();
        for (mac, loc) in locs {
            if let Some(loc) = loc {
                let (cell, part) = loc.cell();
                query!(
                    "insert into wifi (
                        part, mac, cell, lat, lon
                    ) values (
                        $1, $2, $3, $4, $5
                    ) on conflict (part, mac) do update set cell = $3, lat = $4, lon = $5",
                    part,
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
                query!(
                    "update wifi set gone = true where part = $1 and mac = $2",
                    part,
                    mac,
                )
                .execute(&mut *tx)
                .await?;
            }

            pb.inc(1);

            if part == 0 {
                query!("delete from todo where mac = $1", mac)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        for cell in seen_cells {
            query!(
                "insert into cell (part, cell) values ($1, $2) on conflict (part, cell) do nothing",
                cell_part(cell),
                cell
            )
            .execute(&mut *tx)
            .await?;
        }
        for cell in crawled_cells {
            query!(
                "update cell set last_crawled = current_timestamp where part = $1 and cell = $2",
                cell_part(cell),
                cell
            )
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
    }
}

fn cell_part(cell: i64) -> i64 {
    let cell = CellIndex::try_from(cell as u64).unwrap();
    let part: u64 = cell.parent(h3o::Resolution::Six).unwrap().into();
    part as i64
}
