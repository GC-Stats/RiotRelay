// GC-Stats — RiotRelay server
//
// Caching relay in front of the Riot Valorant match-v1 API: serves matches
// from the PostgreSQL cache when available, fetches and stores them otherwise,
// and exposes a cache-renew endpoint that only evicts the old copy once
// Riot has answered (Riot deletes matches after ~3 months).
//
// Copyright (c) 2026 Alice Alleman — GC-Stats-RiotRelay
// License: https://github.com/GC-Stats/RiotRelay/blob/main/LICENSE.md (GC-Stats License v1.0)
// Repository: https://github.com/GC-Stats/RiotRelay

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};
use subtle::ConstantTimeEq;

/// Prefix marking a match as the product of `/merge`: its ID is derived from
/// its source matches rather than assigned by Riot, so it can never be
/// re-fetched or renewed from the Riot API.
const MERGED_ID_PREFIX: &str = "GCS-";

/// Upper bound on how many source matches a single `/merge` request may
/// combine, so one authenticated call can't fan out into an unbounded burst
/// of Riot API requests.
const MAX_MERGE_SEGMENTS: usize = 5;

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
    db: PgPool,
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

    let db = PgPoolOptions::new()
        .connect(&database_url)
        .await
        .expect("failed to connect to PostgreSQL");

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
        .route("/match/{region}/merge", post(merge_match))
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
    let version = std::env::var("APP_VERSION").unwrap_or_else(|_| "dev".to_string());

    match sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => (Json(json!({
            "status": "ok",
            "version": version
        })))
        .into_response(),
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

/// True for match IDs produced by `/merge`: these never existed on Riot's
/// side, so they must never be fetched from or renewed against the Riot API.
fn is_merged_id(id: &str) -> bool {
    id.starts_with(MERGED_ID_PREFIX)
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
        "SELECT body, to_char(fetched_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
         FROM matches WHERE region = $1 AND match_id = $2",
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

    // Merged matches only ever exist as a cache entry: there's no point
    // asking Riot for an ID it never issued.
    if is_merged_id(&id) {
        return json_response(
            StatusCode::NOT_FOUND,
            "MISS",
            json!({ "error": "match not found" }).to_string(),
        );
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
    if is_merged_id(&id) {
        return json_response(
            StatusCode::BAD_REQUEST,
            "NONE",
            json!({ "error": "merged matches cannot be renewed" }).to_string(),
        );
    }

    let existed: bool = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM matches WHERE region = $1 AND match_id = $2",
    )
    .bind(&region)
    .bind(&id)
    .fetch_one(&state.db)
    .await
    {
        Ok(count) => count > 0,
        Err(e) => {
            eprintln!("db error reading cache for {region}/{id}: {e}");
            return db_error_response();
        }
    };

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

    resp.headers_mut().insert(
        "X-Cache-Preserved",
        HeaderValue::from_static(if existed { "true" } else { "false" }),
    );
    if let Some(headers) = forwarded {
        resp.headers_mut().extend(headers);
    }
    resp
}

/// One source match contributing its `[start_round, end_round]` rounds
/// (inclusive) to a merge. The caller resolves any overlap between source
/// matches itself by choosing non-overlapping ranges up front.
#[derive(Clone)]
struct MergeSegment {
    match_id: String,
    start_round: i64,
    end_round: i64,
}

/// Builds a match out of round-by-round data taken from several remade
/// matches (e.g. a match restarted after round 4: rounds 1-4 from the first
/// matchId, rounds 4-20 from the second). The resulting match ID is a
/// deterministic hash of the request, replacing Riot's own match ID scheme,
/// so this endpoint is only reachable once the caller has already decided
/// how the source matches' rounds fit together.
async fn merge_match(
    State(state): State<Arc<AppState>>,
    Path(region): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some(region) = validate_region(&region) else {
        return invalid_region_response();
    };

    let segments = match parse_merge_segments(&body) {
        Ok(segments) => segments,
        Err(resp) => return resp,
    };

    let new_id = compute_merged_id(&segments);

    // Always recompute, even if this exact merge was already stored under
    // this ID: sources are re-fetched fresh from Riot (falling back to our
    // cache only if Riot can't be reached), so a merge is redone rather than
    // served stale just because its deterministic ID collides with a
    // previous run of the same request.
    //
    // Fetched concurrently (bounded by MAX_MERGE_SEGMENTS) since each
    // segment's source match is independent; tasks are awaited in the
    // original segment order so merge_round_data's first/last-segment
    // handling stays correct regardless of fetch completion order.
    let mut tasks = Vec::with_capacity(segments.len());
    for seg in segments {
        let state = state.clone();
        let region = region.clone();
        tasks.push(tokio::spawn(async move {
            let value = fetch_match_for_merge(&state, &region, &seg.match_id).await;
            (seg, value)
        }));
    }

    let mut sources = Vec::with_capacity(tasks.len());
    for task in tasks {
        let (seg, value) = task.await.expect("merge fetch task panicked");
        match value {
            Ok(value) => sources.push((seg, value)),
            Err(resp) => return resp,
        }
    }

    let merged = match merge_round_data(&sources, &new_id) {
        Ok(merged) => merged,
        Err(resp) => return resp,
    };

    let body_str = merged.to_string();
    store_match(&state.db, &region, &new_id, &body_str).await;
    json_response(StatusCode::OK, "MERGED", body_str)
}

/// Parses and validates the `{"segments": [{matchId, startRound, endRound}]}`
/// request body.
#[allow(clippy::result_large_err)]
fn parse_merge_segments(body: &serde_json::Value) -> Result<Vec<MergeSegment>, Response> {
    let bad = |msg: String| {
        json_response(
            StatusCode::BAD_REQUEST,
            "NONE",
            json!({ "error": msg }).to_string(),
        )
    };

    let raw = body
        .get("segments")
        .and_then(|v| v.as_array())
        .ok_or_else(|| bad("expected a \"segments\" array".to_string()))?;

    if raw.len() < 2 {
        return Err(bad(
            "at least 2 segments are required to merge matches".to_string()
        ));
    }
    if raw.len() > MAX_MERGE_SEGMENTS {
        return Err(bad(format!(
            "at most {MAX_MERGE_SEGMENTS} segments are allowed per merge"
        )));
    }

    let mut segments = Vec::with_capacity(raw.len());
    for (i, item) in raw.iter().enumerate() {
        let match_id = item
            .get("matchId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| bad(format!("segments[{i}].matchId is required")))?;
        if !valid_match_id(match_id) {
            return Err(bad(format!("segments[{i}].matchId is invalid")));
        }
        if is_merged_id(match_id) {
            return Err(bad(format!(
                "segments[{i}].matchId is itself a merged match and cannot be re-merged"
            )));
        }

        let start_round = item
            .get("startRound")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| bad(format!("segments[{i}].startRound is required")))?;
        let end_round = item
            .get("endRound")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| bad(format!("segments[{i}].endRound is required")))?;

        if start_round < 0 || end_round < start_round {
            return Err(bad(format!("segments[{i}] has an invalid round range")));
        }

        segments.push(MergeSegment {
            match_id: match_id.to_string(),
            start_round,
            end_round,
        });
    }

    Ok(segments)
}

/// Deterministic new match ID: a SHA-256 hash of the sorted segment list, so
/// the same set of (matchId, startRound, endRound) triples always produces
/// the same merged match regardless of the order they're submitted in, and
/// re-submitting an identical merge request is idempotent. Formatted like a
/// Riot match ID with its first 4 characters replaced by "GCS-", per the
/// merged-match ID convention.
fn compute_merged_id(segments: &[MergeSegment]) -> String {
    let mut parts: Vec<String> = segments
        .iter()
        .map(|s| format!("{}:{}-{}", s.match_id, s.start_round, s.end_round))
        .collect();
    parts.sort();
    let canonical = parts.join("|");

    let digest = Sha256::digest(canonical.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    let full = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    format!("{MERGED_ID_PREFIX}{}", &full[4..])
}

#[allow(clippy::result_large_err)]
async fn fetch_cached_body(
    db: &PgPool,
    region: &str,
    id: &str,
) -> Result<Option<String>, Response> {
    sqlx::query_scalar::<_, String>("SELECT body FROM matches WHERE region = $1 AND match_id = $2")
        .bind(region)
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(|e| {
            eprintln!("db error reading cache for {region}/{id}: {e}");
            db_error_response()
        })
}

/// Fetches one merge source match, always asking Riot first so a merge
/// reflects the freshest data available; only falls back to our cache if
/// Riot can't answer (e.g. the source match has since expired on Riot's
/// side). Parses the resulting body as JSON.
#[allow(clippy::result_large_err)]
async fn fetch_match_for_merge(
    state: &AppState,
    region: &str,
    id: &str,
) -> Result<serde_json::Value, Response> {
    let body = match fetch_from_riot(state, region, id).await {
        RiotFetch::Ok(body) => {
            store_match(&state.db, region, id, &body).await;
            body
        }
        RiotFetch::Error(status, headers, _body) => {
            match fetch_cached_body(&state.db, region, id).await? {
                Some(cached) => cached,
                None => {
                    let mut resp = json_response(
                        status,
                        "MISS",
                        json!({
                            "error": format!("failed to fetch source match {id}"),
                            "riot_status": status.as_u16(),
                        })
                        .to_string(),
                    );
                    resp.headers_mut().extend(headers);
                    return Err(resp);
                }
            }
        }
        RiotFetch::Unreachable(err) => match fetch_cached_body(&state.db, region, id).await? {
            Some(cached) => cached,
            None => {
                return Err(unreachable_response(
                    "MISS",
                    &format!("source match {id}: {err}"),
                ));
            }
        },
    };

    serde_json::from_str::<serde_json::Value>(&body).map_err(|e| {
        json_response(
            StatusCode::BAD_GATEWAY,
            "ERROR",
            json!({ "error": format!("invalid json for match {id}: {e}") }).to_string(),
        )
    })
}

/// Builds the merged match body out of the per-segment round ranges.
///
/// `roundResults` are taken verbatim from their source match, selected by
/// `[startRound, endRound]` and concatenated in segment order; a `roundNum`
/// claimed by more than one segment is rejected as a conflict rather than
/// silently picking one. Per-player `score`/`kills`/`deaths`/`assists` are
/// then recomputed by summing the merged rounds' `playerStats`, so they stay
/// consistent with whatever rounds actually made it into the merge —
/// `playtimeMillis`/`abilityCasts` are summed across the source matches
/// directly since they have no round-level breakdown. Everything else
/// (`matchInfo`, per-player identity fields, `teams[].won`, `coaches`) is
/// taken from the last segment, treated as the match's final state, falling
/// back to the first segment for anything missing there.
#[allow(clippy::result_large_err)]
fn merge_round_data(
    sources: &[(MergeSegment, serde_json::Value)],
    new_id: &str,
) -> Result<serde_json::Value, Response> {
    let mut merged_rounds: Vec<serde_json::Value> = Vec::new();
    let mut round_owner: HashMap<i64, String> = HashMap::new();

    for (seg, value) in sources {
        let rounds = value
            .get("roundResults")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                bad_gateway_json(&format!("match {} has no roundResults", seg.match_id))
            })?;

        for round in rounds {
            let round_num = round
                .get("roundNum")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| {
                    bad_gateway_json(&format!(
                        "match {} has a round without roundNum",
                        seg.match_id
                    ))
                })?;

            if round_num < seg.start_round || round_num > seg.end_round {
                continue;
            }

            if let Some(prev_match) = round_owner.get(&round_num) {
                return Err(json_response(
                    StatusCode::CONFLICT,
                    "NONE",
                    json!({
                        "error": "overlapping round between segments",
                        "roundNum": round_num,
                        "matches": [prev_match, &seg.match_id],
                    })
                    .to_string(),
                ));
            }
            round_owner.insert(round_num, seg.match_id.clone());
            merged_rounds.push(round.clone());
        }
    }

    if merged_rounds.is_empty() {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            "NONE",
            json!({ "error": "no rounds found within the requested ranges" }).to_string(),
        ));
    }

    merged_rounds.sort_by_key(|r| r.get("roundNum").and_then(|v| v.as_i64()).unwrap_or(0));
    let rounds_played = merged_rounds.len() as i64;

    let (_, first_value) = sources.first().expect("at least 2 segments");
    let (_, last_value) = sources.last().expect("at least 2 segments");

    // matchInfo: last segment's snapshot (closest to the finished game),
    // pointed at the new ID and the true (earliest) game start time.
    let mut match_info = last_value
        .get("matchInfo")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(obj) = match_info.as_object_mut() {
        obj.insert("matchId".to_string(), json!(new_id));
        if let Some(start) = first_value.pointer("/matchInfo/gameStartMillis") {
            obj.insert("gameStartMillis".to_string(), start.clone());
        }
    }

    // Recompute per-round-derived stats from the merged rounds.
    let mut kills: HashMap<String, i64> = HashMap::new();
    let mut deaths: HashMap<String, i64> = HashMap::new();
    let mut assists: HashMap<String, i64> = HashMap::new();
    let mut score: HashMap<String, i64> = HashMap::new();

    for round in &merged_rounds {
        let Some(player_stats) = round.get("playerStats").and_then(|v| v.as_array()) else {
            continue;
        };
        for ps in player_stats {
            let Some(subject) = ps.get("puuid").and_then(|v| v.as_str()) else {
                continue;
            };
            *score.entry(subject.to_string()).or_insert(0) +=
                ps.get("score").and_then(|v| v.as_i64()).unwrap_or(0);

            let Some(kill_list) = ps.get("kills").and_then(|v| v.as_array()) else {
                continue;
            };
            for k in kill_list {
                if let Some(killer) = k.get("killer").and_then(|v| v.as_str()) {
                    *kills.entry(killer.to_string()).or_insert(0) += 1;
                }
                if let Some(victim) = k.get("victim").and_then(|v| v.as_str()) {
                    *deaths.entry(victim.to_string()).or_insert(0) += 1;
                }
                if let Some(assistants) = k.get("assistants").and_then(|v| v.as_array()) {
                    for a in assistants.iter().filter_map(|a| a.as_str()) {
                        *assists.entry(a.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    // playtimeMillis / abilityCasts aren't round-scoped: sum them across
    // every source match's snapshot for that player instead.
    let mut playtime: HashMap<String, i64> = HashMap::new();
    let mut ability_casts: HashMap<String, serde_json::Map<String, serde_json::Value>> =
        HashMap::new();

    for (_, value) in sources {
        let Some(players) = value.get("players").and_then(|v| v.as_array()) else {
            continue;
        };
        for p in players {
            let Some(subject) = p.get("puuid").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(pt) = p.pointer("/stats/playtimeMillis").and_then(|v| v.as_i64()) {
                *playtime.entry(subject.to_string()).or_insert(0) += pt;
            }
            if let Some(casts) = p.pointer("/stats/abilityCasts").and_then(|v| v.as_object()) {
                let entry = ability_casts.entry(subject.to_string()).or_default();
                for (k, v) in casts {
                    let add = v.as_i64().unwrap_or(0);
                    let cur = entry.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
                    entry.insert(k.clone(), json!(cur + add));
                }
            }
        }
    }

    // Walk segments last-to-first so a player's most recent snapshot wins on
    // conflict, while still including players who only ever appear in a
    // middle segment (e.g. a 3+ segment merge where someone sits out the
    // final part of the match).
    let mut merged_players: Vec<serde_json::Value> = Vec::new();
    let mut seen_subjects: HashSet<String> = HashSet::new();

    for (_, value) in sources.iter().rev() {
        let Some(players) = value.get("players").and_then(|v| v.as_array()) else {
            continue;
        };
        for p in players {
            let Some(subject) = p.get("puuid").and_then(|v| v.as_str()).map(str::to_string) else {
                continue;
            };
            if !seen_subjects.insert(subject.clone()) {
                continue;
            }

            let mut player = p.clone();
            if let Some(stats) = player.get_mut("stats").and_then(|v| v.as_object_mut()) {
                stats.insert(
                    "score".to_string(),
                    json!(score.get(&subject).copied().unwrap_or(0)),
                );
                stats.insert(
                    "kills".to_string(),
                    json!(kills.get(&subject).copied().unwrap_or(0)),
                );
                stats.insert(
                    "deaths".to_string(),
                    json!(deaths.get(&subject).copied().unwrap_or(0)),
                );
                stats.insert(
                    "assists".to_string(),
                    json!(assists.get(&subject).copied().unwrap_or(0)),
                );
                stats.insert("roundsPlayed".to_string(), json!(rounds_played));
                if let Some(pt) = playtime.get(&subject) {
                    stats.insert("playtimeMillis".to_string(), json!(pt));
                }
                if let Some(casts) = ability_casts.get(&subject) {
                    stats.insert("abilityCasts".to_string(), json!(casts));
                }
            }
            merged_players.push(player);
        }
    }

    // teams: roundsPlayed/roundsWon recomputed from the merged rounds;
    // `won` stays whatever the last segment (the finished game) says.
    let mut rounds_won: HashMap<String, i64> = HashMap::new();
    for round in &merged_rounds {
        if let Some(wt) = round.get("winningTeam").and_then(|v| v.as_str())
            && !wt.is_empty()
        {
            *rounds_won.entry(wt.to_string()).or_insert(0) += 1;
        }
    }

    let last_teams = last_value
        .get("teams")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let merged_teams: Vec<serde_json::Value> = last_teams
        .into_iter()
        .map(|mut team| {
            if let Some(obj) = team.as_object_mut() {
                let team_id = obj
                    .get("teamId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                obj.insert("roundsPlayed".to_string(), json!(rounds_played));
                obj.insert(
                    "roundsWon".to_string(),
                    json!(rounds_won.get(&team_id).copied().unwrap_or(0)),
                );
            }
            team
        })
        .collect();

    let coaches = last_value
        .get("coaches")
        .filter(|v| !v.is_null())
        .or_else(|| first_value.get("coaches"))
        .cloned()
        .unwrap_or_else(|| json!([]));

    Ok(json!({
        "matchInfo": match_info,
        "players": merged_players,
        "coaches": coaches,
        "teams": merged_teams,
        "roundResults": merged_rounds,
    }))
}

fn bad_gateway_json(msg: &str) -> Response {
    json_response(
        StatusCode::BAD_GATEWAY,
        "ERROR",
        json!({ "error": msg }).to_string(),
    )
}

async fn store_match(db: &PgPool, region: &str, id: &str, body: &str) {
    if let Err(e) = sqlx::query(
        "INSERT INTO matches (region, match_id, body, fetched_at) VALUES ($1, $2, $3, now())
         ON CONFLICT (region, match_id) DO UPDATE SET body = EXCLUDED.body, fetched_at = EXCLUDED.fetched_at",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal match-v1 fixture shaped like a real Riot response: `puuid`
    /// (not `subject`) on players/round playerStats/kills, and no `bots`
    /// field at all (matches what the live API actually returns).
    fn fixture(match_id: &str, rounds: serde_json::Value) -> serde_json::Value {
        json!({
            "matchInfo": {
                "matchId": match_id,
                "mapId": "/Game/Maps/Ascent/Ascent",
                "gameVersion": "release-13.00",
                "gameLengthMillis": 100000,
                "region": "ap",
                "gameStartMillis": 1_000_000,
                "provisioningFlowId": "Matchmaking",
                "isCompleted": true,
                "customGameName": "",
                "queueId": "competitive",
                "gameMode": "/Game/GameModes/Bomb/BombGameMode.BombGameMode_C",
                "isRanked": true,
                "seasonId": "season-1",
                "premierMatchInfo": {}
            },
            "players": [
                {
                    "puuid": "p1",
                    "gameName": "Alice",
                    "tagLine": "EUW",
                    "teamId": "Red",
                    "partyId": "party-1",
                    "characterId": "char-1",
                    "stats": {
                        "score": 999,
                        "roundsPlayed": 999,
                        "kills": 999,
                        "deaths": 999,
                        "assists": 999,
                        "playtimeMillis": 50000,
                        "abilityCasts": { "grenadeCasts": 1, "ability1Casts": 2, "ability2Casts": 3, "ultimateCasts": 0 }
                    },
                    "competitiveTier": 10,
                    "isObserver": false,
                    "playerCard": "card-1",
                    "playerTitle": "title-1",
                    "accountLevel": 100
                },
                {
                    "puuid": "p2",
                    "gameName": "Bob",
                    "tagLine": "EUW",
                    "teamId": "Blue",
                    "partyId": "party-2",
                    "characterId": "char-2",
                    "stats": {
                        "score": 999,
                        "roundsPlayed": 999,
                        "kills": 999,
                        "deaths": 999,
                        "assists": 999,
                        "playtimeMillis": 50000,
                        "abilityCasts": { "grenadeCasts": 1, "ability1Casts": 2, "ability2Casts": 3, "ultimateCasts": 0 }
                    },
                    "competitiveTier": 10,
                    "isObserver": false,
                    "playerCard": "card-2",
                    "playerTitle": "title-2",
                    "accountLevel": 100
                }
            ],
            "coaches": [],
            "teams": [
                { "teamId": "Red", "won": true, "roundsPlayed": 999, "roundsWon": 999, "numPoints": 999 },
                { "teamId": "Blue", "won": false, "roundsPlayed": 999, "roundsWon": 999, "numPoints": 999 }
            ],
            "roundResults": rounds
        })
    }

    fn round(num: i64, winner: &str, killer: &str) -> serde_json::Value {
        json!({
            "roundNum": num,
            "roundResult": "Elimination",
            "roundCeremony": "",
            "winningTeam": winner,
            "bombPlanter": "",
            "bombDefuser": "",
            "plantRoundTime": 0,
            "plantPlayerLocations": null,
            "plantLocation": {},
            "plantSite": "",
            "defuseRoundTime": 0,
            "defusePlayerLocations": null,
            "defuseLocation": {},
            "playerStats": [
                {
                    "puuid": "p1",
                    "kills": if killer == "p1" { json!([{ "timeSinceGameStartMillis": 0, "timeSinceRoundStartMillis": 0, "killer": "p1", "victim": "p2", "victimLocation": {}, "assistants": [], "playerLocations": [] }]) } else { json!([]) },
                    "damage": [],
                    "score": 100,
                    "economy": {},
                    "ability": {}
                },
                {
                    "puuid": "p2",
                    "kills": if killer == "p2" { json!([{ "timeSinceGameStartMillis": 0, "timeSinceRoundStartMillis": 0, "killer": "p2", "victim": "p1", "victimLocation": {}, "assistants": [], "playerLocations": [] }]) } else { json!([]) },
                    "damage": [],
                    "score": 50,
                    "economy": {},
                    "ability": {}
                }
            ],
            "roundResultCode": ""
        })
    }

    #[test]
    fn merges_round_ranges_from_two_source_matches() {
        let match_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let match_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        let value_a = fixture(
            match_a,
            json!([round(0, "Red", "p1"), round(1, "Blue", "p2")]),
        );
        let value_b = fixture(
            match_b,
            json!([round(2, "Red", "p1"), round(3, "Blue", "p2")]),
        );

        let sources = vec![
            (
                MergeSegment {
                    match_id: match_a.to_string(),
                    start_round: 0,
                    end_round: 1,
                },
                value_a,
            ),
            (
                MergeSegment {
                    match_id: match_b.to_string(),
                    start_round: 2,
                    end_round: 3,
                },
                value_b,
            ),
        ];

        let new_id = compute_merged_id(
            sources
                .iter()
                .map(|(s, _)| s)
                .cloned()
                .collect::<Vec<_>>()
                .as_slice(),
        );
        assert!(new_id.starts_with(MERGED_ID_PREFIX));

        let merged = merge_round_data(&sources, &new_id).expect("merge should succeed");

        assert_eq!(merged["matchInfo"]["matchId"], json!(new_id));
        assert_eq!(merged["roundResults"].as_array().unwrap().len(), 4);

        let players = merged["players"]
            .as_array()
            .expect("players must be an array");
        assert_eq!(
            players.len(),
            2,
            "both players from the source matches must survive the merge"
        );

        let p1 = players
            .iter()
            .find(|p| p["puuid"] == "p1")
            .expect("p1 must be present");
        assert_eq!(p1["stats"]["kills"], json!(2));
        assert_eq!(p1["stats"]["deaths"], json!(2));
        assert_eq!(p1["stats"]["score"], json!(400));
        assert_eq!(p1["stats"]["roundsPlayed"], json!(4));
        assert_eq!(p1["stats"]["playtimeMillis"], json!(100000));

        assert!(
            merged.get("bots").is_none(),
            "the real API has no bots field, so merged output must not invent one"
        );
    }

    #[test]
    fn rejects_overlapping_round_numbers_between_segments() {
        let match_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let match_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        let value_a = fixture(match_a, json!([round(0, "Red", "p1")]));
        let value_b = fixture(match_b, json!([round(0, "Blue", "p2")]));

        let sources = vec![
            (
                MergeSegment {
                    match_id: match_a.to_string(),
                    start_round: 0,
                    end_round: 0,
                },
                value_a,
            ),
            (
                MergeSegment {
                    match_id: match_b.to_string(),
                    start_round: 0,
                    end_round: 0,
                },
                value_b,
            ),
        ];

        let new_id = compute_merged_id(
            sources
                .iter()
                .map(|(s, _)| s)
                .cloned()
                .collect::<Vec<_>>()
                .as_slice(),
        );
        let err =
            merge_round_data(&sources, &new_id).expect_err("overlapping roundNum must be rejected");
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn merged_id_is_order_independent_and_stable() {
        let seg_a = MergeSegment {
            match_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
            start_round: 0,
            end_round: 3,
        };
        let seg_b = MergeSegment {
            match_id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string(),
            start_round: 4,
            end_round: 20,
        };

        let id_1 = compute_merged_id(&[
            MergeSegment {
                match_id: seg_a.match_id.clone(),
                start_round: seg_a.start_round,
                end_round: seg_a.end_round,
            },
            MergeSegment {
                match_id: seg_b.match_id.clone(),
                start_round: seg_b.start_round,
                end_round: seg_b.end_round,
            },
        ]);
        let id_2 = compute_merged_id(&[seg_b, seg_a]);

        assert_eq!(id_1, id_2);
        assert!(id_1.starts_with(MERGED_ID_PREFIX));
        assert!(valid_match_id(&id_1));
    }
}
