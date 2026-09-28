//! Web settings frontend (Cargo feature `http`): a single embedded SPA plus
//! authenticated read/write endpoints for the search-source configuration.
//!
//! The whole module is gated behind the `http` feature so the default stdio
//! build never links axum. All `/api/*` handlers reuse the same bearer-token
//! auth (`authorize`) and origin allowlist (`origin_allowed`) as `/mcp`.
//! The config list masks secrets. Explicit reveal/export actions return keys
//! behind the same authentication, with caching disabled.

use axum::{
    body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::config::{
    export_source_config, load_source_config, reveal_source_key, validate_edits,
    write_source_config, ConfigConflict, InvalidSourceEdits, SourceEdits,
};
use crate::http::{authorize, origin_allowed, unauthorized_response, AppState};

/// Bound the JSON body for a partial source-config write.
const MAX_CONFIG_BODY_BYTES: usize = 2 * 1024 * 1024;

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
        Ok(view) => no_store((StatusCode::OK, Json(view)).into_response()),
        Err(err) => config_error(format!("failed to read config: {err}")),
    }
}

/// Explicit reveal of one current key; opaque IDs keep secrets out of URLs.
pub(crate) async fn get_key(
    State(state): State<AppState>,
    Path((source, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !authorize(&headers, &state).await {
        return unauthorized_response();
    }
    if !origin_allowed(&headers, &state.allowed_origins) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    let env = state.base_env.clone();
    match tokio::task::spawn_blocking(move || reveal_source_key(&env, &source, &id)).await {
        Ok(Some(value)) => no_store(Json(serde_json::json!({ "value": value })).into_response()),
        Ok(None) => no_store(
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "key no longer exists; refresh configuration" })),
            )
                .into_response(),
        ),
        Err(_) => config_error("failed to read key".into()),
    }
}

/// Downloads a versioned backup of the effective, saved configuration.
pub(crate) async fn export_config(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorize(&headers, &state).await {
        return unauthorized_response();
    }
    if !origin_allowed(&headers, &state.allowed_origins) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    let guard = state.config_write_lock.clone().lock_owned().await;
    let env = state.base_env.clone();
    match tokio::task::spawn_blocking(move || {
        let _guard = guard;
        export_source_config(&env)
    })
    .await
    {
        Ok(backup) => {
            let mut response = no_store(Json(backup).into_response());
            response.headers_mut().insert(
                axum::http::header::CONTENT_DISPOSITION,
                axum::http::HeaderValue::from_static(
                    "attachment; filename=\"nova-veil-search-sources.json\"",
                ),
            );
            response
        }
        Err(_) => config_error("failed to export configuration".into()),
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
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "errors": [
                        { "field": "", "message": "invalid request body: unknown field or wrong value type" }
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
                Ok(_) => no_store((StatusCode::OK, Json(view)).into_response()),
                Err(err) => no_store(
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": format!("config saved, but activation failed: {err}"),
                            "saved": true,
                            "config": view,
                        })),
                    )
                        .into_response(),
                ),
            },
            Err(err) if err.is::<ConfigConflict>() => no_store(
                (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({ "error": "configuration changed; refresh before saving" })),
                )
                    .into_response(),
            ),
            Err(err) if err.is::<InvalidSourceEdits>() => no_store(
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "errors": &err.downcast_ref::<InvalidSourceEdits>().unwrap().0 })),
                )
                    .into_response(),
            ),
            Err(_) => config_error(
                "failed to write config; check the configuration file and its permissions".into(),
            ),
        },
        Err(err) => config_error(format!("config write task failed: {err}")),
    }
}

fn config_error(message: String) -> Response {
    no_store(
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": message })),
        )
            .into_response(),
    )
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
