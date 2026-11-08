//! Heatmap documents: a cell's nearby cells from the database, the coverage model applied
//! to them (with optional terrain), and the HTML map or GeoJSON.

use std::collections::HashMap;

use anyhow::Result;
use base64::Engine;
use h3o::Resolution;
use serde_json::json;
use sqlx::PgPool;

use crate::cells::CellKey;
use crate::cli::ModelArgs;
use crate::geojson::{accuracy_circle, cell_geojson};
use crate::{coverage, lookup, terrain};

pub(crate) const HEATMAP_TEMPLATE: &str = include_str!("heatmap.html");

/// same-operator cells within this distance are included in a heatmap (the nearest
/// HEATMAP_MAX_CELLS of them, so towns stay as dense as before and rural cells, which reach
/// tens of km, still see their competitors)
pub(crate) const DEFAULT_RADIUS_KM: f64 = 50.0;

/// the page models every cell for every pixel, so keep the nearest ones only
pub(crate) const HEATMAP_MAX_CELLS: i64 = 800;

#[derive(sqlx::FromRow, Clone)]
pub(crate) struct NearbyCell {
    pub(crate) tac_id: i32,
    pub(crate) cell_id: i64,
    pub(crate) lat: f64,
    pub(crate) lon: f64,
    pub(crate) acc: Option<i64>,
    pub(crate) earfcn: Option<i32>,
    pub(crate) pci: Option<i32>,
    pub(crate) distance_m: f64,
    /// direction from the chosen cell, degrees clockwise from north
    pub(crate) bearing_deg: Option<f64>,
    pub(crate) unknown_12: Option<i64>,
    /// ISO 8601 UTC; None for cells crawled before crawl times were recorded
    pub(crate) crawled_at: Option<String>,
    /// estimated antenna direction, degrees clockwise from north
    #[sqlx(skip)]
    pub(crate) azimuth_deg: Option<f64>,
    pub(crate) generated: String,
}

/// The cell (if crawled) and the nearest crawled cells of its operator, nearest first.
pub(crate) async fn nearby_from_db(pool: &PgPool, key: CellKey, radius_km: f64) -> Result<Vec<NearbyCell>> {
    let (mcc, mnc, tac_id, cell_id) = key;
    Ok(sqlx::query_as(
        "with t as (select geom from cell \
                    where mcc = $1 and mnc = $2 and tac_id = $3 and cell_id = $4) \
         select c.tac_id, c.cell_id, ST_Y(c.geom::geometry) as lat, ST_X(c.geom::geometry) as lon, \
                c.acc, c.earfcn, c.pci, c.unknown_12, ST_Distance(c.geom, t.geom) as distance_m, \
                to_char(c.crawled_at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as crawled_at, \
                degrees(ST_Azimuth(t.geom, c.geom)) as bearing_deg, \
                to_char(now(), 'YYYY-MM-DD HH24:MI TZ') as generated \
         from cell c, t \
         where c.mcc = $1 and c.mnc = $2 and ST_DWithin(c.geom, t.geom, $5) \
         order by ST_Distance(c.geom, t.geom), c.cell_id \
         limit $6",
    )
    .bind(mcc)
    .bind(mnc)
    .bind(tac_id)
    .bind(cell_id)
    .bind(radius_km * 1000.0)
    .bind(HEATMAP_MAX_CELLS)
    .fetch_all(pool)
    .await?)
}

/// The HTML coverage map, or with `geojson` the GeoJSON, for the cell `key` from `rows`
/// (the cell and its operator's nearby cells, nearest first). Returns the document and the
/// number of nearby cells in it.
pub(crate) async fn heatmap_document(
    key: CellKey,
    mut rows: Vec<NearbyCell>,
    model: &ModelArgs,
    geojson: bool,
    source: &lookup::Source,
) -> Result<(String, usize)> {
    let (mcc, mnc, tac_id, cell_id) = key;

    // estimated from all of the operator's nearby cells, every frequency: a site's sectors
    // use several bands
    let sites: Vec<_> = rows.iter().map(|c| (c.cell_id >> 8, c.pci, c.lat, c.lon)).collect();
    for (c, azimuth) in rows.iter_mut().zip(coverage::estimate_azimuths(&sites)) {
        c.azimuth_deg = azimuth.map(|a| a.round());
    }

    let cell_json = |c: &NearbyCell| {
        json!({
            "mcc": mcc, "mnc": mnc, "tac_id": c.tac_id, "cell_id": c.cell_id,
            "lat": c.lat, "lon": c.lon, "acc": c.acc, "earfcn": c.earfcn, "pci": c.pci,
            "unknown_12": c.unknown_12, "crawled_at": c.crawled_at, "azimuth_deg": c.azimuth_deg,
        })
    };
    let is_target = |c: &&NearbyCell| (c.tac_id, c.cell_id) == (tac_id, cell_id);
    let Some(target) = rows.iter().find(is_target) else {
        anyhow::bail!("no location for cell {mcc}/{mnc}/{tac_id}/{cell_id}");
    };
    let nearby: Vec<&NearbyCell> = rows.iter().filter(|c| !is_target(c)).collect();
    let terrain = match model.terrain {
        true => Some(terrain_losses(target, &nearby, model).await?),
        false => None,
    };
    let terrain_map = terrain.as_ref().map(|(_, map)| map.clone()).unwrap_or_default();

    if geojson {
        let accuracy = target.acc.filter(|&a| a > 0).map(|a| accuracy_circle(target.lat, target.lon, a as f64));
        let coverage = model_coverage(target, &nearby, model, &terrain_map)?;
        let mut geojson = cell_geojson(mcc, mnc, target, &nearby, accuracy, &coverage)?;
        // where the data came from, and whether it was saved
        if let Some(cell) = geojson["features"]
            .as_array_mut()
            .and_then(|f| f.iter_mut().find(|f| f["properties"]["role"] == "cell"))
        {
            cell["properties"]["source"] = json!(source.name());
            cell["properties"]["saved"] = json!(source.saved());
        }
        return Ok((geojson.to_string(), nearby.len()));
    }

    let data = json!({
        "target": cell_json(target),
        "cells": nearby.iter().map(|c| cell_json(c)).collect::<Vec<_>>(),
        "generated": target.generated,
        "note": source.note(),
        // the map's initial propagation model; it can be switched on the page
        "propagation": model.propagation.name(),
        // diffraction loss per cell as base64 bytes of 0.25 dB steps, over a square frame
        // around this cell; null without --terrain
        "terrain": terrain.as_ref().map(|(half_width_m, map)| json!({
            "half_width_m": half_width_m,
            "size": TERRAIN_SIZE,
            "cells": map.iter().map(|((tac, cell), loss)| {
                let bytes: Vec<u8> = loss.loss_db.iter().map(|db| (db * 4.0).round().clamp(0.0, 255.0) as u8).collect();
                (format!("{tac}/{cell}"), json!(base64::engine::general_purpose::STANDARD.encode(bytes)))
            }).collect::<serde_json::Map<_, _>>(),
        })),
    });

    // the JSON goes inside a <script> element
    let data = data.to_string().replace("</", "<\\/");
    let html = HEATMAP_TEMPLATE.replacen("/*DATA*/null", &data, 1);
    Ok((html, nearby.len()))
}

/// Terrain loss is computed for the mapped cell and its nearest competitors: those on its
/// own channel, and (for when every frequency competes) some on others. Further cells get
/// none, i.e. are treated as over flat ground.
pub(crate) const TERRAIN_SAME_CHANNEL: usize = 30;

pub(crate) const TERRAIN_OTHER_CHANNELS: usize = 15;

/// samples per side of each cell's terrain raster
pub(crate) const TERRAIN_SIZE: usize = 192;

/// Folder of the elevation tiles (downloaded on first use).
pub(crate) fn dem_dir() -> std::path::PathBuf {
    dotenvy::var("WTFPS_DEM_DIR").unwrap_or_else(|_| "dem".into()).into()
}

/// Terrain loss for the mapped cell and its nearest competitors, over a square around the
/// mapped cell covering the area either propagation model evaluates: (half-width, losses).
pub(crate) async fn terrain_losses(
    target: &NearbyCell,
    nearby: &[&NearbyCell],
    model: &ModelArgs,
) -> Result<(f64, TerrainMap)> {
    let same = |c: &&&NearbyCell| target.earfcn.is_some() && c.earfcn == target.earfcn;
    let mut chosen: Vec<&NearbyCell> = vec![target];
    chosen.extend(nearby.iter().filter(same).take(TERRAIN_SAME_CHANNEL).copied());
    chosen.extend(nearby.iter().filter(|c| !same(c)).take(TERRAIN_OTHER_CHANNELS).copied());

    // the frame: the larger of the two models' areas, with the same-channel competitors
    let mut competitors = vec![target];
    competitors.extend(nearby.iter().filter(same).copied());
    let cells = model_cells(&competitors, &TerrainMap::new());
    let mut params = model_params(model)?;
    let mut half_width_m: f64 = 0.0;
    for propagation in [coverage::Propagation::Hata, coverage::Propagation::Simple] {
        params.propagation = propagation;
        half_width_m = half_width_m.max(coverage::extent_m(&cells, &params));
    }
    let frame = terrain::Frame {
        lat: target.lat,
        lon: target.lon,
        metres_per_degree_lon: 111_320.0 * target.lat.to_radians().cos(),
        half_width_m,
    };

    // elevation for the frame and every chosen cell's paths into it
    let (north, east) = frame.to_lat_lon(half_width_m, half_width_m);
    let (south, west) = frame.to_lat_lon(-half_width_m, -half_width_m);
    let lats = chosen.iter().map(|c| c.lat).chain([south, north]);
    let lons = chosen.iter().map(|c| c.lon).chain([west, east]);
    let (south, north) = lats.fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let (west, east) = lons.fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let margin = 0.005;
    let dem = terrain::Dem::load(&dem_dir(), south - margin, north + margin, west - margin, east + margin).await?;

    // cells without a known frequency use the mapped cell's, or 1800 MHz
    let fallback_mhz = target.earfcn.and_then(coverage::downlink_mhz).unwrap_or(1800.0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let losses: Vec<((i32, i64), terrain::TerrainLoss)> = tokio::task::block_in_place(|| {
        std::thread::scope(|scope| {
            let workers: Vec<_> = chosen
                .chunks(chosen.len().div_ceil(threads))
                .map(|chunk| {
                    let (dem, frame) = (&dem, &frame);
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .map(|c| {
                                let mhz = c.earfcn.and_then(coverage::downlink_mhz).unwrap_or(fallback_mhz);
                                let loss = terrain::diffraction_loss(dem, frame, c.lat, c.lon, mhz, TERRAIN_SIZE);
                                ((c.tac_id, c.cell_id), loss)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().flat_map(|w| w.join().expect("terrain worker panicked")).collect()
        })
    });
    let map = losses.into_iter().map(|(key, loss)| (key, std::sync::Arc::new(loss))).collect();
    Ok((half_width_m, map))
}

pub(crate) struct ModelledCoverage<'a> {
    pub(crate) model: ModelArgs,
    /// the target first, then the cells it competes with
    pub(crate) competitors: Vec<&'a NearbyCell>,
    pub(crate) same_frequency_only: bool,
    pub(crate) result: coverage::Coverage,
    /// likelihood bands, with --contours
    pub(crate) bands: Vec<contour::Band>,
}

/// Terrain loss of the cells it was computed for, by (TAC, cell ID).
pub(crate) type TerrainMap = HashMap<(i32, i64), std::sync::Arc<terrain::TerrainLoss>>;

pub(crate) fn model_cells(cells: &[&NearbyCell], terrain: &TerrainMap) -> Vec<coverage::ModelCell> {
    cells
        .iter()
        .map(|c| coverage::ModelCell {
            lat: c.lat,
            lon: c.lon,
            acc: c.acc,
            reach: c.unknown_12,
            azimuth_deg: c.azimuth_deg,
            earfcn: c.earfcn,
            terrain: terrain.get(&(c.tac_id, c.cell_id)).cloned(),
        })
        .collect()
}

pub(crate) fn model_params(model: &ModelArgs) -> Result<coverage::Params> {
    Ok(coverage::Params {
        path_loss: model.path_loss,
        sigma_db: model.sigma_db,
        acc_weight: model.acc_weight,
        range_m: model.range_km.map(|km| km * 1000.0),
        use_reach: model.use_unknown_12,
        directional: model.directional,
        propagation: model.propagation,
        resolution: model.hex_res.map(Resolution::try_from).transpose()?,
    })
}

/// Run the coverage model; unless asked otherwise, the cell only competes with cells known
/// to be on its own EARFCN, because operators run several frequency layers over one area.
pub(crate) fn model_coverage<'a>(
    target: &'a NearbyCell,
    nearby: &[&'a NearbyCell],
    model: &ModelArgs,
    terrain: &TerrainMap,
) -> Result<ModelledCoverage<'a>> {
    let same_frequency_only = !model.all_frequencies && target.earfcn.is_some();
    let mut competitors = vec![target];
    competitors.extend(
        nearby
            .iter()
            .filter(|c| !same_frequency_only || c.earfcn == target.earfcn),
    );
    let cells = model_cells(&competitors, terrain);
    let params = model_params(model)?;
    let levels = &model.contour_levels;
    if model.contours
        && (levels.is_empty()
            || levels.iter().any(|l| !(0.0..1.0).contains(l) || *l == 0.0)
            || levels.windows(2).any(|w| w[0] >= w[1]))
    {
        anyhow::bail!("--contour-levels must be ascending values between 0 and 1, e.g. 0.1,0.5,0.9");
    }
    let bands = if model.contours {
        coverage::likelihood_bands(&cells, &params, levels, model.contour_step_m.max(1.0))?
    } else {
        vec![]
    };
    Ok(ModelledCoverage {
        model: model.clone(),
        same_frequency_only,
        result: coverage::compute(&cells, &params)?,
        competitors,
        bands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::default_model;
    use crate::lookup::{nearby_from_replies, Source};

    const KEY: CellKey = (284, 5, 106, 451645);

    fn rows() -> Vec<NearbyCell> {
        nearby_from_replies(KEY, 50.0, 800, crate::lookup::tests::replies()).unwrap()
    }

    #[tokio::test]
    async fn geojson_document() {
        let (document, neighbours) =
            heatmap_document(KEY, rows(), &default_model(), true, &Source::Online("no database")).await.unwrap();
        assert_eq!(neighbours, 2);
        let json: serde_json::Value = serde_json::from_str(&document).unwrap();
        let features = json["features"].as_array().unwrap();
        let role = |r: &str| features.iter().filter(|f| f["properties"]["role"] == r).count();
        assert_eq!((role("cell"), role("nearby"), role("accuracy")), (1, 2, 1));
        assert!(role("coverage") > 0);
        let cell = features.iter().find(|f| f["properties"]["role"] == "cell").unwrap();
        assert_eq!(cell["properties"]["source"], "online");
        assert_eq!(cell["properties"]["saved"], false);
        assert_eq!(cell["properties"]["cell_id"], 451645);
        assert_eq!(cell["geometry"]["coordinates"], serde_json::json!([23.3, 42.7]));
    }

    #[tokio::test]
    async fn html_document_embeds_the_data() {
        let mut model = default_model();
        model.propagation = coverage::Propagation::Simple;
        let (html, _) = heatmap_document(KEY, rows(), &model, false, &Source::Database).await.unwrap();
        assert!(!html.contains("/*DATA*/null"));
        let start = html.find("const DATA = ").unwrap() + "const DATA = ".len();
        let end = start + html[start..].find(";\n").unwrap();
        let data: serde_json::Value = serde_json::from_str(&html[start..end].replace("<\\/", "</")).unwrap();
        assert_eq!(data["target"]["cell_id"], 451645);
        assert_eq!(data["cells"].as_array().unwrap().len(), 2);
        assert_eq!(data["propagation"], "simple");
        assert_eq!(data["note"], serde_json::Value::Null);
        assert_eq!(data["terrain"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn document_without_the_cell_fails() {
        let rows: Vec<_> = rows().into_iter().filter(|r| r.cell_id != 451645).collect();
        assert!(heatmap_document(KEY, rows, &default_model(), true, &Source::Database).await.is_err());
    }

    #[test]
    fn contour_levels_are_checked() {
        let target = &rows()[0];
        let mut model = default_model();
        model.contours = true;
        for levels in [vec![], vec![0.5, 0.1], vec![0.0, 0.5], vec![0.5, 1.0]] {
            model.contour_levels = levels.clone();
            assert!(model_coverage(target, &[], &model, &TerrainMap::new()).is_err(), "{levels:?}");
        }
        model.contour_levels = vec![0.25, 0.75];
        assert!(model_coverage(target, &[], &model, &TerrainMap::new()).is_ok());
    }

    #[test]
    fn same_frequency_competitors_by_default() {
        let rows = rows();
        let mut other_band = rows[1].clone();
        other_band.cell_id = 999;
        other_band.earfcn = Some(6300);
        let nearby = [&rows[1], &rows[2], &other_band];
        let model = default_model();
        let only = model_coverage(&rows[0], &nearby, &model, &TerrainMap::new()).unwrap();
        assert!(only.same_frequency_only);
        assert_eq!(only.competitors.len(), 3);
        let all = model_coverage(&rows[0], &nearby, &ModelArgs { all_frequencies: true, ..model }, &TerrainMap::new()).unwrap();
        assert_eq!(all.competitors.len(), 4);
    }
}
