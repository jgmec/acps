#[macro_use]
extern crate log;

use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use futures::{stream, StreamExt, TryStreamExt};
use h3o::Resolution;
use mac6::Mac;
use mac_address::MacAddress;
use serde_json::json;
use sqlx::{postgres::PgPoolOptions, query, query_scalar, PgPool};

mod fetch;
use crate::fetch::{AppleWPSClient, Location};

pub mod protobuf {
    include!(concat!(env!("OUT_DIR"), "/apple.rs"));
}

const CONCURRENT_PARTS: usize = 8;
const CONCURRENT_CELLS_PER_PART: usize = 8;

#[derive(Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Command,
    #[arg(long)]
    china: bool,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    Explore { source: ExploreSource },
    Query { mac: String },
    Oui { oui: String },
}

#[derive(ValueEnum, Debug, Clone)]
enum ExploreSource {
    Crawl,
    Todo,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let client = AppleWPSClient::new(cli.china);

    match cli.command {
        Command::Explore { source } => {
            let pool = PgPoolOptions::new()
                .max_connections((CONCURRENT_PARTS * 2) as _)
                .connect(dbg!(&dotenvy::var("DATABASE_URL")?))
                .await?;
            sqlx::migrate!().run(&pool).await?;

            // let pb = ProgressBar::new_spinner();
            // pb.set_draw_target(ProgressDrawTarget::hidden());

            loop {
                let mut crawling = BTreeSet::new();
                let todo = match source {
                    ExploreSource::Crawl => {
                        let mut todo = Vec::new();
                        let mut tx = pool.begin().await?;
                        let areas = query_scalar!(
                            "select area from crawl where last_crawled is null order by area limit 256"
                        )
                        .fetch_all(&mut *tx)
                        .await?;
                        if areas.len() == 0 {
                            break;
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
                    }
                    ExploreSource::Todo => {
                        let todo = query_scalar!("select mac from todo limit 256")
                            .fetch_all(&pool)
                            .await?;
                        if todo.len() == 0 {
                            break;
                        }
                        todo
                    }
                };

                // queue requests
                let mut requests = Vec::new();
                for mac in &todo {
                    requests.push(client.query_wifi(*mac));
                }

                info!("making {} requests...", requests.len());
                let mut requests = stream::iter(requests).buffer_unordered(256);
                let mut locs = BTreeMap::new();
                while let Some(response) = requests.try_next().await? {
                    for (mac, loc) in response {
                        locs.insert(mac, loc);
                    }
                }
                commit(pool.clone(), crawling, locs).await?;
            }

            // pb.finish_and_clear();
        }
        Command::Query { mac } => {
            let wifis = client
                .query_wifi(Mac::from_str(&mac).unwrap().0.into())
                .await?;
            if let Some((_, Some(loc))) = wifis.first() {
                println!("{}", osm(loc));
                dbg!(wifis.len());
            } else {
                println!("not found")
            }
        }
        Command::Oui { oui } => {
            for i in 0..(256u64.pow(3)) {
                println!("{oui}{i:06x}");
            }
        }
    }

    Ok(())
}

async fn commit(
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

fn osm(Location { lat, lon, .. }: &Location) -> String {
    format!("https://www.openstreetmap.org/search?query={lat}%2F{lon}#map=12/{lat}/{lon}")
}
