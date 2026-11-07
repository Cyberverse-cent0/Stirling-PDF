use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

/// Root of the Rust engine crate; `.env` files and the default data dir live here.
pub const ENGINE_ROOT: &str = env!("CARGO_MANIFEST_DIR");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentsBackend {
    Sqlite,
    Pgvector,
}

impl FromStr for DocumentsBackend {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "sqlite" => Ok(Self::Sqlite),
            "pgvector" => Ok(Self::Pgvector),
            other => Err(format!("unknown STIRLING_DOCUMENTS_BACKEND {other:?}")),
        }
    }
}

/// Engine configuration, one field per `STIRLING_*` env var the Python
/// `AppSettings` owns. Field names mirror `engine/src/stirling/config/settings.py`;
/// env names must stay identical so deployments can switch runtimes freely.
#[derive(Debug, Clone)]
pub struct AppSettings {
    pub smart_model_name: String,
    pub fast_model_name: String,
    pub chat_provider: String,
    pub smart_model_max_tokens: Option<u32>,
    pub fast_model_max_tokens: Option<u32>,
    pub model_max_concurrency: usize,

    pub documents_backend: DocumentsBackend,
    pub documents_sqlite_path: PathBuf,
    pub documents_pgvector_dsn: String,
    pub documents_pgvector_pool_min_size: u32,
    pub documents_pgvector_pool_max_size: u32,

    pub rag_embedding_model: String,
    pub rag_chunk_size: usize,
    pub rag_chunk_overlap: usize,
    pub rag_default_top_k: usize,
    pub rag_max_searches: usize,

    pub planner_shortlist_size: usize,
    pub documents_reaper_interval_seconds: u64,

    pub chunked_reasoner_chars_per_slice: usize,
    pub chunked_reasoner_concurrency: usize,
    pub chunked_reasoner_worker_timeout_seconds: f64,
    pub chunked_reasoner_notes_char_budget: usize,

    pub contradiction_detect_concurrency: usize,
    pub contradiction_bucket_chunk_size: usize,
    pub contradiction_bucket_chunk_overlap: usize,
    pub contradiction_canonicaliser_batch_size: usize,

    pub max_pages: usize,
    pub max_characters: usize,

    pub require_user_id: bool,

    pub log_level: String,
    pub log_file: String,
    pub http_debug: bool,

    pub posthog_enabled: bool,
    pub posthog_api_key: String,
    pub posthog_host: String,

    pub engine_shared_secret: String,
    pub engine_require_auth: bool,

    pub allow_config_push: bool,
    pub config_cache_poll_interval_seconds: u64,
}

fn env_str(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn env_req(name: &str) -> anyhow::Result<String> {
    env_str(name).ok_or_else(|| anyhow::anyhow!("missing required env var {name}"))
}

fn env_parse<T: FromStr>(name: &str, default: T) -> anyhow::Result<T>
where
    T::Err: std::fmt::Display,
{
    match env_str(name) {
        Some(raw) => raw
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("invalid {name}={raw:?}: {e}")),
        None => Ok(default),
    }
}

fn env_bool(name: &str, default: bool) -> anyhow::Result<bool> {
    match env_str(name) {
        Some(raw) => match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => Err(anyhow::anyhow!("invalid boolean {name}={other:?}")),
        },
        None => Ok(default),
    }
}

fn env_opt_u32(name: &str) -> anyhow::Result<Option<u32>> {
    match env_str(name) {
        Some(raw) => raw
            .parse::<u32>()
            .map(Some)
            .map_err(|e| anyhow::anyhow!("invalid {name}={raw:?}: {e}")),
        None => Ok(None),
    }
}

impl AppSettings {
    /// Load settings from the environment after applying `.env` then
    /// `.env.local` (local wins), matching pydantic-settings' file order.
    pub fn load() -> anyhow::Result<Self> {
        let root = PathBuf::from(ENGINE_ROOT);
        let _ = dotenvy::from_path(root.join(".env"));
        let _ = dotenvy::from_path(root.join(".env.local"));

        let sqlite_path = PathBuf::from(env_str("STIRLING_DOCUMENTS_SQLITE_PATH").unwrap_or_else(
            || {
                root.join("data")
                    .join("documents.db")
                    .to_string_lossy()
                    .into_owned()
            },
        ));
        let documents_sqlite_path = if sqlite_path.is_absolute() {
            sqlite_path
        } else {
            root.join(sqlite_path)
        };

        Ok(Self {
            smart_model_name: env_req("STIRLING_SMART_MODEL")?,
            fast_model_name: env_req("STIRLING_FAST_MODEL")?,
            chat_provider: env_str("STIRLING_CHAT_PROVIDER").unwrap_or_default(),
            smart_model_max_tokens: env_opt_u32("STIRLING_SMART_MODEL_MAX_TOKENS")?,
            fast_model_max_tokens: env_opt_u32("STIRLING_FAST_MODEL_MAX_TOKENS")?,
            model_max_concurrency: env_parse("STIRLING_MODEL_MAX_CONCURRENCY", 8usize)?,

            documents_backend: env_str("STIRLING_DOCUMENTS_BACKEND")
                .unwrap_or_else(|| "sqlite".to_string())
                .parse::<DocumentsBackend>()
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            documents_sqlite_path,
            documents_pgvector_dsn: env_str("STIRLING_DOCUMENTS_PGVECTOR_DSN").unwrap_or_default(),
            documents_pgvector_pool_min_size: env_parse(
                "STIRLING_DOCUMENTS_PGVECTOR_POOL_MIN_SIZE",
                1u32,
            )?,
            documents_pgvector_pool_max_size: env_parse(
                "STIRLING_DOCUMENTS_PGVECTOR_POOL_MAX_SIZE",
                5u32,
            )?,

            rag_embedding_model: env_req("STIRLING_RAG_EMBEDDING_MODEL")?,
            rag_chunk_size: env_parse("STIRLING_RAG_CHUNK_SIZE", 512usize)?,
            rag_chunk_overlap: env_parse("STIRLING_RAG_CHUNK_OVERLAP", 64usize)?,
            rag_default_top_k: env_parse("STIRLING_RAG_TOP_K", 5usize)?,
            rag_max_searches: env_parse("STIRLING_RAG_MAX_SEARCHES", 3usize)?,

            planner_shortlist_size: env_parse("STIRLING_PLANNER_SHORTLIST_SIZE", 20usize)?,
            documents_reaper_interval_seconds: env_parse(
                "STIRLING_DOCUMENTS_REAPER_INTERVAL_SECONDS",
                900u64,
            )?,

            chunked_reasoner_chars_per_slice: env_parse(
                "STIRLING_CHUNKED_REASONER_CHARS_PER_SLICE",
                12000usize,
            )?,
            chunked_reasoner_concurrency: env_parse(
                "STIRLING_CHUNKED_REASONER_CONCURRENCY",
                4usize,
            )?,
            chunked_reasoner_worker_timeout_seconds: env_parse(
                "STIRLING_CHUNKED_REASONER_WORKER_TIMEOUT_SECONDS",
                120.0f64,
            )?,
            chunked_reasoner_notes_char_budget: env_parse(
                "STIRLING_CHUNKED_REASONER_NOTES_CHAR_BUDGET",
                400_000usize,
            )?,

            contradiction_detect_concurrency: env_parse(
                "STIRLING_CONTRADICTION_DETECT_CONCURRENCY",
                5usize,
            )?,
            contradiction_bucket_chunk_size: env_parse(
                "STIRLING_CONTRADICTION_BUCKET_CHUNK_SIZE",
                12usize,
            )?,
            contradiction_bucket_chunk_overlap: env_parse(
                "STIRLING_CONTRADICTION_BUCKET_CHUNK_OVERLAP",
                2usize,
            )?,
            contradiction_canonicaliser_batch_size: env_parse(
                "STIRLING_CONTRADICTION_CANONICALISER_BATCH_SIZE",
                500usize,
            )?,

            max_pages: env_parse("STIRLING_MAX_PAGES", 200usize)?,
            max_characters: env_parse("STIRLING_MAX_CHARACTERS", 200_000usize)?,

            require_user_id: env_bool("STIRLING_REQUIRE_USER_ID", false)?,

            log_level: env_str("STIRLING_LOG_LEVEL").unwrap_or_else(|| "INFO".to_string()),
            log_file: env_str("STIRLING_LOG_FILE").unwrap_or_default(),
            http_debug: env_bool("STIRLING_HTTP_DEBUG", false)?,

            posthog_enabled: env_bool("STIRLING_POSTHOG_ENABLED", false)?,
            posthog_api_key: env_str("STIRLING_POSTHOG_API_KEY").unwrap_or_default(),
            posthog_host: env_str("STIRLING_POSTHOG_HOST")
                .unwrap_or_else(|| "https://us.i.posthog.com".to_string()),

            engine_shared_secret: env_str("STIRLING_ENGINE_SHARED_SECRET").unwrap_or_default(),
            engine_require_auth: env_bool("STIRLING_ENGINE_REQUIRE_AUTH", false)?,

            allow_config_push: env_bool("STIRLING_ALLOW_CONFIG_PUSH", true)?,
            config_cache_poll_interval_seconds: env_parse(
                "STIRLING_CONFIG_CACHE_POLL_INTERVAL_SECONDS",
                15u64,
            )?,
        })
    }

    /// Test fixture mirroring `build_app_settings()` in the Python conftest:
    /// env-free settings with inert providers so `AppState::new` works offline.
    #[doc(hidden)]
    pub fn for_test() -> Self {
        Self {
            smart_model_name: "test".to_string(),
            fast_model_name: "test".to_string(),
            chat_provider: String::new(),
            smart_model_max_tokens: Some(8192),
            fast_model_max_tokens: Some(2048),
            model_max_concurrency: 32,
            documents_backend: DocumentsBackend::Sqlite,
            documents_sqlite_path: std::path::PathBuf::from(":memory:"),
            documents_pgvector_dsn: String::new(),
            documents_pgvector_pool_min_size: 1,
            documents_pgvector_pool_max_size: 10,
            rag_embedding_model: "voyageai:voyage-4".to_string(),
            rag_chunk_size: 512,
            rag_chunk_overlap: 64,
            rag_default_top_k: 5,
            rag_max_searches: 5,
            planner_shortlist_size: 8,
            documents_reaper_interval_seconds: 60,
            chunked_reasoner_chars_per_slice: 16_000,
            chunked_reasoner_concurrency: 10,
            chunked_reasoner_worker_timeout_seconds: 60.0,
            chunked_reasoner_notes_char_budget: 250_000,
            contradiction_detect_concurrency: 5,
            contradiction_bucket_chunk_size: 12,
            contradiction_bucket_chunk_overlap: 2,
            contradiction_canonicaliser_batch_size: 500,
            max_pages: 200,
            max_characters: 200_000,
            require_user_id: false,
            log_level: "info".to_string(),
            log_file: String::new(),
            http_debug: false,
            posthog_enabled: false,
            posthog_api_key: String::new(),
            posthog_host: "https://eu.i.posthog.com".to_string(),
            engine_shared_secret: String::new(),
            engine_require_auth: false,
            allow_config_push: true,
            config_cache_poll_interval_seconds: 15,
        }
    }
}

/// Test fixture mirroring `build_app_settings()` in the Python conftest:
/// env-free settings with inert providers so `AppState::new` works offline.
#[doc(hidden)]
pub fn for_test() -> AppSettings {
    AppSettings {
        smart_model_name: "test".to_string(),
        fast_model_name: "test".to_string(),
        chat_provider: String::new(),
        smart_model_max_tokens: Some(8192),
        fast_model_max_tokens: Some(2048),
        model_max_concurrency: 32,
        documents_backend: DocumentsBackend::Sqlite,
        documents_sqlite_path: std::path::PathBuf::from(":memory:"),
        documents_pgvector_dsn: String::new(),
        documents_pgvector_pool_min_size: 1,
        documents_pgvector_pool_max_size: 10,
        rag_embedding_model: "voyageai:voyage-4".to_string(),
        rag_chunk_size: 512,
        rag_chunk_overlap: 64,
        rag_default_top_k: 5,
        rag_max_searches: 5,
        planner_shortlist_size: 8,
        documents_reaper_interval_seconds: 60,
        chunked_reasoner_chars_per_slice: 16_000,
        chunked_reasoner_concurrency: 10,
        chunked_reasoner_worker_timeout_seconds: 60.0,
        chunked_reasoner_notes_char_budget: 250_000,
        contradiction_detect_concurrency: 5,
        contradiction_bucket_chunk_size: 12,
        contradiction_bucket_chunk_overlap: 2,
        contradiction_canonicaliser_batch_size: 500,
        max_pages: 200,
        max_characters: 200_000,
        require_user_id: false,
        log_level: "info".to_string(),
        log_file: String::new(),
        http_debug: false,
        posthog_enabled: false,
        posthog_api_key: String::new(),
        posthog_host: "https://eu.i.posthog.com".to_string(),
        engine_shared_secret: String::new(),
        engine_require_auth: false,
        allow_config_push: true,
        config_cache_poll_interval_seconds: 15,
    }
}

pub type SharedSettings = Arc<AppSettings>;
