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
//!
//! With `directional`, cells with an estimated antenna direction (see `estimate_azimuths`)
//! get a sector antenna's gain, so they cover a wedge rather than a circle.

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
    /// estimated antenna direction, degrees clockwise from north, used with `directional`
    pub azimuth_deg: Option<f64>,
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
    /// give cells with an estimated antenna direction a sector antenna's gain
    pub directional: bool,
    /// hexagon size; None picks one from the modelled area's size
    pub resolution: Option<Resolution>,
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
    /// hexagon resolution used
    pub resolution: Resolution,
    pub range_m: f64,
    /// with `use_reach`: the target's reach, or None if it has no field 12 (then the
    /// shared range is used)
    pub target_reach_m: Option<f64>,
    /// area of the hexagons where the target is the most likely server (likelihood > 50%)
    pub area_km2: f64,
    /// furthest hexagon centre with likelihood > 50%
    pub reach_m: f64,
}

/// Bounds of the automatic max range: 4x the typical cell spacing, from town cells to rural
/// low-band cells reaching tens of km. The page (src/heatmap.html) uses the same numbers.
const MIN_RANGE_M: f64 = 1500.0;
const MAX_RANGE_M: f64 = 60_000.0;
/// How far out, in max ranges, the coverage is evaluated. The fade-out is gradual (about 15%
/// at twice the range where no other cell competes, 6% at three times), so a tighter area
/// cuts the coverage off with a visible straight edge.
const EXTENT_PER_RANGE: f64 = 3.0;
/// With an automatic hexagon size, the finest resolution with at most this many hexagons
/// across the modelled area's radius (about 20,000 in all), so rural maps stay small.
const AUTO_RES_RINGS: f64 = 80.0;
const FINEST_AUTO_RES: Resolution = Resolution::Ten;

/// The model set up for one target: competitor positions in metres around the target and
/// the constants of the received-power logit. `cells[0]` is the target.
struct Model {
    target_lat: f64,
    target_lon: f64,
    m_lon: f64,
    xy: Vec<(f64, f64)>,
    blur2: Vec<f64>,
    /// antenna direction in radians clockwise from north, if modelled for that cell
    azimuth: Vec<Option<f64>>,
    k: f64,
    temperature: f64,
    p_edge: f64,
    range_m: f64,
    target_reach_m: Option<f64>,
    /// half-width of the modelled area around the target
    extent: f64,
}

impl Model {
    fn new(cells: &[ModelCell], params: &Params) -> Self {
        let target = &cells[0];
        let m_lon = METRES_PER_DEGREE * target.lat.to_radians().cos();
        let mut model = Model {
            target_lat: target.lat,
            target_lon: target.lon,
            m_lon,
            xy: vec![],
            blur2: cells
                .iter()
                .map(|c| {
                    let acc = c.acc.filter(|&a| a > 0).map_or(DEFAULT_ACC_M, |a| a as f64);
                    (params.acc_weight * acc).powi(2)
                })
                .collect(),
            azimuth: cells
                .iter()
                .map(|c| c.azimuth_deg.filter(|_| params.directional).map(f64::to_radians))
                .collect(),
            k: 10.0 * params.path_loss / LN_10,
            temperature: params.sigma_db * 6f64.sqrt() / PI,
            p_edge: 0.0,
            range_m: 0.0,
            target_reach_m: params
                .use_reach
                .then(|| target.reach.filter(|&r| r > 0).map(|r| r as f64))
                .flatten(),
            extent: 0.0,
        };
        model.xy = cells.iter().map(|c| model.to_xy(c.lat, c.lon)).collect();
        model.range_m = params
            .range_m
            .unwrap_or_else(|| (4.0 * typical_spacing(&model.xy)).clamp(MIN_RANGE_M, MAX_RANGE_M));
        let fade = model.target_reach_m.unwrap_or(model.range_m);
        model.p_edge = -model.k * fade.ln();
        model.extent = (fade * EXTENT_PER_RANGE).clamp(MIN_RANGE_M, MAX_RANGE_M);
        model
    }

    fn to_xy(&self, lat: f64, lon: f64) -> (f64, f64) {
        ((lon - self.target_lon) * self.m_lon, (lat - self.target_lat) * METRES_PER_DEGREE)
    }

    /// (target likelihood, target signal above the edge in dB, index of the most likely
    /// server, its likelihood) at (x, y); `power` is scratch space, one slot per cell
    fn evaluate(&self, x: f64, y: f64, power: &mut [f64]) -> (f64, f64, usize, f64) {
        let mut max = self.p_edge;
        let mut best = 0;
        for (j, &(cx, cy)) in self.xy.iter().enumerate() {
            let d2 = (x - cx).powi(2) + (y - cy).powi(2) + self.blur2[j] + 1.0;
            power[j] = -0.5 * self.k * d2.ln();
            if let Some(azimuth) = self.azimuth[j] {
                power[j] += sector_gain_db((x - cx).atan2(y - cy) - azimuth);
            }
            if power[j] > power[best] {
                best = j;
            }
            max = max.max(power[j]);
        }
        let t = self.temperature;
        let sum: f64 =
            ((self.p_edge - max) / t).exp() + power.iter().map(|p| ((p - max) / t).exp()).sum::<f64>();
        let share = |p: f64| ((p - max) / t).exp() / sum;
        (share(power[0]), power[0] - self.p_edge, best, share(power[best]))
    }
}

/// `cells[0]` is the target; the rest are the cells it competes with.
pub fn compute(cells: &[ModelCell], params: &Params) -> Result<Coverage> {
    let model = Model::new(cells, params);
    let mut power = vec![0.0; cells.len()];

    // every hexagon whose centre lies within the modelled extent around the target
    let resolution = params.resolution.unwrap_or_else(|| {
        let mut res = FINEST_AUTO_RES;
        while model.extent / (res.edge_length_m() * 3f64.sqrt()) > AUTO_RES_RINGS {
            match res.pred() {
                Some(coarser) => res = coarser,
                None => break,
            }
        }
        res
    });
    let spacing = resolution.edge_length_m() * 3f64.sqrt();
    let rings = (model.extent / spacing).ceil() as u32 + 1;
    let centre = LatLng::new(model.target_lat, model.target_lon)?.to_cell(resolution);

    let mut fine = resolution;
    for _ in 0..SUMMARY_REFINE {
        fine = fine.succ().unwrap_or(fine);
    }

    let mut coverage = Coverage {
        hexes: vec![],
        resolution,
        range_m: model.range_m,
        target_reach_m: model.target_reach_m,
        area_km2: 0.0,
        reach_m: 0.0,
    };
    for index in centre.grid_disk::<Vec<_>>(rings) {
        let ll = LatLng::from(index);
        let (x, y) = model.to_xy(ll.lat(), ll.lng());
        if x.hypot(y) > model.extent {
            continue;
        }
        let (likelihood, signal_db, best, best_likelihood) = model.evaluate(x, y, &mut power);
        // hexagons this unlikely have no sub-hexagon above 50%, so skip refining them
        if likelihood < MIN_LIKELIHOOD {
            continue;
        }
        for child in index.children(fine) {
            let ll = LatLng::from(child);
            let (cx, cy) = model.to_xy(ll.lat(), ll.lng());
            if model.evaluate(cx, cy, &mut power).0 > 0.5 {
                coverage.area_km2 += child.area_m2() / 1e6;
                coverage.reach_m = coverage.reach_m.max(cx.hypot(cy));
            }
        }
        coverage.hexes.push(Hex { index, likelihood, signal_db, best, best_likelihood });
    }
    Ok(coverage)
}

/// Largest contour grid, per side; coarser steps are used for very large areas.
const MAX_GRID: usize = 600;

/// Likelihood bands between consecutive `levels` (0..1, ascending), traced on a regular
/// grid at least `step_m` fine over the modelled square. Each band is a set of polygons
/// in longitude / latitude; the last band runs from the highest level up to 100%.
pub fn likelihood_bands(
    cells: &[ModelCell],
    params: &Params,
    levels: &[f64],
    step_m: f64,
) -> Result<Vec<contour::Band>> {
    let model = Model::new(cells, params);
    let mut power = vec![0.0; cells.len()];
    let side = ((2.0 * model.extent / step_m).ceil() as usize + 1).clamp(2, MAX_GRID);
    let step = 2.0 * model.extent / (side - 1) as f64;

    // row-major, first row southernmost
    let mut values = Vec::with_capacity(side * side);
    for row in 0..side {
        let y = -model.extent + row as f64 * step;
        for col in 0..side {
            let x = -model.extent + col as f64 * step;
            values.push(model.evaluate(x, y, &mut power).0);
        }
    }

    // the contour crate centres value i on [i, i + 1], so shift the origin half a step
    let (dlon, dlat) = (step / model.m_lon, step / METRES_PER_DEGREE);
    let west = model.target_lon - model.extent / model.m_lon;
    let south = model.target_lat - model.extent / METRES_PER_DEGREE;
    let mut thresholds = levels.to_vec();
    thresholds.push(1.0 + 1e-9);
    let bands = contour::ContourBuilder::new(side, side, true)
        .x_origin(west - dlon / 2.0)
        .y_origin(south - dlat / 2.0)
        .x_step(dlon)
        .y_step(dlat)
        .isobands(&values, &thresholds)?;
    Ok(bands.into_iter().filter(|b| !b.geometry().0.is_empty()).collect())
}

/// Horizontal pattern of a sector antenna (3GPP TR 36.814): -12 (θ / beamwidth)² dB,
/// at most `FRONT_TO_BACK_DB` down, for an angle θ off the antenna's direction.
const BEAMWIDTH_DEG: f64 = 65.0;
const FRONT_TO_BACK_DB: f64 = 20.0;

fn sector_gain_db(off_axis: f64) -> f64 {
    let theta = (off_axis + PI).rem_euclid(2.0 * PI) - PI;
    -(12.0 * (theta.to_degrees() / BEAMWIDTH_DEG).powi(2)).min(FRONT_TO_BACK_DB)
}

/// Sector offsets under this are too small to say which way the antenna points.
const MIN_SECTOR_OFFSET_M: f64 = 30.0;

/// Estimated antenna directions (degrees clockwise from north) for cells given as
/// (site, PCI, latitude, longitude). Apple places each sector where phones saw it, so a
/// little way out in the direction its antenna points: the direction is the bearing from
/// the site's centre to the sector. Cells of a site sharing a PCI are the same direction
/// on different bands, so they are averaged first and the centre is the mean of those
/// directions. A site needs at least two directions; this is rough, as sector positions
/// mostly reflect where phones are.
pub fn estimate_azimuths(cells: &[(i64, Option<i32>, f64, f64)]) -> Vec<Option<f64>> {
    use std::collections::BTreeMap;
    // site -> direction group (PCI, or the cell itself without one) -> cell indices
    let mut sites: BTreeMap<i64, BTreeMap<(Option<i32>, usize), Vec<usize>>> = BTreeMap::new();
    for (i, &(site, pci, ..)) in cells.iter().enumerate() {
        let key = (pci, if pci.is_some() { 0 } else { i });
        sites.entry(site).or_default().entry(key).or_default().push(i);
    }
    let mut azimuths = vec![None; cells.len()];
    for groups in sites.values().filter(|g| g.len() >= 2) {
        let mean = |idx: &[usize]| {
            let n = idx.len() as f64;
            (idx.iter().map(|&i| cells[i].2).sum::<f64>() / n, idx.iter().map(|&i| cells[i].3).sum::<f64>() / n)
        };
        let means: Vec<(f64, f64)> = groups.values().map(|idx| mean(idx)).collect();
        let n = means.len() as f64;
        let (clat, clon) = (means.iter().map(|m| m.0).sum::<f64>() / n, means.iter().map(|m| m.1).sum::<f64>() / n);
        let m_lon = METRES_PER_DEGREE * clat.to_radians().cos();
        for (idx, (lat, lon)) in groups.values().zip(&means) {
            let (dx, dy) = ((lon - clon) * m_lon, (lat - clat) * METRES_PER_DEGREE);
            if dx.hypot(dy) >= MIN_SECTOR_OFFSET_M {
                let azimuth = dx.atan2(dy).to_degrees().rem_euclid(360.0);
                for &i in idx {
                    azimuths[i] = Some(azimuth);
                }
            }
        }
    }
    azimuths
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
