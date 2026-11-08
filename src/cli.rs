//! Command-line interface: the commands and the coverage model's options.

use clap::{Parser, Subcommand};

use crate::coverage;
use crate::heatmap::DEFAULT_RADIUS_KM;

#[derive(Parser)]
pub(crate) struct Cli {
    #[clap(subcommand)]
    pub(crate) command: Command,
    #[arg(long)]
    pub(crate) china: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum Command {
    Explore {
        #[clap(subcommand)]
        source: ExploreSource,
    },
    Query {
        mac: String,
    },
    Cell {
        mcc: u32,
        mnc: u32,
        tacid: u32,
        cellid: u32,
        /// print every field of Apple's raw reply, including ones wtfps doesn't decode
        #[arg(long)]
        raw: bool,
    },
    Oui {
        oui: String,
    },
    /// Write an HTML map estimating the coverage of a cell. Uses the database; a cell
    /// that isn't crawled yet is fetched from Apple (with its site's sibling sectors) and
    /// saved first
    Heatmap {
        mcc: i32,
        mnc: i32,
        tac_id: i32,
        cell_id: i64,
        /// include same-operator cells within this distance
        #[arg(long, default_value_t = DEFAULT_RADIUS_KM)]
        radius_km: f64,
        /// write GeoJSON (the cell, its accuracy circle, the nearby cells and the modelled
        /// coverage as H3 hexagons) instead of HTML
        #[arg(long)]
        geojson: bool,
        #[command(flatten)]
        model: ModelArgs,
        /// output file [default: cell-<mcc>-<mnc>-<tac>-<cell>.html or .geojson]
        #[arg(long)]
        out: Option<String>,
        /// build from Apple's replies only, without a database (nothing is saved)
        #[arg(long, conflicts_with = "no_fetch")]
        online: bool,
        /// only use the database: don't ask Apple for a cell that isn't crawled yet
        #[arg(long)]
        no_fetch: bool,
    },
    /// Apply pending database migrations and exit
    Migrate,
    /// Serve the HTTP API (see src/api.rs)
    Serve {
        #[arg(long, default_value = "0.0.0.0:8090")]
        listen: String,
    },
}

/// Coverage model settings for `heatmap --geojson`; the HTML map has sliders for these
#[derive(clap::Args, Debug, Clone)]
pub(crate) struct ModelArgs {
    /// path-loss exponent: about 3.4-3.7 for masts of 50-15 m (Okumura-Hata's distance slope)
    #[arg(long, default_value_t = 3.5)]
    pub(crate) path_loss: f64,
    /// random signal variation from buildings and terrain, in dB
    #[arg(long, default_value_t = 8.0)]
    pub(crate) sigma_db: f64,
    /// how much each cell's position is blurred by its accuracy (0 = exact)
    #[arg(long, default_value_t = 0.0)]
    pub(crate) acc_weight: f64,
    /// distance at which signal fades out [default: Okumura-Hata's radius for the cell's band
    /// and environment, or 4x the typical spacing of competing cells without a known band]
    #[arg(long)]
    pub(crate) range_km: Option<f64>,
    /// compete with cells on all frequencies, not just the cell's own EARFCN
    #[arg(long)]
    pub(crate) all_frequencies: bool,
    /// experimental: fade out at the cell's own reach from Apple's undocumented field 12
    /// instead of one shared max range (--range-km); falls back to the range without it
    #[arg(long)]
    pub(crate) use_unknown_12: bool,
    /// H3 resolution of the coverage hexagons (10: ~0.015 km², 11: ~0.002 km²)
    /// [default: 10, coarser for large rural areas]
    #[arg(long, value_parser = clap::value_parser!(u8).range(7..=12))]
    pub(crate) hex_res: Option<u8>,
    /// how signal depends on frequency and how far a cell reaches by default: hata
    /// (Okumura-Hata / COST-231) or simple (distance only, range 4x the cell spacing)
    #[arg(long, value_enum, default_value_t = coverage::Propagation::Hata)]
    pub(crate) propagation: coverage::Propagation,
    /// experimental: add diffraction loss from hills and ridges (Copernicus GLO-30 elevation,
    /// downloaded into WTFPS_DEM_DIR, default `dem`, on first use)
    #[arg(long)]
    pub(crate) terrain: bool,
    /// experimental: give sectors a wedge-shaped antenna pattern, pointing the way their
    /// position is offset from their site's centre (a rough estimate)
    #[arg(long)]
    pub(crate) directional: bool,
    /// write smooth likelihood contour bands instead of hexagons
    #[arg(long)]
    pub(crate) contours: bool,
    /// likelihood levels where the bands start, ascending, between 0 and 1
    #[arg(long, value_delimiter = ',', default_values_t = [0.1, 0.25, 0.5, 0.75, 0.9])]
    pub(crate) contour_levels: Vec<f64>,
    /// spacing of the grid the contours are traced on, in metres
    #[arg(long, default_value_t = 20.0)]
    pub(crate) contour_step_m: f64,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ExploreSource {
    Crawl,
    Todo {
        #[arg(long)]
        ignore_extra: bool,
    },
    Cell {
        /// don't guess cell IDs near known sites once the area crawl runs out
        #[arg(long)]
        no_probe: bool,
    },
}

/// The coverage model's default settings (as with no command-line options).
pub(crate) fn default_model() -> ModelArgs {
    #[derive(Parser)]
    struct Defaults {
        #[command(flatten)]
        model: ModelArgs,
    }
    Defaults::parse_from(["wtfps"]).model
}
