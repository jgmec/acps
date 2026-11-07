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
    /// LTE channel; gives the cell's frequency for the Okumura-Hata terms
    pub earfcn: Option<i32>,
}

/// How signal strength depends on frequency and how far a cell reaches by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Propagation {
    /// Okumura-Hata / COST-231: lower frequencies carry further, and the default range is
    /// Hata's radius for the cell's band and environment
    Hata,
    /// distance only: every frequency is equal, and the default range is 4x the typical
    /// spacing of competing cells
    Simple,
}

impl Propagation {
    pub fn name(self) -> &'static str {
        match self {
            Propagation::Hata => "hata",
            Propagation::Simple => "simple",
        }
    }
}

pub struct Params {
    /// path-loss exponent: about 3.4-3.7 for masts of 50-15 m (Okumura-Hata's slope)
    pub path_loss: f64,
    /// random signal variation from buildings and terrain, in dB
    pub sigma_db: f64,
    /// how much each cell's position is blurred by its accuracy (0 = exact)
    pub acc_weight: f64,
    /// distance at which signal fades out; None = chosen by `propagation`
    pub range_m: Option<f64>,
    pub propagation: Propagation,
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
    /// how the range was chosen, e.g. "Okumura-Hata, 1815 MHz, urban"
    pub range_basis: String,
    /// with `use_reach`: the target's reach, or None if it has no field 12 (then the
    /// shared range is used)
    pub target_reach_m: Option<f64>,
    /// area of the hexagons where the target is the most likely server (likelihood > 50%)
    pub area_km2: f64,
    /// furthest hexagon centre with likelihood > 50%
    pub reach_m: f64,
}

/// Bounds of the automatic max range: Okumura-Hata's radius for the cell's band and
/// environment, or without a known band 4x the typical cell spacing; from town cells to rural
/// low-band cells reaching tens of km. The page (src/heatmap.html) uses the same numbers.
const MIN_RANGE_M: f64 = 1500.0;
const MIN_HATA_RANGE_M: f64 = 500.0;
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
    /// extra path loss from each cell's frequency relative to the target's, in dB (zero on
    /// the same frequency, or when either frequency is unknown)
    frequency_loss: Vec<f64>,
    k: f64,
    temperature: f64,
    p_edge: f64,
    range_m: f64,
    range_basis: String,
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
            frequency_loss: vec![],
            k: 10.0 * params.path_loss / LN_10,
            temperature: params.sigma_db * 6f64.sqrt() / PI,
            p_edge: 0.0,
            range_m: 0.0,
            range_basis: String::new(),
            target_reach_m: params
                .use_reach
                .then(|| target.reach.filter(|&r| r > 0).map(|r| r as f64))
                .flatten(),
            extent: 0.0,
        };
        model.xy = cells.iter().map(|c| model.to_xy(c.lat, c.lon)).collect();

        // Okumura-Hata: the environment from how far apart cells on the target's own channel
        // are (with every frequency competing, other bands' sectors on the same masts would
        // make any place look urban); each cell's frequency costs it extra path loss
        // relative to the target's
        let same_channel: Vec<(f64, f64)> = model
            .xy
            .iter()
            .zip(cells)
            .filter(|(_, c)| target.earfcn.is_some() && c.earfcn == target.earfcn)
            .map(|(&xy, _)| xy)
            .collect();
        let spacing = typical_spacing(if same_channel.len() >= 2 { &same_channel } else { &model.xy });
        let environment = Environment::from_spacing(spacing);
        // the simple model ignores frequency: no frequency loss, range from cell spacing
        let target_mhz = target.earfcn.and_then(downlink_mhz).filter(|_| params.propagation == Propagation::Hata);
        let loss = |mhz: f64| hata_loss_1km_db(mhz) + environment.correction_db(mhz);
        model.frequency_loss = cells
            .iter()
            .map(|c| match (target_mhz, c.earfcn.and_then(downlink_mhz)) {
                (Some(target), Some(cell)) => loss(cell) - loss(target),
                _ => 0.0,
            })
            .collect();

        (model.range_m, model.range_basis) = match (params.range_m, target_mhz) {
            (Some(range), _) => (range, "set".into()),
            (None, Some(mhz)) => (
                hata_radius_m(mhz, environment).clamp(MIN_HATA_RANGE_M, MAX_RANGE_M),
                format!("Okumura-Hata, {mhz:.0} MHz, {}", environment.name()),
            ),
            (None, None) => ((4.0 * spacing).clamp(MIN_RANGE_M, MAX_RANGE_M), "4x cell spacing".into()),
        };
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
            power[j] = -0.5 * self.k * d2.ln() - self.frequency_loss[j];
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
        range_basis: model.range_basis.clone(),
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

/// LTE downlink bands (3GPP TS 36.101): first and last EARFCN, band, nominal MHz, and the
/// downlink frequency in MHz at the first EARFCN (each EARFCN adds 0.1 MHz).
pub const LTE_BANDS: [(i32, i32, u32, u32, f64); 15] = [
    (0, 599, 1, 2100, 2110.0),
    (600, 1199, 2, 1900, 1930.0),
    (1200, 1949, 3, 1800, 1805.0),
    (1950, 2399, 4, 1700, 2110.0),
    (2400, 2649, 5, 850, 869.0),
    (2750, 3449, 7, 2600, 2620.0),
    (3450, 3799, 8, 900, 925.0),
    (5010, 5179, 12, 700, 729.0),
    (5180, 5279, 13, 700, 746.0),
    (6150, 6449, 20, 800, 791.0),
    (9210, 9659, 28, 700, 758.0),
    (37750, 38249, 38, 2600, 2570.0),
    (38650, 39649, 40, 2300, 2300.0),
    (39650, 41589, 41, 2500, 2496.0),
    (41590, 43589, 42, 3500, 3400.0),
];

/// Downlink frequency in MHz of an LTE channel.
pub fn downlink_mhz(earfcn: i32) -> Option<f64> {
    LTE_BANDS
        .iter()
        .find(|b| (b.0..=b.1).contains(&earfcn))
        .map(|b| b.4 + 0.1 * f64::from(earfcn - b.0))
}

// Okumura-Hata (150-1500 MHz) and COST-231 Hata (1500-2000 MHz, extrapolated beyond) with
// these assumptions, for the frequency loss between cells and the default range. The page
// (src/heatmap.html) uses the same numbers.
const HATA_MAST_M: f64 = 30.0;
const HATA_PHONE_M: f64 = 1.5;
/// maximum allowed path loss of a typical LTE link budget
const HATA_MAX_PATH_LOSS_DB: f64 = 140.0;
/// typical spacing of competing cells below which an area counts as urban, then suburban
const URBAN_SPACING_M: f64 = 800.0;
const SUBURBAN_SPACING_M: f64 = 2000.0;

#[derive(Clone, Copy)]
enum Environment {
    Urban,
    Suburban,
    Rural,
}

impl Environment {
    fn from_spacing(spacing_m: f64) -> Self {
        if spacing_m < URBAN_SPACING_M {
            Environment::Urban
        } else if spacing_m < SUBURBAN_SPACING_M {
            Environment::Suburban
        } else {
            Environment::Rural
        }
    }

    fn name(self) -> &'static str {
        match self {
            Environment::Urban => "urban",
            Environment::Suburban => "suburban",
            Environment::Rural => "rural",
        }
    }

    /// Hata's correction from urban path loss, in dB
    fn correction_db(self, mhz: f64) -> f64 {
        match self {
            Environment::Urban => 0.0,
            Environment::Suburban => -2.0 * (mhz / 28.0).log10().powi(2) - 5.4,
            Environment::Rural => -4.78 * mhz.log10().powi(2) + 18.33 * mhz.log10() - 40.94,
        }
    }
}

/// Urban Hata path loss at 1 km, in dB (small or medium city).
fn hata_loss_1km_db(mhz: f64) -> f64 {
    let lf = mhz.log10();
    let phone = (1.1 * lf - 0.7) * HATA_PHONE_M - (1.56 * lf - 0.8);
    let base = if mhz <= 1500.0 { 69.55 + 26.16 * lf } else { 46.3 + 33.9 * lf };
    base - 13.82 * HATA_MAST_M.log10() - phone
}

/// Distance at which Hata's path loss reaches HATA_MAX_PATH_LOSS_DB, in metres.
fn hata_radius_m(mhz: f64, environment: Environment) -> f64 {
    let slope = 44.9 - 6.55 * HATA_MAST_M.log10();
    let at_1km = hata_loss_1km_db(mhz) + environment.correction_db(mhz);
    1000.0 * 10f64.powf((HATA_MAX_PATH_LOSS_DB - at_1km) / slope)
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
