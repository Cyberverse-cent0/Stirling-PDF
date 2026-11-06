//! Model spec resolution, mirroring `runtime._build_model`.
//!
//! Env path (no explicit provider/key/base_url): `anthropic:` prefix selects
//! Anthropic, `openai:`/`ollama:` select the matching provider, anything else
//! is treated as an OpenAI model name (pydantic-ai `infer_model` fallback).
//! Explicit path (config push): provider is `anthropic`, `openai`, `ollama`,
//! or `custom` (OpenAI-compatible endpoint).

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    /// OpenAI itself or any OpenAI-compatible endpoint (Ollama, custom).
    OpenAiCompat,
}

#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub kind: ProviderKind,
    pub model_name: String,
    pub api_key: String,
    pub base_url: String,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_required_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Anthropic tolerates a missing key at boot (placeholder fails calls with a
/// clear 401) so Ollama/OpenAI-only deployments still start.
fn anthropic_api_key() -> String {
    static WARNED: OnceLock<()> = OnceLock::new();
    let key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    if key.is_empty() {
        WARNED.get_or_init(|| {
            tracing::warn!(
                "ANTHROPIC_API_KEY is not set - the engine will start, but Anthropic model \
                 calls will fail until a key is provided or a different provider is configured"
            );
        });
        return "unconfigured".to_string();
    }
    key
}

/// `provider`/`api_key`/`base_url` are the config-push overrides; `None` means
/// resolve from the environment exactly like the Python env path.
pub fn resolve_model(
    model_name: &str,
    provider: Option<&str>,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> anyhow::Result<ModelSpec> {
    if provider.is_none() && api_key.is_none() && base_url.is_none() {
        return resolve_from_env(model_name);
    }

    let provider_name = provider.unwrap_or("").to_ascii_lowercase();
    let key = api_key.filter(|k| !k.is_empty());
    match provider_name.as_str() {
        "anthropic" => Ok(ModelSpec {
            kind: ProviderKind::Anthropic,
            model_name: model_name.to_string(),
            api_key: key.map(str::to_string).unwrap_or_else(anthropic_api_key),
            base_url: base_url
                .map(str::to_string)
                .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
        }),
        "openai" => Ok(ModelSpec {
            kind: ProviderKind::OpenAiCompat,
            model_name: model_name.to_string(),
            api_key: key
                .map(str::to_string)
                .unwrap_or_else(|| env_or("OPENAI_API_KEY", "")),
            base_url: base_url
                .map(str::to_string)
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
        }),
        "ollama" | "custom" => Ok(ModelSpec {
            kind: ProviderKind::OpenAiCompat,
            model_name: model_name.to_string(),
            // Ollama ignores the key but the API needs a non-empty one.
            api_key: key
                .map(str::to_string)
                .unwrap_or_else(|| "ollama".to_string()),
            base_url: base_url.map(str::to_string).unwrap_or_else(|| {
                if provider_name == "ollama" {
                    "http://localhost:11434/v1".to_string()
                } else {
                    env_required_or("OPENAI_BASE_URL", "http://localhost:11434/v1")
                }
            }),
        }),
        other => Err(anyhow::anyhow!("Unsupported model provider {other:?}.")),
    }
}

fn resolve_from_env(model_name: &str) -> anyhow::Result<ModelSpec> {
    if let Some(bare) = model_name.strip_prefix("anthropic:") {
        return Ok(ModelSpec {
            kind: ProviderKind::Anthropic,
            model_name: bare.to_string(),
            api_key: anthropic_api_key(),
            base_url: "https://api.anthropic.com".to_string(),
        });
    }
    if let Some(bare) = model_name.strip_prefix("openai:") {
        return Ok(ModelSpec {
            kind: ProviderKind::OpenAiCompat,
            model_name: bare.to_string(),
            api_key: env_required_or("OPENAI_API_KEY", ""),
            base_url: "https://api.openai.com/v1".to_string(),
        });
    }
    if let Some(bare) = model_name.strip_prefix("ollama:") {
        return Ok(ModelSpec {
            kind: ProviderKind::OpenAiCompat,
            model_name: bare.to_string(),
            api_key: "ollama".to_string(),
            base_url: env_or("OLLAMA_BASE_URL", "http://localhost:11434/v1"),
        });
    }
    // pydantic-ai infer_model fallback: bare names are OpenAI models.
    Ok(ModelSpec {
        kind: ProviderKind::OpenAiCompat,
        model_name: model_name.to_string(),
        api_key: env_required_or("OPENAI_API_KEY", ""),
        base_url: "https://api.openai.com/v1".to_string(),
    })
}
