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

/// Max JSON body for a config write. Nine small fields; anything larger is abuse.
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

    let view = load_source_config(&state.base_env);
    (StatusCode::OK, Json(view)).into_response()
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

    match write_source_config(&state.base_env, &edits) {
        Ok(view) => (StatusCode::OK, Json(view)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("failed to write config: {err}")
            })),
        )
            .into_response(),
    }
}
