//! Config-push resolution. Port of `resolve_and_apply` in
//! `stirling.api.routes.config`: merge a pushed `ConfigPushRequest` over the
//! running `AppSettings` and compute the explicit provider credentials the
//! model/embedder builders need. Never swaps live state — the caller does.

use crate::config::AppSettings;
use crate::contracts::config::ConfigPushRequest;
use crate::state::PushCredentials;

const REINDEX_NOTE: &str = "Embedding model changed; existing indexed documents were embedded with the previous model and must be re-indexed. If the embedding dimensionality changed, re-ingest before searching.";

pub struct ResolvedPush {
    pub settings: AppSettings,
    /// Explicit chat credentials; `None` when the push carried no provider
    /// fields (fully env-driven).
    pub chat: Option<PushCredentials>,
    /// `(bare_model, credentials)` when the push touched embedding fields.
    pub embedder: Option<(String, PushCredentials)>,
    pub notes: Vec<String>,
}

fn strip_provider_prefix(model_name: &str) -> String {
    match model_name.split_once(':') {
        Some((_, rest)) => rest.to_string(),
        None => model_name.to_string(),
    }
}

fn split_embedding_ref(reference: &str) -> (String, String) {
    match reference.split_once(':') {
        Some((provider, model)) => (provider.to_string(), model.to_string()),
        None => (String::new(), reference.to_string()),
    }
}

fn compose_embedding_model(provider: &str, model: &str) -> String {
    let provider = provider.trim();
    if provider.is_empty() {
        model.to_string()
    } else {
        format!("{provider}:{model}")
    }
}

fn keep(pushed: Option<u32>, current: usize) -> usize {
    pushed.map(|v| v as usize).unwrap_or(current)
}

fn creds(provider: &str, api_key: &str, base_url: &str) -> PushCredentials {
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    PushCredentials {
        provider: non_empty(provider.trim()),
        api_key: non_empty(api_key),
        base_url: non_empty(base_url),
    }
}

/// Resolve `request` against `current`. Mirrors Python's field-by-field
/// merge: empty pushed strings and `None` numbers keep the running value.
pub fn resolve_and_apply(
    current: &AppSettings,
    request: &ConfigPushRequest,
) -> anyhow::Result<ResolvedPush> {
    let models = &request.models;
    let rag = &request.rag;
    let limits = &request.limits;
    let mut notes = Vec::new();

    let provider = models.provider.trim().to_string();
    let api_key = models.api_key.clone();
    let base_url = models.base_url.clone();
    let use_explicit_provider = !provider.is_empty() || !api_key.is_empty() || !base_url.is_empty();

    let pick = |pushed: &str, current_name: &str, strip: bool| -> String {
        if !pushed.is_empty() {
            return pushed.to_string();
        }
        if use_explicit_provider && current.chat_provider.is_empty() && strip {
            strip_provider_prefix(current_name)
        } else {
            current_name.to_string()
        }
    };
    // First push over an env engine: running names are still "provider:model",
    // strip the prefix. A provider was already pushed → names are bare and may
    // legitimately contain a colon ("llama3.1:8b"); stripping would truncate.
    let smart_name = pick(&models.smart_model, &current.smart_model_name, true);
    let fast_name = pick(&models.fast_model, &current.fast_model_name, true);

    // Scalars: None keeps the current value.
    let mut effective = current.clone();
    effective.chat_provider = provider.clone();
    effective.smart_model_name = smart_name;
    effective.fast_model_name = fast_name;
    if let Some(v) = models.smart_max_tokens {
        effective.smart_model_max_tokens = Some(v);
    }
    if let Some(v) = models.fast_max_tokens {
        effective.fast_model_max_tokens = Some(v);
    }
    effective.rag_default_top_k = keep(rag.top_k, current.rag_default_top_k);
    effective.rag_max_searches = keep(rag.max_searches, current.rag_max_searches);
    effective.max_pages = keep(limits.max_pages, current.max_pages);
    effective.max_characters = keep(limits.max_characters, current.max_characters);
    effective.model_max_concurrency =
        keep(limits.model_max_concurrency, current.model_max_concurrency);

    // Embedding: any non-empty embedding field triggers a rebuild; empty
    // fields fall back to the running provider/model so a partial push never
    // clobbers env.
    let embedding_changed = !rag.embedding_provider.trim().is_empty()
        || !rag.embedding_model.trim().is_empty()
        || !rag.embedding_api_key.is_empty()
        || !rag.embedding_base_url.is_empty();
    let mut embedder = None;
    if embedding_changed {
        let (current_provider, current_model) = split_embedding_ref(&current.rag_embedding_model);
        let embed_provider = {
            let p = rag.embedding_provider.trim();
            if p.is_empty() {
                current_provider
            } else {
                p.to_string()
            }
        };
        let embed_model = {
            let m = rag.embedding_model.trim();
            if m.is_empty() {
                current_model
            } else {
                m.to_string()
            }
        };
        effective.rag_embedding_model = compose_embedding_model(&embed_provider, &embed_model);
        embedder = Some((
            embed_model,
            creds(
                &embed_provider,
                &rag.embedding_api_key,
                &rag.embedding_base_url,
            ),
        ));
        notes.push(REINDEX_NOTE.to_string());
    }

    // Model construction happens in `build_runtime`; an unknown provider
    // surfaces there as the same 400 Python raises from `_build_model`.
    let chat = use_explicit_provider.then(|| creds(&provider, &api_key, &base_url));
    Ok(ResolvedPush {
        settings: effective,
        chat,
        embedder,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DocumentsBackend;
    use std::path::PathBuf;

    fn settings() -> AppSettings {
        AppSettings {
            smart_model_name: "test".to_string(),
            fast_model_name: "test".to_string(),
            chat_provider: String::new(),
            smart_model_max_tokens: Some(8192),
            fast_model_max_tokens: Some(2048),
            model_max_concurrency: 32,
            documents_backend: DocumentsBackend::Sqlite,
            documents_sqlite_path: PathBuf::from(":memory:"),
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

    fn push(
        models: serde_json::Value,
        rag: serde_json::Value,
        limits: serde_json::Value,
    ) -> ConfigPushRequest {
        serde_json::from_value(serde_json::json!({
            "models": models, "rag": rag, "limits": limits
        }))
        .unwrap()
    }

    #[test]
    fn anthropic_push_resolves_names_limits_and_chat_creds() {
        let current = settings();
        let req = push(
            serde_json::json!({
                "provider": "anthropic", "smartModel": "claude-haiku-4-5",
                "fastModel": "claude-haiku-4-5", "smartMaxTokens": 4096,
                "fastMaxTokens": 1024, "apiKey": "k", "baseUrl": ""
            }),
            serde_json::json!({"topK": 7, "maxSearches": 3}),
            serde_json::json!({"maxPages": 50, "maxCharacters": 12345, "modelMaxConcurrency": 8}),
        );
        let resolved = resolve_and_apply(&current, &req).unwrap();
        let s = &resolved.settings;
        assert_eq!(s.smart_model_name, "claude-haiku-4-5");
        assert_eq!(s.chat_provider, "anthropic");
        assert_eq!(s.smart_model_max_tokens, Some(4096));
        assert_eq!(s.rag_default_top_k, 7);
        assert_eq!(s.max_pages, 50);
        assert_eq!(s.model_max_concurrency, 8);
        let chat = resolved.chat.unwrap();
        assert_eq!(chat.provider.as_deref(), Some("anthropic"));
        assert_eq!(chat.api_key.as_deref(), Some("k"));
        assert!(chat.base_url.is_none());
        assert!(resolved.embedder.is_none());
        assert!(resolved.notes.is_empty());
    }

    #[test]
    fn empty_models_keep_env_values_but_limits_apply() {
        let current = settings();
        let req = push(
            serde_json::json!({"provider": "", "smartModel": "", "fastModel": "",
                "smartMaxTokens": null, "fastMaxTokens": null, "apiKey": "", "baseUrl": ""}),
            serde_json::json!({}),
            serde_json::json!({"maxPages": 50}),
        );
        let resolved = resolve_and_apply(&current, &req).unwrap();
        assert_eq!(resolved.settings.smart_model_name, "test");
        assert_eq!(resolved.settings.fast_model_name, "test");
        assert_eq!(resolved.settings.max_pages, 50);
        assert!(resolved.chat.is_none());
    }

    #[test]
    fn first_push_strips_provider_prefix_from_env_names() {
        let mut current = settings();
        current.smart_model_name = "anthropic:claude-x".to_string();
        current.fast_model_name = "openai:gpt-x".to_string();
        let req = push(
            serde_json::json!({"provider": "anthropic", "baseUrl": ""}),
            serde_json::json!({}),
            serde_json::json!({}),
        );
        let resolved = resolve_and_apply(&current, &req).unwrap();
        assert_eq!(resolved.settings.smart_model_name, "claude-x");
        assert_eq!(resolved.settings.fast_model_name, "gpt-x");
    }

    #[test]
    fn second_push_keeps_colon_bearing_model_name() {
        // After a push, chat_provider is set and names are bare — a follow-up
        // push with an empty model must not re-strip "llama3.1:8b" to "8b".
        let mut current = settings();
        current.chat_provider = "ollama".to_string();
        current.smart_model_name = "llama3.1:8b".to_string();
        current.fast_model_name = "llama3.1:8b".to_string();
        let req = push(
            serde_json::json!({"provider": "ollama", "smartModel": "",
                "fastModel": "", "baseUrl": "http://localhost:11434/v1"}),
            serde_json::json!({}),
            serde_json::json!({"maxPages": 42}),
        );
        let resolved = resolve_and_apply(&current, &req).unwrap();
        assert_eq!(resolved.settings.smart_model_name, "llama3.1:8b");
        assert_eq!(resolved.settings.fast_model_name, "llama3.1:8b");
        assert_eq!(resolved.settings.max_pages, 42);
    }

    #[test]
    fn embedding_push_composes_ref_and_notes_reindex() {
        let current = settings();
        let req = push(
            serde_json::json!({"provider": "anthropic"}),
            serde_json::json!({
                "embeddingProvider": "ollama", "embeddingModel": "nomic-embed-text",
                "embeddingBaseUrl": "http://localhost:11434/v1", "topK": 9
            }),
            serde_json::json!({}),
        );
        let resolved = resolve_and_apply(&current, &req).unwrap();
        assert_eq!(
            resolved.settings.rag_embedding_model,
            "ollama:nomic-embed-text"
        );
        assert_eq!(resolved.settings.rag_default_top_k, 9);
        assert_eq!(resolved.notes.len(), 1);
        assert!(resolved.notes[0].contains("re-index"));
        let (model, creds) = resolved.embedder.unwrap();
        assert_eq!(model, "nomic-embed-text");
        assert_eq!(creds.provider.as_deref(), Some("ollama"));
        assert_eq!(creds.base_url.as_deref(), Some("http://localhost:11434/v1"));
    }

    #[test]
    fn partial_embedding_push_falls_back_to_current_ref() {
        let current = settings(); // rag_embedding_model = "voyageai:voyage-4"
        let req = push(
            serde_json::json!({}),
            serde_json::json!({"embeddingApiKey": "new-key"}),
            serde_json::json!({}),
        );
        let resolved = resolve_and_apply(&current, &req).unwrap();
        assert_eq!(resolved.settings.rag_embedding_model, "voyageai:voyage-4");
        let (_, creds) = resolved.embedder.unwrap();
        assert_eq!(creds.provider.as_deref(), Some("voyageai"));
        assert_eq!(creds.api_key.as_deref(), Some("new-key"));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let current = settings();
        let req = serde_json::from_value(serde_json::json!({
            "futureTopLevelField": {"anything": 1},
            "models": {"provider": "anthropic", "experimentalFlag": true},
            "rag": {}, "limits": {}
        }))
        .unwrap();
        let resolved = resolve_and_apply(&current, &req).unwrap();
        assert_eq!(resolved.settings.chat_provider, "anthropic");
    }
}
