//! Platform/config endpoints: `/api/v1/config/*`, `/api/v1/ui-data/*` and
//! `/api/v1/settings/*`. These port the Spring-free parts of
//! `ConfigController`, `UIDataController`, `SettingsController` and
//! `LoginDisclaimerController`/`LoginAgreementService`.
//!
//! `/app-config`'s proprietary auth/licensing fields and personal
//! signatures (they need the proprietary user service) stay Java-side.
//! `update-enable-analytics` writes `configs/settings.yml` with a surgical
//! text edit so the file's comments survive, matching `YamlHelper`'s
//! comment-preserving update closely enough for a single boolean key.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI8, AtomicU16, Ordering};
use std::sync::Mutex;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::endpointconfig::{self, DisableReason};
use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::settings::{self, Settings};

/// Spring relaxed-binding env spellings for `legal.loginAgreement.*` and
/// `system.*` keys the platform endpoints read.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Spring's lenient Boolean binding: true/false/on/off/yes/no/1/0,
/// case-insensitive. Unparseable values yield `None` (file value wins),
/// where Java would fail property binding at startup.
fn env_bool(name: &str) -> Option<bool> {
    env_value(name).and_then(|v| match v.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/config/login-disclaimer
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct DisclaimerQuery {
    pub lang: Option<String>,
}

struct LoginAgreementConfig {
    enabled: bool,
    show_in_anonymous_mode: bool,
    fallback_text: String,
}

fn login_agreement(settings: &Settings) -> LoginAgreementConfig {
    let la = &settings.legal.login_agreement;
    LoginAgreementConfig {
        enabled: env_bool("LEGAL_LOGINAGREEMENT_ENABLED").unwrap_or(la.enabled),
        show_in_anonymous_mode: env_bool("LEGAL_LOGINAGREEMENT_SHOWINANONYMOUSMODE")
            .unwrap_or(la.show_in_anonymous_mode),
        fallback_text: env_value("LEGAL_LOGINAGREEMENT_FALLBACKTEXT")
            .unwrap_or_else(|| la.fallback_text.clone()),
    }
}

fn disclaimer_response(enabled: bool, show_in_anonymous_mode: bool, content: &str) -> Json<Value> {
    Json(json!({
        "enabled": enabled,
        "showInAnonymousMode": show_in_anonymous_mode,
        "content": content,
        "format": "markdown",
    }))
}

/// `LoginAgreementService` port. The disclaimer files are read live on every
/// request so admin edits take effect without a restart, exactly like Java.
pub async fn login_disclaimer(Query(query): Query<DisclaimerQuery>) -> Json<Value> {
    let settings = settings::current();
    let agreement = login_agreement(settings);
    let show = agreement.show_in_anonymous_mode;
    if !agreement.enabled {
        return disclaimer_response(false, show, "");
    }
    let default_locale =
        env_value("SYSTEM_DEFAULTLOCALE").unwrap_or_else(|| settings.system.default_locale.clone());
    let dir = settings::base_dir().join("customFiles").join("disclaimer");
    let content = resolve_disclaimer(
        &dir,
        query.lang.as_deref(),
        &default_locale,
        &agreement.fallback_text,
    );
    // Enabled but unresolvable text reports disabled so clients don't render
    // an empty agreement — same rule as the Java controller.
    let has_content = !content.trim().is_empty();
    disclaimer_response(
        has_content,
        show,
        if has_content { content.as_str() } else { "" },
    )
}

/// `LoginAgreementService.resolveContent`: requested locale candidates,
/// then the configured default locale's candidates, then `fallbackText`.
fn resolve_disclaimer(
    dir: &Path,
    requested: Option<&str>,
    default_locale: &str,
    fallback: &str,
) -> String {
    let mut candidates = Vec::new();
    add_locale_candidates(&mut candidates, requested);
    add_locale_candidates(&mut candidates, Some(default_locale));
    for candidate in &candidates {
        if let Some(text) = read_disclaimer_file(dir, candidate) {
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    fallback.to_string()
}

/// Requested locale then its base (`fr-FR` → `fr`), deduplicated —
/// `addLocaleCandidates`.
fn add_locale_candidates(out: &mut Vec<String>, locale: Option<&str>) {
    let Some(locale) = locale else { return };
    if !is_valid_locale(locale) {
        return;
    }
    if !out.iter().any(|c| c == locale) {
        out.push(locale.to_string());
    }
    let base = locale.split(['_', '-']).next().unwrap_or(locale);
    if base != locale && !out.iter().any(|c| c == base) {
        out.push(base.to_string());
    }
}

/// `LOCALE_PATTERN` (`^[A-Za-z]{2,3}([_-][A-Za-z0-9]{2,8})*$`) plus the
/// 35-char cap, implemented without regex. Locale codes are ASCII-only, so
/// byte length matches Java's `String.length()` check closely enough — the
/// cap only exists to bound the match, and non-ASCII input fails the alpha
/// check anyway.
fn is_valid_locale(locale: &str) -> bool {
    let bytes = locale.as_bytes();
    if bytes.is_empty() || bytes.len() > 35 {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if !(2..=3).contains(&(i)) {
        return false;
    }
    while i < bytes.len() {
        if bytes[i] != b'_' && bytes[i] != b'-' {
            return false;
        }
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if !(2..=8).contains(&(i - start)) {
            return false;
        }
    }
    true
}

/// 256 KiB cap — disclaimers are short markdown and the public endpoint
/// must not load an oversized file into memory per request.
const MAX_DISCLAIMER_BYTES: u64 = 256 * 1024;

/// `readFileIfExists`: regular file only (symlinks rejected via
/// `symlink_metadata`, matching `NOFOLLOW_LINKS`), size-capped, UTF-8.
fn read_disclaimer_file(dir: &Path, locale: &str) -> Option<String> {
    let file = dir.join(format!("{locale}.md"));
    let meta = std::fs::symlink_metadata(&file).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_DISCLAIMER_BYTES {
        return None;
    }
    std::fs::read_to_string(&file).ok()
}

// ---------------------------------------------------------------------------
// GET /api/v1/config/{endpoint-enabled,endpoints-enabled,endpoints-availability,group-enabled}
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct EndpointQuery {
    pub endpoint: String,
}

#[derive(Deserialize)]
pub struct EndpointsQuery {
    pub endpoints: String,
}

#[derive(Deserialize)]
pub struct AvailabilityQuery {
    pub endpoints: Option<String>,
}

#[derive(Deserialize)]
pub struct GroupQuery {
    pub group: String,
}

pub async fn endpoint_enabled(Query(query): Query<EndpointQuery>) -> Json<Value> {
    Json(json!(endpointconfig::is_endpoint_enabled(&query.endpoint)))
}

/// Java's `String.split(",")` drops trailing empty segments; Rust's keeps
/// them, so replicate the trailing-empty trim to keep the response map
/// identical.
fn java_split_commas(value: &str) -> Vec<&str> {
    // Java's split on an empty input yields a single empty string, while
    // Rust yields the same — but the trailing-empty trim below would
    // wrongly empty it, so short-circuit first.
    if value.is_empty() {
        return vec![""];
    }
    let mut parts: Vec<&str> = value.split(',').collect();
    while parts.last() == Some(&"") {
        parts.pop();
    }
    parts
}

pub async fn endpoints_enabled(Query(query): Query<EndpointsQuery>) -> Json<Value> {
    let mut result = Map::new();
    for endpoint in java_split_commas(&query.endpoints) {
        let trimmed = endpoint.trim();
        result.insert(
            trimmed.to_string(),
            json!(endpointconfig::is_endpoint_enabled(trimmed)),
        );
    }
    Json(Value::Object(result))
}

fn reason_value(reason: Option<DisableReason>) -> Value {
    match reason {
        None => Value::Null,
        Some(DisableReason::Config) => json!("CONFIG"),
        Some(DisableReason::Dependency) => json!("DEPENDENCY"),
        Some(DisableReason::Unknown) => json!("UNKNOWN"),
    }
}

pub async fn endpoints_availability(Query(query): Query<AvailabilityQuery>) -> Json<Value> {
    let to_check: Vec<String> = match query.endpoints {
        Some(list) if !list.is_empty() => java_split_commas(&list)
            .into_iter()
            .map(|e| e.trim().to_string())
            .collect(),
        _ => endpointconfig::all_endpoints(),
    };
    let mut result = Map::new();
    for endpoint in to_check {
        let (enabled, reason) = endpointconfig::endpoint_availability(&endpoint);
        result.insert(
            endpoint,
            json!({ "enabled": enabled, "reason": reason_value(reason) }),
        );
    }
    Json(Value::Object(result))
}

pub async fn group_enabled(Query(query): Query<GroupQuery>) -> Json<Value> {
    Json(json!(endpointconfig::is_group_enabled(&query.group)))
}

// ---------------------------------------------------------------------------
// GET /api/v1/ui-data/*
// ---------------------------------------------------------------------------

/// `SHOW_SURVEY`: absent means show; only the literal "true" (any case)
/// keeps it on — `"yes"`/`"1"` disable it, matching
/// `"true".equalsIgnoreCase(showSurvey)`.
pub async fn ui_data_home() -> Json<Value> {
    let show = match std::env::var("SHOW_SURVEY") {
        Ok(v) => v.eq_ignore_ascii_case("true"),
        Err(_) => true,
    };
    Json(json!({ "showSurveyFromDocker": show }))
}

const THIRD_PARTY_LICENSES: &str =
    include_str!("../../../app/core/src/main/resources/static/3rdPartyLicenses.json");

/// `getLicensesData`: pass the `dependencies` array through untouched so the
/// `Dependency` model stays byte-identical to Java's. Missing key → `null`,
/// unreadable/unparsable resource → `[]`, matching the catch block.
pub async fn ui_data_licenses() -> Json<Value> {
    let dependencies = match serde_json::from_str::<Value>(THIRD_PARTY_LICENSES) {
        Ok(doc) => doc.get("dependencies").cloned().unwrap_or(Value::Null),
        Err(_) => json!([]),
    };
    Json(json!({ "dependencies": dependencies }))
}

/// `getOcrPdfData`: `system.tessdataDir` (or `SYSTEM_TESSDATADIR`) wins,
/// then `TESSDATA_PREFIX`, then the compiled-in default — `RuntimePathConfig`'s
/// order. Only top-level `*.traineddata` names are languages; `osd` is excluded.
pub async fn ui_data_ocr_pdf() -> Json<Value> {
    let settings = settings::current();
    let configured = env_value("SYSTEM_TESSDATADIR")
        .or_else(|| {
            (!settings.system.tessdata_dir.trim().is_empty())
                .then(|| settings.system.tessdata_dir.clone())
        })
        .or_else(|| env_value("TESSDATA_PREFIX"))
        .unwrap_or_else(|| "/usr/share/tesseract-ocr/5/tessdata".to_string());
    let mut languages = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&configured) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".traineddata") {
                let lang = name.replace(".traineddata", "");
                if !lang.eq_ignore_ascii_case("osd") {
                    languages.push(lang);
                }
            }
        }
    }
    languages.sort();
    Json(json!({ "languages": languages }))
}

/// `FooterData`: legal URLs plus the tri-state analytics flag. The effective
/// value mirrors the Java bean — env binding wins, then the value written by
/// `update-enable-analytics` at runtime, then the file value from startup.
pub async fn ui_data_footer_info() -> Json<Value> {
    let settings = settings::current();
    let analytics = env_bool("SYSTEM_ENABLEANALYTICS")
        .or_else(analytics_override)
        .or(settings.system.enable_analytics);
    let legal = &settings.legal;
    Json(json!({
        "analyticsEnabled": analytics,
        "termsAndConditions": legal.terms_and_conditions.as_str(),
        "privacyPolicy": legal.privacy_policy.as_str(),
        "accessibilityStatement": legal.accessibility_statement.as_str(),
        "cookiePolicy": legal.cookie_policy.as_str(),
        "impressum": legal.impressum.as_str(),
    }))
}

/// `getPipelineData`: recursively collect `*.json` under the resolved
/// `defaultWebUIConfigs` dir (`system.customPaths.pipeline.webUIConfigsDir`
/// overrides `<pipelineDir>/defaultWebUIConfigs`, which itself defaults to
/// `<base>/pipeline`). Each file's raw text is echoed back with a display
/// name taken from its `name` field or filename.
pub async fn ui_data_pipeline() -> Json<Value> {
    let settings = settings::current();
    let pipeline = &settings.system.custom_paths.pipeline;
    let pipeline_root = if pipeline.pipeline_dir.trim().is_empty() {
        settings::base_dir().join("pipeline")
    } else {
        PathBuf::from(&pipeline.pipeline_dir)
    };
    let configs_dir = if pipeline.web_ui_configs_dir.trim().is_empty() {
        pipeline_root.join("defaultWebUIConfigs")
    } else {
        PathBuf::from(&pipeline.web_ui_configs_dir)
    };

    let mut files = Vec::new();
    collect_json_files(&configs_dir, &mut files);

    let mut with_names = Vec::new();
    let mut raw_configs = Vec::new();
    for file in files {
        // Java aborts the whole walk on a read error; skipping the single
        // unreadable file keeps the remaining configs available instead.
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        let name = serde_json::from_str::<Value>(&content)
            .ok()
            .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_string))
            .filter(|n| !n.is_empty())
            .or_else(|| {
                file.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .and_then(|n| n.rsplit_once('.').map(|(stem, _)| stem.to_string()))
            })
            .unwrap_or_default();
        with_names.push(json!({ "json": content.clone(), "name": name }));
        raw_configs.push(Value::String(content));
    }

    if with_names.is_empty() {
        with_names.push(json!({ "json": "", "name": "No preloaded configs found" }));
    }
    Json(json!({
        "pipelineConfigsWithNames": with_names,
        "pipelineConfigs": raw_configs,
    }))
}

fn collect_json_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, out);
        } else if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(".json"))
        {
            out.push(path);
        }
    }
}

/// `getSignData`: shared signatures from `customFiles/signatures/ALL_USERS`
/// plus the font list. Divergence: Rust has no auth, so the "Personal"
/// category never appears (Java only adds it when a user service is
/// present).
pub async fn ui_data_sign() -> Json<Value> {
    let signatures_dir = settings::base_dir()
        .join("customFiles")
        .join("signatures")
        .join("ALL_USERS");
    let mut signatures = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&signatures_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_lowercase();
            if path.is_file()
                && (lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.ends_with(".png"))
            {
                signatures.push(json!({ "fileName": name, "category": "Shared" }));
            }
        }
    }
    signatures.sort_by(|a, b| {
        a["fileName"]
            .as_str()
            .unwrap_or("")
            .cmp(b["fileName"].as_str().unwrap_or(""))
    });

    let mut fonts: Vec<Value> = crate::fonts::BUNDLED
        .iter()
        .map(|(name, _)| crate::fonts::describe(name))
        .collect();
    if let Some(root) = crate::web::static_root() {
        fonts.extend(crate::fonts::custom_fonts(&root));
    }
    Json(json!({ "signatures": signatures, "fonts": fonts }))
}

/// `AdditionalLanguageJsController`: emits the `supportedLanguages` JS
/// snippet index pages include. Java scans `classpath*:messages_*.properties`;
/// Rust scans the working and executable directories for the same
/// filenames — dependency-jar locales have no Rust equivalent, and no
/// `messages_*.properties` exist in-tree, so both sides currently emit an
/// empty list filtered by `ui.languages`.
pub async fn additional_language_js() -> Response {
    let mut languages: Vec<String> = Vec::new();
    let mut dirs = vec![settings::base_dir()];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(code) = name
                .strip_prefix("messages_")
                .and_then(|s| s.strip_suffix(".properties"))
            {
                languages.push(code.to_string());
            }
        }
    }
    languages.sort();
    languages.dedup();
    if let Some(whitelist) = &settings::current().ui.languages {
        if !whitelist.is_empty() {
            languages.retain(|l| whitelist.contains(l));
        }
    }
    let list = languages
        .iter()
        .map(|l| format!("\"{l}\""))
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(
        "const supportedLanguages = [{list}];\n\
         function getDetailedLanguageCode() {{\n\
         \x20   const userLanguages = navigator.languages ? navigator.languages : [navigator.language];\n\
         \x20   for (let lang of userLanguages) {{\n\
         \x20       let matchedLang = supportedLanguages.find(supportedLang => supportedLang.startsWith(lang.replace('-', '_')));\n\
         \x20       if (matchedLang) {{\n\
         \x20           return matchedLang;\n\
         \x20       }}\n\
         \x20   }}\n\
         \x20   return \"en_US\";\n\
         }}\n"
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/javascript")
        .body(axum::body::Body::from(body))
        .expect("static response")
}

/// `SignatureImageController.getSignature`: serve a shared signature
/// image from `customFiles/signatures/ALL_USERS/`. Personal signatures
/// need the proprietary auth stack — out of scope, same as `/ui-data/sign`.
pub async fn signature_image(
    axum::extract::Path(file_name): axum::extract::Path<String>,
) -> Response {
    // `SharedSignatureService.validateFileName`: no separators, and only
    // [a-zA-Z0-9_.-] — Java throws IllegalArgumentException (500 via the
    // global handler); 400 is the honest status for a bad client input.
    let valid = !file_name.contains("..")
        && !file_name.contains('/')
        && !file_name.contains('\\')
        && !file_name.is_empty()
        && file_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !valid {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let path = settings::base_dir()
        .join("customFiles")
        .join("signatures")
        .join("ALL_USERS")
        .join(&file_name);
    let Ok(bytes) = std::fs::read(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let lower = file_name.to_lowercase();
    let content_type = if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else {
        "image/png"
    };
    let mut res = Response::new(axum::body::Body::from(bytes));
    res.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(content_type),
    );
    res
}

// ---------------------------------------------------------------------------
// GET /api/v1/config/app-config
// ---------------------------------------------------------------------------

/// Spring relaxed-binding env name for a dotted property path:
/// `system.mobileScannerSettings.convertToPdf` →
/// `SYSTEM_MOBILESCANNERSETTINGS_CONVERTTOPDF` (dots → `_`, dashes and camel
/// humps collapse, uppercase).
fn env_prop(path: &str) -> Option<String> {
    env_value(&path.to_uppercase().replace('.', "_").replace('-', ""))
}

fn env_prop_bool(path: &str) -> Option<bool> {
    env_prop(path).and_then(|v| match v.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    })
}

/// The port this process actually bound — `PORT=0` (ephemeral) resolves to
/// the OS-assigned port, matching Java's `local.server.port` fallback.
static BOUND_PORT: AtomicU16 = AtomicU16::new(0);
static BOUND_ADDR: std::sync::RwLock<Option<std::net::SocketAddr>> = std::sync::RwLock::new(None);

pub fn note_bound_port(port: u16) {
    BOUND_PORT.store(port, Ordering::Relaxed);
}

/// Records the listener's actual address so loopback dispatch (the pipeline)
/// can reach this server the way Java's `local.server.port` does.
pub fn note_bound_addr(addr: std::net::SocketAddr) {
    *BOUND_ADDR.write().unwrap() = Some(addr);
    note_bound_port(addr.port());
}

/// The bound socket address, or `None` before the listener is up (tests).
pub fn bound_addr() -> Option<std::net::SocketAddr> {
    *BOUND_ADDR.read().unwrap()
}

fn effective_server_port() -> String {
    let bound = BOUND_PORT.load(Ordering::Relaxed);
    if bound > 0 {
        return bound.to_string();
    }
    std::env::var("PORT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "8081".to_string())
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host == "0:0:0:0:0:0:0:1"
        || host == "[::1]"
}

/// `GeneralUtils.getLocalNetworkIp`: the default-route trick first
/// (connect a UDP socket toward a public address and read the chosen
/// source IP), then Java falls back to enumerating interfaces — std has no
/// interface enumeration, so Rust stops at the route trick.
fn local_network_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:53").ok()?;
    let addr = socket.local_addr().ok()?;
    let ip = addr.ip();
    match ip {
        std::net::IpAddr::V4(v4)
            if !v4.is_loopback() && !v4.is_unspecified() && !v4.is_link_local() =>
        {
            Some(ip.to_string())
        }
        _ => None,
    }
}

/// `resolveFrontendUrl`: configured `system.frontendUrl` wins; then the
/// Host the request arrived on (honouring `X-Forwarded-*` like Java's
/// `server.forward-headers-strategy=NATIVE`); then a detected site-local
/// IPv4; then empty.
/// First entry of a possibly comma-separated header value — proxies append
/// to `X-Forwarded-*`, and the client-facing value is the leftmost.
fn first_header_entry(v: Option<&str>) -> Option<&str> {
    v.and_then(|s| s.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn resolve_frontend_url(headers: &axum::http::HeaderMap, settings: &Settings) -> String {
    let configured = env_prop("system.frontendUrl")
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            (!settings.system.frontend_url.trim().is_empty())
                .then(|| settings.system.frontend_url.clone())
        });
    if let Some(url) = configured {
        return url;
    }

    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let scheme = first_header_entry(header("x-forwarded-proto"))
        .unwrap_or("http")
        .to_string();
    let xf_port = first_header_entry(header("x-forwarded-port")).map(str::to_string);
    let forwarded_host = first_header_entry(header("x-forwarded-host"));
    let host_header = first_header_entry(header("host"));

    // Host may carry ":port"; strip it for the name and keep the port.
    let (host_name, host_port) = match host_header {
        Some(h) => match h.rsplit_once(':') {
            Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
                (name, Some(port.to_string()))
            }
            _ => (h, None),
        },
        None => ("", None),
    };
    let (name, port_in_host) = match forwarded_host {
        Some(fh) => {
            let (n, p) = match fh.rsplit_once(':') {
                Some((n, p)) if p.chars().all(|c| c.is_ascii_digit()) => (n, Some(p.to_string())),
                _ => (fh, None),
            };
            (n, p.or(host_port))
        }
        None => (host_name, host_port),
    };

    if !name.is_empty() && !is_loopback_host(name) {
        let port = xf_port
            .or(port_in_host)
            .unwrap_or_else(effective_server_port);
        let default_port =
            (scheme == "http" && port == "80") || (scheme == "https" && port == "443");
        return if default_port {
            format!("{scheme}://{name}")
        } else {
            format!("{scheme}://{name}:{port}")
        };
    }

    if let Some(ip) = local_network_ip() {
        let scheme = if settings.system.backend_url.starts_with("https") {
            "https"
        } else {
            "http"
        };
        return format!("{scheme}://{ip}:{}", effective_server_port());
    }
    String::new()
}

/// `AppConfig.determineMachineType`.
fn machine_type() -> &'static str {
    if std::env::var("KUBERNETES_SERVICE_HOST").is_ok() {
        return "Kubernetes";
    }
    if Path::new("/.dockerenv").exists() {
        return "Docker";
    }
    if std::env::var("BROWSER_OPEN")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return match std::env::consts::OS {
            "windows" => "Client-windows",
            "macos" => "Client-mac",
            _ => "Client-unix",
        };
    }
    "Server-jar"
}

/// `TimestampController.TSA_PRESETS`.
const TSA_PRESETS: &[(&str, &str)] = &[
    ("DigiCert", "http://timestamp.digicert.com"),
    ("Sectigo", "http://timestamp.sectigo.com"),
    ("SSL.com", "http://ts.ssl.com"),
    ("FreeTSA", "https://freetsa.org/tsr"),
    ("MeSign", "http://tsa.mesign.com"),
];

/// `ConfigController.getAppConfig`. In an OSS build every proprietary
/// service (`userService`, `licenseService`, `serverCertificateService`,
/// `showAdmin`, the `SSOAutoLogin` bean) is absent, so login is always off,
/// premium/licensing keys report the `@Profile("default")` bean values, and
/// admin/user flags are `false` — the same answers Java gives without the
/// proprietary module.
pub async fn app_config(headers: axum::http::HeaderMap) -> Json<Value> {
    let settings = settings::current();
    let sys = &settings.system;
    let ui = &settings.ui;
    let sec = &settings.security;
    let mail = &settings.mail;
    let storage = &settings.storage;
    let gd = &settings.premium.pro_features.google_drive;
    let ai = &settings.ai_engine;

    // Env-bound properties win over file values, as Spring's property-source
    // order dictates. Booleans fall back to the file/default when unset.
    let env_or = |path: &str, file: &str| env_prop(path).unwrap_or_else(|| file.to_string());
    let env_or_bool = |path: &str, file: bool| env_prop_bool(path).unwrap_or(file);

    // `security.enableLogin && userService != null` — the user service is
    // proprietary-only, so the conjunction is always false in an OSS build.
    let enable_login = false;

    let frontend_url = resolve_frontend_url(&headers, settings);
    let frontend_url_configured = !frontend_url.trim().is_empty();

    let storage_enabled = enable_login && env_or_bool("storage.enabled", storage.enabled);
    let sharing = &storage.sharing;
    let sharing_enabled =
        storage_enabled && env_or_bool("storage.sharing.enabled", sharing.enabled);
    let share_links_enabled = sharing_enabled
        && env_or_bool("storage.sharing.linkEnabled", sharing.link_enabled)
        && frontend_url_configured;
    let share_email_enabled = sharing_enabled
        && env_or_bool("storage.sharing.emailEnabled", sharing.email_enabled)
        && env_or_bool("mail.enabled", mail.enabled);

    let show_update = env_or_bool("system.showUpdate", sys.show_update);
    // `showAdmin` is proprietary-only → null → the admin gate is open.
    let should_show_update = show_update;

    let mut map = Map::new();
    map.insert("dependenciesReady".into(), json!(true));
    map.insert(
        "baseUrl".into(),
        json!(env_or("system.backendUrl", &sys.backend_url).pipe_non_blank("http://localhost")),
    );
    map.insert(
        "contextPath".into(),
        json!(env_prop("server.servlet.context-path")
            .or_else(|| env_prop("server.servlet.contextPath"))
            .unwrap_or_else(|| "/".to_string())),
    );
    map.insert("serverPort".into(), json!(effective_server_port()));
    map.insert("frontendUrl".into(), json!(frontend_url));

    map.insert(
        "enableMobileScanner".into(),
        json!(env_or_bool(
            "system.enableMobileScanner",
            sys.enable_mobile_scanner
        )),
    );
    map.insert(
        "enableMobileSignature".into(),
        json!(env_or_bool(
            "system.enableMobileSignature",
            sys.enable_mobile_signature
        )),
    );
    let mss = &sys.mobile_scanner_settings;
    map.insert(
        "mobileScannerConvertToPdf".into(),
        json!(env_or_bool(
            "system.mobileScannerSettings.convertToPdf",
            mss.convert_to_pdf
        )),
    );
    map.insert(
        "mobileScannerImageResolution".into(),
        json!(env_or(
            "system.mobileScannerSettings.imageResolution",
            &mss.image_resolution
        )),
    );
    map.insert(
        "mobileScannerPageFormat".into(),
        json!(env_or(
            "system.mobileScannerSettings.pageFormat",
            &mss.page_format
        )),
    );
    map.insert(
        "mobileScannerStretchToFit".into(),
        json!(env_or_bool(
            "system.mobileScannerSettings.stretchToFit",
            mss.stretch_to_fit
        )),
    );

    // `Ui.getAppNameNavbar` collapses blank to null.
    let app_name = ui.app_name_navbar.trim();
    map.insert(
        "appNameNavbar".into(),
        if app_name.is_empty() {
            Value::Null
        } else {
            json!(app_name)
        },
    );
    map.insert("languages".into(), json!(ui.languages.as_ref()));
    map.insert(
        "defaultLocale".into(),
        json!(env_or("system.defaultLocale", &sys.default_locale)),
    );
    // The Java getters normalise these two at read time.
    let panel_mode = ui.default_tool_panel_mode.to_ascii_lowercase();
    map.insert(
        "defaultToolPanelMode".into(),
        json!(if panel_mode == "fullscreen" {
            "fullscreen"
        } else {
            "sidebar"
        }),
    );
    let startup = ui.default_startup_view.to_ascii_lowercase();
    map.insert(
        "defaultStartupView".into(),
        json!(match startup.as_str() {
            "read" => "read",
            "automate" => "automate",
            _ => "tools",
        }),
    );
    map.insert(
        "defaultHideUnavailableTools".into(),
        json!(env_or_bool(
            "ui.defaultHideUnavailableTools",
            ui.default_hide_unavailable_tools
        )),
    );
    map.insert(
        "defaultHideUnavailableConversions".into(),
        json!(env_or_bool(
            "ui.defaultHideUnavailableConversions",
            ui.default_hide_unavailable_conversions
        )),
    );
    map.insert(
        "hideDisabledToolsGoogleDrive".into(),
        json!(env_or_bool(
            "ui.hideDisabledTools.googleDrive",
            ui.hide_disabled_tools.google_drive
        )),
    );
    map.insert(
        "hideDisabledToolsMobileQRScanner".into(),
        json!(env_or_bool(
            "ui.hideDisabledTools.mobileQRScanner",
            ui.hide_disabled_tools.mobile_qr_scanner
        )),
    );

    let gd_enabled = env_or_bool("premium.proFeatures.googleDrive.enabled", gd.enabled);
    if gd_enabled {
        map.insert("googleDriveEnabled".into(), json!(true));
        map.insert(
            "googleDriveClientId".into(),
            json!(env_or(
                "premium.proFeatures.googleDrive.clientId",
                &gd.client_id
            )),
        );
        map.insert(
            "googleDriveApiKey".into(),
            json!(env_or(
                "premium.proFeatures.googleDrive.apiKey",
                &gd.api_key
            )),
        );
        map.insert(
            "googleDriveAppId".into(),
            json!(env_or("premium.proFeatures.googleDrive.appId", &gd.app_id)),
        );
    }

    map.insert("enableLogin".into(), json!(enable_login));
    map.insert(
        "honorOwnerPasswords".into(),
        json!(env_or_bool(
            "security.honorOwnerPasswords",
            sec.honor_owner_passwords
        )),
    );
    map.insert(
        "showSettingsWhenNoLogin".into(),
        json!(env_or_bool(
            "system.showSettingsWhenNoLogin",
            sys.show_settings_when_no_login
        )),
    );
    map.insert(
        "enableOAuth".into(),
        json!(env_or_bool("security.oauth2.enabled", sec.oauth2.enabled)),
    );
    map.insert(
        "enableSaml".into(),
        json!(env_or_bool("security.saml2.enabled", sec.saml2.enabled)),
    );
    map.insert(
        "enableEmailInvites".into(),
        json!(
            env_or_bool("mail.enabled", mail.enabled)
                && env_or_bool("mail.enableInvites", mail.enable_invites)
        ),
    );

    map.insert("storageEnabled".into(), json!(storage_enabled));
    map.insert("storageSharingEnabled".into(), json!(sharing_enabled));
    map.insert(
        "storageShareLinksEnabled".into(),
        json!(share_links_enabled),
    );
    map.insert(
        "storageShareEmailEnabled".into(),
        json!(share_email_enabled),
    );
    map.insert(
        "storageGroupSigningEnabled".into(),
        json!(storage_enabled && env_or_bool("storage.signing.enabled", storage.signing.enabled)),
    );

    // `userService` null → isAdmin/isNewUser stay false.
    map.insert("isAdmin".into(), json!(false));

    // `InitialSetup.isNewServer`: blank/absent/`0.0.0` appVersion → true.
    let app_version = env_prop("automaticallyGenerated.appVersion")
        .or_else(|| settings.automatically_generated.app_version.clone());
    map.insert(
        "isNewServer".into(),
        json!(app_version
            .as_deref()
            .map(str::trim)
            .is_none_or(|v| v.is_empty() || v == "0.0.0")),
    );
    map.insert("isNewUser".into(), json!(false));

    map.insert(
        "enableAlphaFunctionality".into(),
        json!(env_or_bool(
            "system.enableAlphaFunctionality",
            sys.enable_alpha_functionality
        )),
    );
    map.insert(
        "stirlingOfficeConversion".into(),
        json!(env_or_bool(
            "system.stirlingOfficeConversion",
            sys.stirling_office_conversion
        )),
    );
    map.insert("shouldShowUpdate".into(), json!(should_show_update));
    // Same effective value `footer-info` reports (env → runtime write → file).
    let analytics = env_bool("SYSTEM_ENABLEANALYTICS")
        .or_else(analytics_override)
        .or(sys.enable_analytics);
    map.insert("enableAnalytics".into(), json!(analytics));
    map.insert(
        "enablePosthog".into(),
        json!(env_prop_bool("system.enablePosthog").or(sys.enable_posthog)),
    );
    map.insert(
        "enableScarf".into(),
        json!(env_prop_bool("system.enableScarf").or(sys.enable_scarf)),
    );
    map.insert(
        "enableDesktopInstallSlide".into(),
        json!(
            env_prop_bool("system.enableDesktopInstallSlide").or(sys.enable_desktop_install_slide)
        ),
    );
    map.insert(
        "enableEasterEggs".into(),
        json!(env_or_bool(
            "system.enableEasterEggs",
            sys.enable_easter_eggs
        )),
    );

    // No license service and the `@Profile("default")` beans report false /
    // false / "NORMAL" — the OSS answers.
    map.insert("premiumEnabled".into(), json!(false));
    map.insert(
        "accountLinkAvailable".into(),
        json!(env_prop_bool("stirling.billing.account-link.enabled").unwrap_or(false)),
    );

    map.insert(
        "aiEngineEnabled".into(),
        json!(env_or_bool("aiEngine.enabled", ai.enabled)),
    );
    let f = &ai.features;
    map.insert(
        "aiFeatures".into(),
        json!({
            "chat": f.chat,
            "documentQuestions": f.document_questions,
            "createPdf": f.create_pdf,
            "mathAuditor": f.math_auditor,
            "pdfComment": f.pdf_comment,
            "classify": f.classify,
        }),
    );

    let ts = &sec.timestamp;
    map.insert(
        "timestampDefaultTsaUrl".into(),
        json!(env_or(
            "security.timestamp.defaultTsaUrl",
            &ts.default_tsa_url
        )),
    );
    map.insert(
        "timestampCustomTsaUrls".into(),
        json!(ts.custom_tsa_urls.as_slice()),
    );
    map.insert(
        "timestampTsaPresets".into(),
        json!(TSA_PRESETS
            .iter()
            .map(|(label, url)| json!({ "label": label, "url": url }))
            .collect::<Vec<_>>()),
    );

    // `serverCertificateService` is proprietary-only → always false.
    map.insert("serverCertificateEnabled".into(), json!(false));
    let mt = machine_type();
    // Java reads the `STIRLING_PDF_TAURI_MODE` system property with
    // `Boolean.parseBoolean` (only "true" counts); env is the equivalent
    // signal available to Rust.
    let hardware_signing = std::env::var("STIRLING_PDF_TAURI_MODE")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        || mt.starts_with("Client-");
    map.insert("hardwareSigningAvailable".into(), json!(hardware_signing));

    let legal = &settings.legal;
    map.insert(
        "termsAndConditions".into(),
        json!(legal.terms_and_conditions.as_str()),
    );
    map.insert("privacyPolicy".into(), json!(legal.privacy_policy.as_str()));
    map.insert("cookiePolicy".into(), json!(legal.cookie_policy.as_str()));
    map.insert("impressum".into(), json!(legal.impressum.as_str()));
    map.insert(
        "accessibilityStatement".into(),
        json!(legal.accessibility_statement.as_str()),
    );

    // `@Profile("default")` beans: licenseService is absent so these report
    // the OSS constants.
    map.insert("runningProOrHigher".into(), json!(false));
    map.insert("runningEE".into(), json!(false));
    map.insert("license".into(), json!("NORMAL"));
    // `SSOAutoLogin` bean lives in :proprietary → omitted in OSS.

    map.insert("appVersion".into(), json!(env!("CARGO_PKG_VERSION")));
    map.insert("machineType".into(), json!(mt));
    // `activeSecurity` = proprietary SecurityConfiguration on the classpath
    // → always false for Rust.
    map.insert("activeSecurity".into(), json!(false));

    Json(Value::Object(map))
}

trait PipeNonBlank {
    fn pipe_non_blank(self, fallback: &str) -> String;
}

impl PipeNonBlank for String {
    fn pipe_non_blank(self, fallback: &str) -> String {
        if self.trim().is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/v1/settings/get-endpoints-status
// ---------------------------------------------------------------------------

pub async fn get_endpoints_status() -> Json<Value> {
    let mut map = Map::new();
    for (endpoint, enabled) in endpointconfig::disabled_endpoint_statuses() {
        map.insert(endpoint, json!(enabled));
    }
    Json(Value::Object(map))
}

// ---------------------------------------------------------------------------
// POST /api/v1/settings/update-enable-analytics
// ---------------------------------------------------------------------------

/// Java's `ApplicationProperties` bean is mutable after startup; Rust's
/// `Settings` is frozen, so the endpoint records the written value here and
/// `ui_data_footer_info` prefers it — preserving the "set once, then stable"
/// semantics the 208 guard relies on.
static ANALYTICS_OVERRIDE: AtomicI8 = AtomicI8::new(-1);

fn analytics_override() -> Option<bool> {
    match ANALYTICS_OVERRIDE.load(Ordering::Relaxed) {
        1 => Some(true),
        0 => Some(false),
        _ => None,
    }
}

/// Serialises the Java `SETTINGS_WRITE_LOCK`: the read-modify-write of
/// `settings.yml` must not interleave with another writer.
static SETTINGS_WRITE_LOCK: Mutex<()> = Mutex::new(());

fn settings_yml_path() -> PathBuf {
    settings::base_dir().join("configs").join("settings.yml")
}

pub async fn update_enable_analytics(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    let enabled = match query.get("enabled").map(String::as_str) {
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => true,
            "false" | "off" | "no" | "0" => false,
            _ => return Err(AppError::BadRequest("Invalid value for 'enabled'".into())),
        },
        None => return Err(AppError::BadRequest("Missing 'enabled' parameter".into())),
    };

    // `@AutoJobPostMapping`: `?async=true` runs the write as a job.
    if crate::handlers::jobs::async_requested(&query, &[]) {
        return crate::handlers::jobs::submit(&state, async move {
            tokio::task::spawn_blocking(move || update_analytics_work(enabled))
                .await
                .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
        })
        .await;
    }
    tokio::task::spawn_blocking(move || update_analytics_work(enabled))
        .await
        .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
}

fn update_analytics_work(enabled: bool) -> AppResult<Response> {
    let settings = settings::current();
    // The Java bean already merges env-bound values, so a configured
    // `SYSTEM_ENABLEANALYTICS` also counts as "already set".
    if env_bool("SYSTEM_ENABLEANALYTICS")
        .or(settings.system.enable_analytics)
        .is_some()
    {
        return Ok((
            StatusCode::ALREADY_REPORTED,
            Json(json!({
                "message": format!(
                    "Setting has already been set, To adjust please edit {}",
                    settings_yml_path().display()
                )
            })),
        )
            .into_response());
    }
    write_enable_analytics(&settings_yml_path(), enabled)
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
    ANALYTICS_OVERRIDE.store(if enabled { 1 } else { 0 }, Ordering::Relaxed);
    Ok(Json(json!({ "message": "Updated" })).into_response())
}

/// Sets `system.enableAnalytics` in `settings.yml` while leaving the rest of
/// the file — including every comment — byte-identical. `YamlHelper` achieves
/// the same via snakeyaml's comment-preserving events; a full YAML
/// round-trip in Rust would strip them, hence the surgical edit.
fn write_enable_analytics(path: &Path, enabled: bool) -> std::io::Result<()> {
    write_yaml_scalar(
        path,
        "system",
        "enableAnalytics",
        if enabled { "true" } else { "false" },
    )
}

/// `InitialSetup.initSetAppVersion` equivalent: Java rewrites
/// `AutomaticallyGenerated.appVersion` on every boot and derives
/// `isNewServer` from the pre-write value. Rust only fills the key when it
/// is absent/blank/`0.0.0` — overwriting unconditionally would clobber the
/// real Stirling version a Java backend wrote into a shared `settings.yml`.
pub fn ensure_app_version() {
    let settings = settings::current();
    let current = env_prop("automaticallyGenerated.appVersion")
        .or_else(|| settings.automatically_generated.app_version.clone());
    if current
        .as_deref()
        .map(str::trim)
        .is_some_and(|v| !v.is_empty() && v != "0.0.0")
    {
        return;
    }
    let path = settings_yml_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = upsert_yaml_key(
        &text,
        "AutomaticallyGenerated",
        "appVersion",
        env!("CARGO_PKG_VERSION"),
    );
    if updated == text {
        return;
    }
    let _guard = SETTINGS_WRITE_LOCK.lock().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, updated);
}

fn write_yaml_scalar(path: &Path, section: &str, key: &str, value: &str) -> std::io::Result<()> {
    let _guard = SETTINGS_WRITE_LOCK.lock().unwrap();
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let updated = upsert_yaml_key(&text, section, key, value);
    if updated != text {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, updated)?;
    }
    Ok(())
}

fn upsert_yaml_key(text: &str, section: &str, key: &str, value: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();

    let mut in_section = false;
    let mut child_indent = 0usize;
    let mut saw_child = false;
    let mut insert_line: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if indent == 0 {
            if is_yaml_key_line(trimmed, section) {
                in_section = true;
                saw_child = false;
                insert_line = None;
            } else if in_section {
                // First top-level key after the section ends its block; the
                // new key belongs at the end of the block.
                insert_line = Some(i);
                in_section = false;
            }
            continue;
        }
        if !in_section {
            continue;
        }
        if !saw_child {
            child_indent = indent;
            saw_child = true;
        }
        if indent == child_indent && is_yaml_key_line(trimmed, key) {
            let mut out: Vec<String> = lines[..i].iter().map(|l| l.to_string()).collect();
            out.push(rewrite_scalar(line, value));
            out.extend(lines[i + 1..].iter().map(|l| l.to_string()));
            return join_lines(&out, text);
        }
    }

    // Key absent: append inside the section block (before the next
    // top-level key) or create the block at EOF.
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    match insert_line {
        Some(at) => out.insert(
            at,
            format!("{}{}: {}", " ".repeat(child_indent.max(2)), key, value),
        ),
        None if in_section || saw_section_block(&lines, section) => out.push(format!(
            "{}{}: {}",
            " ".repeat(child_indent.max(2)),
            key,
            value
        )),
        None => {
            if !out.is_empty() && !out.last().is_some_and(|l| l.is_empty()) {
                out.push(String::new());
            }
            out.push(format!("{}:", section));
            out.push(format!("  {}: {}", key, value));
        }
    }
    join_lines(&out, text)
}

fn saw_section_block(lines: &[&str], section: &str) -> bool {
    lines.iter().any(|l| {
        let t = l.trim_start();
        !t.is_empty() && !t.starts_with('#') && is_yaml_key_line(t, section)
    })
}

/// `key:` or `key :` at the start of an already-trimmed line.
fn is_yaml_key_line(trimmed: &str, key: &str) -> bool {
    trimmed
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with(':'))
}

/// Replaces the scalar after the first `:` while keeping the trailing
/// `# comment` intact: `  enableAnalytics: null # note` →
/// `  enableAnalytics: true # note`.
fn rewrite_scalar(line: &str, value: &str) -> String {
    let Some(colon) = line.find(':') else {
        return line.to_string();
    };
    let rest = &line[colon + 1..];
    // A `#` starts a comment only at a word boundary; values here are
    // `null`/`true`/`false`, so the first `#` is always the comment.
    let comment = rest.find('#').map(|i| rest[i..].trim_end().to_string());
    let indent_len = line.len() - line.trim_start().len();
    let key = line[indent_len..colon].trim_end();
    match comment {
        Some(c) => format!("{}{}: {} {}", &line[..indent_len], key, value, c),
        None => format!("{}{}: {}", &line[..indent_len], key, value),
    }
}

fn join_lines(lines: &[String], original: &str) -> String {
    let mut out = lines.join("\n");
    if original.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_validation_matches_java_pattern() {
        for ok in ["en", "en-GB", "fr-FR", "zh-Hant", "pt-BR", "abc", "en_US"] {
            assert!(is_valid_locale(ok), "{ok} should be valid");
        }
        for bad in [
            "",
            "e",
            "en-",
            "-en",
            "en--GB",
            "en_GB_x",
            "../etc",
            "en.md",
            "en US",
            "123",
            "en1",
            &"a".repeat(36),
        ] {
            assert!(!is_valid_locale(bad), "{bad} should be invalid");
        }
    }

    #[test]
    fn locale_candidates_dedupe_and_add_base() {
        let mut out = Vec::new();
        add_locale_candidates(&mut out, Some("fr-FR"));
        add_locale_candidates(&mut out, Some("fr"));
        add_locale_candidates(&mut out, Some("de"));
        assert_eq!(out, vec!["fr-FR", "fr", "de"]);
        // Invalid locales contribute nothing.
        let mut out = Vec::new();
        add_locale_candidates(&mut out, Some("../../x"));
        assert!(out.is_empty());
    }

    #[test]
    fn resolve_disclaimer_prefers_requested_then_default_then_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fr.md"), "# french").unwrap();
        std::fs::write(dir.path().join("en.md"), "# english").unwrap();

        // Requested locale wins over the configured default.
        assert_eq!(
            resolve_disclaimer(dir.path(), Some("fr-FR"), "en", "fb"),
            "# french"
        );
        // Missing requested locale falls back to the default locale chain.
        assert_eq!(
            resolve_disclaimer(dir.path(), Some("de"), "en-US", "fb"),
            "# english"
        );
        // Nothing on disk → fallbackText.
        assert_eq!(
            resolve_disclaimer(dir.path(), Some("de"), "it", "fallback md"),
            "fallback md"
        );
        // Blank file content is skipped like Java's isBlank check.
        std::fs::write(dir.path().join("de.md"), "   \n").unwrap();
        assert_eq!(resolve_disclaimer(dir.path(), Some("de"), "it", "fb"), "fb");
    }

    #[test]
    fn disclaimer_file_rejects_symlinks_and_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.md");
        std::fs::write(&target, "secret").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, dir.path().join("en.md")).unwrap();
            assert!(read_disclaimer_file(dir.path(), "en").is_none());
        }
        std::fs::write(
            dir.path().join("big.md"),
            vec![b'x'; (256 * 1024 + 1) as usize],
        )
        .unwrap();
        assert!(read_disclaimer_file(dir.path(), "big").is_none());
    }

    #[test]
    fn upsert_replaces_value_and_keeps_comment() {
        let text = "system:\n  enableAnalytics: null # Master toggle\n  other: 1\n";
        assert_eq!(
            upsert_yaml_key(text, "system", "enableAnalytics", "true"),
            "system:\n  enableAnalytics: true # Master toggle\n  other: 1\n"
        );
    }

    #[test]
    fn upsert_inserts_missing_key_inside_system_block() {
        let text = "system:\n  corsAllowedOrigins: []\nendpoints:\n  toRemove: []\n";
        assert_eq!(
            upsert_yaml_key(text, "system", "enableAnalytics", "false"),
            "system:\n  corsAllowedOrigins: []\n  enableAnalytics: false\nendpoints:\n  toRemove: []\n"
        );
    }

    #[test]
    fn upsert_appends_system_block_when_absent() {
        let text = "metrics:\n  enabled: true\n";
        let out = upsert_yaml_key(text, "system", "enableAnalytics", "true");
        assert!(out.contains("system:\n  enableAnalytics: true"));
    }

    #[test]
    fn upsert_writes_into_existing_generated_section() {
        let text = "AutomaticallyGenerated:\n  key: abc\n  appVersion: 0.0.0\n";
        let out = upsert_yaml_key(text, "AutomaticallyGenerated", "appVersion", "1.2.3");
        assert_eq!(
            out,
            "AutomaticallyGenerated:\n  key: abc\n  appVersion: 1.2.3\n"
        );
    }

    #[test]
    fn java_split_drops_only_trailing_empties() {
        assert_eq!(java_split_commas("a,b,"), vec!["a", "b"]);
        assert_eq!(java_split_commas("a,,b"), vec!["a", "", "b"]);
        assert_eq!(java_split_commas(""), vec![""]);
    }
}
