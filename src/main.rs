// GC-Stats — RiotRelay server
//
// Caching relay in front of the Riot Valorant match-v1 API: serves matches
// from the MariaDB cache when available, fetches and stores them otherwise,
// and exposes a cache-renew endpoint that only evicts the old copy once
// Riot has answered (Riot deletes matches after ~3 months).
//
// Copyright (c) 2026 Alice Alleman — GC-Stats-RiotRelay
// License: https://github.com/GC-Stats/RiotRelay/blob/main/LICENSE.md (GC-Stats License v1.0)
// Repository: https://github.com/GC-Stats/RiotRelay

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use subtle::ConstantTimeEq;

/// Valorant match-v1 routing regions.
const ALLOWED_REGIONS: &[&str] = &["ap", "br", "esports", "eu", "kr", "latam", "na"];

/// Riot rate-limit / retry headers worth relaying to the caller so it can
/// honour Riot's backoff instead of hammering the shared quota.
const FORWARDED_HEADERS: &[&str] = &[
    "retry-after",
    "x-app-rate-limit",
    "x-app-rate-limit-count",
    "x-method-rate-limit",
    "x-method-rate-limit-count",
    "x-rate-limit-type",
];

struct AppState {
    db: MySqlPool,
    http: reqwest::Client,
    api_key: String,
    auth_key: String,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let api_key = std::env::var("RIOT_API_KEY").expect("RIOT_API_KEY must be set");
    let auth_key = std::env::var("AUTH_KEY").expect("AUTH_KEY must be set");
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let db = MySqlPoolOptions::new()
        .connect(&database_url)
        .await
        .expect("failed to connect to MariaDB");

    sqlx::raw_sql(include_str!("../sql/schema.sql"))
        .execute(&db)
        .await
        .expect("failed to create matches table");

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .expect("failed to build HTTP client");

    let state = Arc::new(AppState {
        db,
        http,
        api_key,
        auth_key,
    });

    let app = Router::new()
        .route("/match/{region}/{id}", get(get_match))
        .route("/match/{region}/{id}/renew", post(renew_match))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .route("/health", get(health))
        .with_state(state);

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind_addr}: {e}"));
    println!("listening on http://{bind_addr}");
    axum::serve(listener, app).await.expect("server error");
}

/// Liveness + DB readiness. Unauthenticated (registered after the auth layer)
/// so orchestrators can probe it, and it fails when the cache DB is down so a
/// database outage is visible rather than silently masked as a cache miss.
async fn health(State(state): State<Arc<AppState>>) -> Response {
    match sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => (StatusCode::OK, "ok").into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "db unavailable").into_response(),
    }
}

/// Rejects any request whose Authorization credential doesn't match AUTH_KEY.
/// Accepts either the bare key or a `Bearer <key>` scheme.
async fn require_auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let authorized = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
        .map(|cred| cred.as_bytes().ct_eq(state.auth_key.as_bytes()).into())
        .unwrap_or(false);

    if !authorized {
        return json_response(
            StatusCode::UNAUTHORIZED,
            "NONE",
            json!({ "error": "unauthorized" }).to_string(),
        );
    }
    next.run(req).await
}

enum RiotFetch {
    /// Riot answered 200 with the match body.
    Ok(String),
    /// Riot answered with an error status (404, 429, ...): relay it as-is,
    /// along with the rate-limit/retry headers so callers can back off.
    Error(StatusCode, HeaderMap, String),
    /// Network / transport failure reaching Riot.
    Unreachable(String),
}

async fn fetch_from_riot(state: &AppState, region: &str, id: &str) -> RiotFetch {
    let url = format!("https://{region}.api.riotgames.com/val/match/v1/matches/{id}");
    let resp = match state
        .http
        .get(&url)
        .header("X-Riot-Token", &state.api_key)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => return RiotFetch::Unreachable(e.to_string()),
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let forwarded = forwarded_headers(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        RiotFetch::Ok(body)
    } else {
        RiotFetch::Error(status, forwarded, body)
    }
}

/// Copies the rate-limit / retry headers we relay from a Riot response.
fn forwarded_headers(src: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for &name in FORWARDED_HEADERS {
        if let Some(value) = src.get(name)
            && let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), value.to_str())
            && let Ok(value) = HeaderValue::from_str(value)
        {
            out.insert(name, value);
        }
    }
    out
}

fn json_response(status: StatusCode, cache_header: &str, body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = HeaderValue::from_str(cache_header) {
        headers.insert("X-Cache", value);
    }
    (status, headers, body).into_response()
}

fn unreachable_response(cache_header: &str, err: &str) -> Response {
    json_response(
        StatusCode::BAD_GATEWAY,
        cache_header,
        json!({ "error": format!("riot api unreachable: {err}") }).to_string(),
    )
}

/// Validates the region against the Valorant routing values, lowercased.
/// The region ends up in the Riot hostname, so anything outside the
/// allowlist is rejected before it can redirect the request (and the
/// API key) elsewhere.
fn validate_region(region: &str) -> Option<String> {
    let region = region.to_ascii_lowercase();
    ALLOWED_REGIONS.contains(&region.as_str()).then_some(region)
}

/// Validates the match id. axum percent-decodes path params, so without this
/// an id like `..%2F..%2Fother` would decode to `../../other` and, once the
/// URL is normalized, redirect the request (carrying the API key) to a
/// different Riot endpoint. Riot match ids are ASCII word chars and hyphens.
fn valid_match_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn invalid_region_response() -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        "NONE",
        json!({
            "error": "invalid region",
            "allowed": ALLOWED_REGIONS,
        })
        .to_string(),
    )
}

fn invalid_id_response() -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        "NONE",
        json!({ "error": "invalid match id" }).to_string(),
    )
}

fn db_error_response() -> Response {
    json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "ERROR",
        json!({ "error": "cache database unavailable" }).to_string(),
    )
}

async fn get_match(
    State(state): State<Arc<AppState>>,
    Path((region, id)): Path<(String, String)>,
) -> Response {
    let Some(region) = validate_region(&region) else {
        return invalid_region_response();
    };
    if !valid_match_id(&id) {
        return invalid_id_response();
    }

    let cached: Option<(String, String)> = match sqlx::query_as(
        "SELECT body, DATE_FORMAT(fetched_at, '%Y-%m-%dT%H:%i:%sZ')
         FROM matches WHERE region = ? AND match_id = ?",
    )
    .bind(&region)
    .bind(&id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(cached) => cached,
        Err(e) => {
            eprintln!("db error reading cache for {region}/{id}: {e}");
            return db_error_response();
        }
    };

    if let Some((body, fetched_at)) = cached {
        let mut resp = json_response(StatusCode::OK, "HIT", body);
        if let Ok(value) = HeaderValue::from_str(&fetched_at) {
            resp.headers_mut().insert("X-Cache-Fetched-At", value);
        }
        return resp;
    }

    match fetch_from_riot(&state, &region, &id).await {
        RiotFetch::Ok(body) => {
            store_match(&state.db, &region, &id, &body).await;
            json_response(StatusCode::OK, "MISS", body)
        }
        RiotFetch::Error(status, headers, body) => {
            let mut resp = json_response(status, "MISS", body);
            resp.headers_mut().extend(headers);
            resp
        }
        RiotFetch::Unreachable(err) => unreachable_response("MISS", &err),
    }
}

/// Re-fetches the match from Riot and only then replaces the cached copy.
/// If Riot fails (matches eventually expire on their side), the old cache
/// entry is left untouched.
async fn renew_match(
    State(state): State<Arc<AppState>>,
    Path((region, id)): Path<(String, String)>,
) -> Response {
    let Some(region) = validate_region(&region) else {
        return invalid_region_response();
    };
    if !valid_match_id(&id) {
        return invalid_id_response();
    }

    let (mut resp, forwarded) = match fetch_from_riot(&state, &region, &id).await {
        RiotFetch::Ok(body) => {
            store_match(&state.db, &region, &id, &body).await;
            return json_response(StatusCode::OK, "RENEWED", body);
        }
        RiotFetch::Error(status, headers, body) => {
            (json_response(status, "RENEW-FAILED", body), Some(headers))
        }
        RiotFetch::Unreachable(err) => (unreachable_response("RENEW-FAILED", &err), None),
    };

    resp.headers_mut()
        .insert("X-Cache-Preserved", HeaderValue::from_static("true"));
    if let Some(headers) = forwarded {
        resp.headers_mut().extend(headers);
    }
    resp
}

async fn store_match(db: &MySqlPool, region: &str, id: &str, body: &str) {
    if let Err(e) = sqlx::query(
        "INSERT INTO matches (region, match_id, body, fetched_at) VALUES (?, ?, ?, UTC_TIMESTAMP())
         ON DUPLICATE KEY UPDATE body = VALUES(body), fetched_at = UTC_TIMESTAMP()",
    )
    .bind(region)
    .bind(id)
    .bind(body)
    .execute(db)
    .await
    {
        eprintln!("failed to cache match {region}/{id}: {e}");
    }
}
