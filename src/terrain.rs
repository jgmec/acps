//! Terrain: extra path loss from hills and ridges between a cell and a point, from
//! Copernicus GLO-30 elevation (30 m grid, free, worldwide).
//!
//! Elevation tiles (1° x 1° GeoTIFFs) are read from a folder (WTFPS_DEM_DIR, default `dem`)
//! and downloaded into it on first use. For each cell, the loss is traced along radial lines
//! from its mast: each line tracks the obstacle rising highest as seen from the mast, and a
//! point behind it gets that obstacle's single knife-edge diffraction loss (ITU-R P.526),
//! with the Earth's curvature (effective radius 4/3 of the real one). This is an
//! approximation: one dominant obstacle per path, a 30 m mast at Apple's estimate of where
//! the cell is used, and no buildings or vegetation.

use std::f64::consts::TAU;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

const METRES_PER_DEGREE: f64 = 111_320.0;
const MAST_M: f64 = 30.0;
const PHONE_M: f64 = 1.5;
/// effective Earth radius for radio paths (4/3 of 6371 km)
const EARTH_RADIUS_M: f64 = 6_371_000.0 * 4.0 / 3.0;
/// radial lines per cell (every 0.5°)
const RAYS: usize = 720;
const TILE_URL: &str = "https://copernicus-dem-30m.s3.amazonaws.com";

/// A rectangle of elevation in whole metres, rows from north to south.
struct Grid {
    /// position of the first sample (the north-west one), and the spacing, in degrees
    north: f64,
    west: f64,
    dlat: f64,
    dlon: f64,
    width: usize,
    height: usize,
    metres: Vec<i16>,
}

impl Grid {
    /// bilinear elevation, or None outside the grid
    fn elevation(&self, lat: f64, lon: f64) -> Option<f64> {
        let fx = (lon - self.west) / self.dlon;
        let fy = (self.north - lat) / self.dlat;
        if fx < 0.0 || fy < 0.0 || fx > (self.width - 1) as f64 || fy > (self.height - 1) as f64 {
            return None;
        }
        let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (u, v) = (fx - x0 as f64, fy - y0 as f64);
        let at = |x: usize, y: usize| f64::from(self.metres[y * self.width + x]);
        Some(
            at(x0, y0) * (1.0 - u) * (1.0 - v)
                + at(x1, y0) * u * (1.0 - v)
                + at(x0, y1) * (1.0 - u) * v
                + at(x1, y1) * u * v,
        )
    }
}

/// Elevation for an area: the parts of the tiles that cover it.
pub struct Dem {
    grids: Vec<Grid>,
}

impl Dem {
    /// Elevation in metres; 0 (sea level) where there's no tile, e.g. over the sea.
    pub fn elevation(&self, lat: f64, lon: f64) -> f64 {
        self.grids.iter().find_map(|g| g.elevation(lat, lon)).unwrap_or(0.0)
    }

    /// Load the elevation covering south..north x west..east (degrees), downloading tiles
    /// that aren't in `dir` yet.
    pub async fn load(dir: &Path, south: f64, north: f64, west: f64, east: f64) -> Result<Dem> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let client = reqwest::Client::new();
        let mut grids = vec![];
        for lat in south.floor() as i32..=north.floor() as i32 {
            for lon in west.floor() as i32..=east.floor() as i32 {
                let Some(path) = tile_file(&client, dir, lat, lon).await? else { continue };
                let grid = read_tile(&path, south, north, west, east)
                    .with_context(|| format!("reading {}", path.display()))?;
                grids.push(grid);
            }
        }
        Ok(Dem { grids })
    }
}

/// Copernicus GLO-30 name of the 1° tile whose south-west corner is (lat, lon).
fn tile_name(lat: i32, lon: i32) -> String {
    let (ns, ew) = (if lat < 0 { 'S' } else { 'N' }, if lon < 0 { 'W' } else { 'E' });
    format!("Copernicus_DSM_COG_10_{ns}{:02}_00_{ew}{:03}_00_DEM", lat.abs(), lon.abs())
}

/// The tile's file in `dir`, downloading it first if needed; None for squares without a
/// tile (open sea), which are remembered with a `.missing` marker.
async fn tile_file(client: &reqwest::Client, dir: &Path, lat: i32, lon: i32) -> Result<Option<PathBuf>> {
    let name = tile_name(lat, lon);
    let path = dir.join(format!("{name}.tif"));
    let missing = dir.join(format!("{name}.missing"));
    if path.exists() {
        return Ok(Some(path));
    }
    if missing.exists() {
        return Ok(None);
    }
    let url = format!("{TILE_URL}/{name}/{name}.tif");
    info!("downloading elevation tile {name} (about 30-50 MB)");
    let response = client.get(&url).send().await.with_context(|| format!("downloading {url}"))?;
    match response.status().as_u16() {
        200 => {}
        // the bucket answers 403 or 404 for squares without a tile
        403 | 404 => {
            std::fs::write(&missing, "")?;
            return Ok(None);
        }
        status => bail!("downloading {url}: HTTP {status}"),
    }
    let bytes = response.bytes().await.with_context(|| format!("downloading {url}"))?;
    // write to a temporary name first, so an interrupted download isn't taken for a tile
    let partial = dir.join(format!("{name}.tif.partial"));
    std::fs::write(&partial, &bytes)?;
    std::fs::rename(&partial, &path)?;
    Ok(Some(path))
}

/// The part of a GeoTIFF tile inside south..north x west..east (plus one sample).
fn read_tile(path: &Path, south: f64, north: f64, west: f64, east: f64) -> Result<Grid> {
    let mut decoder = Decoder::new(BufReader::new(File::open(path)?))?;
    let (width, height) = decoder.dimensions()?;
    let (width, height) = (width as usize, height as usize);
    // pixel-is-point georeferencing: the tie point is the centre of the first sample
    let tie = decoder.get_tag_f64_vec(Tag::ModelTiepointTag)?;
    let scale = decoder.get_tag_f64_vec(Tag::ModelPixelScaleTag)?;
    let (tile_west, tile_north, dlon, dlat) = (tie[3], tie[4], scale[0], scale[1]);
    let DecodingResult::F32(values) = decoder.read_image()? else {
        bail!("expected 32-bit float elevation");
    };

    let col = |lon: f64| (((lon - tile_west) / dlon).floor() as i64).clamp(0, width as i64 - 1) as usize;
    let row = |lat: f64| (((tile_north - lat) / dlat).floor() as i64).clamp(0, height as i64 - 1) as usize;
    let (c0, c1) = (col(west).saturating_sub(1), (col(east) + 1).min(width - 1));
    let (r0, r1) = (row(north).saturating_sub(1), (row(south) + 1).min(height - 1));
    let (w, h) = (c1 - c0 + 1, r1 - r0 + 1);
    let mut metres = Vec::with_capacity(w * h);
    for r in r0..=r1 {
        let line = &values[r * width + c0..=r * width + c1];
        metres.extend(line.iter().map(|&m| if m.is_finite() { m.round() as i16 } else { 0 }));
    }
    Ok(Grid {
        north: tile_north - r0 as f64 * dlat,
        west: tile_west + c0 as f64 * dlon,
        dlat,
        dlon,
        width: w,
        height: h,
        metres,
    })
}

/// A square of metres around the mapped cell, in the coverage model's local frame: x east,
/// y north, metres from the mapped cell, with `metres_per_degree_lon` at its latitude.
pub struct Frame {
    pub lat: f64,
    pub lon: f64,
    pub metres_per_degree_lon: f64,
    pub half_width_m: f64,
}

impl Frame {
    pub fn to_lat_lon(&self, x: f64, y: f64) -> (f64, f64) {
        (self.lat + y / METRES_PER_DEGREE, self.lon + x / self.metres_per_degree_lon)
    }

    pub fn to_xy(&self, lat: f64, lon: f64) -> (f64, f64) {
        ((lon - self.lon) * self.metres_per_degree_lon, (lat - self.lat) * METRES_PER_DEGREE)
    }
}

/// Diffraction loss of one cell over a frame, in dB: `size` x `size` samples, rows from
/// north to south, sample (i, j) at the centre of its square.
pub struct TerrainLoss {
    pub half_width_m: f64,
    pub size: usize,
    pub loss_db: Vec<f32>,
}

impl TerrainLoss {
    /// bilinear loss at local (x, y); 0 outside the frame
    pub fn at(&self, x: f64, y: f64) -> f64 {
        let step = 2.0 * self.half_width_m / self.size as f64;
        let fx = (x + self.half_width_m) / step - 0.5;
        let fy = (self.half_width_m - y) / step - 0.5;
        let last = (self.size - 1) as f64;
        if fx < -0.5 || fy < -0.5 || fx > last + 0.5 || fy > last + 0.5 {
            return 0.0;
        }
        let (fx, fy) = (fx.clamp(0.0, last), fy.clamp(0.0, last));
        let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.size - 1), (y0 + 1).min(self.size - 1));
        let (u, v) = (fx - x0 as f64, fy - y0 as f64);
        let at = |x: usize, y: usize| f64::from(self.loss_db[y * self.size + x]);
        at(x0, y0) * (1.0 - u) * (1.0 - v) + at(x1, y0) * u * (1.0 - v) + at(x0, y1) * (1.0 - u) * v + at(x1, y1) * u * v
    }
}

/// ITU-R P.526 single knife-edge diffraction loss in dB for the parameter ν.
fn knife_edge_db(nu: f64) -> f64 {
    if nu <= -0.78 {
        0.0
    } else {
        6.9 + 20.0 * (((nu - 0.1).powi(2) + 1.0).sqrt() + nu - 0.1).log10()
    }
}

/// Diffraction loss from a cell at (lat, lon) transmitting at `mhz`, over `frame`.
pub fn diffraction_loss(dem: &Dem, frame: &Frame, lat: f64, lon: f64, mhz: f64, size: usize) -> TerrainLoss {
    let (tx, ty) = frame.to_xy(lat, lon);
    let half = frame.half_width_m;
    let farthest = [(-half, -half), (-half, half), (half, -half), (half, half)]
        .iter()
        .map(|(cx, cy)| (cx - tx).hypot(cy - ty))
        .fold(0.0, f64::max);
    // ~30 m steps (the elevation grid) for town cells, coarser for far-reaching rural ones
    let step = (farthest / 1500.0).max(30.0);
    let samples = (farthest / step).ceil() as usize + 1;
    let wavelength = 299.792_458 / mhz;
    let mast = dem.elevation(lat, lon) + MAST_M;

    // loss along each ray, sample k at distance k * step
    let mut rays = vec![0f32; RAYS * samples];
    for ray in 0..RAYS {
        let bearing = TAU * ray as f64 / RAYS as f64;
        let (east, north) = (bearing.sin(), bearing.cos());
        // the obstacle rising highest as seen from the mast so far: (slope, distance, height)
        let mut obstacle: Option<(f64, f64, f64)> = None;
        for k in 1..samples {
            let d = k as f64 * step;
            let (plat, plon) = frame.to_lat_lon(tx + east * d, ty + north * d);
            // flat-earth geometry with the ground lowered by the Earth's curvature
            let ground = dem.elevation(plat, plon) - d * d / (2.0 * EARTH_RADIUS_M);
            if let Some((_, od, oh)) = obstacle {
                let phone = ground + PHONE_M;
                let clearance = oh - (mast + (phone - mast) * od / d);
                let nu = clearance * (2.0 * d / (wavelength * od * (d - od))).sqrt();
                rays[ray * samples + k] = knife_edge_db(nu) as f32;
            }
            let slope = (ground - mast) / d;
            if obstacle.is_none_or(|(s, ..)| slope > s) {
                obstacle = Some((slope, d, ground));
            }
        }
    }

    // resample onto the frame's square grid (nearest ray and sample)
    let cell = 2.0 * half / size as f64;
    let mut loss_db = Vec::with_capacity(size * size);
    for row in 0..size {
        let y = half - (row as f64 + 0.5) * cell;
        for col in 0..size {
            let x = -half + (col as f64 + 0.5) * cell;
            let (dx, dy) = (x - tx, y - ty);
            let d = dx.hypot(dy);
            let ray = ((dx.atan2(dy).rem_euclid(TAU) / TAU * RAYS as f64).round() as usize) % RAYS;
            let k = ((d / step).round() as usize).min(samples - 1);
            loss_db.push(rays[ray * samples + k]);
        }
    }
    TerrainLoss { half_width_m: half, size, loss_db }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_names() {
        assert_eq!(tile_name(42, 25), "Copernicus_DSM_COG_10_N42_00_E025_00_DEM");
        assert_eq!(tile_name(-1, -71), "Copernicus_DSM_COG_10_S01_00_W071_00_DEM");
        assert_eq!(tile_name(0, 0), "Copernicus_DSM_COG_10_N00_00_E000_00_DEM");
    }

    #[test]
    fn knife_edge_loss() {
        assert_eq!(knife_edge_db(-1.0), 0.0);
        // ITU-R P.526: about 6 dB with the edge just touching the path
        assert!((knife_edge_db(0.0) - 6.0).abs() < 0.1, "{}", knife_edge_db(0.0));
        assert!((knife_edge_db(1.0) - 13.9).abs() < 0.2, "{}", knife_edge_db(1.0));
        let mut last = 0.0;
        for i in -7..50 {
            let db = knife_edge_db(i as f64 * 0.1);
            assert!(db >= last, "not increasing at {i}");
            last = db;
        }
    }

    /// a DEM of 0.001° samples around (42, 25) with `height(lat, lon)` in metres
    fn dem(height: impl Fn(f64, f64) -> f64) -> Dem {
        let (north, west, d, n) = (42.2, 24.8, 0.001, 401);
        let metres = (0..n * n)
            .map(|i| height(north - (i / n) as f64 * d, west + (i % n) as f64 * d) as i16)
            .collect();
        Dem { grids: vec![Grid { north, west, dlat: d, dlon: d, width: n, height: n, metres }] }
    }

    fn frame() -> Frame {
        Frame { lat: 42.0, lon: 25.0, metres_per_degree_lon: METRES_PER_DEGREE * 42f64.to_radians().cos(), half_width_m: 8000.0 }
    }

    #[test]
    fn elevation_is_bilinear_and_zero_outside() {
        let dem = dem(|lat, _| (lat - 41.8) * 1000.0);
        assert!((dem.elevation(42.0, 25.0) - 200.0).abs() < 1.0);
        assert!((dem.elevation(42.0005, 25.0) - 200.5).abs() < 1.0);
        assert_eq!(dem.elevation(50.0, 25.0), 0.0);
    }

    #[test]
    fn frame_round_trip() {
        let frame = frame();
        let (lat, lon) = frame.to_lat_lon(1234.0, -567.0);
        let (x, y) = frame.to_xy(lat, lon);
        assert!((x - 1234.0).abs() < 1e-6 && (y + 567.0).abs() < 1e-6);
    }

    #[test]
    fn terrain_loss_lookup() {
        let loss = TerrainLoss { half_width_m: 1000.0, size: 4, loss_db: vec![8.0; 16] };
        assert_eq!(loss.at(0.0, 0.0), 8.0);
        assert_eq!(loss.at(990.0, -990.0), 8.0);
        assert_eq!(loss.at(1300.0, 0.0), 0.0);
        // rows run from north to south
        let mut rows = vec![0.0; 16];
        rows[..4].fill(10.0);
        let loss = TerrainLoss { half_width_m: 1000.0, size: 4, loss_db: rows };
        assert_eq!(loss.at(0.0, 900.0), 10.0);
        assert_eq!(loss.at(0.0, -900.0), 0.0);
    }

    #[test]
    fn flat_ground_has_no_diffraction_loss() {
        let dem = dem(|_, _| 300.0);
        let loss = diffraction_loss(&dem, &frame(), 42.0, 25.0, 1800.0, 48);
        let max = loss.loss_db.iter().copied().fold(0.0, f32::max);
        assert!(max < 1.0, "{max} dB");
    }

    #[test]
    fn a_ridge_shadows_the_far_side() {
        // a 200 m east-west ridge 2 km north of the cell
        let ridge_lat = 42.0 + 2000.0 / METRES_PER_DEGREE;
        let dem = dem(|lat, _| if (lat - ridge_lat).abs() < 0.0015 { 200.0 } else { 0.0 });
        let frame = frame();
        let loss = diffraction_loss(&dem, &frame, 42.0, 25.0, 1800.0, 64);
        let behind = loss.at(0.0, 5000.0);
        let in_front = loss.at(0.0, 1000.0);
        let south = loss.at(0.0, -5000.0);
        assert!(behind > 15.0, "{behind} dB behind the ridge");
        assert!(in_front < 1.0 && south < 1.0, "{in_front} {south}");
    }

    /// Needs the N42 E025 tile in WTFPS_DEM_DIR (or `dem`): `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn reads_known_elevations() {
        let dir: PathBuf = std::env::var("WTFPS_DEM_DIR").unwrap_or_else(|_| "dem".into()).into();
        let dem = Dem::load(&dir, 42.6, 43.0, 25.5, 25.9).await.unwrap();
        for (place, lat, lon, low, high) in [
            ("Elena", 42.929, 25.877, 250.0, 450.0),
            ("Veliko Tarnovo centre", 42.9814, 25.6450, 150.0, 300.0),
            ("Balkan ridge south of Elena", 42.71, 25.80, 800.0, 1700.0),
        ] {
            let m = dem.elevation(lat, lon);
            println!("{place}: {m:.0} m");
            assert!((low..=high).contains(&m), "{place}: {m} m");
        }
    }
}

