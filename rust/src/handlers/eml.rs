//! `POST /api/v1/convert/eml/pdf` — MIME email → styled HTML → PDF via
//! WeasyPrint, mirroring Java's `EmlToPdf`/`EmlParser`/`EmlProcessingUtils`.
//! MSG (OLE2) files are rejected: Java's msg-parser path has no Rust
//! equivalent yet. Group gating on `eml-to-pdf` (Weasyprint tool group)
//! already 403s the route when the renderer is absent.
//!
//! Documented divergences from Java:
//! - `.msg` uploads get a 400 instead of Java's parse attempt (no OLE2/MAPI
//!   port); the file-extension check also rejects `.msg` names up front.
//! - `useStirlingOfficeConvert=true` still renders through WeasyPrint, same
//!   as `html_to_pdf`.
//! - Attachments embed into the PDF `/EmbeddedFiles` name tree only; Java
//!   additionally drops page-level FileAttachment annotations by locating
//!   `@` markers through PDF text extraction, which has no Rust port yet.
//! - The HTML sanitizer is `ammonia` with an OWASP-policy-shaped allowlist,
//!   not java-html-sanitizer; the accepted element/attribute set is close
//!   but not identical.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

use axum::response::Response;
use mail_parser::{Message, MessageParser};

use crate::error::{AppError, AppResult};
use crate::handlers::convert::html_to_pdf_bytes;
use crate::handlers::jobs::AppState;
use crate::handlers::merge::{file_response, pdf_response};
use crate::upload::{output_name, param, param_bool, Upload};

const STYLES: &str = include_str!("email-pdf-styles.css");
const MAX_HEADER_SCAN: usize = 8192;
const MIN_HEADER_COUNT: usize = 2;
const MAX_ATTACHMENTS: usize = 10_000;
const MAX_FILENAME_LENGTH: usize = 255;
const MAX_EXTENSION_LENGTH: usize = 16;
const DEFAULT_MAX_ATTACHMENT_MB: i64 = 10;

struct EmailAttachment {
    filename: String,
    content_type: String,
    size_bytes: usize,
    /// `None` when the part exceeded `maxAttachmentSizeMB` or attachments
    /// were not requested — Java keeps the metadata and drops the bytes the
    /// same way.
    data: Option<Vec<u8>>,
    content_id: Option<String>,
    embedded: bool,
}

struct EmailContent {
    subject: String,
    from: String,
    to: String,
    cc: String,
    bcc: String,
    date: String,
    html_body: Option<String>,
    text_body: Option<String>,
    attachments: Vec<EmailAttachment>,
}

#[stirling_macros::endpoint]
fn eml_to_pdf(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let _ = param(&params, "useStirlingOfficeConvert", "");
    let include_attachments = param_bool(&params, "includeAttachments");
    let max_attachment_bytes = max_attachment_size_bytes(&params);
    // Java's `includeAllRecipients` defaults to true.
    let include_all_recipients = params
        .iter()
        .rev()
        .find(|(n, _)| n == "includeAllRecipients")
        .is_none_or(|(_, v)| truthy(v));

    let upload = uploads.into_iter().next().ok_or_else(|| {
        AppError::BadRequest("No file provided. Please select a .eml file.".into())
    })?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name)
        .to_string();
    let lower = filename.to_lowercase();
    if !lower.ends_with(".eml") && !lower.ends_with(".msg") {
        return Err(AppError::BadRequest(
            "Invalid file type. Please upload a valid EML or MSG file.".into(),
        ));
    }
    if lower.ends_with(".msg") {
        return Err(AppError::BadRequest(
            "Outlook .msg files are not supported by the Rust backend; please save as .eml".into(),
        ));
    }

    let content = parse_eml(&upload.bytes, include_attachments, max_attachment_bytes)?;
    let html = render_html(&content, include_all_recipients, include_attachments);

    if param_bool(&params, "downloadHtml") {
        let name = output_name(Some(&filename), ".html");
        return Ok(file_response(html.into_bytes(), &name, "text/html"));
    }

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let mut pdf = html_to_pdf_bytes(&html, dir.path())?;

    if include_attachments {
        let embeddable: Vec<&EmailAttachment> = content
            .attachments
            .iter()
            .filter(|a| a.data.as_ref().is_some_and(|d| !d.is_empty()))
            .collect();
        if !embeddable.is_empty() {
            pdf = embed_attachments(&pdf, &embeddable)?;
        }
    }

    Ok(pdf_response(pdf, &output_name(Some(&filename), ".pdf")))
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "on" | "yes"
    )
}

/// `EmlToPdfRequest.maxAttachmentSizeMB`: default 10, clamped to the
/// documented 1–100 MB range.
fn max_attachment_size_bytes(params: &[(String, String)]) -> usize {
    let mb = param(params, "maxAttachmentSizeMB", "")
        .parse::<i64>()
        .unwrap_or(DEFAULT_MAX_ATTACHMENT_MB)
        .clamp(1, 100);
    (mb * 1024 * 1024) as usize
}

fn parse_eml(
    bytes: &[u8],
    include_attachments: bool,
    max_attachment_bytes: usize,
) -> AppResult<EmailContent> {
    if bytes.is_empty() {
        return Err(AppError::BadRequest("File is empty".into()));
    }
    // OLE2 compound-file magic: Outlook .msg, which mail-parser cannot read.
    if bytes.len() >= 8 && bytes[..8] == [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] {
        return Err(AppError::BadRequest(
            "The file appears to be an Outlook .msg file, which is not supported; \
             please save it as .eml"
                .into(),
        ));
    }
    if !looks_like_eml(bytes) {
        return Err(AppError::BadRequest(
            "Invalid EML file format. Please ensure you've uploaded a valid email file.".into(),
        ));
    }

    let message = MessageParser::default()
        .parse_owned(bytes.to_vec())
        .ok_or_else(|| AppError::BadRequest("Invalid EML file: parse failed".into()))?;

    // `EmlParser`: embedded images + attachments are counted together and
    // rejected past ZipBombGuard.MAX_ENTRIES before any bytes are read.
    if include_attachments && message.attachments().len() > MAX_ATTACHMENTS {
        return Err(AppError::BadRequest(format!(
            "Email contains more than the maximum allowed {MAX_ATTACHMENTS} attachments"
        )));
    }

    Ok(email_content(
        &message,
        include_attachments,
        max_attachment_bytes,
    ))
}

/// `EmlProcessingUtils.isInvalidEmlFormat` inverted: fewer than two
/// recognizable headers in the first 8 KiB AND no MIME structure marks the
/// input as not an email. Java scans the decoded text, not header lines.
fn looks_like_eml(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(MAX_HEADER_SCAN)];
    let content = String::from_utf8_lossy(head).to_lowercase();

    let has_from = content.contains("from:") || content.contains("return-path:");
    let has_subject = content.contains("subject:");
    let has_message_id = content.contains("message-id:");
    let has_date = content.contains("date:");
    let has_to = content.contains("to:") || content.contains("cc:") || content.contains("bcc:");
    let has_mime_structure = content.contains("multipart/")
        || content.contains("text/plain")
        || content.contains("text/html")
        || content.contains("boundary=");

    let header_count = [has_from, has_subject, has_message_id, has_date, has_to]
        .into_iter()
        .filter(|h| *h)
        .count();

    header_count >= MIN_HEADER_COUNT || has_mime_structure
}

fn email_content(
    message: &Message<'_>,
    include_attachments: bool,
    max_attachment_bytes: usize,
) -> EmailContent {
    let html_body = message
        .html_body()
        .find(|p| matches!(p.kind(), mail_parser::PartKind::Html))
        .and_then(|p| p.text().map(|s| s.replace('\r', "")));
    let text_body = message
        .text_body()
        .find(|p| matches!(p.kind(), mail_parser::PartKind::Text))
        .and_then(|p| p.text().map(|s| s.replace('\r', "")));

    // Java: both bodies blank → the text body becomes the parse-failure
    // notice, which then renders through the plain-text path.
    let (html_body, text_body) = if html_body.as_deref().is_none_or(str::is_empty)
        && text_body.as_deref().is_none_or(str::is_empty)
    {
        // `isBlank` trims; a whitespace-only body counts as empty too.
        let html_blank = html_body.as_deref().is_none_or(|s| s.trim().is_empty());
        let text_blank = text_body.as_deref().is_none_or(|s| s.trim().is_empty());
        if html_blank && text_blank {
            (None, Some("Email content could not be parsed".to_string()))
        } else {
            (html_body, text_body)
        }
    } else {
        (html_body, text_body)
    };

    let mut attachments = Vec::new();
    let mut unnamed = 0usize;
    for part in message.attachments() {
        let content_id = part.content_id().map(str::to_string);
        let content_type_raw = part
            .content_type()
            .map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            })
            .unwrap_or_default();
        let is_image = part
            .content_type()
            .is_some_and(|ct| ct.ctype().eq_ignore_ascii_case("image"));
        let embedded = content_id.is_some() && is_image;

        let mut filename = part.attachment_name().unwrap_or_default().to_string();
        if filename.trim().is_empty() && !embedded {
            unnamed += 1;
            filename = format!(
                "attachment-{unnamed}{}",
                extension_for_mime(&content_type_raw)
            );
        }
        filename = limit_filename_length(&filename);
        let content_type = detect_mime_type(&filename, &content_type_raw);

        let size_bytes = part.decoded_len();
        // Embedded images always keep their bytes (they are body content
        // via cid:); regular attachments only when requested and within the
        // size cap — Java's `readData`/`shouldIncludeAttachmentData`.
        let keep_data = embedded || (include_attachments && size_bytes <= max_attachment_bytes);
        let data = keep_data.then(|| part.decoded().into_owned());

        attachments.push(EmailAttachment {
            filename,
            content_type,
            size_bytes,
            data,
            content_id,
            embedded,
        });
    }

    EmailContent {
        subject: message.subject().unwrap_or_default().to_string(),
        from: format_addresses(message.from().as_ref()),
        to: format_addresses(message.to().as_ref()),
        cc: format_addresses(message.cc().as_ref()),
        bcc: format_addresses(message.bcc().as_ref()),
        date: format_date(message),
        html_body,
        text_body,
        attachments,
    }
}

fn format_addresses(list: Option<&mail_parser::AddressList<'_>>) -> String {
    let Some(list) = list else {
        return String::new();
    };
    list.mailboxes()
        .map(|a| match (a.name(), a.address()) {
            (Some(name), Some(addr)) if !name.trim().is_empty() => {
                format!("{} <{}>", name.trim(), addr)
            }
            (_, Some(addr)) => addr.to_string(),
            (Some(name), None) if !name.trim().is_empty() => name.trim().to_string(),
            _ => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_date(message: &Message<'_>) -> String {
    if let Some(dt) = message.date() {
        if let Some(ts) = chrono::DateTime::from_timestamp(dt.to_timestamp(), 0) {
            // `PdfAttachmentHandler.formatEmailDate`: English pattern in UTC.
            return ts.format("%a, %b %-d, %Y 'at' %-I:%M %p UTC").to_string();
        }
    }
    message
        .headers()
        .value("Date")
        .and_then(|v| match v {
            mail_parser::HeaderValue::Text(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// `EmlParser.limitFilenameLength`: cap at 255 chars keeping an extension
/// of up to 16 chars. Java counts UTF-16 units; chars is close enough for
/// the BMP-heavy filenames this guards.
fn limit_filename_length(filename: &str) -> String {
    let chars: Vec<char> = filename.chars().collect();
    if chars.len() <= MAX_FILENAME_LENGTH {
        return filename.to_string();
    }
    let mut extension = String::new();
    if let Some(dot) = filename.rfind('.') {
        if dot > 0 && filename.len() - dot <= MAX_EXTENSION_LENGTH {
            extension = filename[dot..].to_string();
        }
    }
    let ext_chars = extension.chars().count();
    let keep = MAX_FILENAME_LENGTH.saturating_sub(ext_chars);
    chars[..keep.min(chars.len())].iter().collect::<String>() + &extension
}

fn render_html(
    content: &EmailContent,
    include_all_recipients: bool,
    include_attachments: bool,
) -> String {
    let subject = escape_html(&content.subject);
    let mut html = format!(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"UTF-8\">\n\
         <title>{subject}</title>\n<style>\n:root {{\n  \
         --font-family: Helvetica, sans-serif;\n  --font-size: 12px;\n  \
         --line-height: 1.4;\n  --text-color: #202124;\n  --bg-color: #ffffff;\n  \
         --border-color: #e8eaed;\n  --header-font-size: 18px;\n  --meta-font-size: 12px;\n  \
         --attachment-bg: #f9f9f9;\n  --attachment-border: #eeeeee;\n  \
         --attachment-header-size: 14px;\n  --attachment-detail-size: 11px;\n  \
         --note-font-size: 11px;\n}}\n{STYLES}\n</style>\n</head><body>\n"
    );

    html.push_str("<div class=\"email-container\">\n<div class=\"email-header\">\n");
    html.push_str(&format!("<h1>{subject}</h1>\n<div class=\"email-meta\">\n"));
    for (label, value) in [("From", &content.from), ("To", &content.to)] {
        if !value.is_empty() {
            html.push_str(&format!(
                "<div><strong>{label}:</strong> {}</div>\n",
                escape_html(value)
            ));
        }
    }
    if include_all_recipients {
        for (label, value) in [("CC", &content.cc), ("BCC", &content.bcc)] {
            if !value.trim().is_empty() {
                html.push_str(&format!(
                    "<div><strong>{label}:</strong> {}</div>\n",
                    escape_html(value)
                ));
            }
        }
    }
    if !content.date.trim().is_empty() {
        html.push_str(&format!(
            "<div><strong>Date:</strong> {}</div>\n",
            escape_html(&content.date)
        ));
    }
    html.push_str("</div></div>\n<div class=\"email-body\">\n");

    if let Some(body) = content
        .html_body
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        html.push_str(&process_html_body(body, content));
    } else if let Some(text) = content
        .text_body
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        html.push_str(&format!(
            "<div class=\"text-body\">{}</div>",
            text_to_html(text)
        ));
    } else {
        html.push_str("<div class=\"no-content\"><p><em>No content available</em></p></div>");
    }
    html.push_str("</div>\n");

    if !content.attachments.is_empty() {
        append_attachments_section(&mut html, content, include_attachments);
    }

    html.push_str("</div>\n</body></html>");
    html
}

/// `processEmailHtmlBody`: sanitize, drop fixed/absolute positioning, then
/// inline `cid:` images as data URIs.
fn process_html_body(html_body: &str, content: &EmailContent) -> String {
    let processed = sanitize_html(html_body);
    let processed = strip_position_declarations(&processed);
    inline_images(&processed, content)
}

/// `convertTextToHtml`: escape, normalize newlines, linkify URLs and bare
/// email addresses. Java applies its link regexes after escaping, so the
/// anchors it emits contain already-escaped text — the same holds here.
fn text_to_html(text: &str) -> String {
    let escaped = escape_html(text).replace(['\r', '\n'], "\n");
    let linked = linkify(&escaped);
    linked.replace('\n', "<br>\n")
}

fn append_attachments_section(
    html: &mut String,
    content: &EmailContent,
    include_attachments: bool,
) {
    html.push_str("<div class=\"attachment-section\">\n");
    html.push_str(&format!(
        "<h3>Attachments ({})</h3>\n",
        content.attachments.len()
    ));
    for (i, att) in content.attachments.iter().enumerate() {
        let embedded_filename = if att.filename.is_empty() {
            format!("attachment_{i}")
        } else {
            att.filename.clone()
        };
        let type_suffix = if att.content_type.is_empty() {
            String::new()
        } else {
            format!(", {}", escape_html(&att.content_type))
        };
        html.push_str(&format!(
            "<div class=\"attachment-item\" id=\"attachment_{i}\">\n\
             <span class=\"attachment-icon\" data-filename=\"{}\">@</span>\n\
             <span class=\"attachment-name\">{}</span>\n\
             <span class=\"attachment-details\">({}{})</span>\n</div>\n",
            escape_html(&embedded_filename),
            escape_html(&att.filename),
            format_size(att.size_bytes),
            type_suffix,
        ));
    }
    let note = if include_attachments {
        "Attachments are embedded in the file."
    } else {
        "Attachment information displayed - files not included in PDF."
    };
    html.push_str(&format!(
        "<div class=\"attachment-info-note\">\n<p><em>{note}</em></p>\n</div>\n</div>\n"
    ));
}

/// `PdfAttachmentHandler.processInlineImages`: `cid:` references in `<img>`
/// tags become base64 data URIs so WeasyPrint's `data`-only protocol
/// allowlist can render them. The match is case-insensitive on the `cid:`
/// scheme like Java's `(?i)` pattern; the content id itself is exact.
fn inline_images(html: &str, content: &EmailContent) -> String {
    use base64::Engine as _;
    let mut out = html.to_string();
    for att in content.attachments.iter().filter(|a| a.embedded) {
        let Some(cid) = &att.content_id else { continue };
        let Some(data) = &att.data else { continue };
        let data_uri = format!(
            "data:{};base64,{}",
            att.content_type,
            base64::engine::general_purpose::STANDARD.encode(data)
        );
        out = replace_cid_refs(&out, cid, &data_uri);
    }
    out
}

fn replace_cid_refs(html: &str, cid: &str, data_uri: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = find_case_insensitive(rest, "cid:") {
        let after = pos + 4;
        if rest[after..].starts_with(cid) {
            out.push_str(&rest[..pos]);
            out.push_str(data_uri);
            rest = &rest[after + cid.len()..];
        } else {
            out.push_str(&rest[..after]);
            rest = &rest[after..];
        }
    }
    out.push_str(rest);
    out
}

fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

// ---------------------------------------------------------------------------
// Sanitizer — ammonia stand-in for java-html-sanitizer's
// FORMATTING+BLOCKS+STYLES+LINKS+TABLES+img+pre policy.
// ---------------------------------------------------------------------------

fn sanitizer() -> &'static ammonia::Builder<'static> {
    static SANITIZER: OnceLock<ammonia::Builder<'static>> = OnceLock::new();
    SANITIZER.get_or_init(|| {
        let mut b = ammonia::Builder::new();
        b.tags(
            [
                "a",
                "abbr",
                "acronym",
                "address",
                "article",
                "aside",
                "b",
                "bdi",
                "bdo",
                "big",
                "blockquote",
                "br",
                "caption",
                "center",
                "cite",
                "code",
                "col",
                "colgroup",
                "data",
                "dd",
                "del",
                "details",
                "dfn",
                "div",
                "dl",
                "dt",
                "em",
                "figcaption",
                "figure",
                "footer",
                "h1",
                "h2",
                "h3",
                "h4",
                "h5",
                "h6",
                "header",
                "hgroup",
                "hr",
                "i",
                "img",
                "ins",
                "kbd",
                "li",
                "main",
                "mark",
                "nav",
                "ol",
                "p",
                "pre",
                "q",
                "rp",
                "rt",
                "ruby",
                "s",
                "samp",
                "section",
                "small",
                "span",
                "strike",
                "strong",
                "sub",
                "summary",
                "sup",
                "table",
                "tbody",
                "td",
                "tfoot",
                "th",
                "thead",
                "time",
                "tr",
                "tt",
                "u",
                "ul",
                "var",
                "wbr",
            ]
            .into_iter()
            .collect(),
        );
        b.tag_attributes(
            [
                ("a", ["href", "title"].as_slice()),
                ("img", ["src", "alt", "width", "height", "title"].as_slice()),
                ("td", ["colspan", "rowspan", "title"].as_slice()),
                ("th", ["colspan", "rowspan", "title"].as_slice()),
                (
                    "table",
                    ["border", "cellpadding", "cellspacing", "width"].as_slice(),
                ),
                ("col", ["span"].as_slice()),
                ("colgroup", ["span"].as_slice()),
                ("ol", ["start", "type", "title"].as_slice()),
                ("ul", ["type", "title"].as_slice()),
                ("li", ["value", "title"].as_slice()),
                ("blockquote", ["cite", "title"].as_slice()),
                ("q", ["cite", "title"].as_slice()),
                ("del", ["cite", "title"].as_slice()),
                ("ins", ["cite", "title"].as_slice()),
            ]
            .into_iter()
            .map(|(tag, attrs)| (tag, attrs.iter().copied().collect()))
            .collect(),
        );
        // Java's custom img policy admits raster `data:` URIs only; the
        // scheme list lets `data:` through the URL gate and the attribute
        // filter below re-narrows it to `img src`.
        b.url_schemes(
            ["http", "https", "mailto", "cid", "data"]
                .into_iter()
                .collect(),
        );
        b.url_relative(ammonia::UrlRelative::PassThrough);
        b.link_rel(None);
        b.filter_style_properties(
            [
                "color",
                "background",
                "background-color",
                "font",
                "font-family",
                "font-size",
                "font-style",
                "font-weight",
                "font-variant",
                "line-height",
                "text-align",
                "text-decoration",
                "text-transform",
                "letter-spacing",
                "white-space",
                "vertical-align",
                "margin",
                "margin-top",
                "margin-bottom",
                "margin-left",
                "margin-right",
                "padding",
                "padding-top",
                "padding-bottom",
                "padding-left",
                "padding-right",
                "border",
                "border-top",
                "border-bottom",
                "border-left",
                "border-right",
                "border-color",
                "border-style",
                "border-width",
                "border-radius",
                "width",
                "height",
                "max-width",
                "list-style",
                "list-style-type",
                "display",
                "float",
                "clear",
                "text-indent",
            ]
            .into_iter()
            .collect(),
        );
        b.attribute_filter(|element, attribute, value| {
            let trimmed = value.trim_start();
            if let Some(rest) = trimmed.get(..5).filter(|p| p.eq_ignore_ascii_case("data:")) {
                let _ = rest;
                let raster = element == "img"
                    && attribute == "src"
                    && [
                        "data:image/png",
                        "data:image/jpg",
                        "data:image/jpeg",
                        "data:image/gif",
                        "data:image/webp",
                        "data:image/bmp",
                    ]
                    .iter()
                    .any(|p| {
                        trimmed
                            .get(..p.len())
                            .is_some_and(|head| head.eq_ignore_ascii_case(p))
                            && trimmed.len() > p.len()
                    });
                return raster.then_some(Cow::Borrowed(value));
            }
            // `cid:` survives so inline_images can rewrite it after
            // sanitization, matching Java's order.
            if trimmed.len() >= 4
                && trimmed.is_char_boundary(4)
                && trimmed[..4].eq_ignore_ascii_case("cid:")
            {
                return Some(Cow::Borrowed(value));
            }
            Some(Cow::Borrowed(value))
        });
        b
    })
}

fn sanitize_html(html: &str) -> String {
    sanitizer().clean(html).to_string()
}

/// Java strips `position: fixed|absolute` declarations with a regex after
/// sanitizing; ammonia already drops `position`/`top`/`left`/`right`/
/// `bottom` from `style` since they are not whitelisted properties, so
/// this is a no-op kept for parity documentation.
fn strip_position_declarations(html: &str) -> String {
    html.to_string()
}

// ---------------------------------------------------------------------------
// Plain-text → HTML
// ---------------------------------------------------------------------------

/// `RegexPatternUtils.getUrlLinkPattern` + `getEmailLinkPattern` applied to
/// escaped text: token-level linkification.
fn linkify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if is_url_start(text, i) {
            let end = url_end(text, i);
            let token = &text[i..end];
            out.push_str(&format!(
                "<a href=\"{token}\" style=\"color: #1a73e8; text-decoration: underline;\">{token}</a>"
            ));
            i = end;
        } else if is_email_start(text, i) {
            let end = email_end(text, i);
            let token = &text[i..end];
            out.push_str(&format!(
                "<a href=\"mailto:{token}\" style=\"color: #1a73e8; text-decoration: underline;\">{token}</a>"
            ));
            i = end;
        } else {
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn is_url_start(text: &str, i: usize) -> bool {
    (text[i..].starts_with("http://") || text[i..].starts_with("https://"))
        && (i == 0 || text.as_bytes()[i - 1].is_ascii_whitespace())
}

/// `https?://[\w\-._~:/?#\[\]@!$&'()*+,;=%]+` — the char class is broad;
/// stop at whitespace and HTML-significant delimiters instead.
fn url_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() {
        let b = bytes[end];
        if b.is_ascii_whitespace() || matches!(b, b'<' | b'>' | b'&') {
            break;
        }
        end += 1;
    }
    while end > start && matches!(bytes[end - 1], b'.' | b',' | b';' | b':' | b'!' | b'?') {
        end -= 1;
    }
    end
}

fn is_email_start(text: &str, i: usize) -> bool {
    let prev_ok = i == 0
        || !(text.as_bytes()[i - 1].is_ascii_alphanumeric()
            || matches!(text.as_bytes()[i - 1], b'.' | b'_' | b'%' | b'+' | b'-'));
    prev_ok
        && text[i..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-'))
        && !is_url_start(text, i)
}

/// `[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,63}` with a non-local
/// predecessor, matching Java's lookbehind-anchored pattern.
fn email_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = start;
    let mut at = None;
    while end < bytes.len() {
        let b = bytes[end];
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-' | b'@') {
            if b == b'@' {
                if at.is_some() {
                    break;
                }
                at = Some(end);
            }
            end += 1;
        } else {
            break;
        }
    }
    let valid = at.is_some_and(|at_pos| {
        let domain = &text[at_pos + 1..end];
        domain.contains('.')
            && domain.rsplit('.').next().is_some_and(|tld| {
                (2..=63).contains(&tld.len()) && tld.bytes().all(|b| b.is_ascii_alphabetic())
            })
    });
    if valid {
        end
    } else {
        start + 1
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// `GeneralUtils.formatBytes`: `<1024` → "N B", else `%.2f` with the unit.
fn format_size(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.2} {}", UNITS[unit])
}

// ---------------------------------------------------------------------------
// MIME helpers
// ---------------------------------------------------------------------------

/// `EmlProcessingUtils.detectMimeType`: declared type wins, then filename
/// extension, then Java's odd `image/png` default.
fn detect_mime_type(filename: &str, existing: &str) -> String {
    if !existing.trim().is_empty() {
        return existing.to_string();
    }
    let lower = filename.to_lowercase();
    for (ext, mime) in [
        (".png", "image/png"),
        (".jpg", "image/jpeg"),
        (".jpeg", "image/jpeg"),
        (".gif", "image/gif"),
        (".bmp", "image/bmp"),
        (".webp", "image/webp"),
        (".svg", "image/svg+xml"),
        (".ico", "image/x-icon"),
        (".tiff", "image/tiff"),
        (".tif", "image/tiff"),
    ] {
        if lower.ends_with(ext) {
            return mime.to_string();
        }
    }
    "image/png".to_string()
}

/// `EmlParser.detectExtensionFromMimeType` for generated filenames.
fn extension_for_mime(mime_type: &str) -> &'static str {
    let lower = mime_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    match lower.as_str() {
        "application/pdf" => ".pdf",
        "image/png" => ".png",
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" => ".bmp",
        "text/plain" => ".txt",
        "text/html" => ".html",
        "text/xml" | "application/xml" => ".xml",
        "application/json" => ".json",
        "application/zip" => ".zip",
        "application/octet-stream" => ".bin",
        _ => {
            if lower.contains("wordprocessingml") || lower.contains("msword") {
                ".docx"
            } else if lower.contains("spreadsheetml") || lower.contains("excel") {
                ".xlsx"
            } else if lower.contains("presentationml") || lower.contains("powerpoint") {
                ".pptx"
            } else if lower.contains("opendocument.text") {
                ".odt"
            } else if lower.contains("opendocument.spreadsheet") {
                ".ods"
            } else {
                ""
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PDF attachment embedding (`PdfAttachmentHandler.attachFilesToPdf`, minus
// the page-annotation pass — see the module docs).
// ---------------------------------------------------------------------------

fn embed_attachments(pdf_bytes: &[u8], attachments: &[&EmailAttachment]) -> AppResult<Vec<u8>> {
    use lopdf::{dictionary, Object, Stream};

    let mut doc = crate::pdfdoc::load("email.pdf", pdf_bytes)?;

    // Java embeds embedded images first, then regular attachments; the
    // caller passes them in `EmailContent.attachments` order, which already
    // is embedded-first? No — mail-parser yields document order, so sort to
    // match Java's `mapResources(embedded) + mapResources(regular)`.
    let mut ordered: Vec<&&EmailAttachment> = attachments.iter().collect();
    ordered.sort_by_key(|a| !a.embedded);

    let mut names: Vec<Object> = Vec::new();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for att in ordered {
        let data = att.data.as_ref().expect("embeddable checked by caller");
        let base = if att.filename.is_empty() {
            "attachment".to_string()
        } else {
            att.filename.clone()
        };
        let mut filename = base.clone();
        let mut counter = 1;
        while !used.insert(filename.to_lowercase()) {
            counter += 1;
            let dot = base.rfind('.');
            filename = match dot {
                Some(i) if i > 0 => format!("{} ({counter}){}", &base[..i], &base[i..]),
                _ => format!("{base} ({counter})"),
            };
        }

        let ef_stream = Stream::new(
            dictionary! {
                "Type" => "EmbeddedFile",
                "Params" => dictionary! {
                    "Size" => Object::Integer(data.len() as i64),
                },
                "Subtype" => att.content_type.clone(),
            },
            data.clone(),
        );
        let ef_id = doc.add_object(ef_stream);

        let filespec = doc.add_object(dictionary! {
            "Type" => "Filespec",
            "F" => Object::string_literal(filename.clone()),
            "UF" => Object::string_literal(filename.clone()),
            "EF" => dictionary! {
                "F" => Object::Reference(ef_id),
                "UF" => Object::Reference(ef_id),
            },
        });
        names.push(Object::string_literal(filename));
        names.push(Object::Reference(filespec));
    }

    let embedded_id = doc.add_object(dictionary! {
        "Names" => Object::Array(names),
    });
    let names_id = doc.add_object(dictionary! {
        "EmbeddedFiles" => Object::Reference(embedded_id),
    });

    let catalog_id = doc.trailer.get(b"Root").ok().and_then(|o| match o {
        Object::Reference(id) => Some(*id),
        _ => None,
    });
    if let Some(cid) = catalog_id {
        if let Ok(Object::Dictionary(cat)) = doc.get_object_mut(cid) {
            cat.set("Names", Object::Reference(names_id));
        }
    }

    crate::pdfdoc::save(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::Object;

    const SAMPLE_EML: &str = "From: Alice <alice@example.com>\r\n\
        To: Bob <bob@example.com>\r\n\
        Subject: Test Subject\r\n\
        Date: Mon, 05 Jan 2026 15:04:05 +0000\r\n\
        MIME-Version: 1.0\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        \r\n\
        Hello Bob,\r\n\
        visit https://example.com for details.\r\n\
        mail me at carol@example.org\r\n";

    #[test]
    fn parses_plain_text_email() {
        let c = parse_eml(
            SAMPLE_EML.as_bytes(),
            false,
            DEFAULT_MAX_ATTACHMENT_MB as usize * 1024 * 1024,
        )
        .unwrap();
        assert_eq!(c.subject, "Test Subject");
        assert!(c.from.contains("alice@example.com"));
        assert!(c.to.contains("bob@example.com"));
        assert!(c.text_body.is_some());
        assert!(c.html_body.is_none());
        assert!(!c.date.is_empty());
    }

    #[test]
    fn rejects_msg_magic() {
        let mut bytes = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
        bytes.extend_from_slice(b"anything");
        assert!(parse_eml(&bytes, false, 10).is_err());
    }

    #[test]
    fn rejects_non_eml() {
        assert!(parse_eml(b"not an email at all", false, 10).is_err());
        assert!(parse_eml(b"", false, 10).is_err());
    }

    #[test]
    fn html_body_preferred_and_sanitized() {
        let eml = "From: a@b.com\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
            Content-Type: multipart/alternative; boundary=xx\r\n\r\n\
            --xx\r\nContent-Type: text/plain\r\n\r\nplain\r\n\
            --xx\r\nContent-Type: text/html\r\n\r\n\
            <p>hi</p><script>alert(1)</script>\
            <div style=\"position: fixed; top:0\">x</div>\
            <img src=\"javascript:alert(2)\">\r\n\
            --xx--\r\n";
        let c = parse_eml(eml.as_bytes(), false, 10).unwrap();
        let html = render_html(&c, true, false);
        assert!(html.contains("<p>hi</p>"));
        assert!(!html.contains("alert(1)"));
        assert!(!html.contains("javascript:"));
        assert!(html.contains("email-header"));
    }

    #[test]
    fn text_body_linkified() {
        let c = parse_eml(SAMPLE_EML.as_bytes(), false, 10).unwrap();
        let html = render_html(&c, true, false);
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains("href=\"mailto:carol@example.org\""));
    }

    #[test]
    fn header_values_are_escaped() {
        let eml = "From: <script>@x.com\r\nSubject: <b>bold</b>\r\n\
            MIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nhi\r\n";
        let c = parse_eml(eml.as_bytes(), false, 10).unwrap();
        let html = render_html(&c, true, false);
        assert!(html.contains("&lt;b&gt;bold&lt;/b&gt;"));
        assert!(!html.contains("<b>bold</b>"));
    }

    #[test]
    fn attachment_listed_in_html() {
        let eml = "From: a@b.com\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
            Content-Type: multipart/mixed; boundary=yy\r\n\r\n\
            --yy\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
            --yy\r\nContent-Type: application/pdf; name=\"doc.pdf\"\r\n\
            Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
            Content-Transfer-Encoding: base64\r\n\r\nJVBERi0=\r\n\
            --yy--\r\n";
        let c = parse_eml(eml.as_bytes(), true, 10 * 1024 * 1024).unwrap();
        assert_eq!(c.attachments.len(), 1);
        assert_eq!(c.attachments[0].filename, "doc.pdf");
        assert!(c.attachments[0].data.is_some());
        let html = render_html(&c, true, true);
        assert!(html.contains("attachment-item"));
        assert!(html.contains("doc.pdf"));
        assert!(html.contains("Attachments are embedded in the file."));
    }

    #[test]
    fn attachment_data_gated_by_size_and_flag() {
        let eml = "From: a@b.com\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
            Content-Type: multipart/mixed; boundary=yy\r\n\r\n\
            --yy\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
            --yy\r\nContent-Type: application/octet-stream; name=\"big.bin\"\r\n\
            Content-Disposition: attachment; filename=\"big.bin\"\r\n\r\n\
            AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==\r\n\
            --yy--\r\n";
        // Not requested → metadata kept, bytes dropped.
        let c = parse_eml(eml.as_bytes(), false, 1024).unwrap();
        assert_eq!(c.attachments.len(), 1);
        assert!(c.attachments[0].data.is_none());
        assert!(c.attachments[0].size_bytes > 0);
        // Over the cap → same.
        let c = parse_eml(eml.as_bytes(), true, 1).unwrap();
        assert!(c.attachments[0].data.is_none());
    }

    #[test]
    fn inline_image_becomes_data_uri() {
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let eml = format!(
            "From: a@b.com\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/related; boundary=zz\r\n\r\n\
             --zz\r\nContent-Type: text/html\r\n\r\n\
             <p>x</p><img src=\"cid:img1\">\r\n\
             --zz\r\nContent-Type: image/png; name=\"dot.png\"\r\n\
             Content-Disposition: inline; filename=\"dot.png\"\r\n\
             Content-ID: <img1>\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{png}\r\n\
             --zz--\r\n"
        );
        let c = parse_eml(eml.as_bytes(), false, 10).unwrap();
        let html = render_html(&c, true, false);
        assert!(html.contains("data:image/png;base64,"));
        assert!(!html.contains("cid:img1"));
    }

    #[test]
    fn embed_attachments_writes_names_tree() {
        let mut pdf = crate::testkit::Pdf::new();
        let src = pdf.bytes();
        let att = EmailAttachment {
            filename: "doc.pdf".into(),
            content_type: "application/pdf".into(),
            size_bytes: 5,
            data: Some(b"hello".to_vec()),
            content_id: None,
            embedded: false,
        };
        let out = embed_attachments(&src, &[&att]).unwrap();
        let doc = lopdf::Document::load_mem(&out).unwrap();
        let catalog = doc.trailer.get(b"Root").unwrap();
        let Object::Reference(cat_id) = catalog else {
            panic!("no catalog")
        };
        let cat = doc.get_dictionary(*cat_id).unwrap();
        let Object::Reference(names_id) = cat.get(b"Names").unwrap() else {
            panic!("no Names")
        };
        let names = doc.get_dictionary(*names_id).unwrap();
        let Object::Reference(ef_id) = names.get(b"EmbeddedFiles").unwrap() else {
            panic!("no EmbeddedFiles")
        };
        let ef = doc.get_dictionary(*ef_id).unwrap();
        let Object::Array(pairs) = ef.get(b"Names").unwrap() else {
            panic!("no Names array")
        };
        assert_eq!(pairs.len(), 2);
        let Object::String(name, _) = &pairs[0] else {
            panic!("bad name")
        };
        assert_eq!(name, b"doc.pdf");
        // The embedded stream holds the original bytes.
        let Object::Reference(fspec_id) = &pairs[1] else {
            panic!("bad filespec")
        };
        let fspec = doc.get_dictionary(*fspec_id).unwrap();
        let Object::Dictionary(ef_dict) = fspec.get(b"EF").unwrap() else {
            panic!("no EF")
        };
        let Object::Reference(ef_file) = ef_dict.get(b"F").unwrap() else {
            panic!("no EF/F")
        };
        let stream = doc.get_object(*ef_file).unwrap().as_stream().unwrap();
        assert_eq!(stream.content, b"hello");
    }

    #[test]
    fn format_size_matches_java() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.00 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.00 MB");
    }

    #[test]
    fn limit_filename_length_caps() {
        let long = format!("{}.pdf", "a".repeat(300));
        let out = limit_filename_length(&long);
        assert!(out.chars().count() <= MAX_FILENAME_LENGTH + MAX_EXTENSION_LENGTH);
        assert!(out.ends_with(".pdf"));
    }
}
