//! Embedding service. Port of `stirling.documents.embedder` + `voyage` —
//! speaks the OpenAI `/embeddings` wire format directly via reqwest (no SDK),
//! with Voyage's `input_type` field forwarded for retrieval-vs-document
//! embedding asymmetry.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::documents::chunker::chunk_text;
use crate::documents::store::Document;

pub const DEFAULT_EMBED_BATCH_SIZE: usize = 256;
pub const VOYAGE_BASE_URL: &str = "https://api.voyageai.com/v1";
pub const VOYAGE_API_KEY_ENV: &str = "VOYAGE_API_KEY";
const MISSING_API_KEY: &str = "stirling-voyage-api-key-not-configured";
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedInputType {
    Document,
    Query,
}

impl EmbedInputType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Query => "query",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingProvider {
    Voyage,
    OpenAi,
    Ollama,
    Custom,
}

impl EmbeddingProvider {
    fn parse(name: &str) -> anyhow::Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "voyageai" => Ok(Self::Voyage),
            "openai" => Ok(Self::OpenAi),
            "ollama" => Ok(Self::Ollama),
            "custom" => Ok(Self::Custom),
            other => anyhow::bail!("Unsupported embedding provider {other:?}."),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmbedderConfig {
    pub model: String,
    pub provider: EmbeddingProvider,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

/// Resolve config the way `_build_embedder` does: explicit
/// provider/api_key/base_url is the config-push path; otherwise the
/// `"provider:model"` env-string form, with bare names defaulting to OpenAI.
pub fn resolve_embedder(
    model_name: &str,
    provider: Option<&str>,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> anyhow::Result<EmbedderConfig> {
    let explicit = provider.is_some() || api_key.is_some() || base_url.is_some();
    if !explicit {
        let (env_provider, sep, env_model) = match model_name.split_once(':') {
            Some((p, m)) => (p, true, m),
            None => ("", false, model_name),
        };
        if sep && env_provider.eq_ignore_ascii_case("voyageai") {
            return Ok(EmbedderConfig {
                model: env_model.to_string(),
                provider: EmbeddingProvider::Voyage,
                api_key: std::env::var(VOYAGE_API_KEY_ENV).ok(),
                base_url: None,
            });
        }
        return Ok(EmbedderConfig {
            model: model_name.to_string(),
            provider: EmbeddingProvider::OpenAi,
            api_key: std::env::var("OPENAI_API_KEY").ok(),
            base_url: None,
        });
    }

    let provider_name = provider.unwrap_or("");
    let provider = EmbeddingProvider::parse(provider_name)?;
    match provider {
        EmbeddingProvider::Voyage => Ok(EmbedderConfig {
            model: model_name.to_string(),
            provider,
            api_key: api_key
                .map(str::to_string)
                .or_else(|| std::env::var(VOYAGE_API_KEY_ENV).ok())
                .or_else(|| Some(MISSING_API_KEY.to_string())),
            base_url: base_url.map(str::to_string),
        }),
        EmbeddingProvider::OpenAi => Ok(EmbedderConfig {
            model: model_name.to_string(),
            provider,
            api_key: api_key.map(str::to_string),
            base_url: None,
        }),
        EmbeddingProvider::Ollama | EmbeddingProvider::Custom => Ok(EmbedderConfig {
            model: model_name.to_string(),
            provider,
            api_key: api_key
                .map(str::to_string)
                .or_else(|| Some("ollama".to_string())),
            base_url: base_url.map(str::to_string),
        }),
    }
}

#[derive(Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    input_type: Option<&'static str>,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

pub struct EmbeddingService {
    client: reqwest::Client,
    config: EmbedderConfig,
    chunk_size: usize,
    chunk_overlap: usize,
    embed_batch_size: usize,
}

impl EmbeddingService {
    pub fn new(
        client: reqwest::Client,
        config: EmbedderConfig,
        chunk_size: usize,
        chunk_overlap: usize,
        embed_batch_size: usize,
    ) -> Self {
        Self {
            client,
            config,
            chunk_size,
            chunk_overlap,
            embed_batch_size,
        }
    }

    /// Whether the client has credentials; does not make a provider request.
    /// Only Voyage needs a key check — the others resolve at call time like
    /// pydantic-ai's `Embedder` does.
    pub fn configured(&self) -> bool {
        match self.config.provider {
            EmbeddingProvider::Voyage => {
                self.config.api_key.as_deref() != Some(MISSING_API_KEY)
                    && self.config.api_key.as_deref() != Some("")
                    && self.config.api_key.is_some()
            }
            _ => true,
        }
    }

    fn endpoint(&self) -> String {
        let base = match self.config.provider {
            EmbeddingProvider::Voyage => self
                .config
                .base_url
                .clone()
                .unwrap_or_else(|| VOYAGE_BASE_URL.to_string()),
            EmbeddingProvider::OpenAi => OPENAI_BASE_URL.to_string(),
            EmbeddingProvider::Ollama | EmbeddingProvider::Custom => self
                .config
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434/v1".to_string()),
        };
        format!("{}/embeddings", base.trim_end_matches('/'))
    }

    async fn embed_batch(
        &self,
        texts: &[String],
        input_type: EmbedInputType,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        if self.config.provider == EmbeddingProvider::Voyage
            && self.config.api_key.as_deref() == Some(MISSING_API_KEY)
        {
            anyhow::bail!(
                "VoyageAI embeddings need an API key: set {VOYAGE_API_KEY_ENV} or push one via admin AI settings."
            );
        }
        let mut body = EmbeddingsRequest {
            model: &self.config.model,
            input: texts,
            input_type: None,
        };
        // Voyage speaks the OpenAI shape but forwards input_type at top level
        // (Python's extra_body merge lands it in the JSON body).
        if self.config.provider == EmbeddingProvider::Voyage {
            body.input_type = Some(input_type.as_str());
        }
        let mut req = self
            .client
            .post(self.endpoint())
            .json(&body)
            .header("content-type", "application/json");
        if let Some(key) = &self.config.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Embedding request failed ({status}): {text}");
        }
        let parsed: EmbeddingsResponse = resp.json().await?;
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }

    /// Embed a search query, optimised for retrieval.
    pub async fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut out = self
            .embed_batch(&[text.to_string()], EmbedInputType::Query)
            .await?;
        Ok(out.remove(0))
    }

    /// Embed document texts for indexing, batched under the provider's
    /// per-request limit.
    pub async fn embed_documents(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut all = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.embed_batch_size) {
            all.extend(self.embed_batch(batch, EmbedInputType::Document).await?);
        }
        Ok(all)
    }

    /// Chunk text and return Documents ready for embedding. Each chunk gets a
    /// unique id `{source}:chunk:{i}` and `source`/`chunk_index` metadata.
    pub fn chunk_and_prepare(
        &self,
        text: &str,
        source: &str,
        base_metadata: Option<&HashMap<String, String>>,
    ) -> Vec<Document> {
        let chunks = chunk_text(text, self.chunk_size, self.chunk_overlap);
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                let mut meta = base_metadata.cloned().unwrap_or_default();
                meta.insert("source".to_string(), source.to_string());
                meta.insert("chunk_index".to_string(), i.to_string());
                let doc_id = if source.is_empty() {
                    format!("chunk:{i}")
                } else {
                    format!("{source}:chunk:{i}")
                };
                Document {
                    id: doc_id,
                    text: chunk,
                    metadata: meta,
                }
            })
            .collect()
    }
}
