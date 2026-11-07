//! Chunked reasoning over long documents. Port of
//! `stirling.agents.shared.chunked_reasoner`: map over pages with a fast-model
//! extractor, regroup the notes until they fit the synthesis budget, then
//! optionally synthesise a typed answer with the smart model.
//!
//! First-round scheduling (slice, semaphore, timeout, completion-order
//! progress) is delegated to [`ChunkedMapper`]. Compression rounds reuse the
//! same extractor and semaphore but carry a `fallback` so a failed group
//! preserves its input notes — the part the generic mapper deliberately
//! does not cover.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agents::chunked_mapper::{ChunkOutput, ChunkedMapper};
use crate::agents::runtime::AgentError;
use crate::agents::structured::{AgentOutput, StructuredAgent};
use crate::config::AppSettings;
use crate::contracts::documents::Page;
use crate::contracts::ProgressEvent;
use crate::llm::client::LlmClient;
use crate::services::progress::emit_progress;

const EXTRACTOR_SYSTEM_PROMPT: &str =
    "You are reading content from a document - either raw page text or \
condensed notes from an earlier extraction pass - and your job is to \
produce a tight set of notes that captures everything relevant to the \
user's question. The same job runs many times in parallel across the \
document and may run again to consolidate notes into smaller batches, \
so be thorough: anything you skip cannot be recovered later.\n\
\n\
Output:\n\
- summary: 1-3 sentences covering the supplied content.\n\
- relevant_excerpts: short verbatim quotes from the supplied content \
that bear on the question. Deduplicate; drop quotes that don't help.\n\
- facts: concrete facts (numbers, names, dates, claims). Deduplicate; \
drop irrelevant ones. For aggregation questions (largest, smallest, \
count, total) retain ALL candidate values across the content so a \
later step can still pick the global winner.\n\
\n\
Stay grounded in the supplied content. Do not infer or fabricate \
anything that isn't already present. If nothing in the content is \
relevant to the question, return empty excerpts and facts and a short \
neutral summary.";

/// Public-facing notes for a span of pages. Field names are the camelCase
/// wire form pydantic's `alias_generator=to_camel` produces.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChunkNotes {
    /// Page numbers covered by these notes (1-indexed).
    pub pages: Vec<i64>,
    /// One- to three-sentence summary of the covered range.
    pub summary: String,
    #[serde(default)]
    #[serde(alias = "relevant_excerpts")]
    pub relevant_excerpts: Vec<String>,
    #[serde(default)]
    pub facts: Vec<String>,
}

/// Model output for one extractor call. No `pages` field: page numbers are
/// mechanical aggregation the wrapper computes deterministically.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ExtractedNotes {
    summary: String,
    #[serde(default)]
    relevant_excerpts: Vec<String>,
    #[serde(default)]
    facts: Vec<String>,
}

struct CompressionChunk {
    content: String,
    pages: Vec<i64>,
    /// Input group preserved on extractor failure so page coverage survives.
    fallback: Vec<ChunkNotes>,
    label: String,
}

struct RoundResult {
    notes: Vec<ChunkNotes>,
    successes: usize,
}

fn note_range_label(notes: &[ChunkNotes]) -> String {
    let mut page_numbers: Vec<i64> = notes.iter().flat_map(|n| n.pages.iter().copied()).collect();
    page_numbers.sort_unstable();
    page_numbers.dedup();
    match page_numbers.as_slice() {
        [] => "pages=?".to_string(),
        [p] => format!("pages={p}"),
        ps => format!("pages={}-{}", ps[0], ps[ps.len() - 1]),
    }
}

pub struct ChunkedReasoner {
    smart_model: LlmClient,
    chat_provider: String,
    notes_char_budget: usize,
    extractor: StructuredAgent,
    mapper: ChunkedMapper<ExtractedNotes>,
}

impl ChunkedReasoner {
    /// `settings` supplies the `chunked_reasoner_*` knobs; `None` overrides
    /// fall back to them (same as the Python keyword args).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        settings: &AppSettings,
        fast_model: LlmClient,
        smart_model: LlmClient,
        chat_provider: &str,
        chars_per_slice: Option<usize>,
        concurrency: Option<usize>,
        worker_timeout_seconds: Option<f64>,
        notes_char_budget: Option<usize>,
    ) -> Result<Self, ChunkedReasonerError> {
        let chars = chars_per_slice.unwrap_or(settings.chunked_reasoner_chars_per_slice);
        let conc = concurrency.unwrap_or(settings.chunked_reasoner_concurrency);
        let timeout =
            worker_timeout_seconds.unwrap_or(settings.chunked_reasoner_worker_timeout_seconds);
        let budget = notes_char_budget.unwrap_or(settings.chunked_reasoner_notes_char_budget);
        if budget == 0 {
            return Err(ChunkedReasonerError::BadBudget);
        }

        let extractor = StructuredAgent::new(
            fast_model.clone(),
            EXTRACTOR_SYSTEM_PROMPT,
            AgentOutput::Json {
                schema: serde_json::to_value(schemars::schema_for!(ExtractedNotes))
                    .unwrap_or(serde_json::Value::Null),
            },
            chat_provider,
        );
        let mapper = ChunkedMapper::new(
            extractor.clone(),
            chars,
            conc,
            Duration::from_secs_f64(timeout),
            None,
            Some(Arc::new(|n: &ExtractedNotes| {
                (n.relevant_excerpts.len(), n.facts.len())
            })),
        )?;
        Ok(Self {
            smart_model,
            chat_provider: chat_provider.to_string(),
            notes_char_budget: budget,
            extractor,
            mapper,
        })
    }

    /// Return notes covering every page that fit the synthesis budget.
    /// Worker failures are tolerated; an empty result means every
    /// first-round chunk failed.
    pub async fn gather_notes(
        &self,
        pages: &[Page],
        question: &str,
    ) -> Result<Vec<ChunkNotes>, ChunkedReasonerError> {
        if pages.is_empty() {
            return Err(ChunkedReasonerError::EmptyInput);
        }
        let outputs: Vec<ChunkOutput<ExtractedNotes>> =
            self.mapper.map_pages(pages, question).await?;
        let mut notes: Vec<ChunkNotes> = outputs
            .into_iter()
            .map(|o| Self::build_chunk_notes(o.output, o.pages))
            .collect();
        notes.sort_by_key(|n| n.pages.first().copied().unwrap_or(0));
        Ok(self.compress_until_fits(notes, question).await)
    }

    /// Map over pages, then synthesise a structured answer of type `T`.
    /// `answer_prompt` is owned by the caller — answer tone and grounding
    /// rules are domain-specific.
    pub async fn reason<T>(
        &self,
        pages: &[Page],
        question: &str,
        answer_prompt: &str,
    ) -> Result<T, ChunkedReasonerError>
    where
        T: serde::de::DeserializeOwned + JsonSchema + Send,
    {
        let notes = self.gather_notes(pages, question).await?;
        if notes.is_empty() {
            return Err(ChunkedReasonerError::NoNotes);
        }
        self.synthesise(question, &notes, answer_prompt).await
    }

    /// Render notes as readable text for inclusion in another agent's tool
    /// result. Order preserved; empty sections omitted.
    pub fn format_notes(notes: &[ChunkNotes]) -> String {
        notes
            .iter()
            .map(|n| {
                let page_label = match n.pages.as_slice() {
                    [] => "unknown pages".to_string(),
                    [p] => format!("page {p}"),
                    ps => format!("pages {}-{}", ps[0], ps[ps.len() - 1]),
                };
                let mut block = vec![
                    format!("[Notes from {page_label}]"),
                    format!("Summary: {}", n.summary),
                ];
                if !n.relevant_excerpts.is_empty() {
                    block.push("Relevant excerpts:".to_string());
                    block.extend(n.relevant_excerpts.iter().map(|e| format!("- {e}")));
                }
                if !n.facts.is_empty() {
                    block.push("Facts:".to_string());
                    block.extend(n.facts.iter().map(|f| format!("- {f}")));
                }
                block.join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn build_chunk_notes(extracted: ExtractedNotes, pages: Vec<i64>) -> ChunkNotes {
        ChunkNotes {
            pages,
            summary: extracted.summary,
            relevant_excerpts: extracted.relevant_excerpts,
            facts: extracted.facts,
        }
    }

    fn rendered_notes_size(notes: &[ChunkNotes]) -> usize {
        Self::format_notes(notes).chars().count()
    }

    fn group_notes_for_compression(&self, notes: &[ChunkNotes]) -> Vec<Vec<ChunkNotes>> {
        let mut groups: Vec<Vec<ChunkNotes>> = Vec::new();
        let mut current: Vec<ChunkNotes> = Vec::new();
        let mut current_chars = 0usize;
        for note in notes {
            let note_chars = Self::rendered_notes_size(std::slice::from_ref(note));
            if !current.is_empty() && current_chars + note_chars > self.mapper.chars_per_slice() {
                groups.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            current_chars += note_chars;
            current.push(note.clone());
        }
        if !current.is_empty() {
            groups.push(current);
        }
        groups
    }

    fn chunk_from_notes(&self, group: &[ChunkNotes]) -> CompressionChunk {
        let mut pages: Vec<i64> = group.iter().flat_map(|n| n.pages.iter().copied()).collect();
        pages.sort_unstable();
        pages.dedup();
        CompressionChunk {
            content: Self::format_notes(group),
            pages,
            fallback: group.to_vec(),
            label: note_range_label(group),
        }
    }

    async fn compress_until_fits(
        &self,
        mut notes: Vec<ChunkNotes>,
        question: &str,
    ) -> Vec<ChunkNotes> {
        let mut round_number = 0i64;
        loop {
            let rendered_size = Self::rendered_notes_size(&notes);
            if rendered_size <= self.notes_char_budget || notes.len() <= 1 {
                if round_number > 0 {
                    tracing::info!(
                        round_number,
                        notes = notes.len(),
                        chars = rendered_size,
                        "[chunked-reasoner] compression done"
                    );
                }
                return notes;
            }

            round_number += 1;
            let groups = self.group_notes_for_compression(&notes);
            let chunks: Vec<CompressionChunk> =
                groups.iter().map(|g| self.chunk_from_notes(g)).collect();
            tracing::info!(
                round_number,
                notes = notes.len(),
                chars = rendered_size,
                groups = chunks.len(),
                "[chunked-reasoner] compression round"
            );
            emit_progress(ProgressEvent::WholeDocCompressionRound {
                round_number,
                notes_in: notes.len() as i64,
                groups: chunks.len() as i64,
            });

            let chunk_total = chunks.len();
            let result = self.run_compression_round(chunks, question).await;
            tracing::info!(
                round_number,
                succeeded = result.successes,
                total = chunk_total,
                "[chunked-reasoner] round finished"
            );
            if result.successes == 0 {
                tracing::warn!(
                    round_number,
                    notes = result.notes.len(),
                    "[chunked-reasoner] round produced no successful extractions; bailing"
                );
                return result.notes;
            }
            notes = result.notes;
        }
    }

    async fn run_compression_round(
        &self,
        chunks: Vec<CompressionChunk>,
        question: &str,
    ) -> RoundResult {
        let semaphore = self.mapper.semaphore();
        let timeout = self.mapper.worker_timeout();
        let mut pending = FuturesUnordered::new();
        for chunk in chunks {
            let extractor = self.extractor.clone();
            let semaphore = semaphore.clone();
            let prompt = format_extraction_prompt(&chunk.content, question);
            pending.push(async move {
                let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
                let start = Instant::now();
                let result =
                    tokio::time::timeout(timeout, extractor.run::<ExtractedNotes>(&prompt)).await;
                (chunk, result, start.elapsed())
            });
        }

        let mut notes = Vec::new();
        let mut successes = 0usize;
        while let Some((chunk, result, _duration)) = pending.next().await {
            match result {
                Ok(Ok(extracted)) => {
                    successes += 1;
                    notes.push(Self::build_chunk_notes(extracted, chunk.pages));
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        chunk = %chunk.label,
                        error = %e,
                        fallback_notes = chunk.fallback.len(),
                        "[chunked-reasoner] compression chunk failed; preserving inputs"
                    );
                    notes.extend(chunk.fallback);
                }
                Err(_) => {
                    tracing::warn!(
                        chunk = %chunk.label,
                        timeout_secs = timeout.as_secs_f64(),
                        fallback_notes = chunk.fallback.len(),
                        "[chunked-reasoner] compression chunk timed out; preserving inputs"
                    );
                    notes.extend(chunk.fallback);
                }
            }
        }
        notes.sort_by_key(|n| n.pages.first().copied().unwrap_or(0));
        RoundResult { notes, successes }
    }

    async fn synthesise<T>(
        &self,
        question: &str,
        notes: &[ChunkNotes],
        answer_prompt: &str,
    ) -> Result<T, ChunkedReasonerError>
    where
        T: serde::de::DeserializeOwned + JsonSchema + Send,
    {
        let agent = StructuredAgent::new(
            self.smart_model.clone(),
            answer_prompt,
            AgentOutput::Json {
                schema: serde_json::to_value(schemars::schema_for!(T))
                    .unwrap_or(serde_json::Value::Null),
            },
            &self.chat_provider,
        );
        let prompt = format!(
            "User question:\n{question}\n\nNotes from across the document:\n\n{}",
            Self::format_notes(notes)
        );
        agent
            .run::<T>(&prompt)
            .await
            .map_err(ChunkedReasonerError::Agent)
    }
}

fn format_extraction_prompt(content: &str, question: &str) -> String {
    format!("User question:\n{question}\n\nContent:\n{content}")
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkedReasonerError {
    #[error("notes_char_budget must be positive")]
    BadBudget,
    #[error("gather_notes requires at least one page")]
    EmptyInput,
    #[error("all chunked-reasoning workers failed; no notes to synthesise from")]
    NoNotes,
    #[error(transparent)]
    Mapper(#[from] crate::agents::chunked_mapper::ChunkedMapperError),
    #[error(transparent)]
    Agent(#[from] AgentError),
}
