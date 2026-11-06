//! Whole-document reader capability. Port of
//! `stirling.agents.shared.whole_doc_reader`: a `read_full_document` tool
//! that reads every page of the attached documents through
//! [`ChunkedReasoner`] and returns question-relevant notes. Companion to
//! `RagCapability` — targeted retrieval vs whole-document reading; the agent
//! picks per tool description.
//!
//! One instance per agent run. The per-run read budget is enforced by the
//! tool's `visible` predicate (Python's `prepare` hook): after
//! `max_reads` calls the tool disappears from the toolset.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::Value;

use crate::agents::chunked_reasoner::{ChunkedReasoner, ChunkedReasonerError};
use crate::agents::runtime::{AgentTool, ToolContext};
use crate::contracts::common::AiFile;
use crate::documents::service::DocumentService;
use crate::models::PrincipalId;

/// Cap on per-run calls: one pass already reads every page, so a second call
/// is almost always the model second-guessing itself. `search_knowledge` is
/// the escape hatch for follow-ups.
pub const DEFAULT_MAX_READS: usize = 1;

const READ_FULL_DOCUMENT_PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "A focused description of what to extract from the documents, phrased so a worker reading just one slice can decide what's relevant to the user's question."
    }
  },
  "required": ["query"],
  "additionalProperties": false
}"#;

const INSTRUCTIONS_TEMPLATE: &str =
    "You have a 'read_full_document' tool that reads every page of \
{names} in parallel and returns notes relevant to a query. \
Use it when answering requires seeing the whole document end-to-end \
(summaries, aggregations, comparisons across sections). One call \
already reads everything; phrase the query to cover all the angles \
you need in a single pass. For follow-ups or specific lookups use \
'search_knowledge', which is cheaper and targeted.";

pub struct WholeDocReaderCapability {
    documents: Arc<DocumentService>,
    files: Vec<AiFile>,
    principals: Vec<PrincipalId>,
    reasoner: Arc<ChunkedReasoner>,
    max_reads: usize,
    read_count: Arc<AtomicUsize>,
}

impl WholeDocReaderCapability {
    pub fn new(
        documents: Arc<DocumentService>,
        files: Vec<AiFile>,
        principals: Vec<PrincipalId>,
        reasoner: Arc<ChunkedReasoner>,
        max_reads: usize,
    ) -> Self {
        Self {
            documents,
            files,
            principals,
            reasoner,
            max_reads,
            read_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Dynamic instruction naming the attached files (or a generic phrase
    /// when none are attached).
    pub fn instructions(&self) -> String {
        let names = if self.files.is_empty() {
            "the attached documents".to_string()
        } else {
            self.files
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        INSTRUCTIONS_TEMPLATE.replace("{names}", &names)
    }

    /// The `read_full_document` tool, or `None`-equivalent visibility once the
    /// budget is spent (the tool stays constructed; `visible` hides it).
    pub fn tool(&self) -> AgentTool {
        let documents = self.documents.clone();
        let files = self.files.clone();
        let principals = self.principals.clone();
        let reasoner = self.reasoner.clone();
        let read_count = self.read_count.clone();
        let budget_count = self.read_count.clone();
        let max_reads = self.max_reads;
        AgentTool::new(
            "read_full_document",
            "Read every page of the attached documents and return notes relevant to the query. \
             Use this when answering needs the whole document end-to-end - summaries, \
             aggregations like 'largest number' or 'shortest chapter', or comparisons \
             across sections. Slow and expensive (one fast-model call per slice per \
             document); prefer search_knowledge for targeted lookups.",
            serde_json::from_str(READ_FULL_DOCUMENT_PARAMETERS).unwrap(),
            move |args: Value, _ctx: &ToolContext| {
                let documents = documents.clone();
                let files = files.clone();
                let principals = principals.clone();
                let reasoner = reasoner.clone();
                let read_count = read_count.clone();
                Box::pin(async move {
                    read_count.fetch_add(1, Ordering::SeqCst);
                    let query = args
                        .get("query")
                        .and_then(Value::as_str)
                        .ok_or("query is required")?
                        .to_string();
                    read_full_document(&documents, &files, &principals, &reasoner, &query).await
                })
            },
        )
        .visible(move || budget_count.load(Ordering::SeqCst) < max_reads)
    }
}

async fn read_full_document(
    documents: &DocumentService,
    files: &[AiFile],
    principals: &[PrincipalId],
    reasoner: &ChunkedReasoner,
    query: &str,
) -> Result<String, String> {
    if files.is_empty() {
        return Ok("No documents attached to read.".to_string());
    }

    let mut sections = Vec::new();
    for file in files {
        let pages = documents
            .read_pages(&file.id, principals, None)
            .await
            .map_err(|e| e.to_string())?;
        if pages.is_empty() {
            tracing::info!(
                file = %file.name,
                id = %file.id,
                "[whole-doc-reader] no stored pages; skipping"
            );
            continue;
        }
        let notes = reasoner
            .gather_notes(&pages, query)
            .await
            .map_err(reasoner_error)?;
        if notes.is_empty() {
            continue;
        }
        sections.push(format!(
            "=== {} ===\n{}",
            file.name,
            ChunkedReasoner::format_notes(&notes)
        ));
    }

    if sections.is_empty() {
        return Ok("Could not read any document content.".to_string());
    }
    tracing::info!(
        query,
        files = files.len(),
        chars = sections.iter().map(String::len).sum::<usize>(),
        "[whole-doc-reader] read done"
    );
    Ok(sections.join("\n\n"))
}

fn reasoner_error(e: ChunkedReasonerError) -> String {
    e.to_string()
}
