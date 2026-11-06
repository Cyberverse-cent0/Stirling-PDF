//! PDF Create Agent — chunked multi-agent pipeline. Port of
//! `stirling.agents.pdf_create.agent`.
//!
//! Flow:
//! 1. Meta planner (smart model) analyses the request → `DocumentMeta`
//!    (title, tone, shared terms, style, cannot_do_reason). No sections.
//! 2. Sections planner (smart model) reads the meta → `DocumentSections`.
//! 3. Python-side assembly into `DocumentPlan`, then grouping into chunks
//!    under the output-token ceiling.
//! 4. Section writers (smart model) run in parallel, bounded by a semaphore.
//! 5. Sections collected in plan order → `GeneratedDocument`.
//! 6. Emitted as structured fields on an `ADD`-style plan step; the LLM
//!    never writes HTML.
//!
//! The planner is split into two calls (meta then sections) so each output
//! schema stays small enough for grammar compilation on all model tiers.

use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::agents::structured::StructuredAgent;
use crate::contracts::common::{format_conversation_history, ToolOperationStep};
use crate::contracts::orchestrator::OrchestratorRequest;
use crate::contracts::pdf_create::{
    DocumentMeta, DocumentPlan, DocumentSection, DocumentSections, GeneratedDocument,
    PdfCreateOrchestrateResponse, PlannedSection, SectionDepth, SectionType, WrittenSections,
};
use crate::contracts::pdf_edit::{EditCannotDoPayload, EditPlanPayload};
use crate::llm::client::LlmClient;
use crate::models::tool_models::AgentToolId;
use crate::models::ToolId;
use crate::services::language::language_directive;

/// Conservative per-section token estimates mapped from planner-assigned depth.
fn depth_tokens(depth: SectionDepth) -> usize {
    match depth {
        SectionDepth::Brief => 250,
        SectionDepth::Standard => 550,
        SectionDepth::Detailed => 1200,
    }
}

/// Maximum output tokens per writer call — stays well below the quality
/// cliff (~4k).
const CHUNK_CEILING: usize = 3000;

/// Cap on simultaneous writer calls so a large document doesn't open a
/// burst of LLM connections and trip provider rate limits.
const MAX_PARALLEL_WRITERS: usize = 10;

struct Chunk {
    /// 0-based chunk index, used for logging only — `asyncio.gather`
    /// ordering is preserved by collecting futures in order.
    index: usize,
    sections: Vec<PlannedSection>,
    /// One-line summaries of neighbouring chunks, passed to writers as
    /// read-only context so they open/close sections naturally.
    context_before: Option<String>,
    context_after: Option<String>,
}

fn describe_sections(sections: &[PlannedSection]) -> String {
    sections
        .iter()
        .map(|s| format!("\"{}\" ({})", s.heading, section_type_label(s.section_type)))
        .collect::<Vec<_>>()
        .join("; ")
}

fn section_type_label(t: SectionType) -> &'static str {
    match t {
        SectionType::Text => "text",
        SectionType::KeyValue => "key_value",
        SectionType::LineItems => "line_items",
        SectionType::BulletList => "bullet_list",
        SectionType::Signature => "signature",
    }
}

fn depth_label(d: SectionDepth) -> &'static str {
    match d {
        SectionDepth::Brief => "brief",
        SectionDepth::Standard => "standard",
        SectionDepth::Detailed => "detailed",
    }
}

/// Group planned sections into chunks each under [`CHUNK_CEILING`] output
/// tokens. Section boundaries are atomic — a section is never split; a
/// single oversized section gets its own chunk.
fn make_chunks(sections: &[PlannedSection]) -> Vec<Chunk> {
    if sections.is_empty() {
        return Vec::new();
    }
    let mut groups: Vec<Vec<PlannedSection>> = Vec::new();
    let mut current: Vec<PlannedSection> = Vec::new();
    let mut current_tokens = 0usize;
    for section in sections {
        let cost = depth_tokens(section.depth);
        if !current.is_empty() && current_tokens + cost > CHUNK_CEILING {
            groups.push(std::mem::take(&mut current));
            current_tokens = cost;
        } else {
            current_tokens += cost;
        }
        current.push(section.clone());
    }
    if !current.is_empty() {
        groups.push(current);
    }

    let mut chunks = Vec::with_capacity(groups.len());
    for (i, group) in groups.iter().enumerate() {
        chunks.push(Chunk {
            index: i,
            sections: group.clone(),
            context_before: (i > 0).then(|| describe_sections(&groups[i - 1])),
            context_after: (i + 1 < groups.len()).then(|| describe_sections(&groups[i + 1])),
        });
    }
    chunks
}

const META_PLANNER_SYSTEM_PROMPT: &str = "You are a document planner. Your job is Step 1 of 2: produce the document header — NOT the\nsection list (that comes in Step 2) and NOT any body text (section writers handle that).\n\nAnalyse the user's request and produce a DocumentMeta with:\n\n- title, subtitle (if appropriate), reference_number (only if the user supplies one explicitly)\n\n- tone_brief: one sentence describing register and style\n  (e.g. \"Formal legal language, third person, present tense.\" or\n  \"Professional business tone, active voice.\")\n\n- shared_terms: consistent names for key entities AND ground-truth facts used throughout\n  the document. Two rules:\n  1. Capture EVERY value the user states explicitly that could be referenced in more than\n     one section. This includes — but is not limited to:\n       · Named parties, organisations, products, or systems\n       · Numeric values: amounts, quantities, percentages, durations, limits\n       · Identifiers: version numbers, reference codes, model names\n       · Dates and time periods\n       · Units of measure or currency\n  2. For any fact that will appear in two or more sections and that the user did NOT specify\n     (e.g. a default time, a standard rate, a typical threshold), assign ONE specific value\n     here. Do NOT let multiple writers independently invent the same fact.\n  Examples: {\"the Agreement\": \"this Non-Disclosure Agreement\", \"the Client\": \"Acme Corp\",\n             \"contract value\": \"£120,000\", \"notice period\": \"30 days\"}\n\n- document_context: a single sentence anchoring the temporal or versioning context of the\n  document, if the user provides one. Leave empty if the user provides no such context.\n\n- style_primary_color: accent and heading colour. Set ONLY when the user explicitly names a\n  colour or colour scheme (e.g. \"make it red\", \"use navy blue\"). Express it as a 6-digit hex\n  code in #RRGGBB format (map any named colour to its hex value yourself, e.g. \"navy\" →\n  \"#000080\"). No other format is accepted. Leave null if no colour is stated.\n- style_background_color: page background colour, same #RRGGBB format. Set only if explicitly\n  requested.\n- style_body_text_color: body text colour, same #RRGGBB format. Set only if explicitly\n  requested.\n\n- cannot_do_reason: set this ONLY when the request is not asking to create a document at all\n  (e.g. a question, a greeting, an edit request to an existing document). Never set it\n  because the document is large, complex, or technically detailed. Leave null otherwise.\n\nRULES:\n1. Extract ALL information the user provides. Do not invent content.\n2. Do not produce any sections — that is Step 2.\n";

const SECTIONS_PLANNER_SYSTEM_PROMPT: &str = "You are a document planner. Your job is Step 2 of 2: produce the ordered section list for\na document whose header has already been decided. Do NOT write any body text.\n\nYou will be given:\n  - The document meta (title, tone, shared terms, etc.) produced in Step 1\n  - The original user request\n\nProduce a DocumentSections with an ordered list of PlannedSection objects.\n\nFor each section choose:\n  type — the most appropriate section type:\n    text       — prose paragraphs (narrative, obligations, terms, descriptions)\n    key_value  — labelled fields (parties, dates, metadata, identifiers)\n    line_items — tables with column headers (expenses, schedules, item lists)\n    bullet_list — unordered items (requirements, responsibilities, definitions)\n    signature  — sign-off blocks for named parties or roles\n\n  depth — honest estimate of content volume:\n    brief    (~250 tokens) — 1-2 items, a short paragraph, or a small table\n    standard (~550 tokens) — a few paragraphs, a medium table, or a moderate list\n    detailed (~1200 tokens) — long clauses, complex multi-row tables, or dense content\n\n  key_points — specific points this section MUST cover, taken directly from the user's input.\n    These are instructions to the writer, not summaries. Be precise and complete.\n    Every fact, name, date, amount, and requirement the user provides must appear somewhere.\n    For large documents, include enough key_points that the writer can produce substantial\n    content.\n\nRULES:\n1. Extract ALL information the user provides. Do not invent content.\n2. Assign depth honestly — for a long detailed document most sections will be detailed.\n3. For large documents, produce as many sections as needed — there is no section count limit.\n4. Use the shared_terms from the meta exactly when writing key_points.\n";

const WRITER_SYSTEM_PROMPT: &str = "You are a section writer for a structured document.\nWrite ONLY the sections assigned to you — no extras, no merging, no skipping.\n\nSECTION TYPES — produce sections of exactly the requested type:\n  text       — prose paragraphs. Use \\n\\n between paragraphs.\n  key_value  — list of (label, value) pairs. Labels ≤ 5 words. Values verbatim from the data.\n  line_items — table. Every row must have exactly as many cells as there are columns.\n  bullet_list — flat list of items.\n  signature  — list of signatory names/roles.\n\nRULES:\n1. Write ONLY the sections in your assignment list, in the order given.\n2. Cover every key_point listed for each section. Do not omit any.\n3. Use the shared_terms exactly — no paraphrasing or substituting alternatives.\n   Shared terms are ground truth. If your general knowledge or a common default would\n   produce a different value (e.g. a different duration, amount, date, or version number),\n   the shared term takes precedence. This applies everywhere in the document, including\n   boilerplate, FAQ, and summary sections.\n4. Match the depth for each section: brief = concise, standard = moderate, \\ndetailed = thorough.\n5. Maintain the document's tone throughout.\n6. Do not reference other sections by number (e.g. \"as defined in Section 3\").\n7. If a document_context is provided, use it to anchor any dates, versions, or time\n   references you generate. Do not invent a different temporal or versioning context.\n";

fn build_sections_prompt(meta: &DocumentMeta, user_request: &str, history: &str) -> String {
    let mut lines = vec![
        "Document meta from Step 1:".to_string(),
        format!("  Title: {}", meta.title),
        format!("  Tone: {}", meta.tone_brief),
    ];
    if let Some(subtitle) = &meta.subtitle {
        lines.push(format!("  Subtitle: {subtitle}"));
    }
    if !meta.document_context.is_empty() {
        lines.push(format!("  Document context: {}", meta.document_context));
    }
    if !meta.shared_terms.is_empty() {
        lines.push("  Shared terms:".to_string());
        for (term, referent) in &meta.shared_terms {
            lines.push(format!("    {term} → {referent}"));
        }
    }
    lines.push(format!("\nConversation history:\n{history}"));
    lines.push(format!("\nUser request: {user_request}"));
    lines.push(format!("\n{}", language_directive()));
    lines.join("\n")
}

fn build_writer_prompt(plan: &DocumentPlan, chunk: &Chunk) -> String {
    let mut lines = vec![
        format!("Document: {}", plan.title),
        format!("Tone: {}", plan.tone_brief),
    ];
    if !plan.document_context.is_empty() {
        lines.push(format!("Document context: {}", plan.document_context));
    }
    if !plan.shared_terms.is_empty() {
        lines.push(
            "Ground-truth facts and shared terms (use exactly — these override defaults):"
                .to_string(),
        );
        for (term, referent) in &plan.shared_terms {
            lines.push(format!("  {term} → {referent}"));
        }
    }
    if let Some(before) = &chunk.context_before {
        lines.push(format!("\nThe sections BEFORE yours cover: {before}"));
    }
    if let Some(after) = &chunk.context_after {
        lines.push(format!("The sections AFTER yours cover: {after}"));
    }
    lines.push(format!(
        "\nWrite these {} section(s) in order:",
        chunk.sections.len()
    ));
    for (i, s) in chunk.sections.iter().enumerate() {
        lines.push(format!("\n--- Section {} ---", i + 1));
        lines.push(format!("Heading: {}", s.heading));
        lines.push(format!("Type: {}", section_type_label(s.section_type)));
        lines.push(format!("Depth: {}", depth_label(s.depth)));
        lines.push("Key points to cover:".to_string());
        for point in &s.key_points {
            lines.push(format!("  - {point}"));
        }
    }
    lines.push(format!("\n{}", language_directive()));
    lines.join("\n")
}

fn safe_filename(title: &str) -> String {
    // Python: slug = re.sub(r"[^\w\s-]", "", title.lower());
    //        slug = re.sub(r"[\s_-]+", "-", slug).strip("-")
    // `\w` is unicode-aware in Python — keep alphanumerics plus `_`.
    // Disallowed characters are dropped, not replaced.
    let lower = title.to_lowercase();
    let stripped: String = lower
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || c.is_whitespace() || *c == '-')
        .collect();
    // Collapse runs of whitespace/_/- into a single '-', trim '-'.
    let mut slug = String::new();
    let mut last_dash = true; // leading run collapses away
    for c in stripped.chars() {
        if c.is_whitespace() || c == '_' || c == '-' {
            if !last_dash {
                slug.push('-');
                last_dash = true;
            }
        } else {
            slug.push(c);
            last_dash = false;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    let slug: String = slug.chars().take(60).collect();
    let slug = slug.trim_end_matches('-');
    format!("{}.pdf", if slug.is_empty() { "document" } else { slug })
}

pub struct PdfCreateAgent {
    meta_planner: StructuredAgent,
    sections_planner: StructuredAgent,
    writer: StructuredAgent,
}

impl PdfCreateAgent {
    pub fn new(smart_model: LlmClient, chat_provider: &str) -> Self {
        Self {
            meta_planner: StructuredAgent::json::<DocumentMeta>(
                smart_model.clone(),
                META_PLANNER_SYSTEM_PROMPT,
                chat_provider,
            ),
            sections_planner: StructuredAgent::json::<DocumentSections>(
                smart_model.clone(),
                SECTIONS_PLANNER_SYSTEM_PROMPT,
                chat_provider,
            ),
            writer: StructuredAgent::json::<WrittenSections>(
                smart_model,
                WRITER_SYSTEM_PROMPT,
                chat_provider,
            ),
        }
    }

    pub async fn orchestrate(
        &self,
        request: &OrchestratorRequest,
    ) -> Result<PdfCreateOrchestrateResponse, crate::agents::runtime::AgentError> {
        let history = format_conversation_history(&request.conversation_history);

        // Phase 1: plan meta.
        tracing::info!("[pdf-create] phase 1/6: planning document meta");
        let meta_prompt = format!(
            "Conversation history:\n{history}\n\nUser request: {}\n\n{}",
            request.user_message,
            language_directive(),
        );
        let meta: DocumentMeta = self.meta_planner.run(&meta_prompt).await?;

        if let Some(reason) = &meta.cannot_do_reason {
            tracing::info!("[pdf-create] cannot_do: {reason}");
            return Ok(PdfCreateOrchestrateResponse::CannotDo(
                EditCannotDoPayload {
                    reason: reason.clone(),
                },
            ));
        }
        tracing::info!(
            "[pdf-create] meta: title={:?} tone={:?}",
            meta.title,
            meta.tone_brief
        );

        // Phase 2: plan sections.
        tracing::info!("[pdf-create] phase 2/6: planning sections");
        let sections_prompt = build_sections_prompt(&meta, &request.user_message, &history);
        let planned: DocumentSections = self.sections_planner.run(&sections_prompt).await?;

        if planned.sections.is_empty() {
            tracing::info!("[pdf-create] sections planner returned empty sections");
            return Ok(PdfCreateOrchestrateResponse::CannotDo(
                EditCannotDoPayload {
                    reason: "No document sections could be planned from the request.".to_string(),
                },
            ));
        }

        let plan = DocumentPlan::assemble(meta, planned);

        // Phase 3: chunk.
        let chunks = make_chunks(&plan.sections);
        tracing::info!(
            "[pdf-create] phase 3/6: chunked — sections={} chunks={}",
            plan.sections.len(),
            chunks.len()
        );

        // Phase 4: write in parallel, bounded. Futures are created in chunk
        // order so results stay in plan order (gather semantics).
        tracing::info!(
            "[pdf-create] phase 4/6: writing {} chunk(s) in parallel",
            chunks.len()
        );
        let total_chunks = chunks.len();
        let semaphore = Arc::new(Semaphore::new(MAX_PARALLEL_WRITERS));
        let mut handles = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let prompt = build_writer_prompt(&plan, chunk);
            let writer = &self.writer;
            let sem = semaphore.clone();
            let index = chunk.index;
            handles.push(async move {
                let _permit = sem.acquire().await;
                let result: WrittenSections = writer.run(&prompt).await?;
                tracing::info!(
                    "[pdf-create] chunk {}/{} wrote {} sections",
                    index + 1,
                    total_chunks,
                    result.sections.len()
                );
                Ok::<WrittenSections, crate::agents::runtime::AgentError>(result)
            });
        }
        let mut written_chunks = Vec::with_capacity(handles.len());
        for handle in handles {
            written_chunks.push(handle.await?);
        }

        // Phase 5: assemble in plan order.
        let mut all_sections: Vec<DocumentSection> = Vec::new();
        for written in &written_chunks {
            all_sections.extend(written.sections.iter().cloned());
        }
        tracing::info!(
            "[pdf-create] phase 5/6: assembled {} sections",
            all_sections.len()
        );

        let doc = GeneratedDocument {
            title: plan.title.clone(),
            subtitle: plan.subtitle.clone(),
            reference_number: plan.reference_number.clone(),
            style: plan.style.clone(),
            sections: all_sections,
        };

        // Phase 6: emit.
        let filename = safe_filename(&plan.title);
        tracing::info!(
            "[pdf-create] done — filename={:?} sections={}",
            filename,
            doc.sections.len()
        );
        // CreatePdfFromHtmlAgentParams(document=doc.model_dump_json(), filename=...)
        let parameters = serde_json::json!({
            "document": serde_json::to_string(&doc).unwrap_or_default(),
            "filename": filename,
        });
        Ok(PdfCreateOrchestrateResponse::Plan(EditPlanPayload {
            summary: format!("Created {}", plan.title),
            rationale: None,
            steps: vec![ToolOperationStep {
                tool: ToolId::Agent(AgentToolId::CreatePdfFromHtmlAgent),
                parameters,
            }],
            resume_with: None,
        }))
    }
}

