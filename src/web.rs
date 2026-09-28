//! Web settings frontend (Cargo feature `http`): a single embedded SPA plus
//! authenticated read/write endpoints for the search-source configuration.
//!
//! The whole module is gated behind the `http` feature so the default stdio
//! build never links axum. All `/api/*` handlers reuse the same bearer-token
//! auth (`authorize`) and origin allowlist (`origin_allowed`) as `/mcp`.
//! Secrets are never returned: key values surface only as `"set"`/`"unset"`.

use axum::{
    body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::config::{load_source_config, validate_edits, write_source_config, SourceEdits};
use crate::http::{authorize, origin_allowed, unauthorized_response, AppState};

/// Bound the JSON body for a partial source-config write.
const MAX_CONFIG_BODY_BYTES: usize = 16 * 1024;

/// `GET /` — the SPA shell. Unauthenticated by design: the page contains no
/// secrets; it only fetches config behind an authenticated API.
pub(crate) async fn serve_index() -> Response {
    let mut response = Response::new(axum::body::Body::from(include_str!("../web/index.html")));
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

/// `GET /api/config` — effective, masked source configuration (200).
pub(crate) async fn get_config(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorize(&headers, &state).await {
        return unauthorized_response();
    }
    if !origin_allowed(&headers, &state.allowed_origins) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }

    let env = state.base_env.clone();
    match tokio::task::spawn_blocking(move || load_source_config(&env)).await {
        Ok(view) => (StatusCode::OK, Json(view)).into_response(),
        Err(err) => config_error(format!("failed to read config: {err}")),
    }
}

/// `PUT /api/config` — partial merge + atomic write of the editable subset.
pub(crate) async fn put_config(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;

    if !authorize(&headers, &state).await {
        return unauthorized_response();
    }
    if !origin_allowed(&headers, &state.allowed_origins) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }

    let body = match body::to_bytes(body, MAX_CONFIG_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };

    let edits: SourceEdits = match serde_json::from_slice(&body) {
        Ok(edits) => edits,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "errors": [
                        { "field": "", "message": format!("invalid request body: {err}") }
                    ]
                })),
            )
                .into_response();
        }
    };

    let errors = validate_edits(&edits);
    if !errors.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "errors": errors })),
        )
            .into_response();
    }

    let write_guard = state.config_write_lock.clone().lock_owned().await;
    let env = state.base_env.clone();
    let result = tokio::task::spawn_blocking(move || {
        // The blocking write can outlive a disconnected HTTP handler. Keep the
        // lock inside its task, then return it to cover activation as well.
        let result = write_source_config(&env, &edits);
        (result, write_guard)
    })
    .await;
    match result {
        Ok((result, _write_guard)) => match result {
            Ok(view) => match state.search_service(true).await {
                Ok(_) => (StatusCode::OK, Json(view)).into_response(),
                Err(err) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("config saved, but activation failed: {err}"),
                        "saved": true,
                        "config": view,
                    })),
                )
                    .into_response(),
            },
            Err(err) => config_error(format!("failed to write config: {err}")),
        },
        Err(err) => config_error(format!("config write task failed: {err}")),
    }
}

fn config_error(message: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}
