//! HTTP API. `GET /cells/{mcc}/{mnc}/{tac}/{cell}/heatmap` returns a cell's coverage map:
//! the cell is looked up in the database; if it isn't crawled yet it's fetched from Apple
//! and saved together with every other cell in the replies, then the map is drawn.
//! HTML by default, GeoJSON with `?format=geojson`.

use std::collections::HashMap;

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use sqlx::PgPool;

use crate::fetch::AppleWPSClient;
use crate::lookup::{self, NotFound, Source};
use crate::{default_model, heatmap_document, DEFAULT_RADIUS_KM};

#[derive(Clone)]
struct AppState {
    client: AppleWPSClient,
    pool: PgPool,
}

pub async fn serve(client: AppleWPSClient, pool: PgPool, listen: &str) -> Result<()> {
    let app = Router::new()
        .route("/cells/{mcc}/{mnc}/{tac}/{cell}/heatmap", get(heatmap))
        .with_state(AppState { client, pool });
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

    let rows = match lookup::nearby_or_fetch(&state.client, &state.pool, key, DEFAULT_RADIUS_KM, true).await {
        Ok((rows, source)) => {
            if let Source::Fetched(found) = source {
                info!("cell {key:?} wasn't crawled yet: fetched it from Apple and saved {found} cells");
            }
            rows
        }
        Err(e) => return error_response(e),
    };

    match heatmap_document(key, rows, geojson.then(default_model)) {
        Ok((document, _)) => {
            let content_type = if geojson { "application/geo+json" } else { "text/html; charset=utf-8" };
            ([(header::CONTENT_TYPE, content_type)], document).into_response()
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
