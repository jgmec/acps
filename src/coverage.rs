//! Coverage model for a single cell, evaluated over an H3 hexagon grid.
//!
//! Same model as the HTML map (src/heatmap.html); keep the two in sync. Signal falls off
//! as -10 n log10(distance), and a phone uses the strongest competing cell, with random
//! signal variation of sigma dB modelled as a logit over the received powers. A virtual
//! "no service" competitor at the max range makes coverage fade out.
//!
//! With `use_reach`, the fade-out distance is the target cell's own reach from Apple's
//! undocumented location field 12 (which grows with cell spacing like a coverage radius)
//! instead of one shared max range. Competition stays distance-based: treating field 12
//! as each cell's strength let a few far-reaching outliers (40 km+) swamp whole towns.

use std::f64::consts::{LN_10, PI};

use anyhow::Result;
use h3o::{CellIndex, LatLng, Resolution};

/// Apple's smallest reported cell accuracy; used when a cell has none.
const DEFAULT_ACC_M: f64 = 1414.0;
const METRES_PER_DEGREE: f64 = 111_320.0;
/// hexagons where the cell is less likely than this to serve are left out
const MIN_LIKELIHOOD: f64 = 0.01;

pub struct ModelCell {
    pub lat: f64,
    pub lon: f64,
    pub acc: Option<i64>,
    /// Apple's field 12; the target's is used as its reach in metres with `use_reach`
    pub reach: Option<i64>,
}

pub struct Params {
    /// path-loss exponent: about 2 in open country, 3-4 in towns
    pub path_loss: f64,
    /// random signal variation from buildings and terrain, in dB
    pub sigma_db: f64,
    /// how much each cell's position is blurred by its accuracy (0 = exact)
    pub acc_weight: f64,
    /// distance at which signal fades out; None = 4x the typical spacing of competing cells
    pub range_m: Option<f64>,
    /// fade out at the target's own reach (field 12) instead of one shared max range
    pub use_reach: bool,
    pub resolution: Resolution,
}

pub struct Hex {
    pub index: CellIndex,
    /// chance that the target cell is the strongest at this hexagon's centre
    pub likelihood: f64,
    /// target cell's modelled signal above the edge of coverage, ignoring other cells
    pub signal_db: f64,
    /// index into the competitor list (0 = target) of the most likely server here
    pub best: usize,
    pub best_likelihood: f64,
}

/// The summary is measured on hexagons this many resolutions finer than the output ones,
/// so that a small area (a few output hexagons) isn't off by a whole hexagon.
const SUMMARY_REFINE: usize = 2;

pub struct Coverage {
    pub hexes: Vec<Hex>,
    pub range_m: f64,
    /// with `use_reach`: the target's reach, or None if it has no field 12 (then the
    /// shared range is used)
    pub target_reach_m: Option<f64>,
    /// area of the hexagons where the target is the most likely server (likelihood > 50%)
    pub area_km2: f64,
    /// furthest hexagon centre with likelihood > 50%
    pub reach_m: f64,
}

/// `cells[0]` is the target; the rest are the cells it competes with.
pub fn compute(cells: &[ModelCell], params: &Params) -> Result<Coverage> {
    let target = &cells[0];
    let m_lon = METRES_PER_DEGREE * target.lat.to_radians().cos();
    let to_xy = |lat: f64, lon: f64| ((lon - target.lon) * m_lon, (lat - target.lat) * METRES_PER_DEGREE);

    let xy: Vec<(f64, f64)> = cells.iter().map(|c| to_xy(c.lat, c.lon)).collect();
    let blur2: Vec<f64> = cells
        .iter()
        .map(|c| {
            let acc = c.acc.filter(|&a| a > 0).map_or(DEFAULT_ACC_M, |a| a as f64);
            (params.acc_weight * acc).powi(2)
        })
        .collect();

    let range_m = params
        .range_m
        .unwrap_or_else(|| (4.0 * typical_spacing(&xy)).clamp(1500.0, 15000.0));
    let k = 10.0 * params.path_loss / LN_10;
    let temperature = params.sigma_db * 6f64.sqrt() / PI;

    let target_reach_m = params
        .use_reach
        .then(|| target.reach.filter(|&r| r > 0).map(|r| r as f64))
        .flatten();
    let p_edge = -k * target_reach_m.unwrap_or(range_m).ln();

    let mut power = vec![0.0; cells.len()];
    let mut evaluate = |x: f64, y: f64| {
        let mut max = p_edge;
        let mut best = 0;
        for (j, &(cx, cy)) in xy.iter().enumerate() {
            let d2 = (x - cx).powi(2) + (y - cy).powi(2) + blur2[j] + 1.0;
            power[j] = -0.5 * k * d2.ln();
            if power[j] > power[best] {
                best = j;
            }
            max = max.max(power[j]);
        }
        let sum: f64 = ((p_edge - max) / temperature).exp()
            + power.iter().map(|p| ((p - max) / temperature).exp()).sum::<f64>();
        let share = |p: f64| ((p - max) / temperature).exp() / sum;
        (share(power[0]), power[0] - p_edge, best, share(power[best]))
    };

    // every hexagon whose centre lies within the modelled extent around the target
    let extent = (target_reach_m.unwrap_or(range_m) * 1.2).clamp(1500.0, 15000.0);
    let spacing = params.resolution.edge_length_m() * 3f64.sqrt();
    let rings = (extent / spacing).ceil() as u32 + 1;
    let centre = LatLng::new(target.lat, target.lon)?.to_cell(params.resolution);

    let mut fine = params.resolution;
    for _ in 0..SUMMARY_REFINE {
        fine = fine.succ().unwrap_or(fine);
    }

    let mut coverage = Coverage {
        hexes: vec![],
        range_m,
        target_reach_m,
        area_km2: 0.0,
        reach_m: 0.0,
    };
    for index in centre.grid_disk::<Vec<_>>(rings) {
        let ll = LatLng::from(index);
        let (x, y) = to_xy(ll.lat(), ll.lng());
        let distance = x.hypot(y);
        if distance > extent {
            continue;
        }
        let (likelihood, signal_db, best, best_likelihood) = evaluate(x, y);
        // hexagons this unlikely have no sub-hexagon above 50%, so skip refining them
        if likelihood >= MIN_LIKELIHOOD {
            for child in index.children(fine) {
                let ll = LatLng::from(child);
                let (cx, cy) = to_xy(ll.lat(), ll.lng());
                let (child_likelihood, ..) = evaluate(cx, cy);
                if child_likelihood > 0.5 {
                    coverage.area_km2 += child.area_m2() / 1e6;
                    coverage.reach_m = coverage.reach_m.max(cx.hypot(cy));
                }
            }
        }
        if likelihood >= MIN_LIKELIHOOD {
            coverage.hexes.push(Hex { index, likelihood, signal_db, best, best_likelihood });
        }
    }
    Ok(coverage)
}

/// Median distance from each of the nearest cells to its nearest other cell, ignoring
/// cells under 30 m apart (sectors on the same mast).
fn typical_spacing(xy: &[(f64, f64)]) -> f64 {
    let mut nearest: Vec<f64> = xy
        .iter()
        .take(300)
        .enumerate()
        .filter_map(|(i, &(x, y))| {
            xy.iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .map(|(_, &(ox, oy))| (x - ox).hypot(y - oy))
                .filter(|&d| d > 30.0)
                .min_by(f64::total_cmp)
        })
        .collect();
    nearest.sort_by(f64::total_cmp);
    if nearest.is_empty() { 1000.0 } else { nearest[nearest.len() / 2] }
}
