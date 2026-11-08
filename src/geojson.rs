//! GeoJSON output of a heatmap: the cell, its accuracy circle, nearby cells and the
//! modelled coverage, with simplestyle styling.

use anyhow::Result;
use serde_json::json;

use crate::heatmap::{ModelledCoverage, NearbyCell};

/// A circle of `radius_m` around a point as a GeoJSON polygon geometry (64 sides).
pub(crate) fn accuracy_circle(lat: f64, lon: f64, radius_m: f64) -> serde_json::Value {
    let m_lon = 111_320.0 * lat.to_radians().cos();
    let ring: Vec<[f64; 2]> = (0..=64)
        .map(|i| {
            let a = std::f64::consts::TAU * f64::from(i % 64) / 64.0;
            let round = |v: f64| (v * 1e7).round() / 1e7;
            [round(lon + radius_m * a.cos() / m_lon), round(lat + radius_m * a.sin() / 111_320.0)]
        })
        .collect();
    json!({"type": "Polygon", "coordinates": [ring]})
}

/// GeoJSON FeatureCollection: the cell (role "cell"), its accuracy circle ("accuracy")
/// and nearby cells of the same operator ("nearby") with distance and bearing from it.
pub(crate) fn cell_geojson(
    mcc: i32,
    mnc: i32,
    target: &NearbyCell,
    nearby: &[&NearbyCell],
    accuracy: Option<serde_json::Value>,
    coverage: &ModelledCoverage,
) -> Result<serde_json::Value> {
    let properties = |c: &NearbyCell, role: &str| {
        let mut p = json!({
            "role": role,
            "mcc": mcc, "mnc": mnc, "tac_id": c.tac_id, "cell_id": c.cell_id,
            "site": c.cell_id >> 8, "sector": c.cell_id & 255,
            "acc_m": c.acc, "earfcn": c.earfcn, "pci": c.pci, "unknown_12": c.unknown_12,
            "crawled_at": c.crawled_at, "azimuth_deg": c.azimuth_deg,
        });
        if role == "nearby" {
            p["distance_m"] = json!(c.distance_m.round());
            p["bearing_deg"] = json!(c.bearing_deg.map(|b| b.round()));
            p["same_frequency"] = match (c.earfcn, target.earfcn) {
                (Some(a), Some(b)) => json!(a == b),
                _ => serde_json::Value::Null,
            };
        }
        p
    };
    let point = |c: &NearbyCell, role: &str| {
        json!({
            "type": "Feature",
            "geometry": {"type": "Point", "coordinates": [c.lon, c.lat]},
            "properties": properties(c, role),
        })
    };

    let mut cell = point(target, "cell");
    let m = &coverage.model;
    cell["properties"]["coverage"] = json!({
        "area_km2": (coverage.result.area_km2 * 1000.0).round() / 1000.0,
        "reach_m": coverage.result.reach_m.round(),
        "competing_cells": coverage.competitors.len() - 1,
        "same_frequency_only": coverage.same_frequency_only,
        "range_m": coverage.result.range_m.round(),
        "range_basis": coverage.result.range_basis,
        "propagation": m.propagation.name(),
        "terrain": m.terrain,
        "reach_from_unknown_12": m.use_unknown_12,
        "directional": m.directional,
        "target_reach_m": coverage.result.target_reach_m.map(f64::round),
        "path_loss": m.path_loss,
        "sigma_db": m.sigma_db,
        "acc_weight": m.acc_weight,
        "hex_res": u8::from(coverage.result.resolution),
        "contour_levels": m.contours.then_some(&m.contour_levels),
    });
    // drawn in order: coverage underneath, then the accuracy circle, nearby cells, the cell
    let mut features = vec![];
    for band in &coverage.bands {
        let round = |v: f64| (v * 1e7).round() / 1e7;
        let polygons: Vec<Vec<Vec<[f64; 2]>>> = band
            .geometry()
            .0
            .iter()
            .map(|p| {
                std::iter::once(p.exterior())
                    .chain(p.interiors())
                    .map(|ring| ring.coords().map(|c| [round(c.x), round(c.y)]).collect())
                    .collect()
            })
            .collect();
        let (low, high) = (band.min_v(), band.max_v().min(1.0));
        let middle = (low + high) / 2.0;
        features.push(json!({
            "type": "Feature",
            "geometry": {"type": "MultiPolygon", "coordinates": polygons},
            "properties": {
                "role": "contour",
                "likelihood_min": low,
                "likelihood_max": high,
                "label": format!("{:.0}–{:.0}%", low * 100.0, high * 100.0),
                "fill": ramp_color(middle),
                "fill-opacity": heat_opacity(middle),
                "stroke-width": 0,
                "stroke-opacity": 0,
            },
        }));
    }
    for hex in coverage.result.hexes.iter().filter(|_| !m.contours) {
        let mut ring: Vec<[f64; 2]> = hex.index.boundary().iter().map(|ll| [ll.lng(), ll.lat()]).collect();
        ring.push(ring[0]);
        let best = coverage.competitors[hex.best];
        features.push(json!({
            "type": "Feature",
            "geometry": {"type": "Polygon", "coordinates": [ring]},
            "properties": {
                "role": "coverage",
                "h3": hex.index.to_string(),
                "likelihood": (hex.likelihood * 1000.0).round() / 1000.0,
                "signal_db": (hex.signal_db * 10.0).round() / 10.0,
                "most_likely_cell_id": best.cell_id,
                "most_likely_tac_id": best.tac_id,
                "most_likely_likelihood": (hex.best_likelihood * 1000.0).round() / 1000.0,
                "fill": ramp_color(hex.likelihood),
                "fill-opacity": heat_opacity(hex.likelihood),
                "stroke-width": 0,
                "stroke-opacity": 0,
            },
        }));
    }
    if let Some(circle) = accuracy {
        features.push(json!({
            "type": "Feature",
            "geometry": circle,
            "properties": {
                "role": "accuracy", "cell_id": target.cell_id, "radius_m": target.acc,
                "stroke": STYLE_TARGET, "stroke-width": 1.5, "stroke-opacity": 1, "fill-opacity": 0,
            },
        }));
    }
    for c in nearby {
        let mut feature = point(c, "nearby");
        let other_frequency = coverage.same_frequency_only && c.earfcn != target.earfcn;
        let style = feature["properties"].as_object_mut().unwrap();
        style.insert("marker-color".into(), json!(if other_frequency { STYLE_MUTED } else { STYLE_CELL }));
        style.insert("marker-size".into(), json!("small"));
        features.push(feature);
    }
    if let (true, Some(azimuth)) = (m.directional, target.azimuth_deg) {
        // a line from the cell in its estimated antenna direction, a quarter of the range it
        // fades out at (at least 300 m), as on the HTML map
        let fade = coverage.result.target_reach_m.unwrap_or(coverage.result.range_m);
        let length = (fade / 4.0).max(300.0);
        let rad = azimuth.to_radians();
        let end = [
            target.lon + length * rad.sin() / (111_320.0 * target.lat.to_radians().cos()),
            target.lat + length * rad.cos() / 111_320.0,
        ];
        features.push(json!({
            "type": "Feature",
            "geometry": {"type": "LineString", "coordinates": [[target.lon, target.lat], end]},
            "properties": {
                "role": "antenna", "cell_id": target.cell_id, "azimuth_deg": azimuth,
                "stroke": STYLE_TARGET, "stroke-width": 2, "stroke-opacity": 1,
            },
        }));
    }
    let style = cell["properties"].as_object_mut().unwrap();
    style.insert("marker-color".into(), json!(STYLE_TARGET));
    style.insert("marker-size".into(), json!("medium"));
    features.push(cell);
    Ok(json!({"type": "FeatureCollection", "features": features}))
}

// Styling uses the simplestyle properties (fill, fill-opacity, marker-color, ...) that
// geojson.io and GitHub's map preview apply automatically; colours match the HTML map.
pub(crate) const STYLE_TARGET: &str = "#eb6834";

pub(crate) const STYLE_CELL: &str = "#52514e";

pub(crate) const STYLE_MUTED: &str = "#898781";

pub(crate) const HEAT_RAMP: [[u8; 3]; 13] = [
    [0xcd, 0xe2, 0xfb], [0xb7, 0xd3, 0xf6], [0x9e, 0xc5, 0xf4], [0x86, 0xb6, 0xef],
    [0x6d, 0xa7, 0xec], [0x55, 0x98, 0xe7], [0x39, 0x87, 0xe5], [0x2a, 0x78, 0xd6],
    [0x25, 0x6a, 0xbf], [0x1c, 0x5c, 0xab], [0x18, 0x4f, 0x95], [0x10, 0x42, 0x81],
    [0x0d, 0x36, 0x6b],
];

/// light-to-dark blue for a value in 0..=1, interpolated between ramp steps
pub(crate) fn ramp_color(value: f64) -> String {
    let f = value.clamp(0.0, 1.0) * (HEAT_RAMP.len() - 1) as f64;
    let i = (f.floor() as usize).min(HEAT_RAMP.len() - 2);
    let u = f - i as f64;
    let mix = |c: usize| (HEAT_RAMP[i][c] as f64 * (1.0 - u) + HEAT_RAMP[i + 1][c] as f64 * u).round() as u8;
    format!("#{:02x}{:02x}{:02x}", mix(0), mix(1), mix(2))
}

/// weak values fade into the basemap, as on the HTML map: from nothing at 2% up to 0.265
/// at 10%, then 0.2 + 0.65 v, at most 0.8
pub(crate) fn heat_opacity(value: f64) -> f64 {
    if value < 0.02 {
        return 0.0;
    }
    let opacity = if value < 0.1 { 2.65 * value } else { (0.2 + 0.65 * value.min(1.0)).min(0.8) };
    (opacity * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_ends_and_clamps() {
        assert_eq!(ramp_color(0.0), "#cde2fb");
        assert_eq!(ramp_color(1.0), "#0d366b");
        assert_eq!(ramp_color(-1.0), ramp_color(0.0));
        assert_eq!(ramp_color(2.0), ramp_color(1.0));
        assert_eq!(ramp_color(0.5), "#3987e5");
    }

    #[test]
    fn opacity_fades_in() {
        assert_eq!(heat_opacity(0.01), 0.0);
        assert_eq!(heat_opacity(0.05), 0.13);
        assert_eq!(heat_opacity(0.1), 0.27);
        assert_eq!(heat_opacity(0.5), 0.53);
        assert_eq!(heat_opacity(1.0), 0.8);
    }

    #[test]
    fn accuracy_circle_is_a_closed_ring_at_the_radius() {
        let circle = accuracy_circle(42.7, 23.3, 1000.0);
        let ring = circle["coordinates"][0].as_array().unwrap();
        assert_eq!(ring.len(), 65);
        assert_eq!(ring[0], ring[64]);
        let north = &ring[16];
        let metres = (north[1].as_f64().unwrap() - 42.7) * 111_320.0;
        assert!((metres - 1000.0).abs() < 0.1, "{metres}");
    }
}
