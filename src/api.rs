//! HTTP API. `GET /cells/{mcc}/{mnc}/{tac}/{cell}/heatmap` returns a cell's coverage map:
//! the cell is looked up in the database; if it isn't crawled yet it's fetched from Apple
//! and saved together with every other cell in the replies, then the map is drawn.
//! HTML by default, GeoJSON with `?format=geojson`; `?propagation=simple` switches from the
//! default Okumura-Hata model to the distance-only one. When the database can't be reached the
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
use crate::{default_model, heatmap_document, DEFAULT_RADIUS_KM};

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
    let geojson = match query.get("format").map(String::as_str) {
        None | Some("html") => false,
        Some("geojson") => true,
        Some(other) => {
            let message = format!("unknown format {other:?}: use html or geojson\n");
            return (StatusCode::BAD_REQUEST, message).into_response();
        }
    };
    let mut model = default_model();
    if let Some(name) = query.get("propagation") {
        match Propagation::from_str(name, true) {
            Ok(propagation) => model.propagation = propagation,
            Err(_) => {
                let message = format!("unknown propagation {name:?}: use hata or simple\n");
                return (StatusCode::BAD_REQUEST, message).into_response();
            }
        }
    }

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

    match heatmap_document(key, rows, &model, geojson, &source) {
        Ok((document, _)) => {
            let content_type = if geojson { "application/geo+json" } else { "text/html; charset=utf-8" };
            // database, fetched (from Apple and saved) or online (from Apple, not saved)
            let headers = [(header::CONTENT_TYPE, content_type), (SOURCE_HEADER, source.name())];
            (headers, document).into_response()
        }
        Err(e) => error_response(e),
    }
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
