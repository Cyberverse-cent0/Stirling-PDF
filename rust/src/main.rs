mod config;
mod error;
mod handlers;
mod page_select;
mod pdfdoc;
mod upload;

use axum::http::Method;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = config::Config::from_env()?;
    let state = handlers::jobs::AppState::new(config::JavaBackend::from_env());
    if state.backend.is_none() {
        tracing::warn!("JAVA_BACKEND_URL disabled; unmigrated /api routes will 404");
    }
    let app = handlers::router(state)
        .layer(
            CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers(tower_http::cors::Any),
        )
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(config.bind_addr()).await?;
    tracing::info!(addr = %config.bind_addr(), "stirling-server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
