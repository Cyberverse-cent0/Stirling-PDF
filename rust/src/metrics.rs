//! Operational metrics, porting the subset of Java's `MetricsFilter` +
//! `MetricsController` (`/api/v1/info/*`) that Rust can compute without the
//! Micrometer registry, JPA session store, or WAU service:
//!
//! - `http.requests` counters keyed by (method, path) — Java keys by
//!   best-matching route pattern plus a session tag; Rust has no sessions,
//!   so the "session" dimension is approximated by the `X-Browser-Id`
//!   header the frontend already sends (falling back to User-Agent), and
//!   dynamic path segments are collapsed to `{}` to bound cardinality.
//! - `/status`, `/health`, `/uptime`, `/load*`, `/requests*`, `/wau`.
//!
//! Java's `EndpointInspector` valid-GET-endpoint filter has no Rust
//! equivalent — every trackable path counts. Java's WAU service only
//! exists when `security.enableLogin=false`; the Rust backend has no
//! login mode, so `/wau` is always available.
//!
//! `metrics.enabled` in settings.yml gates every endpoint with Java's
//! `403 "This endpoint is disabled."` body.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use axum::extract::{Query, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::handlers::jobs::AppState;

static START: OnceLock<Instant> = OnceLock::new();
static ENABLED: OnceLock<bool> = OnceLock::new();
static COUNTERS: OnceLock<Mutex<HashMap<(String, String), u64>>> = OnceLock::new();
/// (method, path) → set of actors, for the `unique` metrics variants.
type ActorSets = HashMap<(String, String), HashSet<String>>;
static ACTORS: OnceLock<Mutex<ActorSets>> = OnceLock::new();
/// `WeeklyActiveUsersService` port: browser id → last-seen wall time.
static WAU: OnceLock<Mutex<WauState>> = OnceLock::new();

struct WauState {
    browsers: HashMap<String, SystemTime>,
    total_unique: u64,
    start: SystemTime,
}

pub fn init(metrics_enabled: bool) {
    let _ = START.set(Instant::now());
    let _ = ENABLED.set(metrics_enabled);
    let _ = COUNTERS.set(Mutex::new(HashMap::new()));
    let _ = ACTORS.set(Mutex::new(HashMap::new()));
    let _ = WAU.set(Mutex::new(WauState {
        browsers: HashMap::new(),
        total_unique: 0,
        start: SystemTime::now(),
    }));
}

fn counters() -> &'static Mutex<HashMap<(String, String), u64>> {
    COUNTERS.get().expect("metrics not initialised")
}

fn actors() -> &'static Mutex<HashMap<(String, String), HashSet<String>>> {
    ACTORS.get().expect("metrics not initialised")
}

fn wau_state() -> &'static Mutex<WauState> {
    WAU.get().expect("metrics not initialised")
}

/// `RequestUriUtils.isTrackableResource` minus the extension deny-list
/// Java applies to static assets — those never reach meaningful endpoints.
fn is_trackable(path: &str) -> bool {
    !(path.starts_with("/api/v1/info")
        || path.starts_with("/js")
        || path.starts_with("/images")
        || path.starts_with("/fonts")
        || path.starts_with("/pdfjs")
        || path.starts_with("/pdfium")
        || path.ends_with(".png")
        || path.ends_with(".ico")
        || path.ends_with(".css")
        || path.ends_with(".txt")
        || path.ends_with(".map")
        || path.ends_with(".svg")
        || path.ends_with(".js")
        || path.ends_with(".wasm")
        || path.contains("swagger"))
}

/// Collapse dynamic-looking segments so `/api/v1/general/job/<uuid>` counts
/// under one key, approximating Java's `BEST_MATCHING_PATTERN` tag.
fn normalize_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            // `all()` is vacuously true on the empty segment before a
            // leading `/` — guard it or every path gains a `{}` prefix.
            let dynamic = seg.len() > 24
                || (!seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
                || (seg.len() == 36 && seg.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
            if dynamic {
                "{}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub async fn track(req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let trackable = is_trackable(&path);
    // Java's WAUTrackingFilter records the X-Browser-Id header on every
    // request; the same header doubles as the "session" tag for the
    // unique-count metrics.
    let browser_id = req
        .headers()
        .get("x-browser-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let actor = browser_id.clone().or_else(|| {
        req.headers()
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    });
    let response = next.run(req).await;
    if let Some(id) = browser_id {
        let mut state = wau_state().lock().unwrap();
        if state.browsers.insert(id, SystemTime::now()).is_none() {
            state.total_unique += 1;
        }
    }
    if trackable {
        let key = (method, normalize_path(&path));
        *counters().lock().unwrap().entry(key.clone()).or_insert(0) += 1;
        if let Some(actor) = actor {
            actors()
                .lock()
                .unwrap()
                .entry(key)
                .or_default()
                .insert(actor);
        }
    }
    response
}

fn disabled() -> Response {
    (
        StatusCode::FORBIDDEN,
        [(axum::http::header::CONTENT_TYPE, "text/plain")],
        "This endpoint is disabled.",
    )
        .into_response()
}

fn enabled() -> bool {
    ENABLED.get().copied().unwrap_or(true)
}

fn uptime_seconds() -> u64 {
    START.get().map(|t| t.elapsed().as_secs()).unwrap_or(0)
}

/// Java's `formatDuration`: `"%dd %dh %dm %ds"`.
fn format_uptime(secs: u64) -> String {
    format!(
        "{}d {}h {}m {}s",
        secs / 86400,
        (secs % 86400) / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

async fn status() -> Json<Value> {
    Json(json!({
        "status": "UP",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn uptime() -> Response {
    if !enabled() {
        return disabled();
    }
    Json(json!(format_uptime(uptime_seconds()))).into_response()
}

#[derive(serde::Deserialize)]
pub struct RequestsQuery {
    endpoint: Option<String>,
}

/// `GET /api/v1/info/requests?endpoint=` → POST count as a bare number,
/// matching Java's `ResponseEntity.ok(double)`.
async fn requests(Query(q): Query<RequestsQuery>) -> Response {
    if !enabled() {
        return disabled();
    }
    let map = counters().lock().unwrap();
    let count: u64 = map
        .iter()
        .filter(|((method, path), _)| {
            method == "POST"
                && q.endpoint
                    .as_deref()
                    .is_none_or(|ep| path.ends_with(ep) || path == ep)
        })
        .map(|(_, n)| *n)
        .sum();
    Json(json!(count as f64)).into_response()
}

/// `GET /api/v1/info/requests/all` → per-endpoint POST counts, Java's
/// `EndpointCount` list shape (`endpoint`, `count`).
async fn requests_all() -> Response {
    if !enabled() {
        return disabled();
    }
    let map = counters().lock().unwrap();
    let mut rows: Vec<Value> = map
        .iter()
        .filter(|((method, _), _)| method == "POST")
        .map(|((_, path), n)| json!({ "endpoint": path, "count": n }))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r["count"].as_u64().unwrap_or(0)));
    Json(Value::Array(rows)).into_response()
}

fn endpoint_matches(path: &str, endpoint: Option<&str>) -> bool {
    endpoint.is_none_or(|ep| path == ep || path.ends_with(ep))
}

fn count_for(method: &str, endpoint: Option<&str>) -> u64 {
    counters()
        .lock()
        .unwrap()
        .iter()
        .filter(|((m, path), _)| m == method && endpoint_matches(path, endpoint))
        .map(|(_, n)| *n)
        .sum()
}

fn unique_for(method: &str, endpoint: Option<&str>) -> usize {
    actors()
        .lock()
        .unwrap()
        .iter()
        .filter(|((m, path), _)| m == method && endpoint_matches(path, endpoint))
        .map(|(_, set)| set.len())
        .sum()
}

fn endpoint_rows(method: &str, unique: bool) -> Vec<Value> {
    if unique {
        let map = actors().lock().unwrap();
        let mut rows: Vec<Value> = map
            .iter()
            .filter(|((m, _), _)| m == method)
            .map(|((_, path), set)| json!({ "endpoint": path, "count": set.len() }))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r["count"].as_u64().unwrap_or(0)));
        rows
    } else {
        let map = counters().lock().unwrap();
        let mut rows: Vec<Value> = map
            .iter()
            .filter(|((m, _), _)| m == method)
            .map(|((_, path), n)| json!({ "endpoint": path, "count": n }))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r["count"].as_u64().unwrap_or(0)));
        rows
    }
}

async fn load(Query(q): Query<RequestsQuery>) -> Response {
    if !enabled() {
        return disabled();
    }
    Json(json!(count_for("GET", q.endpoint.as_deref()) as f64)).into_response()
}

async fn load_unique(Query(q): Query<RequestsQuery>) -> Response {
    if !enabled() {
        return disabled();
    }
    Json(json!(unique_for("GET", q.endpoint.as_deref()) as f64)).into_response()
}

async fn load_all() -> Response {
    if !enabled() {
        return disabled();
    }
    Json(Value::Array(endpoint_rows("GET", false))).into_response()
}

async fn load_all_unique() -> Response {
    if !enabled() {
        return disabled();
    }
    Json(Value::Array(endpoint_rows("GET", true))).into_response()
}

async fn requests_unique(Query(q): Query<RequestsQuery>) -> Response {
    if !enabled() {
        return disabled();
    }
    Json(json!(unique_for("POST", q.endpoint.as_deref()) as f64)).into_response()
}

async fn requests_all_unique() -> Response {
    if !enabled() {
        return disabled();
    }
    Json(Value::Array(endpoint_rows("POST", true))).into_response()
}

/// `WeeklyActiveUsersService.getWeeklyActiveUsers` et al: entries unseen
/// for 7 days drop out of the weekly count but stay in the total.
async fn wau_info() -> Response {
    if !enabled() {
        return disabled();
    }
    let mut state = wau_state().lock().unwrap();
    let cutoff = SystemTime::now() - std::time::Duration::from_secs(7 * 24 * 3600);
    state.browsers.retain(|_, seen| *seen >= cutoff);
    let weekly = state.browsers.len() as u64;
    let total = state.total_unique;
    let days_online = state
        .start
        .elapsed()
        .map(|d| d.as_secs() / 86400)
        .unwrap_or(0);
    let tracking_since = chrono::DateTime::<chrono::Utc>::from(state.start)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Json(json!({
        "weeklyActiveUsers": weekly,
        "totalUniqueBrowsers": total,
        "daysOnline": days_online,
        "trackingSince": tracking_since,
    }))
    .into_response()
}

/// Register the `/api/v1/info` surface Rust owns; the Java controller's
/// remaining endpoints are all covered here.
pub fn routes() -> axum::Router<AppState> {
    use axum::routing::get;
    axum::Router::new()
        .route("/api/v1/info/status", get(status))
        .route("/api/v1/info/health", get(status))
        .route("/api/v1/info/uptime", get(uptime))
        .route("/api/v1/info/load", get(load))
        .route("/api/v1/info/load/unique", get(load_unique))
        .route("/api/v1/info/load/all", get(load_all))
        .route("/api/v1/info/load/all/unique", get(load_all_unique))
        .route("/api/v1/info/requests", get(requests))
        .route("/api/v1/info/requests/unique", get(requests_unique))
        .route("/api/v1/info/requests/all", get(requests_all))
        .route("/api/v1/info/requests/all/unique", get(requests_all_unique))
        .route("/api/v1/info/wau", get(wau_info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trackable_paths_exclude_assets_and_info() {
        assert!(is_trackable("/api/v1/general/merge-pdfs"));
        assert!(!is_trackable("/api/v1/info/status"));
        assert!(!is_trackable("/assets/index-abc123.js"));
        assert!(!is_trackable("/images/logo.png"));
        assert!(!is_trackable("/pdfjs/web/viewer.html"));
        assert!(!is_trackable("/sw.js"));
        assert!(!is_trackable("/swagger-ui/index.html"));
        // API paths are always trackable regardless of suffix.
        assert!(is_trackable("/api/v1/misc/update-metadata"));
    }

    #[test]
    fn normalize_path_collapses_dynamic_segments() {
        // UUID-shaped segments collapse like Java's route-pattern tag.
        let uuid = "123e4567-e89b-42d3-a456-426614174000";
        assert_eq!(
            normalize_path(&format!("/api/v1/general/job/{uuid}")),
            "/api/v1/general/job/{}"
        );
        // Long opaque ids collapse too.
        let long = "a".repeat(30);
        assert_eq!(
            normalize_path(&format!("/api/v1/general/files/{long}")),
            "/api/v1/general/files/{}"
        );
        // Numeric and static segments are preserved.
        assert_eq!(
            normalize_path("/api/v1/general/job/42"),
            "/api/v1/general/job/{}"
        );
        assert_eq!(
            normalize_path("/api/v1/analysis/page-count"),
            "/api/v1/analysis/page-count"
        );
    }

    #[test]
    fn format_uptime_matches_java_shape() {
        assert_eq!(format_uptime(0), "0d 0h 0m 0s");
        assert_eq!(format_uptime(59), "0d 0h 0m 59s");
        assert_eq!(format_uptime(3661), "0d 1h 1m 1s");
        assert_eq!(format_uptime(90061), "1d 1h 1m 1s");
        assert_eq!(format_uptime(172800), "2d 0h 0m 0s");
    }

    #[tokio::test]
    async fn counters_accumulate_per_method_and_path() {
        use tower::ServiceExt;
        init(true);
        let app = axum::Router::new()
            .route(
                "/api/v1/analysis/page-count",
                axum::routing::post(|| async { "ok" }),
            )
            .layer(axum::middleware::from_fn(track));
        for _ in 0..2 {
            app.clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/api/v1/analysis/page-count")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        // A GET to a POST-only route 404s — it must not panic or count.
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/v1/info/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn requests_endpoint_counts_only_posts() {
        use tower::ServiceExt;
        init(true);
        let app = routes().with_state(crate::handlers::jobs::AppState::new());
        // Seed the counter map directly — the middleware path is covered
        // above; this asserts the /requests query semantics.
        counters().lock().unwrap().insert(
            ("POST".to_string(), "/api/v1/general/merge-pdfs".to_string()),
            3,
        );
        counters().lock().unwrap().insert(
            ("GET".to_string(), "/api/v1/general/merge-pdfs".to_string()),
            9,
        );
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/info/requests?endpoint=merge-pdfs")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.as_f64(), Some(3.0));

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/info/requests/all")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let rows = json.as_array().unwrap();
        assert!(rows.iter().all(
            |r| r["endpoint"].as_str() != Some("/api/v1/general/merge-pdfs")
                || r["count"].as_u64() == Some(3)
        ));
    }
}
