//! HTTP API. `GET /cells/{mcc}/{mnc}/{tac}/{cell}/heatmap` returns a cell's coverage map:
//! the cell is looked up in the database; if it isn't crawled yet it's fetched from Apple
//! and saved together with every other cell in the replies, then the map is drawn.
//! HTML by default, GeoJSON with `?format=geojson`; `?propagation=simple` switches from the
//! default Okumura-Hata model to the distance-only one; `?terrain=true` adds diffraction loss
//! from hills and ridges (elevation tiles are downloaded on first use). When the database can't be reached the
//! map is built from Apple's replies without saving; `X-Wtfps-Source` says which happened.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use sqlx::PgPool;

use clap::ValueEnum;

use crate::coverage::Propagation;
use crate::fetch::AppleWPSClient;
use crate::lookup::{self, NotFound, Source};
use crate::cli::{default_model, ModelArgs};
use crate::heatmap::{heatmap_document, DEFAULT_RADIUS_KM};

const SOURCE_HEADER: header::HeaderName = header::HeaderName::from_static("x-wtfps-source");

#[derive(Clone)]
struct AppState {
    client: AppleWPSClient,
    pool: PgPool,
    /// set once migrations have run; until then the database counts as unavailable
    schema_ready: Arc<AtomicBool>,
}

pub async fn serve(
    client: AppleWPSClient,
    pool: PgPool,
    schema_ready: Arc<AtomicBool>,
    listen: &str,
) -> Result<()> {
    let app = Router::new()
        .route("/cells/{mcc}/{mnc}/{tac}/{cell}/heatmap", get(heatmap))
        .with_state(AppState { client, pool, schema_ready });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn heatmap(
    State(state): State<AppState>,
    Path(key): Path<(i32, i32, i32, i64)>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (geojson, model) = match parse_query(&query) {
        Ok(parsed) => parsed,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };

    let pool = match state.schema_ready.load(Ordering::Relaxed) {
        true => Ok(&state.pool),
        false => Err("database unavailable"),
    };
    let (rows, source) =
        match lookup::heatmap_rows(&state.client, pool, key, DEFAULT_RADIUS_KM, true).await {
            Ok(found) => found,
            Err(e) => return error_response(e),
        };
    if let Source::Fetched(found) = source {
        info!("cell {key:?} wasn't crawled yet: fetched it from Apple and saved {found} cells");
    }

    match heatmap_document(key, rows, &model, geojson, &source).await {
        Ok((document, _)) => {
            let content_type = if geojson { "application/geo+json" } else { "text/html; charset=utf-8" };
            // database, fetched (from Apple and saved) or online (from Apple, not saved)
            let headers = [(header::CONTENT_TYPE, content_type), (SOURCE_HEADER, source.name())];
            (headers, document).into_response()
        }
        Err(e) => error_response(e),
    }
}

/// Whether GeoJSON was asked for, and the model settings, from the query parameters; or
/// the message for a 400 response.
fn parse_query(query: &HashMap<String, String>) -> std::result::Result<(bool, ModelArgs), String> {
    let geojson = match query.get("format").map(String::as_str) {
        None | Some("html") => false,
        Some("geojson") => true,
        Some(other) => return Err(format!("unknown format {other:?}: use html or geojson\n")),
    };
    let mut model = default_model();
    if let Some(value) = query.get("terrain") {
        model.terrain = match value.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err(format!("unknown terrain {value:?}: use true or false\n")),
        };
    }
    if let Some(name) = query.get("propagation") {
        model.propagation = Propagation::from_str(name, true)
            .map_err(|_| format!("unknown propagation {name:?}: use hata or simple\n"))?;
    }
    Ok((geojson, model))
}

/// 404 for a cell Apple has no location for, 500 for anything else, with the message.
fn error_response(e: anyhow::Error) -> Response {
    let status = if e.downcast_ref::<NotFound>().is_some() {
        StatusCode::NOT_FOUND
    } else {
        warn!("heatmap request failed: {e:#}");
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, format!("{e:#}\n")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn defaults() {
        let (geojson, model) = parse_query(&query(&[])).unwrap();
        assert!(!geojson);
        assert_eq!(model.propagation, Propagation::Hata);
        assert!(!model.terrain);
    }

    #[test]
    fn options() {
        let (geojson, model) =
            parse_query(&query(&[("format", "geojson"), ("propagation", "Simple"), ("terrain", "TRUE")])).unwrap();
        assert!(geojson);
        assert_eq!(model.propagation, Propagation::Simple);
        assert!(model.terrain);
        assert!(parse_query(&query(&[("terrain", "1")])).unwrap().1.terrain);
        assert!(!parse_query(&query(&[("terrain", "0")])).unwrap().1.terrain);
        assert!(!parse_query(&query(&[("format", "html")])).unwrap().0);
    }

    #[test]
    fn bad_values_are_rejected() {
        for (name, value) in [("format", "png"), ("propagation", "okumura"), ("terrain", "yes")] {
            let message = parse_query(&query(&[(name, value)])).unwrap_err();
            assert!(message.starts_with(&format!("unknown {name} \"{value}\"")), "{message}");
        }
    }

    #[test]
    fn status_codes() {
        let not_found = error_response(NotFound("no such cell".into()).into());
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        assert_eq!(error_response(anyhow::anyhow!("boom")).status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
