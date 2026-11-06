//! Parallel chunked map primitive over long documents. Port of
//! `stirling.agents.shared.chunked_mapper`: pages are sliced into
//! character-budgeted chunks (page boundaries preserved), each chunk runs
//! through a caller-supplied structured extractor under a semaphore with a
//! per-worker timeout, and typed outputs come back in document order.
//!
//! Scheduling mirrors the Python `asyncio` version: `FuturesUnordered`
//! yields completions in finish order for progress events, worker failures
//! are logged and dropped, and dropping the `map_pages` future cancels every
//! in-flight extractor call (the frontend-disconnect path).

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use crate::agents::structured::StructuredAgent;
use crate::contracts::documents::Page;
use crate::contracts::ProgressEvent;
use crate::services::progress::emit_progress;

/// Per-page marker rendered by [`ChunkedMapper::format_chunk_content`].
/// Downstream extractor prompts reference this shape, so it lives here.
pub const PAGE_MARKER_TEMPLATE: &str = "[Page {n}]";

/// One chunk's typed extractor output plus the pages it covered. Workers
/// that failed produce no `ChunkOutput`; outputs sort by first page.
pub struct ChunkOutput<T> {
    pub pages: Vec<i64>,
    pub output: T,
    /// `"pages=3-7"` descriptor for logs and progress events.
    pub label: String,
}

struct MapperChunk {
    content: String,
    pages: Vec<i64>,
    label: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkedMapperError {
    #[error("chars_per_slice must be positive")]
    BadCharsPerSlice,
    #[error("concurrency must be positive")]
    BadConcurrency,
    #[error("worker_timeout must be positive")]
    BadTimeout,
    #[error("map_pages requires at least one page")]
    EmptyInput,
}

fn page_range_label(pages: &[Page]) -> String {
    match pages {
        [] => "pages=?".to_string(),
        [p] => format!("pages={}", p.page_number),
        ps => format!(
            "pages={}-{}",
            ps[0].page_number,
            ps[ps.len() - 1].page_number
        ),
    }
}

fn default_build_prompt(content: &str, query: &str) -> String {
    format!("User question:\n{query}\n\nContent:\n{content}")
}

fn zero_counts<T>(_: &T) -> (usize, usize) {
    (0, 0)
}

/// Generic parallel map: `pages -> Vec<T>` via a structured extractor agent.
/// Construct once per consuming agent; `T` is the extractor's output type.
pub struct ChunkedMapper<T> {
    agent: StructuredAgent,
    chars_per_slice: usize,
    worker_timeout: Duration,
    build_prompt: Arc<dyn Fn(&str, &str) -> String + Send + Sync>,
    summary_counts: Arc<dyn Fn(&T) -> (usize, usize) + Send + Sync>,
    semaphore: Arc<Semaphore>,
    _t: PhantomData<fn() -> T>,
}

impl<T> ChunkedMapper<T>
where
    T: DeserializeOwned + JsonSchema + Send + 'static,
{
    /// `agent` is the extractor (`Agent[None, T]` in Python terms).
    /// `build_prompt` defaults to `"User question:\n{query}\n\nContent:\n{content}"`;
    /// `summary_counts` fills the per-slice progress counters (default `(0,0)`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent: StructuredAgent,
        chars_per_slice: usize,
        concurrency: usize,
        worker_timeout: Duration,
        build_prompt: Option<Arc<dyn Fn(&str, &str) -> String + Send + Sync>>,
        summary_counts: Option<Arc<dyn Fn(&T) -> (usize, usize) + Send + Sync>>,
    ) -> Result<Self, ChunkedMapperError> {
        if chars_per_slice == 0 {
            return Err(ChunkedMapperError::BadCharsPerSlice);
        }
        if concurrency == 0 {
            return Err(ChunkedMapperError::BadConcurrency);
        }
        if worker_timeout.is_zero() {
            return Err(ChunkedMapperError::BadTimeout);
        }
        Ok(Self {
            agent,
            chars_per_slice,
            worker_timeout,
            build_prompt: build_prompt.unwrap_or_else(|| Arc::new(default_build_prompt)),
            summary_counts: summary_counts.unwrap_or_else(|| Arc::new(zero_counts::<T>)),
            semaphore: Arc::new(Semaphore::new(concurrency)),
            _t: PhantomData,
        })
    }

    pub fn chars_per_slice(&self) -> usize {
        self.chars_per_slice
    }

    pub fn worker_timeout(&self) -> Duration {
        self.worker_timeout
    }

    /// Shared with secondary scheduling loops (e.g. compression rounds) so
    /// they respect the same concurrency cap.
    pub fn semaphore(&self) -> Arc<Semaphore> {
        self.semaphore.clone()
    }

    /// Slice `pages`, run the extractor per slice in parallel, return outputs
    /// sorted by first covered page. Emits `WholeDocReadStarted` /
    /// `WholeDocSliceDone` / `WholeDocReadDone` progress events; worker
    /// failures are logged and dropped.
    pub async fn map_pages(
        &self,
        pages: &[Page],
        query: &str,
    ) -> Result<Vec<ChunkOutput<T>>, ChunkedMapperError> {
        if pages.is_empty() {
            return Err(ChunkedMapperError::EmptyInput);
        }

        let chunks: Vec<MapperChunk> = Self::slice_pages(pages, self.chars_per_slice)
            .iter()
            .map(|slice| Self::chunk_from_pages(slice))
            .collect();
        let slice_total = chunks.len() as i64;
        tracing::info!(
            query,
            pages = pages.len(),
            slices = slice_total,
            "[chunked-mapper] map start"
        );
        emit_progress(ProgressEvent::WholeDocReadStarted {
            question: query.to_string(),
            pages: pages.len() as i64,
            slices: slice_total,
        });

        let gather_start = Instant::now();
        let outputs = self.extract_chunks(chunks, query, slice_total).await;
        emit_progress(ProgressEvent::WholeDocReadDone {
            completed: outputs.len() as i64,
            slices: slice_total,
            duration_seconds: (gather_start.elapsed().as_secs_f64() * 100.0).round() / 100.0,
        });
        Ok(outputs)
    }

    async fn extract_chunks(
        &self,
        chunks: Vec<MapperChunk>,
        query: &str,
        total: i64,
    ) -> Vec<ChunkOutput<T>> {
        let mut pending = FuturesUnordered::new();
        for chunk in chunks {
            let agent = self.agent.clone();
            let semaphore = self.semaphore.clone();
            let timeout = self.worker_timeout;
            let build_prompt = self.build_prompt.clone();
            let prompt = build_prompt(&chunk.content, query);
            pending.push(async move {
                // Semaphore wait is unbounded; the timeout covers only the
                // model call, matching Python's `async with semaphore` +
                // `wait_for(run)` nesting.
                let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
                let start = Instant::now();
                let result = tokio::time::timeout(timeout, agent.run::<T>(&prompt)).await;
                (chunk, result, start.elapsed())
            });
        }

        let mut outputs = Vec::new();
        let mut completed = 0i64;
        let mut slowest: Option<(String, f64)> = None;
        while let Some((chunk, result, duration)) = pending.next().await {
            let output = match result {
                Ok(Ok(output)) => output,
                Ok(Err(e)) => {
                    tracing::warn!(chunk = %chunk.label, error = %e, "[chunked-mapper] chunk failed");
                    continue;
                }
                Err(_) => {
                    tracing::warn!(
                        chunk = %chunk.label,
                        timeout_ms = timeout_ms(self.worker_timeout),
                        "[chunked-mapper] chunk timed out"
                    );
                    continue;
                }
            };
            completed += 1;
            let secs = duration.as_secs_f64();
            if slowest.as_ref().is_none_or(|(_, s)| secs > *s) {
                slowest = Some((chunk.label.clone(), secs));
            }
            let (excerpts, facts) = (self.summary_counts)(&output);
            emit_progress(ProgressEvent::WholeDocSliceDone {
                completed,
                total,
                pages: chunk.label.clone(),
                duration_ms: duration.as_millis() as i64,
                excerpts: excerpts as i64,
                facts: facts as i64,
            });
            outputs.push(ChunkOutput {
                pages: chunk.pages,
                output,
                label: chunk.label,
            });
        }

        match &slowest {
            Some((label, secs)) => tracing::info!(
                completed,
                total,
                slowest = %label,
                secs = format!("{secs:.1}"),
                "[chunked-mapper] extraction done"
            ),
            None => tracing::info!(completed = 0, total, "[chunked-mapper] extraction done"),
        }
        outputs.sort_by_key(|o| o.pages.first().copied().unwrap_or(0));
        outputs
    }

    fn chunk_from_pages(pages: &[Page]) -> MapperChunk {
        MapperChunk {
            content: Self::format_chunk_content(pages),
            pages: pages.iter().map(|p| p.page_number).collect(),
            label: page_range_label(pages),
        }
    }

    /// Group consecutive pages into character-budgeted slices. A page is never
    /// split; an over-budget single page becomes its own slice.
    pub fn slice_pages(pages: &[Page], chars_per_slice: usize) -> Vec<Vec<Page>> {
        assert!(chars_per_slice > 0, "chars_per_slice must be positive");
        let mut slices: Vec<Vec<Page>> = Vec::new();
        let mut current: Vec<Page> = Vec::new();
        let mut current_chars = 0i64;
        for page in pages {
            if !current.is_empty() && current_chars + page.char_count > chars_per_slice as i64 {
                slices.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            current_chars += page.char_count;
            current.push(page.clone());
        }
        if !current.is_empty() {
            slices.push(current);
        }
        slices
    }

    /// Render pages as `[Page N]\n<text>` joined by blank lines.
    pub fn format_chunk_content(pages: &[Page]) -> String {
        pages
            .iter()
            .map(|p| {
                format!(
                    "{}\n{}",
                    PAGE_MARKER_TEMPLATE.replace("{n}", &p.page_number.to_string()),
                    p.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

fn timeout_ms(timeout: Duration) -> u64 {
    timeout.as_millis() as u64
}
