#[cfg(test)]
mod apitests;
mod attachments;
mod auth;
mod certsign;
mod config;
mod convert;
mod endpointconfig;
mod error;
mod fontmetrics;
mod fonts;
mod forms;
mod guard;
mod handlers;
mod image_split;
mod imgcomp;
mod imgextract;
mod jobstore;
mod metrics;
mod page_select;
mod pdfdoc;
mod pdfua;
// Port of jpdfium's text model + the proprietary pdf package; not yet
// routed to a handler, so its public surface is intentionally unused.
#[allow(dead_code)]
mod pdftext;
mod pipeline_table;
mod settings;
mod signature;
mod tabula;
#[cfg(test)]
mod testkit;
mod text;
mod timestamp;
mod upload;
mod verapdf;
mod web;

use axum::http::Method;
use std::time::Duration;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer, ExposeHeaders};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

/// Java's `WebMvcConfig.addCorsMappings` header/method lists, mirrored so
/// browser clients see identical CORS behaviour on Rust-owned routes.
const CORS_ALLOWED_HEADERS: &[&str] = &[
    "authorization",
    "content-type",
    "x-requested-with",
    "accept",
    "origin",
    "x-api-key",
    "x-csrf-token",
    "x-xsrf-token",
    "x-browser-id",
];

const CORS_EXPOSED_HEADERS: &[&str] = &[
    "www-authenticate",
    "x-total-count",
    "x-page-number",
    "x-page-size",
    "content-disposition",
    "content-type",
    "x-stirling-skipped-field-edits",
    "x-stirling-skipped-field-edits-total",
    "x-stirling-detected-fields",
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // Bundled engine payloads keep their shared-library closure in
    // <engines>/lib; children inherit LD_LIBRARY_PATH so gs/qpdf resolve
    // libgs & friends without system installs. Prepend: a user-set path
    // still applies after the bundled dir.
    if let Some(lib) = convert::engines_lib_dir() {
        let mut paths = vec![lib];
        if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            std::env::set_var("LD_LIBRARY_PATH", joined);
        }
    }

    let config = config::Config::from_env()?;
    upload::configure_max_part_bytes(config.max_upload_bytes);
    endpointconfig::init(&config.settings);
    settings::init(&config.settings);
    handlers::platform::ensure_app_version();
    metrics::init(config.metrics_enabled);
    auth::warn_if_secured_deployment();
    let state = handlers::jobs::AppState::persistent();
    let cors_patterns = config::cors_origin_patterns(&config);
    let app = handlers::router(state)
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::predicate(move |origin, _| {
                    origin
                        .to_str()
                        .map(|o| config::cors_origin_allowed(&cors_patterns, o))
                        .unwrap_or(false)
                }))
                .allow_methods(AllowMethods::list([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                    Method::OPTIONS,
                ]))
                .allow_headers(AllowHeaders::list(
                    CORS_ALLOWED_HEADERS
                        .iter()
                        .copied()
                        .map(axum::http::HeaderName::from_static),
                ))
                .expose_headers(ExposeHeaders::list(
                    CORS_EXPOSED_HEADERS
                        .iter()
                        .copied()
                        .map(axum::http::HeaderName::from_static),
                ))
                .allow_credentials(true)
                .max_age(Duration::from_secs(3600)),
        )
        .layer(TraceLayer::new_for_http())
        // Only text-like payloads earn the CPU: PDFs, images, ZIPs and
        // other binary containers are already dense.
        .layer(CompressionLayer::new().compress_when(
            |_status: axum::http::StatusCode,
             _version: axum::http::Version,
             headers: &axum::http::HeaderMap,
             _extensions: &axum::http::Extensions| {
                headers
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|ct| {
                        ct.starts_with("text/")
                            || ct.contains("json")
                            || ct.contains("xml")
                            || ct.contains("javascript")
                            || ct.contains("svg")
                    })
            },
        ));

    let listener = tokio::net::TcpListener::bind(config.bind_addr()).await?;
    if let Ok(addr) = listener.local_addr() {
        handlers::platform::note_bound_addr(addr);
        // The Tauri sidecar monitor parses Java's "running on port: N"
        // banner to learn the ephemeral PORT=0 assignment; emit the same
        // line with the port the OS actually granted.
        println!("Stirling-PDF running on port: {}", addr.port());
    }
    tracing::info!(addr = %config.bind_addr(), "stirling-server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    // The desktop shell can't signal a sidecar portably, so it drops a
    // sentinel file (STIRLING_PDF_SHUTDOWN_FILE) instead — same contract
    // the Java backend honours.
    let shutdown_file = async {
        let Some(path) = std::env::var_os("STIRLING_PDF_SHUTDOWN_FILE") else {
            return std::future::pending::<()>().await;
        };
        let path = std::path::PathBuf::from(path);
        loop {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
        _ = shutdown_file => {},
    }
    tracing::info!("shutdown signal received");
}
