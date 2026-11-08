#[macro_use]
extern crate log;

use std::str::FromStr;

use anyhow::Result;
use clap::Parser;
use mac6::Mac;
use sqlx::postgres::PgPoolOptions;

mod api;
mod cells;
mod cli;
mod coverage;
mod db;
mod fetch;
mod geojson;
mod heatmap;
mod lookup;
mod raw;
mod terrain;
mod wifi;

use crate::cells::{explore_cells, radio_value, CONCURRENT_PARTS, SENTINEL_CELL_ID};
use crate::cli::{Cli, Command, ExploreSource};
use crate::coverage::lte_band;
use crate::db::{database_url, lazy_pool, migrate_when_reachable};
use crate::fetch::{AppleWPSClient, Location};
use crate::heatmap::heatmap_document;
use crate::protobuf::CellTower;
use crate::wifi::explore_wifi;

pub mod protobuf {
    include!(concat!(env!("OUT_DIR"), "/apple.rs"));
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
                .connect(&database_url()?)
                .await?;
            sqlx::migrate!().run(&pool).await?;

            match source {
                ExploreSource::Cell { no_probe } => {
                    explore_cells(&client, &pool, !no_probe).await?
                }

                ExploreSource::Crawl => explore_wifi(&client, &pool, true, false).await?,
                ExploreSource::Todo { ignore_extra } => {
                    explore_wifi(&client, &pool, false, ignore_extra).await?
                }
            }
        }
        Command::Query { mac } => {
            let wifis = client
                .query_wifi(
                    Mac::from_str(&mac)
                        .map_err(|_| anyhow::anyhow!("not a MAC address: {mac}"))?
                        .0
                        .into(),
                )
                .await?;
            if let Some((_, Some(loc))) = wifis.first() {
                println!("{}", osm(loc));
                println!("{} networks in reply", wifis.len());
            } else {
                println!("not found")
            }
        }

        Command::Cell { mcc, mnc, tacid, cellid, raw } =>{
            let request = CellTower{
                mcc,
                mnc,
                cell_id: cellid,
                tac_id: tacid,
                location: None,
                uarfcn: None,
                pid: None,
            };
            if raw {
                print!("{}", raw::summarize(&client.query_cell_raw(request).await?));
                return Ok(());
            }
            println!("cellid: {cellid}");

            let cells = client.query_cell(request).await?;
            if let Some((_, Some(loc))) = cells.first() {
                println!("{}", osm(loc));
                println!("{} entries in reply", cells.len());
            } else {
                println!("not found")
            }
            println!(
                "{:<12} {:>4} {:>7} {:>7}  {:<14} {:>4} {:>7}  location",
                "cell_id", "mnc", "tac", "earfcn", "band", "pci", "acc_m"
            );
            for (cell, loc) in cells {
                let dash = || "-".to_string();
                let earfcn = radio_value(cell.uarfcn);
                let band = earfcn.and_then(lte_band).map_or_else(dash, |(b, mhz)| format!("{b} ({mhz} MHz)"));
                let acc = cell.location.and_then(|l| l.horizontal_accuracy);
                let id = if cell.cell_id == SENTINEL_CELL_ID { "area est.".into() } else { cell.cell_id.to_string() };
                println!(
                    "{id:<12} {:>4} {:>7} {:>7}  {band:<14} {:>4} {:>7}  {}",
                    cell.mnc,
                    cell.tac_id,
                    earfcn.map_or_else(dash, |e| e.to_string()),
                    radio_value(cell.pid).map_or_else(dash, |p| p.to_string()),
                    acc.map_or_else(dash, |a| a.to_string()),
                    loc.as_ref().map_or_else(|| "not found".to_string(), osm),
                );
            }

        }

        Command::Heatmap { mcc, mnc, tac_id, cell_id, radius_km, geojson, model, out, online, no_fetch } => {
            let key = (mcc, mnc, tac_id, cell_id);
            let pool = if online { None } else { Some(lazy_pool(1)?) };
            let pool = pool.as_ref().ok_or("database not used");
            let (rows, source) = lookup::heatmap_rows(&client, pool, key, radius_km, !no_fetch).await?;
            match &source {
                lookup::Source::Database => {}
                lookup::Source::Fetched(found) => {
                    println!("cell wasn't crawled yet: fetched it from Apple and saved {found} cells")
                }
                lookup::Source::Online(reason) => {
                    println!("fetched {} cells of this operator from Apple; not saved ({reason})", rows.len())
                }
            }
            let extension = if geojson { "geojson" } else { "html" };
            let path =
                out.unwrap_or_else(|| format!("cell-{mcc}-{mnc}-{tac_id}-{cell_id}.{extension}"));
            let (document, neighbours) = heatmap_document(key, rows, &model, geojson, &source).await?;
            std::fs::write(&path, document)?;
            println!("wrote {path} ({neighbours} nearby cells)");
        }

        Command::Serve { listen } => {
            // starts even when the database is down: requests then fall back to Apple, and
            // the schema is brought up to date once it can be reached
            let pool = lazy_pool(CONCURRENT_PARTS as u32)?;
            let schema_ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            tokio::spawn(migrate_when_reachable(pool.clone(), schema_ready.clone()));
            api::serve(client, pool, schema_ready, &listen).await?;
        }

        Command::Migrate => {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&database_url()?)
                .await?;
            sqlx::migrate!().run(&pool).await?;
            println!("database schema is up to date");
        }

        Command::Oui { oui } => {
            for i in 0..256u64.pow(3) {
                println!("{oui}{i:06x}");
            }
        }
    }

    Ok(())
}

fn osm(Location { lat, lon, .. }: &Location) -> String {
    format!("https://www.openstreetmap.org/search?query={lat}%2F{lon}#map=16/{lat}/{lon}")
}
