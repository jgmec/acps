#[macro_use]
extern crate log;

use std::string::String;
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::{stream, StreamExt, TryStreamExt};
use h3o::Resolution;
use mac6::Mac;
use mac_address::MacAddress;
use serde_json::json;

use sqlx::{postgres::PgPoolOptions, query, query_scalar, PgPool};


mod fetch;
use crate::fetch::{AppleWPSClient, Location};
use crate::protobuf::CellTower;

pub mod protobuf {
    include!(concat!(env!("OUT_DIR"), "/apple.rs"));
}

const CONCURRENT_PARTS: usize = 8;
//const CONCURRENT_CELLS_PER_PART: usize = 8;

#[derive(Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Command,
    #[arg(long)]
    china: bool,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    Explore {
        #[clap(subcommand)]
        source: ExploreSource,
    },
    Query {
        mac: String,
    },
    Cell {
        mcc: String,
        mnc: String,
        tacid: String,
        cellid: String,
    },
    Oui {
        oui: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum ExploreSource {
    Crawl,
    Todo {
        #[arg(long)]
        ignore_extra: bool,
    },
    Cell,
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

            match source {
                ExploreSource::Cell => loop {
                    let todo: Vec<(i32, i32, i32, i64)> = sqlx::query_as(
                        "select mcc, mnc, tac_id, cell_id from cell_todo limit 256",
                    )
                    .fetch_all(&pool)
                    .await?;

                    if todo.is_empty() {
                        break;
                    }

                    let requests: Vec<_> = todo
                        .iter()
                        .map(|&(mcc, mnc, tac_id, cell_id)| {
                            client.query_cell(CellTower {
                                mcc: mcc as u32,
                                mnc: mnc as u32,
                                tac_id: tac_id as u32,
                                cell_id: cell_id as u32,
                                location: None,
                                uarfcn: None,
                                pid: None,
                            })
                        })
                        .collect();

                    info!("making {} cell requests...", requests.len());
                    let mut stream = stream::iter(requests).buffer_unordered(CONCURRENT_PARTS);
                    let mut results = Vec::new();
                    while let Some(resp) = stream.try_next().await? {
                        results.extend(resp);
                    }

                    commit_cells(pool.clone(), todo, results).await?;
                },

                _ => loop {
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
                        ExploreSource::Todo { .. } => {
                            let todo = query_scalar!("select mac from todo limit 256")
                                .fetch_all(&pool)
                                .await?;
                            if todo.len() == 0 {
                                break;
                            }
                            todo
                        }
                        ExploreSource::Cell => unreachable!(),
                    };

                    let mut requests = Vec::new();
                    for mac in &todo {
                        requests.push(client.query_wifi(*mac));
                    }

                    let ignore_extra = match source {
                        ExploreSource::Todo { ignore_extra } => ignore_extra,
                        _ => false,
                    };

                    info!("making {} requests...", requests.len());
                    let mut requests = stream::iter(requests).buffer_unordered(256);
                    let mut locs = BTreeMap::new();
                    while let Some(response) = requests.try_next().await? {
                        for (mac, loc) in response {
                            if ignore_extra {
                                if !todo.contains(&mac) {
                                    continue;
                                }
                            }
                            locs.insert(mac, loc);
                        }
                    }
                    commit(pool.clone(), crawling, locs).await?;
                },
            }
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

        Command::Cell { mcc, mnc, tacid, cellid } =>{
            println!("cellid: {cellid}");

            let cells = client
                .query_cell(CellTower{
                    mcc: (&mcc).parse().unwrap(),
                    mnc: (&mnc).parse().unwrap(),
                    cell_id: (&cellid).parse().unwrap(),
                    tac_id: (&tacid).parse().unwrap(),
                    location: None,
                    uarfcn: None,
                    pid: None,
                })
                .await?;
            if let Some((_, Some(loc))) = cells.first() {
                println!("{}", osm(loc));
                dbg!(cells.len());
            } else {
                println!("not found")
            }
            for (cell, loc) in cells {
                print!("cellid {}", cell.cell_id);
                if let Some(raw_loc) = cell.location {
                    print!("\t\thorizontal_accuracy {:?}", raw_loc.horizontal_accuracy);
                }
                match loc {
                    Some(l) => println!("\t\tlocation {}", osm(&l)),
                    None => println!("\t\tnot found"),
                }
            }

        }

        Command::Oui { oui } => {
            for i in 0..256u64.pow(3) {
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

async fn commit_cells(
    pool: PgPool,
    queried: Vec<(i32, i32, i32, i64)>,
    results: Vec<(CellTower, Option<Location>)>,
) -> Result<()> {
    let mut tx = pool.begin().await?;

    let queried_set: BTreeSet<_> = queried.iter().cloned().collect();

    info!(
        "{} cells with location, {} not found",
        results.iter().filter(|(_, l)| l.is_some()).count(),
        results.iter().filter(|(_, l)| l.is_none()).count(),
    );

    for (cell, loc) in results {
        let mcc = cell.mcc as i32;
        let mnc = cell.mnc as i32;
        let tac_id = cell.tac_id as i32;
        let cell_id = cell.cell_id as i64;

        let Some(loc) = loc else { continue };
        let Location { lat, lon, .. } = loc;

        let inserted: Option<i64> = sqlx::query_scalar(
            "insert into cell (mcc, mnc, tac_id, cell_id, geom) \
             values ($1, $2, $3, $4, ST_SetSRID(ST_MakePoint($5, $6), 4326)) \
             on conflict do nothing returning cell_id",
        )
        .bind(mcc)
        .bind(mnc)
        .bind(tac_id)
        .bind(cell_id)
        .bind(lon)
        .bind(lat)
        .fetch_optional(&mut *tx)
        .await?;

        if inserted.is_some() && !queried_set.contains(&(mcc, mnc, tac_id, cell_id)) {
            sqlx::query(
                "insert into cell_todo (mcc, mnc, tac_id, cell_id) \
                 values ($1, $2, $3, $4) on conflict do nothing",
            )
            .bind(mcc)
            .bind(mnc)
            .bind(tac_id)
            .bind(cell_id)
            .execute(&mut *tx)
            .await?;
        }
    }

    for (mcc, mnc, tac_id, cell_id) in &queried {
        sqlx::query(
            "delete from cell_todo where mcc=$1 and mnc=$2 and tac_id=$3 and cell_id=$4",
        )
        .bind(mcc)
        .bind(mnc)
        .bind(tac_id)
        .bind(cell_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

fn osm(Location { lat, lon, .. }: &Location) -> String {
    format!("https://www.openstreetmap.org/search?query={lat}%2F{lon}#map=16/{lat}/{lon}")
}
