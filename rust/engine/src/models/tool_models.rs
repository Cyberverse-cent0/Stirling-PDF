//! Tool identifiers. `ToolEndpoint` values are the Java controller paths;
//! `AgentToolId` covers engine-internal agent tools hidden from the OpenAPI
//! spec. Port of `stirling.models.tool_models` / `agent_tool_models` — the
//! per-endpoint parameter models are generated separately (typify) and land
//! in `tool_params.rs`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Every PDF tool endpoint the planner may emit, keyed by its API path.
/// Unknown paths are dropped on ingest (`enabled_endpoints`), mirroring
/// `drop_unknown_tool_endpoints`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum ToolEndpoint {
    #[serde(rename = "/api/v1/convert/cbr/pdf")]
    CbrToPdf,
    #[serde(rename = "/api/v1/convert/cbz/pdf")]
    CbzToPdf,
    #[serde(rename = "/api/v1/convert/ebook/pdf")]
    EbookToPdf,
    #[serde(rename = "/api/v1/convert/eml/pdf")]
    EmlToPdf,
    #[serde(rename = "/api/v1/convert/file/pdf")]
    FileToPdf,
    #[serde(rename = "/api/v1/convert/html/pdf")]
    HtmlToPdf,
    #[serde(rename = "/api/v1/convert/img/pdf")]
    ImgToPdf,
    #[serde(rename = "/api/v1/convert/markdown/pdf")]
    MarkdownToPdf,
    #[serde(rename = "/api/v1/convert/pdf/cbr")]
    PdfToCbr,
    #[serde(rename = "/api/v1/convert/pdf/cbz")]
    PdfToCbz,
    #[serde(rename = "/api/v1/convert/pdf/csv")]
    PdfToCsv,
    #[serde(rename = "/api/v1/convert/pdf/epub")]
    PdfToEpub,
    #[serde(rename = "/api/v1/convert/pdf/html")]
    PdfToHtml,
    #[serde(rename = "/api/v1/convert/pdf/img")]
    PdfToImg,
    #[serde(rename = "/api/v1/convert/pdf/markdown")]
    PdfToMarkdown,
    #[serde(rename = "/api/v1/convert/pdf/pdfa")]
    PdfToPdfa,
    #[serde(rename = "/api/v1/convert/pdf/presentation")]
    PdfToPresentation,
    #[serde(rename = "/api/v1/convert/pdf/text")]
    PdfToText,
    #[serde(rename = "/api/v1/convert/pdf/ua")]
    PdfToUa,
    #[serde(rename = "/api/v1/convert/pdf/vector")]
    PdfToVector,
    #[serde(rename = "/api/v1/convert/pdf/word")]
    PdfToWord,
    #[serde(rename = "/api/v1/convert/pdf/xlsx")]
    PdfToXlsx,
    #[serde(rename = "/api/v1/convert/pdf/xml")]
    PdfToXml,
    #[serde(rename = "/api/v1/convert/svg/pdf")]
    SvgToPdf,
    #[serde(rename = "/api/v1/convert/url/pdf")]
    UrlToPdf,
    #[serde(rename = "/api/v1/convert/vector/pdf")]
    VectorToPdf,
    #[serde(rename = "/api/v1/general/booklet-imposition")]
    BookletImposition,
    #[serde(rename = "/api/v1/general/crop")]
    Crop,
    #[serde(rename = "/api/v1/general/edit-table-of-contents")]
    EditTableOfContents,
    #[serde(rename = "/api/v1/general/merge-pdfs")]
    MergePdfs,
    #[serde(rename = "/api/v1/general/multi-page-layout")]
    MultiPageLayout,
    #[serde(rename = "/api/v1/general/pdf-to-single-page")]
    PdfToSinglePage,
    #[serde(rename = "/api/v1/general/rearrange-pages")]
    RearrangePages,
    #[serde(rename = "/api/v1/general/remove-image-pdf")]
    RemoveImagePdf,
    #[serde(rename = "/api/v1/general/remove-pages")]
    RemovePages,
    #[serde(rename = "/api/v1/general/rotate-pdf")]
    RotatePdf,
    #[serde(rename = "/api/v1/general/scale-pages")]
    ScalePages,
    #[serde(rename = "/api/v1/general/split-by-size-or-count")]
    SplitBySizeOrCount,
    #[serde(rename = "/api/v1/general/split-for-poster-print")]
    SplitForPosterPrint,
    #[serde(rename = "/api/v1/general/split-pages")]
    SplitPages,
    #[serde(rename = "/api/v1/general/split-pdf-by-chapters")]
    SplitPdfByChapters,
    #[serde(rename = "/api/v1/general/split-pdf-by-sections")]
    SplitPdfBySections,
    #[serde(rename = "/api/v1/misc/add-comments")]
    AddComments,
    #[serde(rename = "/api/v1/misc/add-page-numbers")]
    AddPageNumbers,
    #[serde(rename = "/api/v1/misc/add-stamp")]
    AddStamp,
    #[serde(rename = "/api/v1/misc/auto-rename")]
    AutoRename,
    #[serde(rename = "/api/v1/misc/auto-rotate-pdf")]
    AutoRotatePdf,
    #[serde(rename = "/api/v1/misc/auto-split-pdf")]
    AutoSplitPdf,
    #[serde(rename = "/api/v1/misc/batch-process-attachments")]
    BatchProcessAttachments,
    #[serde(rename = "/api/v1/misc/compress-pdf")]
    CompressPdf,
    #[serde(rename = "/api/v1/misc/create-portfolio")]
    CreatePortfolio,
    #[serde(rename = "/api/v1/misc/delete-attachment")]
    DeleteAttachment,
    #[serde(rename = "/api/v1/misc/extract-attachments")]
    ExtractAttachments,
    #[serde(rename = "/api/v1/misc/extract-image-scans")]
    ExtractImageScans,
    #[serde(rename = "/api/v1/misc/extract-images")]
    ExtractImages,
    #[serde(rename = "/api/v1/misc/extract-single-attachment")]
    ExtractSingleAttachment,
    #[serde(rename = "/api/v1/misc/flatten")]
    Flatten,
    #[serde(rename = "/api/v1/misc/flatten-portfolio")]
    FlattenPortfolio,
    #[serde(rename = "/api/v1/misc/ocr-pdf")]
    OcrPdf,
    #[serde(rename = "/api/v1/misc/remove-blanks")]
    RemoveBlanks,
    #[serde(rename = "/api/v1/misc/rename-attachment")]
    RenameAttachment,
    #[serde(rename = "/api/v1/misc/repair")]
    Repair,
    #[serde(rename = "/api/v1/misc/replace-invert-pdf")]
    ReplaceInvertPdf,
    #[serde(rename = "/api/v1/misc/scanner-effect")]
    ScannerEffect,
    #[serde(rename = "/api/v1/misc/unlock-pdf-forms")]
    UnlockPdfForms,
    #[serde(rename = "/api/v1/misc/update-metadata")]
    UpdateMetadata,
    #[serde(rename = "/api/v1/security/accessibility-report")]
    AccessibilityReport,
    #[serde(rename = "/api/v1/security/add-password")]
    AddPassword,
    #[serde(rename = "/api/v1/security/add-watermark")]
    AddWatermark,
    #[serde(rename = "/api/v1/security/auto-redact")]
    AutoRedact,
    #[serde(rename = "/api/v1/security/redact")]
    Redact,
    #[serde(rename = "/api/v1/security/redact-execute")]
    RedactExecute,
    #[serde(rename = "/api/v1/security/remove-cert-sign")]
    RemoveCertSign,
    #[serde(rename = "/api/v1/security/remove-password")]
    RemovePassword,
    #[serde(rename = "/api/v1/security/sanitize-pdf")]
    SanitizePdf,
    #[serde(rename = "/api/v1/security/timestamp-pdf")]
    TimestampPdf,
}

impl ToolEndpoint {
    /// The endpoint path, for building Java-side URLs and prompt text.
    pub fn as_str(&self) -> &'static str {
        // serde's rename table is the single source; match keeps this cheap.
        match self {
            Self::CbrToPdf => "/api/v1/convert/cbr/pdf",
            Self::CbzToPdf => "/api/v1/convert/cbz/pdf",
            Self::EbookToPdf => "/api/v1/convert/ebook/pdf",
            Self::EmlToPdf => "/api/v1/convert/eml/pdf",
            Self::FileToPdf => "/api/v1/convert/file/pdf",
            Self::HtmlToPdf => "/api/v1/convert/html/pdf",
            Self::ImgToPdf => "/api/v1/convert/img/pdf",
            Self::MarkdownToPdf => "/api/v1/convert/markdown/pdf",
            Self::PdfToCbr => "/api/v1/convert/pdf/cbr",
            Self::PdfToCbz => "/api/v1/convert/pdf/cbz",
            Self::PdfToCsv => "/api/v1/convert/pdf/csv",
            Self::PdfToEpub => "/api/v1/convert/pdf/epub",
            Self::PdfToHtml => "/api/v1/convert/pdf/html",
            Self::PdfToImg => "/api/v1/convert/pdf/img",
            Self::PdfToMarkdown => "/api/v1/convert/pdf/markdown",
            Self::PdfToPdfa => "/api/v1/convert/pdf/pdfa",
            Self::PdfToPresentation => "/api/v1/convert/pdf/presentation",
            Self::PdfToText => "/api/v1/convert/pdf/text",
            Self::PdfToUa => "/api/v1/convert/pdf/ua",
            Self::PdfToVector => "/api/v1/convert/pdf/vector",
            Self::PdfToWord => "/api/v1/convert/pdf/word",
            Self::PdfToXlsx => "/api/v1/convert/pdf/xlsx",
            Self::PdfToXml => "/api/v1/convert/pdf/xml",
            Self::SvgToPdf => "/api/v1/convert/svg/pdf",
            Self::UrlToPdf => "/api/v1/convert/url/pdf",
            Self::VectorToPdf => "/api/v1/convert/vector/pdf",
            Self::BookletImposition => "/api/v1/general/booklet-imposition",
            Self::Crop => "/api/v1/general/crop",
            Self::EditTableOfContents => "/api/v1/general/edit-table-of-contents",
            Self::MergePdfs => "/api/v1/general/merge-pdfs",
            Self::MultiPageLayout => "/api/v1/general/multi-page-layout",
            Self::PdfToSinglePage => "/api/v1/general/pdf-to-single-page",
            Self::RearrangePages => "/api/v1/general/rearrange-pages",
            Self::RemoveImagePdf => "/api/v1/general/remove-image-pdf",
            Self::RemovePages => "/api/v1/general/remove-pages",
            Self::RotatePdf => "/api/v1/general/rotate-pdf",
            Self::ScalePages => "/api/v1/general/scale-pages",
            Self::SplitBySizeOrCount => "/api/v1/general/split-by-size-or-count",
            Self::SplitForPosterPrint => "/api/v1/general/split-for-poster-print",
            Self::SplitPages => "/api/v1/general/split-pages",
            Self::SplitPdfByChapters => "/api/v1/general/split-pdf-by-chapters",
            Self::SplitPdfBySections => "/api/v1/general/split-pdf-by-sections",
            Self::AddComments => "/api/v1/misc/add-comments",
            Self::AddPageNumbers => "/api/v1/misc/add-page-numbers",
            Self::AddStamp => "/api/v1/misc/add-stamp",
            Self::AutoRename => "/api/v1/misc/auto-rename",
            Self::AutoRotatePdf => "/api/v1/misc/auto-rotate-pdf",
            Self::AutoSplitPdf => "/api/v1/misc/auto-split-pdf",
            Self::BatchProcessAttachments => "/api/v1/misc/batch-process-attachments",
            Self::CompressPdf => "/api/v1/misc/compress-pdf",
            Self::CreatePortfolio => "/api/v1/misc/create-portfolio",
            Self::DeleteAttachment => "/api/v1/misc/delete-attachment",
            Self::ExtractAttachments => "/api/v1/misc/extract-attachments",
            Self::ExtractImageScans => "/api/v1/misc/extract-image-scans",
            Self::ExtractImages => "/api/v1/misc/extract-images",
            Self::ExtractSingleAttachment => "/api/v1/misc/extract-single-attachment",
            Self::Flatten => "/api/v1/misc/flatten",
            Self::FlattenPortfolio => "/api/v1/misc/flatten-portfolio",
            Self::OcrPdf => "/api/v1/misc/ocr-pdf",
            Self::RemoveBlanks => "/api/v1/misc/remove-blanks",
            Self::RenameAttachment => "/api/v1/misc/rename-attachment",
            Self::Repair => "/api/v1/misc/repair",
            Self::ReplaceInvertPdf => "/api/v1/misc/replace-invert-pdf",
            Self::ScannerEffect => "/api/v1/misc/scanner-effect",
            Self::UnlockPdfForms => "/api/v1/misc/unlock-pdf-forms",
            Self::UpdateMetadata => "/api/v1/misc/update-metadata",
            Self::AccessibilityReport => "/api/v1/security/accessibility-report",
            Self::AddPassword => "/api/v1/security/add-password",
            Self::AddWatermark => "/api/v1/security/add-watermark",
            Self::AutoRedact => "/api/v1/security/auto-redact",
            Self::Redact => "/api/v1/security/redact",
            Self::RedactExecute => "/api/v1/security/redact-execute",
            Self::RemoveCertSign => "/api/v1/security/remove-cert-sign",
            Self::RemovePassword => "/api/v1/security/remove-password",
            Self::SanitizePdf => "/api/v1/security/sanitize-pdf",
            Self::TimestampPdf => "/api/v1/security/timestamp-pdf",
        }
    }
}

/// Engine-emitted agent tools not present in the OpenAPI spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum AgentToolId {
    #[serde(rename = "/api/v1/ai/tools/math-auditor-agent")]
    MathAuditorAgent,
    #[serde(rename = "/api/v1/ai/tools/pdf-comment-agent")]
    PdfCommentAgent,
    #[serde(rename = "/api/v1/ai/tools/create-pdf-from-html-agent")]
    CreatePdfFromHtmlAgent,
}

impl AgentToolId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MathAuditorAgent => "/api/v1/ai/tools/math-auditor-agent",
            Self::PdfCommentAgent => "/api/v1/ai/tools/pdf-comment-agent",
            Self::CreatePdfFromHtmlAgent => "/api/v1/ai/tools/create-pdf-from-html-agent",
        }
    }
}

/// `AnyToolId` — a tool step can target a Java endpoint or an engine agent.
/// Both serialize as their path string, so untagged round-trips cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ToolId {
    Endpoint(ToolEndpoint),
    Agent(AgentToolId),
}

impl ToolId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Endpoint(e) => e.as_str(),
            Self::Agent(a) => a.as_str(),
        }
    }
}

/// Deserialize a list of endpoint strings, silently dropping unknown paths —
/// the Rust port of `drop_unknown_tool_endpoints` (Java/engine version drift
/// must not fail the request).
pub fn deserialize_endpoints<'de, D>(deserializer: D) -> Result<Vec<ToolEndpoint>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<String> = Vec::deserialize(deserializer)?;
    Ok(raw
        .iter()
        .filter_map(|s| serde_json::from_value(serde_json::Value::String(s.clone())).ok())
        .collect())
}
