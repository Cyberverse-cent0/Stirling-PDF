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
