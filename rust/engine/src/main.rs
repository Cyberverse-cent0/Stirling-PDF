use std::net::SocketAddr;
use std::time::Duration;

use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use stirling_engine::config::AppSettings;
use stirling_engine::config_cache;
use stirling_engine::config_push::resolve_and_apply;
use stirling_engine::contracts::config::ConfigPushRequest;
use stirling_engine::middleware;
use stirling_engine::routes;
use stirling_engine::state::{AppState, PushCredentials};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env_settings = AppSettings::load()?;

    let filter =
        EnvFilter::try_new(&env_settings.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if env_settings.engine_shared_secret.is_empty() && env_settings.engine_require_auth {
        tracing::error!(
            "STIRLING_ENGINE_REQUIRE_AUTH is enabled but STIRLING_ENGINE_SHARED_SECRET is not \
             set - the engine will REFUSE every non-public request (HTTP 503, fail-closed)"
        );
    } else if env_settings.engine_shared_secret.is_empty() {
        tracing::warn!(
            "STIRLING_ENGINE_SHARED_SECRET not set - engine shared-secret enforcement is \
             DISABLED. Set this secret in any deployment that exposes the engine beyond localhost."
        );
    }

    // Precedence: env < persisted cache < live push. The stamp is claimed
    // before restore so a push landing mid-boot is re-adopted by the watcher
    // rather than mistaken for the config we just restored.
    let mut settings = env_settings;
    let mut chat: Option<PushCredentials> = None;
    let mut embedder: Option<(String, PushCredentials)> = None;
    let boot_stamp = config_cache::cache_stamp();
    if settings.allow_config_push {
        if let Some(cached) = config_cache::load_config(&settings) {
            match restore_cached(&settings, cached) {
                Ok((effective, c, e, notes)) => {
                    settings = effective;
                    chat = c;
                    embedder = e;
                    tracing::info!(
                        smart_model = %settings.smart_model_name,
                        fast_model = %settings.fast_model_name,
                        notes = ?notes,
                        "Restored cached AI config"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Cached AI config could not be applied; falling back to env settings: {e}"
                    );
                }
            }
        }
    }

    let state = AppState::with_push(settings.clone(), chat, embedder)?;
    *state.config_stamp.lock().unwrap() = boot_stamp;
    stirling_engine::services::tracking::init(&settings);

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

    tokio::spawn(document_reaper(state.clone()));
    if settings.allow_config_push {
        tokio::spawn(config_watcher(state.clone()));
    }

    let addr = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(5001);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", addr)).await?;
    tracing::info!(addr = %format!("0.0.0.0:{addr}"), "stirling-engine listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Resolve a cached push into effective settings plus the credentials the
/// model/embedder builders need. Model construction is deferred to
/// `AppState::with_push`, which surfaces provider errors at boot the same
/// way Python's `build_app_state` does.
fn restore_cached(
    current: &AppSettings,
    cached: ConfigPushRequest,
) -> anyhow::Result<(
    AppSettings,
    Option<PushCredentials>,
    Option<(String, PushCredentials)>,
    Vec<String>,
)> {
    let resolved = resolve_and_apply(current, &cached)?;
    Ok((
        resolved.settings,
        resolved.chat,
        resolved.embedder,
        resolved.notes,
    ))
}

/// Periodic expiry sweep over the document store — Python's
/// `_run_expired_doc_reaper`. A failed iteration is logged and retried on
/// the next tick rather than killing the loop.
async fn document_reaper(state: AppState) {
    let interval = Duration::from_secs(
        state
            .runtime
            .load()
            .settings
            .documents_reaper_interval_seconds
            .max(1),
    );
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let documents = state.runtime.load().documents.clone();
        match documents.reap_expired().await {
            Ok(deleted) if deleted > 0 => {
                tracing::info!(deleted, "Reaped expired document collection(s)")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("Document reaper iteration failed; will retry: {e}"),
        }
    }
}

/// Poll the shared config cache so every worker converges on the last push —
/// a push reaches only one process; this watcher is how the rest adopt it.
async fn config_watcher(state: AppState) {
    let interval = Duration::from_secs(
        state
            .runtime
            .load()
            .settings
            .config_cache_poll_interval_seconds
            .max(1) as u64,
    );
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // first tick is immediate — skip it
    loop {
        ticker.tick().await;
        adopt_cached_config_if_changed(&state).await;
    }
}

async fn adopt_cached_config_if_changed(state: &AppState) {
    // Read the stamp before the payload: a write landing between the two
    // re-applies next tick, whereas the reverse order would record the newer
    // stamp and skip forever.
    let stamp = config_cache::cache_stamp();
    let claimed = {
        let mut guard = state.config_stamp.lock().unwrap();
        if stamp.is_none() || stamp == *guard {
            return;
        }
        // Claim up front so a cache we cannot read or apply is not retried
        // every tick.
        *guard = stamp;
        stamp
    };
    let _ = claimed;
    let cached = {
        let settings = state.runtime.load().settings.clone();
        config_cache::load_config(&settings)
    };
    let Some(cached) = cached else { return };
    let current = state.runtime.load().settings.clone();
    let Ok(resolved) = resolve_and_apply(&current, &cached) else {
        tracing::warn!("Config pushed to another worker could not be applied here");
        return;
    };
    let embedder = resolved.embedder.clone();
    if let Err(e) = state
        .apply_push(
            resolved.settings,
            resolved.chat.as_ref(),
            embedder.as_ref().map(|(m, c)| (m.as_str(), c)),
        )
        .await
    {
        tracing::warn!("Config pushed to another worker could not be applied here: {e}");
        return;
    }
    tracing::info!(
        smart_model = %state.runtime.load().settings.smart_model_name,
        fast_model = %state.runtime.load().settings.fast_model_name,
        "Adopted AI config pushed to another worker"
    );
}
