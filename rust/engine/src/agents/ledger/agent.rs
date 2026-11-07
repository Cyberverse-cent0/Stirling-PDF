//! Ledger / Math Auditor agent. Port of `stirling.agents.ledger.agent` —
//! the `/api/v1/ai/math-auditor-agent/{examine,deliberate}` pipeline.
//!
//! Round 1 (`examine`): Examiner agent reads a `FolioManifest`, returns a
//! `Requisition` declaring which pages Java must extract.
//!
//! Round 2 (`audit`): deterministic-first pipeline per page —
//!   1. `ArithmeticScanner` on every folio (no LLM)
//!   2. Parallel fast-model calls: formula inference per table, figure
//!      extraction + statement verification per page (bounded semaphore)
//!   3. `FigureTracker` cross-page consistency (no LLM)
//!   4. Fast-model summary with deterministic fallback
//!   5. `Verdict` assembled programmatically
//!
//! Neither stage touches a PDF; all content arrives pre-extracted by Java.

use std::sync::Arc;

use futures_util::future::join_all;
use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::agents::ledger::prompts::{
    EXAMINER_SYSTEM_PROMPT, FIGURE_EXTRACTOR_PROMPT, STATEMENT_VERIFIER_PROMPT, SUMMARY_PROMPT,
    TABLE_FORMULA_PROMPT,
};
use crate::agents::ledger::validators::{ArithmeticScanner, FigureTracker, FormulaEvaluator};
use crate::agents::runtime::AgentError;
use crate::agents::structured::StructuredAgent;
use crate::contracts::ledger::{
    Discrepancy, DiscrepancyKind, Evidence, Folio, FolioManifest, Requisition, Severity, Verdict,
};
use crate::llm::client::LlmClient;

const LLM_CONCURRENCY: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtractedFigure {
    /// Normalised name, e.g. 'Total Revenue', 'VAT'.
    pub label: String,
    /// Numeric value as a string, e.g. '1200.00'.
    pub value: String,
    /// Original text from the document, e.g. '£1,200.00'.
    pub raw: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FigureExtractionResult {
    #[serde(default)]
    pub figures: Vec<ExtractedFigure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormulaCheck {
    /// Human-readable, e.g. 'Line Total = Qty × Unit Price'.
    pub description: String,
    /// 'col3 = col1 * col2' or 'cell(4,3) = sum(col3, 1-3)'.
    pub formula: String,
    /// 'each_row' | 'column_total' | 'single_cell'.
    pub scope: String,
    #[serde(default)]
    pub row_range: Option<Vec<i64>>,
    #[serde(default)]
    pub target_row: Option<i64>,
    #[serde(default)]
    pub target_col: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableFormulas {
    #[serde(default)]
    pub formulas: Vec<FormulaCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatementCheck {
    /// The exact text of the claim.
    pub claim: String,
    /// 'percentage_change' | 'comparison' | 'ratio' | 'trend' | 'average' | 'other'.
    pub verification: String,
    #[serde(default)]
    pub values_referenced: Vec<String>,
    /// What the calculation actually yields.
    pub expected_result: String,
    /// What the text claims.
    pub actual_claim: String,
    /// True if the claim is correct within tolerance.
    pub is_valid: bool,
    /// One-line working showing the calculation.
    pub explanation: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatementsResult {
    #[serde(default)]
    pub statements: Vec<StatementCheck>,
}

pub struct MathAuditorAgent {
    examiner: StructuredAgent,
    figure_extractor: StructuredAgent,
    table_analyser: StructuredAgent,
    statement_verifier: StructuredAgent,
    summary_agent: StructuredAgent,
    llm_semaphore: Arc<Semaphore>,
}

impl MathAuditorAgent {
    pub fn new(fast_model: LlmClient, chat_provider: &str) -> Self {
        Self {
            examiner: StructuredAgent::json::<Requisition>(
                fast_model.clone(),
                EXAMINER_SYSTEM_PROMPT,
                chat_provider,
            ),
            figure_extractor: StructuredAgent::json::<FigureExtractionResult>(
                fast_model.clone(),
                FIGURE_EXTRACTOR_PROMPT,
                chat_provider,
            ),
            table_analyser: StructuredAgent::json::<TableFormulas>(
                fast_model.clone(),
                TABLE_FORMULA_PROMPT,
                chat_provider,
            ),
            statement_verifier: StructuredAgent::json::<StatementsResult>(
                fast_model.clone(),
                STATEMENT_VERIFIER_PROMPT,
                chat_provider,
            ),
            summary_agent: StructuredAgent::text(fast_model, SUMMARY_PROMPT, chat_provider),
            llm_semaphore: Arc::new(Semaphore::new(LLM_CONCURRENCY)),
        }
    }

    /// Round 1: inspect a FolioManifest and declare the Requisition.
    pub async fn examine(&self, manifest: &FolioManifest) -> Result<Requisition, AgentError> {
        tracing::info!(
            session = %manifest.session_id,
            round = manifest.round,
            pages = manifest.page_count,
            "[math-auditor-agent] examining folios"
        );
        let prompt = format!(
            "Examine this folio manifest and declare your requisition:\n{}",
            serde_json::to_string(manifest).unwrap_or_default()
        );
        let req: Requisition = self.examiner.run(&prompt).await?;
        tracing::info!(
            session = %manifest.session_id,
            need_text = ?req.need_text,
            need_tables = ?req.need_tables,
            need_ocr = ?req.need_ocr,
            "[math-auditor-agent] requisition"
        );
        Ok(req)
    }

    /// Round 2: audit the evidence using the deterministic-first pipeline.
    pub async fn audit(
        &self,
        evidence: &Evidence,
        tolerance: Decimal,
    ) -> Result<Verdict, AgentError> {
        self.audit_inner(evidence, tolerance).await
    }

    async fn audit_inner(
        &self,
        evidence: &Evidence,
        tolerance: Decimal,
    ) -> Result<Verdict, AgentError> {
        tracing::info!(
            session = %evidence.session_id,
            round = evidence.round,
            folios = evidence.folios.len(),
            final_round = evidence.final_round,
            "[math-auditor-agent] auditing"
        );

        let mut all_discrepancies: Vec<Discrepancy> = Vec::new();
        let mut pages_examined: Vec<i64> = Vec::new();
        let mut figure_tracker = FigureTracker::new(tolerance);

        // Step 1: Arithmetic scanning (deterministic, instant).
        let arithmetic_scanner = ArithmeticScanner::new(tolerance);
        for folio in &evidence.folios {
            pages_examined.push(folio.page);
            let text = folio.readable_text();
            if !text.trim().is_empty() {
                let results = arithmetic_scanner.scan(folio.page, text);
                all_discrepancies.extend(results);
            }
        }

        // Step 2: Parallel LLM calls — formula inference per table, figure
        // extraction + statement verification per page. Independent, so all
        // fire concurrently under the semaphore.
        let formula_evaluator = FormulaEvaluator::new(tolerance);
        let folios_with_text: Vec<&Folio> = evidence
            .folios
            .iter()
            .filter(|f| !f.readable_text().trim().is_empty())
            .collect();

        let table_tasks: Vec<(i64, &str)> = evidence
            .folios
            .iter()
            .flat_map(|f| {
                f.tables
                    .iter()
                    .flatten()
                    .map(move |csv| (f.page, csv.as_str()))
            })
            .collect();

        tracing::info!(
            session = %evidence.session_id,
            formulas = table_tasks.len(),
            figures = folios_with_text.len(),
            "[math-auditor-agent] step 2: parallel LLM calls"
        );

        let formula_futs = table_tasks
            .iter()
            .map(|(_, csv)| self.throttled(self.infer_formulas(csv)));
        let figure_futs = folios_with_text
            .iter()
            .map(|f| self.throttled(self.extract_figures_for_page(f)));
        let statement_futs = folios_with_text
            .iter()
            .map(|f| self.throttled(self.verify_statements(f)));

        let (formula_results, figure_results, statement_results) = tokio::join!(
            join_all(formula_futs),
            join_all(figure_futs),
            join_all(statement_futs),
        );

        // Process formula results — inference failures already degraded to
        // an empty TableFormulas inside `infer_formulas`.
        for ((page, table_csv), formulas) in table_tasks.iter().zip(formula_results.iter()) {
            if formulas.formulas.is_empty() {
                continue;
            }
            for fc in &formulas.formulas {
                let checked = formula_evaluator.evaluate(
                    *page,
                    table_csv,
                    &fc.formula,
                    &fc.scope,
                    &fc.description,
                    fc.row_range.as_deref(),
                    fc.target_row,
                    fc.target_col,
                );
                all_discrepancies.extend(checked);
            }
        }

        // Process figure results — non-numeric values are skipped.
        for (folio, figures) in folios_with_text.iter().zip(figure_results.iter()) {
            for fig in figures {
                let cleaned = fig.value.replace(',', "").trim().to_string();
                let Ok(decimal_value) = cleaned.parse::<Decimal>() else {
                    tracing::warn!(
                        "[math-auditor-agent] skipping figure {:?} on page {}: non-numeric value {:?}",
                        fig.label,
                        folio.page,
                        fig.value
                    );
                    continue;
                };
                figure_tracker.record(&fig.label, decimal_value, folio.page, &fig.raw);
            }
        }

        // Process statement verification results.
        for (folio, stmts) in folios_with_text.iter().zip(statement_results.iter()) {
            for sc in &stmts.statements {
                if !sc.is_valid {
                    all_discrepancies.push(Discrepancy {
                        page: folio.page,
                        kind: DiscrepancyKind::Statement,
                        severity: Severity::Error,
                        description: format!("{}: {}", sc.claim, sc.explanation),
                        stated: sc.actual_claim.clone(),
                        expected: sc.expected_result.clone(),
                        context: sc.claim.clone(),
                    });
                }
            }
        }

        tracing::info!(
            session = %evidence.session_id,
            figures = figure_tracker.entry_count(),
            "[math-auditor-agent] step 2 complete"
        );

        // Step 3: Cross-page consistency — deterministic.
        all_discrepancies.extend(figure_tracker.conflicts());

        // Step 4: Summary — fast model with deterministic fallback.
        let total_tables: usize = evidence
            .folios
            .iter()
            .map(|f| f.tables.as_ref().map_or(0, Vec::len))
            .sum();
        let total_formulas_checked: usize = formula_results.iter().map(|f| f.formulas.len()).sum();
        let total_statements_checked: usize =
            statement_results.iter().map(|s| s.statements.len()).sum();
        let verification_stats = format!(
            "Verified: {} pages, {} tables ({total_formulas_checked} formulas), \
             {} figures tracked, {total_statements_checked} prose claims checked.",
            pages_examined.len(),
            total_tables,
            figure_tracker.entry_count(),
        );

        tracing::info!(
            session = %evidence.session_id,
            discrepancies = all_discrepancies.len(),
            "[math-auditor-agent] step 4: generating summary"
        );
        pages_examined.sort_unstable();
        let summary = self
            .generate_summary(
                &all_discrepancies,
                &pages_examined,
                &evidence.unauditable_pages,
                &verification_stats,
            )
            .await;

        // Step 5: Assemble Verdict.
        let error_count = all_discrepancies
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count();
        let verdict = Verdict {
            kind: Default::default(),
            session_id: evidence.session_id.clone(),
            discrepancies: all_discrepancies,
            pages_examined,
            rounds_taken: evidence.round,
            summary,
            clean: error_count == 0,
            unauditable_pages: evidence.unauditable_pages.clone(),
        };
        tracing::info!(
            session = %evidence.session_id,
            errors = verdict.error_count(),
            warnings = verdict.warning_count(),
            clean = verdict.clean,
            "[math-auditor-agent] verdict"
        );
        Ok(verdict)
    }

    /// Wrap a future with the LLM concurrency semaphore.
    async fn throttled<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future,
    {
        let _permit = self.llm_semaphore.acquire().await;
        fut.await
    }

    /// Ask the fast model to infer verifiable formulas from a CSV table.
    /// Agent failures degrade to an empty result, matching Python's
    /// `except AgentRunError` fallback.
    async fn infer_formulas(&self, table_csv: &str) -> TableFormulas {
        match self
            .table_analyser
            .run::<TableFormulas>(&format!("CSV table:\n{table_csv}"))
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    "[math-auditor-agent] formula inference failed, skipping table: {e}"
                );
                TableFormulas::default()
            }
        }
    }

    async fn verify_statements(&self, folio: &Folio) -> StatementsResult {
        let text = folio.readable_text();
        if text.trim().is_empty() {
            return StatementsResult::default();
        }
        let mut prompt = format!("Page {} text:\n{text}", folio.page + 1);
        if let Some(tables) = &folio.tables {
            if !tables.is_empty() {
                prompt.push_str("\n\nTable data on this page:\n");
                for (i, csv) in tables.iter().enumerate() {
                    prompt.push_str(&format!("\nTable {}:\n{csv}", i + 1));
                }
            }
        }
        match self
            .statement_verifier
            .run::<StatementsResult>(&prompt)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "[math-auditor-agent] statement verification failed for page {}: {e}",
                    folio.page
                );
                StatementsResult::default()
            }
        }
    }

    async fn extract_figures_for_page(&self, folio: &Folio) -> Vec<ExtractedFigure> {
        let text = folio.readable_text();
        if text.trim().is_empty() {
            return Vec::new();
        }
        let prompt = format!("Page {} text:\n{text}", folio.page + 1);
        match self
            .figure_extractor
            .run::<FigureExtractionResult>(&prompt)
            .await
        {
            Ok(r) => r.figures,
            Err(e) => {
                tracing::warn!(
                    "[math-auditor-agent] figure extraction failed for page {}, skipping: {e}",
                    folio.page
                );
                Vec::new()
            }
        }
    }

    async fn generate_summary(
        &self,
        discrepancies: &[Discrepancy],
        pages_examined: &[i64],
        unauditable_pages: &[i64],
        verification_stats: &str,
    ) -> String {
        let error_count = discrepancies
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count();
        let warning_count = discrepancies
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count();

        let mut prompt = format!(
            "{verification_stats}\n\
             Errors: {error_count}, Warnings: {warning_count}, \
             Pages examined: {}, \
             Unauditable pages: {}.\n",
            pages_examined.len(),
            if unauditable_pages.is_empty() {
                "none".to_string()
            } else {
                format!("{unauditable_pages:?}")
            },
        );
        if !discrepancies.is_empty() {
            prompt.push_str("Discrepancies:\n");
            for d in discrepancies {
                let severity = match d.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                };
                prompt.push_str(&format!(
                    "  - [{severity}] p{}: {}\n",
                    d.page + 1,
                    d.description
                ));
            }
        }

        match self.summary_agent.run_text(&prompt).await {
            Ok(summary) => summary,
            Err(e) => {
                tracing::warn!(
                    "[math-auditor-agent] summary generation failed, using fallback: {e}"
                );
                Self::fallback_summary(
                    error_count,
                    warning_count,
                    pages_examined,
                    unauditable_pages,
                )
            }
        }
    }

    fn fallback_summary(
        error_count: usize,
        warning_count: usize,
        pages_examined: &[i64],
        unauditable_pages: &[i64],
    ) -> String {
        let mut parts = Vec::new();
        if error_count == 0 && warning_count == 0 {
            parts.push(format!(
                "No mathematical errors found across {} pages.",
                pages_examined.len()
            ));
        } else {
            if error_count > 0 {
                parts.push(format!(
                    "Found {error_count} error{}.",
                    if error_count == 1 { "" } else { "s" }
                ));
            }
            if warning_count > 0 {
                parts.push(format!(
                    "Found {warning_count} warning{}.",
                    if warning_count == 1 { "" } else { "s" }
                ));
            }
        }
        if !unauditable_pages.is_empty() {
            let pages = unauditable_pages
                .iter()
                .map(|p| (p + 1).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!(
                "Pages {pages} could not be audited (OCR unavailable)."
            ));
        }
        parts.join(" ")
    }
}
