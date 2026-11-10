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
mod gsm;
mod heatmap;
mod lookup;
mod raw;
mod terrain;
mod wifi;

use crate::cells::{explore_cells, radio_value, Radio, CONCURRENT_PARTS};
use crate::cli::{Cli, Command, ExploreSource};
use crate::coverage::{lte_band, nr_arfcn_mhz};
use crate::db::{database_url, lazy_pool, migrate_when_reachable};
use crate::fetch::{AppleWPSClient, Location};
use crate::heatmap::heatmap_document;
use crate::protobuf::{CellTower, GsmCellTower, NrCellTower};
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

        Command::Cell { mcc, mnc, tacid, cellid, raw, nr, gsm } => {
            let rows: Vec<CellRow> = if gsm {
                let Ok(cell_id) = u32::try_from(cellid) else {
                    anyhow::bail!("cell ID {cellid} is longer than 32 bits, too long for GSM");
                };
                let request = GsmCellTower {
                    mcc,
                    mnc,
                    cell_id,
                    lac: tacid,
                    location: None,
                    unknown_11: None,
                    unknown_12: None,
                };
                if raw {
                    print!("{}", raw::summarize(&client.query_gsm_cell_raw(request).await?));
                    return Ok(());
                }
                client.query_gsm_cells(vec![request]).await?.into_iter().map(|(cell, loc)| CellRow {
                    area_estimate: false,
                    cell_id: cell.cell_id.into(),
                    mnc: cell.mnc,
                    tac_id: cell.lac,
                    // undocumented; shown as they come
                    channel: radio_value(cell.unknown_11),
                    band: radio_value(cell.unknown_12).map(|v| v.to_string()),
                    pci: None,
                    acc: cell.location.and_then(|l| l.horizontal_accuracy),
                    loc,
                }).collect()
            } else if nr {
                let request = NrCellTower {
                    mcc,
                    mnc,
                    cell_id: cellid,
                    tac_id: tacid,
                    location: None,
                    nrarfcn: None,
                    pid: None,
                };
                if raw {
                    print!("{}", raw::summarize(&client.query_nr_cell_raw(request).await?));
                    return Ok(());
                }
                client.query_nr_cell(request).await?.into_iter().map(|(cell, loc)| {
                    let channel = radio_value(cell.nrarfcn);
                    CellRow {
                        area_estimate: cell.cell_id == u64::MAX,
                        cell_id: cell.cell_id,
                        mnc: cell.mnc,
                        tac_id: cell.tac_id,
                        channel,
                        band: channel.and_then(nr_arfcn_mhz).map(|mhz| format!("{mhz:.0} MHz")),
                        pci: radio_value(cell.pid),
                        acc: cell.location.and_then(|l| l.horizontal_accuracy),
                        loc,
                    }
                }).collect()
            } else {
                let Ok(cell_id) = u32::try_from(cellid) else {
                    anyhow::bail!("cell ID {cellid} is longer than 32 bits, too long for LTE: for a 5G cell add --nr");
                };
                let request = CellTower {
                    mcc,
                    mnc,
                    cell_id,
                    tac_id: tacid,
                    location: None,
                    uarfcn: None,
                    pid: None,
                };
                if raw {
                    print!("{}", raw::summarize(&client.query_cell_raw(request).await?));
                    return Ok(());
                }
                client.query_cell(request).await?.into_iter().map(|(cell, loc)| {
                    let channel = radio_value(cell.uarfcn);
                    CellRow {
                        area_estimate: cell.cell_id == u32::MAX,
                        cell_id: cell.cell_id.into(),
                        mnc: cell.mnc,
                        tac_id: cell.tac_id,
                        channel,
                        band: channel.and_then(lte_band).map(|(b, mhz)| format!("{b} ({mhz} MHz)")),
                        pci: radio_value(cell.pid),
                        acc: cell.location.and_then(|l| l.horizontal_accuracy),
                        loc,
                    }
                }).collect()
            };
            println!("cellid: {cellid}");
            let columns = if gsm {
                ("lac", "f11", "field 12")
            } else if nr {
                ("tac", "nrarfcn", "frequency")
            } else {
                ("tac", "earfcn", "band")
            };
            print_cells(&rows, columns);
        }

        Command::Heatmap { mcc, mnc, tac_id, cell_id, radius_km, geojson, model, out, online, no_fetch, gsm, nr } => {
            let key = (mcc, mnc, tac_id, cell_id);
            let radio = if gsm { Radio::Gsm } else if nr { Radio::Nr } else { Radio::Lte };
            let pool = if online { None } else { Some(lazy_pool(1)?) };
            let pool = pool.as_ref().ok_or("database not used");
            let (rows, source) = lookup::heatmap_rows(&client, pool, key, radio, radius_km, !no_fetch).await?;
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
                out.unwrap_or_else(|| {
                    let prefix = if radio == Radio::Lte { String::new() } else { format!("{}-", radio.as_str()) };
                    format!("cell-{prefix}{mcc}-{mnc}-{tac_id}-{cell_id}.{extension}")
                });
            let (document, neighbours) = heatmap_document(key, radio, rows, &model, geojson, &source).await?;
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

/// One entry of a `cell` reply, LTE or 5G NR.
struct CellRow {
    area_estimate: bool,
    cell_id: u64,
    mnc: u32,
    tac_id: u32,
    /// EARFCN or NR-ARFCN
    channel: Option<i32>,
    /// LTE band, or the NR channel's frequency
    band: Option<String>,
    pci: Option<i32>,
    acc: Option<i64>,
    loc: Option<Location>,
}

/// The `cell` command's output: the asked cell's location, then every entry of the reply.
/// `columns` names the area (TAC or LAC), channel and band columns.
fn print_cells(rows: &[CellRow], (area, channel, band): (&str, &str, &str)) {
    if let Some(CellRow { loc: Some(loc), .. }) = rows.first() {
        println!("{}", osm(loc));
        println!("{} entries in reply", rows.len());
    } else {
        println!("not found")
    }
    println!(
        "{:<12} {:>4} {area:>7} {channel:>7}  {band:<14} {:>4} {:>7}  location",
        "cell_id", "mnc", "pci", "acc_m"
    );
    let dash = || "-".to_string();
    for row in rows {
        let id = if row.area_estimate { "area est.".into() } else { row.cell_id.to_string() };
        println!(
            "{id:<12} {:>4} {:>7} {:>7}  {:<14} {:>4} {:>7}  {}",
            row.mnc,
            row.tac_id,
            row.channel.map_or_else(dash, |c| c.to_string()),
            row.band.clone().unwrap_or_else(dash),
            row.pci.map_or_else(dash, |p| p.to_string()),
            row.acc.map_or_else(dash, |a| a.to_string()),
            row.loc.as_ref().map_or_else(|| "not found".to_string(), osm),
        );
    }
}

fn osm(Location { lat, lon, .. }: &Location) -> String {
    format!("https://www.openstreetmap.org/search?query={lat}%2F{lon}#map=16/{lat}/{lon}")
}
