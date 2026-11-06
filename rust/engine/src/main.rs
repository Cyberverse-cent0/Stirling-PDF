use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use stirling_engine::config::AppSettings;
use stirling_engine::middleware;
use stirling_engine::routes;
use stirling_engine::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = AppSettings::load()?;

    let filter = EnvFilter::try_new(&settings.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if settings.engine_shared_secret.is_empty() && settings.engine_require_auth {
        tracing::error!(
            "STIRLING_ENGINE_REQUIRE_AUTH is enabled but STIRLING_ENGINE_SHARED_SECRET is not \
             set - the engine will REFUSE every non-public request (HTTP 503, fail-closed)"
        );
    } else if settings.engine_shared_secret.is_empty() {
        tracing::warn!(
            "STIRLING_ENGINE_SHARED_SECRET not set - engine shared-secret enforcement is \
             DISABLED. Set this secret in any deployment that exposes the engine beyond localhost."
        );
    }

    let state = AppState::new(settings)?;
    // Layer order (outermost last): Trace -> Cors -> engine_auth -> user_id ->
    // per-router user-id gate -> handler. Matches Python's middleware stack.
    let app = routes::router(state.clone())
        .layer(axum::middleware::from_fn(middleware::user_id))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::engine_auth,
        ))
        .layer(
            CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::PUT,
                    axum::http::Method::DELETE,
                    axum::http::Method::OPTIONS,
                ])
                .allow_headers(tower_http::cors::Any),
        )
        .layer(TraceLayer::new_for_http());

    let addr = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(5001);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", addr)).await?;
    tracing::info!(addr = %format!("0.0.0.0:{addr}"), "stirling-engine listening");
    axum::serve(listener, app).await?;
    Ok(())
}
