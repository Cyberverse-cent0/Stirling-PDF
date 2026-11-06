//! Contradiction detector — orchestrates the five-stage pipeline. Port of
//! `stirling.agents.contradiction.detector`.
//!
//! Stage 1 — per-chunk claim extraction via [`ChunkedMapper`].
//! Stage 2 — subject canonicalisation (batched fast-model calls; lexical fallback).
//! Stage 3 — pre-filter heuristics (identical-quote post-filter).
//! Stage 4 — per-bucket pair detection (parallel, oversize-aware windowing).
//! Stage 5 — summary (one fast-model call; deterministic fallback).
//!
//! The detector never touches PDF files directly: pages arrive via
//! [`DocumentService::read_pages`]. Extraction runs per file because
//! concatenating pages across files would merge same-numbered pages into one
//! lookup and validate quotes against the wrong file's text.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::agents::chunked_mapper::{ChunkedMapper, ChunkOutput, ChunkedMapperError};
use crate::agents::contradiction::ledger::ClaimLedger;
use crate::agents::contradiction::prompts;
use crate::agents::runtime::AgentError;
use crate::agents::structured::{AgentOutput, StructuredAgent};
use crate::config::AppSettings;
use crate::contracts::common::AiFile;
use crate::contracts::contradiction::{
    AnchorQuality, Claim, ClaimPolarity, Contradiction, ContradictionReport,
    ContradictionSeverity,
};
use crate::contracts::documents::Page;
use crate::documents::service::DocumentService;
use crate::llm::client::LlmClient;
use crate::models::PrincipalId;

// ---------------------------------------------------------------------------
// Internal LLM-output schemas
// ---------------------------------------------------------------------------

/// One claim emitted by the per-chunk claim extractor. Carries the page the
/// model reported; the wrapper validates it against the chunk's coverage
/// before promoting to a public [`Claim`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ExtractedClaim {
    /// 1-indexed page from the `[Page N]` marker.
    page: i64,
    subject: String,
    polarity: ClaimPolarity,
    text: String,
    quote: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ExtractedClaims {
    #[serde(default)]
    claims: Vec<ExtractedClaim>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct SubjectAlias {
    /// Original subject phrase exactly as seen on a claim.
    raw: String,
    /// Chosen canonical phrasing for the group.
    canonical: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct SubjectMapping {
    #[serde(default)]
    aliases: Vec<SubjectAlias>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct DetectedPair {
    /// 0-based index into the bucket window.
    i: i64,
    /// 0-based index into the bucket window (i < j).
    j: i64,
    explanation: String,
    severity: ContradictionSeverity,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct BucketContradictions {
    #[serde(default)]
    pairs: Vec<DetectedPair>,
}

struct FileExtractionResult {
    claims: Vec<Claim>,
    /// Pages whose extraction pass ran (union over successful chunks).
    pages_attempted: HashSet<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum DetectorError {
    #[error(transparent)]
    Mapper(#[from] ChunkedMapperError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Documents(#[from] anyhow::Error),
    #[error("invalid bucket window config: {0}")]
    BadWindowConfig(String),
}

// ---------------------------------------------------------------------------
// Detector
// ---------------------------------------------------------------------------

pub struct ContradictionDetector {
    documents: Arc<DocumentService>,
    settings: Arc<AppSettings>,
    subject_canonicaliser: StructuredAgent,
    pair_detector: StructuredAgent,
    summary_agent: StructuredAgent,
    mapper: ChunkedMapper<ExtractedClaims>,
    detect_semaphore: Arc<Semaphore>,
}

impl ContradictionDetector {
    pub fn new(
        settings: Arc<AppSettings>,
        fast_model: LlmClient,
        chat_provider: &str,
        documents: Arc<DocumentService>,
    ) -> Result<Self, ChunkedMapperError> {
        let json_agent = |system: String, schema: Value| StructuredAgent::new(
            fast_model.clone(),
            system,
            AgentOutput::Json { schema },
            chat_provider,
        );
        let mapper = ChunkedMapper::new(
            json_agent(
                prompts::claim_extractor_prompt(),
                serde_json::to_value(schemars::schema_for!(ExtractedClaims))
                    .unwrap_or(Value::Null),
            ),
            settings.chunked_reasoner_chars_per_slice,
            settings.chunked_reasoner_concurrency,
            Duration::from_secs_f64(settings.chunked_reasoner_worker_timeout_seconds),
            Some(Arc::new(|content: &str, query: &str| {
                format!("Extraction focus: {query}\n<content>\n{content}\n</content>")
            })),
            None,
        )?;
        Ok(Self {
            detect_semaphore: Arc::new(Semaphore::new(
                settings.contradiction_detect_concurrency,
            )),
            subject_canonicaliser: json_agent(
                prompts::subject_canonicaliser_prompt(),
                serde_json::to_value(schemars::schema_for!(SubjectMapping))
                    .unwrap_or(Value::Null),
            ),
            pair_detector: json_agent(
                prompts::contradiction_detector_prompt(),
                serde_json::to_value(schemars::schema_for!(BucketContradictions))
                    .unwrap_or(Value::Null),
            ),
            summary_agent: StructuredAgent::new(
                fast_model,
                prompts::summary_prompt(),
                AgentOutput::Text,
                chat_provider,
            ),
            documents,
            settings,
            mapper,
        })
    }

    /// Run the full pipeline over the supplied files. Files must already be
    /// ingested — callers do the `has_collection` precheck.
    pub async fn detect(
        &self,
        files: &[AiFile],
        principals: &[PrincipalId],
        query: Option<&str>,
    ) -> Result<ContradictionReport, DetectorError> {
        tracing::info!(
            files = ?files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            query = ?query,
            "[contradiction] detect start"
        );

        let effective_query = query.unwrap_or("extract claims");

        // Stages 0+1 — per-file page load + chunked claim extraction, in
        // parallel. The mapper's internal semaphore caps total per-chunk
        // concurrency so the LLM pool isn't overcommitted.
        let mut pending = FuturesUnordered::new();
        for file in files {
            pending.push(self.extract_claims_for_file(file, effective_query, principals));
        }

        let mut claims: Vec<Claim> = Vec::new();
        let mut pages_attempted: HashSet<(String, i64)> = HashSet::new();
        let mut any_pages_seen = false;
        while let Some(result) = pending.next().await {
            match result {
                Ok((file_name, extraction)) => {
                    if !extraction.pages_attempted.is_empty() {
                        any_pages_seen = true;
                    }
                    claims.extend(extraction.claims);
                    pages_attempted.extend(
                        extraction
                            .pages_attempted
                            .iter()
                            .map(|p| (file_name.clone(), *p)),
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "[contradiction] per-file extraction failed");
                    continue;
                }
            }
        }

        if !any_pages_seen {
            return Ok(empty_report(
                "No document content was available to audit.".to_string(),
                Vec::new(),
            ));
        }

        // Page numbers legitimately repeat across files — page 1 of
        // report.pdf and page 1 of memo.pdf are distinct pages and BOTH were
        // examined, so duplicates across files are kept.
        let mut pages_examined: Vec<i64> = pages_attempted.iter().map(|(_, p)| *p).collect();
        pages_examined.sort_unstable();
        tracing::info!(
            claims = claims.len(),
            pages = pages_examined.len(),
            "[contradiction] stage 1 done"
        );

        if claims.is_empty() {
            let summary = self
                .generate_summary(0, 0, &pages_examined)
                .await
                .unwrap_or_else(|_| fallback_summary(0, 0, &pages_examined));
            return Ok(empty_report(summary, pages_examined));
        }

        // Stage 2 — canonicalise subjects.
        let mut ledger = ClaimLedger::new();
        for claim in claims {
            ledger.record(claim);
        }
        let unique_subjects = ledger.unique_subjects();
        if !unique_subjects.is_empty() {
            let mapping = self.canonicalise_subjects(&unique_subjects).await;
            if !mapping.is_empty() {
                ledger.rekey_with_canonical(&mapping);
            }
        }

        // Stages 3+4 — pre-filter + per-bucket detection.
        let mut contradictions = self.detect_all_buckets(&ledger).await?;
        contradictions.sort_by_key(|c| (c.page1(), c.page2()));

        let error_count = contradictions
            .iter()
            .filter(|c| c.severity == ContradictionSeverity::Error)
            .count() as i64;
        let warning_count = contradictions
            .iter()
            .filter(|c| c.severity == ContradictionSeverity::Warning)
            .count() as i64;

        // Stage 5 — summary.
        let summary = self
            .generate_summary(error_count, warning_count, &pages_examined)
            .await
            .unwrap_or_else(|_| fallback_summary(error_count, warning_count, &pages_examined));

        Ok(ContradictionReport {
            contradictions,
            pages_examined,
            clean: error_count == 0,
            summary,
        })
    }

    async fn extract_claims_for_file(
        &self,
        file: &AiFile,
        query: &str,
        principals: &[PrincipalId],
    ) -> Result<(String, FileExtractionResult), DetectorError> {
        let file_pages = self
            .documents
            .read_pages(&file.id, principals, None)
            .await?;
        if file_pages.is_empty() {
            tracing::info!(file = %file.name, id = %file.id, "[contradiction] no stored pages; skipping");
            return Ok((
                file.name.clone(),
                FileExtractionResult {
                    claims: Vec::new(),
                    pages_attempted: HashSet::new(),
                },
            ));
        }

        let pages_by_num: HashMap<i64, &Page> =
            file_pages.iter().map(|p| (p.page_number, p)).collect();
        let chunk_outputs = self.mapper.map_pages(&file_pages, query).await?;

        let mut file_claims = Vec::new();
        let mut pages_attempted = HashSet::new();
        for chunk in &chunk_outputs {
            pages_attempted.extend(chunk.pages.iter().copied());
            let chunk_char_count: i64 = chunk
                .pages
                .iter()
                .filter_map(|p| pages_by_num.get(p))
                .map(|p| p.char_count)
                .sum();
            if chunk.output.claims.is_empty() && chunk_char_count > 500 {
                tracing::warn!(
                    chunk = %chunk.label,
                    chars = chunk_char_count,
                    "[contradiction] chunk produced 0 claims for substantial content"
                );
            }
            for raw in &chunk.output.claims {
                if let Some(claim) =
                    validate_extracted_claim(raw, chunk, &pages_by_num, Some(file.name.as_str()))
                {
                    file_claims.push(claim);
                }
            }
        }
        Ok((
            file.name.clone(),
            FileExtractionResult {
                claims: file_claims,
                pages_attempted,
            },
        ))
    }

    // -- Stage 2: canonicalisation -------------------------------------

    async fn canonicalise_subjects(
        &self,
        subjects: &[String],
    ) -> HashMap<String, String> {
        if subjects.is_empty() {
            return HashMap::new();
        }
        let batch_size = self.settings.contradiction_canonicaliser_batch_size.max(1);
        let batches: Vec<&[String]> = subjects.chunks(batch_size).collect();

        let mut pending = FuturesUnordered::new();
        for batch in batches {
            pending.push(self.canonicalise_batch(batch));
        }

        let mut mapping: HashMap<String, String> = HashMap::new();
        while let Some(batch_result) = pending.next().await {
            let Ok(batch_mapping) = batch_result else {
                continue; // already logged at the per-batch site
            };
            for (raw, canonical) in batch_mapping {
                // Lexicographically smallest canonical wins when batches
                // disagree — deterministic regardless of finish order.
                match mapping.get(&raw) {
                    Some(existing) if canonical >= *existing => {}
                    _ => {
                        mapping.insert(raw, canonical);
                    }
                }
            }
        }
        mapping
    }

    async fn canonicalise_batch(
        &self,
        subjects: &[String],
    ) -> Result<HashMap<String, String>, AgentError> {
        let payload = escape_for_tag(&serde_json::to_string(subjects).unwrap_or_default());
        let prompt = format!("<subjects>{payload}</subjects>");
        let _permit = self
            .detect_semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed");
        let timeout = Duration::from_secs_f64(
            self.settings.chunked_reasoner_worker_timeout_seconds,
        );
        let result = tokio::time::timeout(timeout, self.subject_canonicaliser.run::<SubjectMapping>(&prompt))
            .await
            .map_err(|_| AgentError::Output("subject canonicalisation timed out".to_string()))??;

        let mut batch_mapping = HashMap::new();
        for alias in result.aliases {
            // Defensive: a whitespace-only canonical would make the ledger
            // silently drop claims.
            if alias.canonical.trim().is_empty() {
                continue;
            }
            batch_mapping.insert(alias.raw, alias.canonical);
        }
        Ok(batch_mapping)
    }

    // -- Stages 3+4: bucket detection ----------------------------------

    async fn detect_all_buckets(
        &self,
        ledger: &ClaimLedger,
    ) -> Result<Vec<Contradiction>, DetectorError> {
        let buckets = ledger.buckets();
        if buckets.is_empty() {
            return Ok(Vec::new());
        }

        let mut pending = FuturesUnordered::new();
        for (canonical, claims) in buckets {
            let claims = claims.to_vec();
            let semaphore = self.detect_semaphore.clone();
            pending.push(async move {
                let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
                self.detect_for_bucket(&canonical, claims).await
            });
        }

        let mut out = Vec::new();
        while let Some(result) = pending.next().await {
            match result {
                Ok(found) => out.extend(found),
                Err(e) => {
                    tracing::warn!(error = %e, "[contradiction] bucket detection failed");
                }
            }
        }
        Ok(out)
    }

    async fn detect_for_bucket(
        &self,
        canonical_subject: &str,
        claims: Vec<Claim>,
    ) -> Result<Vec<Contradiction>, DetectorError> {
        if claims.len() < 2 {
            return Ok(Vec::new());
        }
        let deduped = dedupe_claims_for_detection(&claims);
        if deduped.len() < 2 {
            return Ok(Vec::new());
        }

        let size = self.settings.contradiction_bucket_chunk_size;
        let overlap = self.settings.contradiction_bucket_chunk_overlap;
        if size == 0 || overlap >= size {
            return Err(DetectorError::BadWindowConfig(format!(
                "size={size} overlap={overlap}"
            )));
        }

        let mut seen_pairs: HashSet<(usize, usize)> = HashSet::new();
        let mut out = Vec::new();
        for (chunk_start, window) in windows(&deduped, size, overlap) {
            let pairs = match self.run_detector_chunk(canonical_subject, &window).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        subject = %canonical_subject,
                        chunk_start,
                        error = %e,
                        "[contradiction] detector chunk failed"
                    );
                    continue;
                }
            };
            for pair in pairs {
                if pair.i == pair.j || pair.i < 0 || pair.j < 0 {
                    continue;
                }
                let (i, j) = (pair.i as usize, pair.j as usize);
                if i >= window.len() || j >= window.len() {
                    continue;
                }
                let (global_i, global_j) = (chunk_start + i, chunk_start + j);
                let (lo, hi) = if global_i < global_j {
                    (global_i, global_j)
                } else {
                    (global_j, global_i)
                };
                if lo == hi || seen_pairs.contains(&(lo, hi)) {
                    continue;
                }
                seen_pairs.insert((lo, hi));

                let claim_lo = &deduped[lo];
                let claim_hi = &deduped[hi];
                // Identical-quote pairs are detector self-pairings, not
                // contradictions.
                if claim_lo.quote.trim() == claim_hi.quote.trim() {
                    continue;
                }
                out.push(Contradiction {
                    subject: canonical_subject.to_string(),
                    claim1: claim_lo.clone(),
                    claim2: claim_hi.clone(),
                    explanation: pair.explanation,
                    severity: pair.severity,
                });
            }
        }
        Ok(out)
    }

    async fn run_detector_chunk(
        &self,
        canonical_subject: &str,
        chunk: &[Claim],
    ) -> Result<Vec<DetectedPair>, AgentError> {
        // Each claim renders as `[i] {json}` inside <claims>; the whole block
        // passes through escape_for_tag so a literal "</claims>" inside a
        // quote can't close the envelope.
        let rendered: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(index, claim)| format!("[{index}] {}", claim_prompt_json(claim)))
            .collect();
        let claims_block = escape_for_tag(&rendered.join("\n"));
        let prompt = format!(
            "Canonical subject: {}\n<claims>\n{claims_block}\n</claims>",
            python_repr(canonical_subject)
        );
        let timeout = Duration::from_secs_f64(
            self.settings.chunked_reasoner_worker_timeout_seconds,
        );
        let result = tokio::time::timeout(timeout, self.pair_detector.run::<BucketContradictions>(&prompt))
            .await
            .map_err(|_| AgentError::Output("pair detection timed out".to_string()))??;
        Ok(result.pairs)
    }

    // -- Stage 5: summary ------------------------------------------------

    async fn generate_summary(
        &self,
        error_count: i64,
        warning_count: i64,
        pages_examined: &[i64],
    ) -> Result<String, AgentError> {
        // Internal prompt payload — snake_case keys exactly as the summary
        // system prompt expects (not the camelCase wire form).
        let stats = json!({
            "pages_examined": pages_examined.len() as i64,
            "errors": error_count,
            "warnings": warning_count,
        });
        let prompt = format!("<verdict>{}</verdict>", escape_for_tag(&stats.to_string()));
        let timeout = Duration::from_secs_f64(
            self.settings.chunked_reasoner_worker_timeout_seconds,
        );
        tokio::time::timeout(timeout, self.summary_agent.run_text(&prompt))
            .await
            .map_err(|_| AgentError::Output("summary generation timed out".to_string()))?
    }
}

// ---------------------------------------------------------------------------
// Module-level helpers
// ---------------------------------------------------------------------------

/// `claim.model_dump_json(include={page,polarity,text,quote})` — the field
/// subset the detector prompt cares about, in the model's own field names.
fn claim_prompt_json(claim: &Claim) -> String {
    json!({
        "page": claim.page,
        "polarity": claim.polarity,
        "text": claim.text,
        "quote": claim.quote,
    })
    .to_string()
}

/// Escape `<`/`>` as JSON unicode escapes so a payload can't prematurely
/// close a wrapping XML-style tag. Applied to already-serialised JSON, the
/// escapes remain valid JSON and decode identically.
pub(crate) fn escape_for_tag(text: &str) -> String {
    text.replace('<', "\\u003c").replace('>', "\\u003e")
}

/// Minimal Python `repr()` for prompt/report interpolation: prefers single
/// quotes, switches to double quotes when the text contains a single quote.
pub(crate) fn python_repr(text: &str) -> String {
    if text.contains('\'') && !text.contains('"') {
        return format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
    }
    let escaped = text
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r");
    format!("'{escaped}'")
}

/// Stage-1 claim validation: page traceability + verbatim/paraphrased
/// anchor quality. `pages_by_num` must be the lookup for a SINGLE file —
/// passing a cross-file aggregate produces wrong substring matches when
/// files share page numbers.
fn validate_extracted_claim(
    raw: &ExtractedClaim,
    chunk: &ChunkOutput<ExtractedClaims>,
    pages_by_num: &HashMap<i64, &Page>,
    file_name: Option<&str>,
) -> Option<Claim> {
    if raw.subject.trim().is_empty() || raw.text.trim().is_empty() || raw.quote.trim().is_empty() {
        return None;
    }

    let mut page = raw.page;
    if !chunk.pages.contains(&page) {
        // Mechanical fallback: exactly one chunk page whose text contains
        // the quote gets the reassignment; anything else drops the claim.
        let matches: Vec<i64> = chunk
            .pages
            .iter()
            .copied()
            .filter(|p| {
                pages_by_num
                    .get(p)
                    .is_some_and(|pg| pg.text.contains(&raw.quote))
            })
            .collect();
        if matches.len() == 1 {
            tracing::debug!(
                from = raw.page,
                to = matches[0],
                "[contradiction] reassigning claim page via quote search"
            );
            page = matches[0];
        } else {
            tracing::warn!(
                page = raw.page,
                chunk = %chunk.label,
                matches = matches.len(),
                "[contradiction] dropping claim with unverifiable page"
            );
            return None;
        }
    }

    let anchor_quality = match pages_by_num.get(&page) {
        Some(pg) if pg.text.contains(&raw.quote) => AnchorQuality::Verbatim,
        _ => AnchorQuality::Paraphrased,
    };

    Some(Claim {
        page,
        subject: raw.subject.clone(),
        polarity: raw.polarity,
        text: raw.text.clone(),
        quote: raw.quote.clone(),
        anchor_quality,
        file_name: file_name.map(str::to_string),
    })
}

/// Collapse claims with the same `(file_name, page, quote)` to one. The
/// ledger keeps everything; the detector sees the deduped view.
/// `file_name` is in the key so multi-file audits don't collapse claims
/// that share a page number across different source files.
fn dedupe_claims_for_detection(claims: &[Claim]) -> Vec<Claim> {
    let mut seen: HashSet<(Option<String>, i64, String)> = HashSet::new();
    let mut out = Vec::new();
    for claim in claims {
        let key = (
            claim.file_name.clone(),
            claim.page,
            claim.quote.trim().to_string(),
        );
        if seen.insert(key) {
            out.push(claim.clone());
        }
    }
    out
}

/// Overlapping windows over `items`. Every item appears in at least one
/// window; `len <= size` yields a single full window. Cross-window reach is
/// roughly `size - overlap` claims — oversized buckets trade recall for
/// bounded prompt size.
fn windows(items: &[Claim], size: usize, overlap: usize) -> Vec<(usize, Vec<Claim>)> {
    debug_assert!(size > 0 && overlap < size);
    let n = items.len();
    if n <= size {
        return vec![(0, items.to_vec())];
    }
    let step = size - overlap;
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < n {
        let end = (start + size).min(n);
        out.push((start, items[start..end].to_vec()));
        if end >= n {
            break;
        }
        start += step;
    }
    out
}

fn empty_report(summary: String, pages_examined: Vec<i64>) -> ContradictionReport {
    // Always clean: every caller enters through a "no claims"/"no pages"
    // branch and never produced contradictions to begin with.
    ContradictionReport {
        contradictions: Vec::new(),
        pages_examined,
        clean: true,
        summary,
    }
}

fn fallback_summary(error_count: i64, warning_count: i64, pages_examined: &[i64]) -> String {
    let mut parts = Vec::new();
    if error_count == 0 && warning_count == 0 {
        parts.push(format!(
            "No contradictions found across {} page(s).",
            pages_examined.len()
        ));
    } else {
        if error_count > 0 {
            let noun = if error_count == 1 {
                "contradiction"
            } else {
                "contradictions"
            };
            parts.push(format!("Found {error_count} {noun}."));
        }
        if warning_count > 0 {
            let noun = if warning_count == 1 {
                "possible tension"
            } else {
                "possible tensions"
            };
            parts.push(format!("Found {warning_count} {noun}."));
        }
        parts.push(format!("Pages examined: {}.", pages_examined.len()));
    }
    parts.join(" ")
}
