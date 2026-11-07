//! Port of `TimestampController`: RFC 3161 document timestamps. The PDF
//! never leaves the server — only a SHA-256 hash goes to the TSA, and the
//! returned CMS token is embedded as a `/DocTimeStamp` signature through a
//! lopdf incremental update, the `PDDocument.saveIncremental` equivalent,
//! so signatures already in the file keep their ByteRanges valid.
//!
//! Deliberate divergences from Java:
//! - `/Contents` is padded with zeros to PDFBox's `DEFAULT_SIGNATURE_SIZE`
//!   (0x2500) reserve, matching what `saveIncremental` produces; the TSA
//!   token must fit or the request fails like Java's "not enough space".
//! - TSA responses larger than 1 MiB are rejected while streaming, same
//!   cap as Java's `MAX_TSA_RESPONSE_SIZE`.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::time::Duration;

use axum::response::Response;
use cms::content_info::ContentInfo;
use der::asn1::{Int, OctetString, Uint};
use der::{Any, Decode, Encode, Sequence, Tag, Tagged};
use lopdf::{Dictionary, Document, IncrementalDocument, Object, ObjectId, Stream, StringFormat};
use sha2::Digest as _;
use spki::AlgorithmIdentifierOwned;

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::pdf_response;
use crate::pdfdoc;
use crate::settings;
use crate::signature::{self, oid, MessageImprint, TstInfo};
use crate::upload::{first_upload, output_name, param, Upload};

/// PDFBox `SignatureOptions.DEFAULT_SIGNATURE_SIZE` — the byte count
/// `/Contents` is padded to, so the hex string is always 0x4A00 chars.
pub(crate) const RESERVED_CONTENTS_LEN: usize = 0x2500;
const MAX_TSA_RESPONSE_SIZE: u64 = 1024 * 1024;
const TSA_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const TSA_READ_TIMEOUT: Duration = Duration::from_secs(60);

const OID_SHA256: &str = "2.16.840.1.101.3.4.2.1";
const SUBFILTER_RFC3161: &str = "ETSI.RFC3161";

/// `TimestampController.TSA_PRESETS` — the built-in allowlist entries.
const TSA_PRESETS: &[(&str, &str)] = &[
    ("DigiCert", "http://timestamp.digicert.com"),
    ("Sectigo", "http://timestamp.sectigo.com"),
    ("SSL.com", "http://ts.ssl.com"),
    ("FreeTSA", "https://freetsa.org/tsr"),
    ("MeSign", "http://tsa.mesign.com"),
];

/// `POST /api/v1/security/timestamp-pdf` —
/// `TimestampController.timestampPdf`: embeds an RFC 3161 document
/// timestamp into the PDF via the configured or requested TSA.
#[stirling_macros::endpoint]
fn timestamp_pdf(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let document = pdfdoc::load(&upload.name, &upload.bytes)?;

    let tsa_url = effective_tsa_url(&params);
    if !allowed_tsa_urls().contains(&normalize_tsa_url(tsa_url)) {
        return Err(AppError::BadRequest(
            "TSA URL is not in the allowed list. Contact your administrator to add it \
             via settings.yml (security.timestamp.customTsaUrls)."
                .into(),
        ));
    }

    let bytes = timestamped_pdf(&upload.bytes, document, tsa_url)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_timestamped.pdf"),
    ))
}

fn effective_tsa_url(params: &[(String, String)]) -> &str {
    let requested = param(params, "tsaUrl", "");
    if !requested.trim().is_empty() {
        requested
    } else {
        // `settings::current()` is 'static, so the default coerces into the
        // params lifetime.
        settings::current()
            .security
            .timestamp
            .default_tsa_url
            .as_str()
    }
}

/// `TimestampController`'s effective allowlist: presets plus the
/// admin-configured default and custom URLs, filtered to http(s) and
/// normalised for case-insensitive comparison.
fn allowed_tsa_urls() -> HashSet<String> {
    let ts = &settings::current().security.timestamp;
    let mut urls: HashSet<String> = TSA_PRESETS
        .iter()
        .map(|(_, url)| normalize_tsa_url(url))
        .collect();
    let mut add = |url: &str| {
        if !url.trim().is_empty() && is_valid_tsa_url_protocol(url) {
            urls.insert(normalize_tsa_url(url));
        }
    };
    add(&ts.default_tsa_url);
    for url in &ts.custom_tsa_urls {
        add(url);
    }
    urls
}

fn is_valid_tsa_url_protocol(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// `TimestampController.normalizeTsaUrl`: scheme and host lowercased,
/// query and fragment discarded. Parsed with `reqwest::Url` — the same
/// parser the actual request uses — so the host that is allowlist-checked
/// is the host that gets fetched; a hand-rolled parse could disagree on
/// `\`, userinfo, or odd ports and open an SSRF gap. Entries that do not
/// parse fall back to lowercase, which cannot match a normalised preset.
fn normalize_tsa_url(url: &str) -> String {
    let trimmed = url.trim();
    match reqwest::Url::parse(trimmed) {
        Ok(parsed) => {
            let scheme = parsed.scheme().to_lowercase();
            let host = parsed.host_str().unwrap_or_default().to_lowercase();
            let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
            format!("{scheme}://{host}{port}{}", parsed.path())
        }
        Err(_) => trimmed.to_lowercase(),
    }
}

// ---------------------------------------------------------------------------
// RFC 3161 TimeStampReq / TimeStampResp
// ---------------------------------------------------------------------------

#[derive(Sequence)]
struct TimeStampReq {
    version: Uint,
    message_imprint: MessageImprint,
    #[asn1(optional = "true")]
    nonce: Option<Uint>,
    cert_req: bool,
}

#[derive(Sequence)]
struct PkiStatusInfo {
    status: Uint,
    /// PKIFreeText — `SEQUENCE OF DisplayText`; each element is decoded as
    /// whichever string type the TSA chose.
    #[asn1(optional = "true")]
    status_string: Option<Vec<Any>>,
    #[asn1(optional = "true")]
    fail_info: Option<Any>,
}

#[derive(Sequence)]
struct TimeStampResp {
    status: PkiStatusInfo,
    #[asn1(optional = "true")]
    time_stamp_token: Option<Any>,
}

const PKI_STATUS_GRANTED: u8 = 0;
const PKI_STATUS_GRANTED_WITH_MODS: u8 = 1;

fn uint_value(uint: &Uint) -> u64 {
    uint.as_bytes()
        .iter()
        .fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
}

fn build_time_stamp_req(hash: &[u8], nonce: u64) -> AppResult<Vec<u8>> {
    let magnitude = {
        let bytes = nonce.to_be_bytes();
        let start = bytes
            .iter()
            .position(|b| *b != 0)
            .unwrap_or(bytes.len() - 1);
        bytes[start..].to_vec()
    };
    let req = TimeStampReq {
        version: Uint::new(&[1u8]).map_err(|e| AppError::Internal(e.into()))?,
        message_imprint: MessageImprint {
            hash_algorithm: AlgorithmIdentifierOwned {
                oid: oid(OID_SHA256),
                parameters: None,
            },
            hashed_message: OctetString::new(hash.to_vec())
                .map_err(|e| AppError::Internal(e.into()))?,
        },
        nonce: Some(Uint::new(&magnitude).map_err(|e| AppError::Internal(e.into()))?),
        cert_req: true,
    };
    req.to_der()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("TimeStampReq encode failed: {e}")))
}

/// `TimestampController.requestTimestampToken`: POST the request, enforce
/// the 1 MiB response cap, then run BouncyCastle's
/// `TimeStampResponse.validate(request)` checks — granted status, message
/// imprint, and nonce.
fn request_timestamp_token(hash: &[u8], tsa_url: &str) -> AppResult<Vec<u8>> {
    let nonce = rand::random::<u64>() & i64::MAX as u64;
    let req_der = build_time_stamp_req(hash, nonce)?;

    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(TSA_CONNECT_TIMEOUT)
        .timeout(TSA_READ_TIMEOUT)
        .build()
        .map_err(|e| AppError::Internal(e.into()))?;
    let response = client
        .post(tsa_url)
        .header(reqwest::header::CONTENT_TYPE, "application/timestamp-query")
        .body(req_der)
        .send()
        .map_err(|e| {
            AppError::Internal(anyhow::anyhow!(
                "Failed to obtain RFC 3161 timestamp from {tsa_url}: {e}"
            ))
        })?;

    let status = response.status();
    if status != reqwest::StatusCode::OK {
        let mut err_body = Vec::new();
        let _ = response.take(2048).read_to_end(&mut err_body);
        let detail = String::from_utf8_lossy(&err_body).trim().to_string();
        return Err(AppError::Internal(anyhow::anyhow!(
            "TSA server returned HTTP {} for URL: {}{}",
            status.as_u16(),
            tsa_url,
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        )));
    }

    let mut body = Vec::new();
    response
        .take(MAX_TSA_RESPONSE_SIZE + 1)
        .read_to_end(&mut body)
        .map_err(|e| AppError::Internal(e.into()))?;
    if body.len() as u64 > MAX_TSA_RESPONSE_SIZE {
        return Err(AppError::Internal(anyhow::anyhow!(
            "TSA response exceeds maximum allowed size of {MAX_TSA_RESPONSE_SIZE} bytes"
        )));
    }

    let resp = TimeStampResp::from_der(&body)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("invalid TSA response: {e}")))?;
    let status = uint_value(&resp.status.status);
    if status != u64::from(PKI_STATUS_GRANTED) && status != u64::from(PKI_STATUS_GRANTED_WITH_MODS)
    {
        return Err(AppError::Internal(anyhow::anyhow!(
            "TSA request was rejected: {}",
            status_text(resp.status.status_string.as_deref())
                .unwrap_or_else(|| { format!("status {status}") })
        )));
    }
    let Some(token_any) = resp.time_stamp_token else {
        return Err(AppError::Internal(anyhow::anyhow!(
            "TSA server did not return a timestamp token. Status: {status}"
        )));
    };
    let token_der = token_any
        .to_der()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("timestamp token encode failed: {e}")))?;
    validate_token(&token_der, hash, nonce)?;
    Ok(token_der)
}

fn status_text(strings: Option<&[Any]>) -> Option<String> {
    let parts: Vec<String> = strings?.iter().filter_map(display_text).collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// One `DisplayText` CHOICE element of PKIFreeText, decoded by tag.
fn display_text(any: &Any) -> Option<String> {
    Some(match any.tag() {
        Tag::Utf8String
        | Tag::PrintableString
        | Tag::Ia5String
        | Tag::TeletexString
        | Tag::VideotexString => String::from_utf8_lossy(any.value()).into_owned(),
        Tag::BmpString => {
            let units: Vec<u16> = any
                .value()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_be_bytes(*c))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => return None,
    })
}

/// The subset of `TimeStampResponse.validate` that matters here: the
/// TSTInfo's message imprint must equal the request's SHA-256 hash, and
/// the nonce must echo ours.
fn validate_token(token_der: &[u8], hash: &[u8], nonce: u64) -> AppResult<()> {
    let content_info = ContentInfo::from_der(token_der)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("invalid timestamp token: {e}")))?;
    let signed = signature::signed_data_from_content_info(&content_info).ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("timestamp token is not CMS SignedData"))
    })?;
    let tst: TstInfo = signature::tst_info_from_signed_data(&signed)
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("timestamp token has no TSTInfo")))?;

    let imprint_ok = tst.message_imprint.hash_algorithm.oid == oid(OID_SHA256)
        && tst.message_imprint.hashed_message.as_bytes() == hash;
    if !imprint_ok {
        return Err(AppError::Internal(anyhow::anyhow!(
            "TSA response message imprint does not match the request"
        )));
    }
    let nonce_ok = tst.nonce.as_ref().is_some_and(|n| int_eq_uint(n, nonce));
    if !nonce_ok {
        eprintln!(
            "DEBUG nonce mismatch: request={nonce} tst={:?}",
            tst.nonce.as_ref().map(|n| n.as_bytes())
        );
        return Err(AppError::Internal(anyhow::anyhow!(
            "TSA response nonce does not match the request"
        )));
    }
    Ok(())
}

/// `Int` stores two's-complement bytes while the request nonce is an
/// unsigned magnitude — compare after stripping sign extension/leading
/// zeroes. Nonces are always non-negative, so this is exact.
fn int_eq_uint(int: &Int, value: u64) -> bool {
    let bytes = value.to_be_bytes();
    let start = bytes
        .iter()
        .position(|b| *b != 0)
        .unwrap_or(bytes.len() - 1);
    let magnitude = &bytes[start..];
    let mut int_bytes = int.as_bytes();
    while let Some((0, rest)) = int_bytes.split_first() {
        int_bytes = rest;
    }
    int_bytes == magnitude
}

// ---------------------------------------------------------------------------
// PDF incremental update
// ---------------------------------------------------------------------------

/// Everything the appended revision needs from the previous one, cloned
/// out before `prev_documents` moves into the `IncrementalDocument`.
struct PrevLayout {
    catalog_id: ObjectId,
    page_id: Option<ObjectId>,
    acroform: Option<Dictionary>,
    field_items: Vec<Object>,
    field_names: HashSet<String>,
    annots: AnnotsLayout,
}

enum AnnotsLayout {
    /// `/Annots` absent — the page gets a fresh direct array.
    Missing,
    /// `/Annots n 0 R` — clone the array object into the new revision.
    Indirect(ObjectId),
    /// `/Annots [ ... ]` inline in the page dict.
    Direct,
}

fn prev_layout(doc: &Document, page_index: usize) -> AppResult<PrevLayout> {
    let catalog_id = doc
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .ok_or_else(|| AppError::Pdf("trailer /Root is not a reference".into()))?;
    let page_id = pdfdoc::page_ids(doc).into_iter().nth(page_index);

    let acroform = pdfdoc::acroform(doc);
    let mut field_items = Vec::new();
    if let Some(form) = &acroform {
        if let Ok(fields) = form.get(b"Fields") {
            let (_, resolved) = doc
                .dereference(fields)
                .map_err(|e| AppError::Pdf(format!("invalid PDF: {e}")))?;
            if let Object::Array(items) = resolved {
                field_items = items.clone();
            }
        }
    }

    let mut field_names = HashSet::new();
    let mut visited = HashSet::new();
    for item in &field_items {
        collect_field_names(doc, item, &mut visited, &mut field_names);
    }

    let annots = match page_id {
        Some(id) => match doc
            .get_dictionary(id)
            .ok()
            .and_then(|d| d.get(b"Annots").ok())
        {
            None => AnnotsLayout::Missing,
            Some(Object::Reference(annots_id)) => AnnotsLayout::Indirect(*annots_id),
            Some(_) => AnnotsLayout::Direct,
        },
        None => AnnotsLayout::Missing,
    };

    Ok(PrevLayout {
        catalog_id,
        page_id,
        acroform,
        field_items,
        field_names,
        annots,
    })
}

fn collect_field_names(
    doc: &Document,
    object: &Object,
    visited: &mut HashSet<ObjectId>,
    out: &mut HashSet<String>,
) {
    let Some(dict) = pdfdoc::dict_owned(doc, object).ok().flatten() else {
        return;
    };
    if let Ok(name) = dict.get(b"T") {
        if let Ok((_, resolved)) = doc.dereference(name) {
            if let Ok(text) = lopdf::decode_text_string(resolved) {
                out.insert(text);
            }
        }
    }
    let Some(id) = object.as_reference().ok() else {
        return;
    };
    if !visited.insert(id) {
        return;
    }
    let kids = dict.get(b"Kids").ok().and_then(|k| {
        doc.dereference(k).ok().and_then(|(_, o)| match o {
            Object::Array(items) => Some(items.as_slice()),
            _ => None,
        })
    });
    if let Some(items) = kids {
        for kid in items {
            collect_field_names(doc, kid, visited, out);
        }
    }
}

/// `PDSignatureField.generatePartialName`: `Signature` + smallest index
/// not already used by a field.
pub(crate) fn signature_field_name(names: &HashSet<String>) -> String {
    let mut i = 1;
    while names.contains(&format!("Signature{i}")) {
        i += 1;
    }
    format!("Signature{i}")
}

pub(crate) fn pdf_date_now() -> String {
    let now = chrono::Local::now();
    let offset_seconds = now.offset().local_minus_utc();
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let abs = offset_seconds.unsigned_abs();
    format!(
        "D:{}{}{:02}'{:02}'",
        now.format("%Y%m%d%H%M%S"),
        sign,
        abs / 3600,
        (abs % 3600) / 60
    )
}

fn signature_dict(byte_range: [i64; 4]) -> Dictionary {
    let mut sig = Dictionary::new();
    sig.set("Type", "DocTimeStamp");
    sig.set("Filter", "Adobe.PPKLite");
    sig.set("SubFilter", SUBFILTER_RFC3161);
    sig.set("M", Object::string_literal(pdf_date_now()));
    sig.set(
        "Contents",
        Object::String(vec![0u8; RESERVED_CONTENTS_LEN], StringFormat::Hexadecimal),
    );
    sig.set(
        "ByteRange",
        Object::Array(byte_range.iter().map(|v| Object::Integer(*v)).collect()),
    );
    sig
}

fn assemble_revision(
    inc: &mut IncrementalDocument,
    sig_id: ObjectId,
    byte_range: [i64; 4],
) -> AppResult<Vec<u8>> {
    inc.new_document
        .get_object_mut(sig_id)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| AppError::Pdf(format!("signature dict: {e}")))?
        .set(
            "ByteRange",
            Object::Array(byte_range.iter().map(|v| Object::Integer(*v)).collect()),
        );
    let mut out = Vec::new();
    inc.save_to(&mut out)
        .map_err(|e| AppError::Pdf(format!("incremental save: {e}")))?;
    Ok(out)
}

/// Locate the reserved `/Contents` hex string — `<` + 2×reserve zero
/// chars + `>` — and return `(lt_offset, after_gt_offset)`.
fn contents_span(file: &[u8]) -> AppResult<(usize, usize)> {
    let needle = format!("<{}>", "0".repeat(RESERVED_CONTENTS_LEN * 2)).into_bytes();
    let pos = file
        .windows(needle.len())
        .position(|w| w == needle.as_slice())
        .ok_or_else(|| AppError::Pdf("reserved /Contents not found in output".into()))?;
    Ok((pos, pos + needle.len()))
}

/// The assembled revision waiting on a signature payload: the full file
/// bytes with `/Contents` still zero padding, the covered-byte SHA-256
/// the CMS signature must attest, and the hex span the token is patched
/// into. Shared by `timestamp-pdf` (token comes from a TSA) and
/// `cert-sign` (token is built locally).
pub(crate) struct PreparedRevision {
    pub file: Vec<u8>,
    pub hash: [u8; 32],
    pub contents_hex_start: usize,
    pub contents_hex_end: usize,
}

/// Assemble the incremental revision with a correct `/ByteRange` and
/// hash the covered bytes — everything up to producing the CMS payload.
fn prepare_timestamp_revision(
    prev_bytes: &[u8],
    document: Document,
) -> AppResult<PreparedRevision> {
    prepare_signed_revision(prev_bytes, document, signature_dict([0; 4]), None, 0)
}

/// Appearance stream factory for a visible signature: called with the
/// new revision's document so it can embed images/fonts as indirect
/// objects and return the `/AP /N` form XObject.
pub(crate) type AppearanceFactory<'a> = dyn FnOnce(&mut Document) -> AppResult<Stream> + 'a;

/// Same assembly as `prepare_timestamp_revision` but with a caller-built
/// signature dictionary — `cert-sign` writes `/Type /Sig` with
/// name/location/reason where the timestamp dict carries none — and an
/// optional visible-signature widget (`rect` + appearance factory).
/// `None` reproduces the invisible widget: zero rect, empty appearance.
pub(crate) fn prepare_signed_revision(
    prev_bytes: &[u8],
    document: Document,
    sig_dict: Dictionary,
    widget: Option<([i64; 4], Box<AppearanceFactory<'_>>)>,
    page_index: usize,
) -> AppResult<PreparedRevision> {
    let layout = prev_layout(&document, page_index)?;
    let Some(page_id) = layout.page_id else {
        return Err(AppError::BadRequest(
            "Cannot timestamp an empty document".into(),
        ));
    };

    let mut inc = IncrementalDocument::create_from(prev_bytes.to_vec(), document);

    // Objects appended by this revision, in PDFBox's shape: an empty
    // appearance stream, the merged signature-field/widget dict, and the
    // signature dictionary itself.
    let ap_id = inc.new_document.new_object_id();
    let field_id = inc.new_document.new_object_id();
    let sig_id = inc.new_document.new_object_id();

    let (rect, ap_stream) = match widget {
        Some((rect, factory)) => (rect, factory(&mut inc.new_document)?),
        None => {
            let mut ap_dict = Dictionary::new();
            ap_dict.set("Type", "XObject");
            ap_dict.set("Subtype", "Form");
            ap_dict.set(
                "BBox",
                Object::Array(vec![0.into(), 0.into(), 0.into(), 0.into()]),
            );
            ([0, 0, 0, 0], Stream::new(ap_dict, Vec::new()))
        }
    };
    inc.new_document
        .set_object(ap_id, Object::Stream(ap_stream));

    let mut field = Dictionary::new();
    field.set("FT", "Sig");
    field.set("Type", "Annot");
    field.set("Subtype", "Widget");
    field.set("F", 132i64);
    field.set(
        "T",
        Object::string_literal(signature_field_name(&layout.field_names)),
    );
    field.set("V", Object::Reference(sig_id));
    field.set("P", Object::Reference(page_id));
    field.set(
        "Rect",
        Object::Array(rect.iter().map(|v| Object::Integer(*v)).collect()),
    );
    let mut appearances = Dictionary::new();
    appearances.set("N", Object::Reference(ap_id));
    field.set("AP", Object::Dictionary(appearances));
    inc.new_document
        .set_object(field_id, Object::Dictionary(field));

    inc.new_document
        .set_object(sig_id, Object::Dictionary(sig_dict));

    // Catalog: rewritten with a direct AcroForm carrying every previous
    // entry plus the appended field (`acroForm.setDirect(true)`).
    inc.opt_clone_object_to_new_document(layout.catalog_id)
        .map_err(|e| AppError::Pdf(format!("catalog clone: {e}")))?;
    {
        let catalog = inc
            .new_document
            .get_object_mut(layout.catalog_id)
            .and_then(|o| o.as_dict_mut())
            .map_err(|e| AppError::Pdf(format!("catalog: {e}")))?;
        let mut form = Dictionary::new();
        if let Some(old) = &layout.acroform {
            for (key, value) in old.iter() {
                if key.as_slice() != b"Fields" {
                    form.set(key.clone(), value.clone());
                }
            }
        }
        let mut fields = layout.field_items.clone();
        fields.push(Object::Reference(field_id));
        form.set("Fields", Object::Array(fields));
        // Java's setFlag preserves unrelated SigFlags bits.
        let old_flags = layout
            .acroform
            .as_ref()
            .and_then(|old| old.get(b"SigFlags").ok())
            .and_then(|o| o.as_i64().ok())
            .unwrap_or(0);
        form.set("SigFlags", old_flags | 3);
        catalog.set("AcroForm", Object::Dictionary(form));
    }

    // Page: append the widget to /Annots, cloning an indirect array into
    // this revision or writing a direct one like PDFBox does.
    inc.opt_clone_object_to_new_document(page_id)
        .map_err(|e| AppError::Pdf(format!("page clone: {e}")))?;
    match layout.annots {
        AnnotsLayout::Missing => {
            inc.new_document
                .get_object_mut(page_id)
                .and_then(|o| o.as_dict_mut())
                .map_err(|e| AppError::Pdf(format!("page: {e}")))?
                .set("Annots", Object::Array(vec![Object::Reference(field_id)]));
        }
        AnnotsLayout::Indirect(annots_id) => {
            inc.opt_clone_object_to_new_document(annots_id)
                .map_err(|e| AppError::Pdf(format!("annots clone: {e}")))?;
            inc.new_document
                .get_object_mut(annots_id)
                .and_then(|o| o.as_array_mut())
                .map_err(|e| AppError::Pdf(format!("annots: {e}")))?
                .push(Object::Reference(field_id));
        }
        AnnotsLayout::Direct => {
            let page = inc
                .new_document
                .get_object_mut(page_id)
                .and_then(|o| o.as_dict_mut())
                .map_err(|e| AppError::Pdf(format!("page: {e}")))?;
            let annots = page
                .get_mut(b"Annots")
                .and_then(|o| o.as_array_mut())
                .map_err(|e| AppError::Pdf(format!("annots: {e}")))?;
            annots.push(Object::Reference(field_id));
        }
    }

    // /ByteRange is inside the signed region, so its own digit widths feed
    // back into the offsets: iterate the assembly until the values the
    // file carries are the values they produce.
    let mut byte_range = [0i64; 4];
    let mut file = Vec::new();
    // The decimal widths of the ByteRange values feed back into the file
    // length and therefore the offsets; a fixed point is reached within a
    // few passes. The cap guards against a pathological oscillation.
    let mut lt_after_gt = None;
    for _ in 0..8 {
        file = assemble_revision(&mut inc, sig_id, byte_range)?;
        let (lt, after_gt) = contents_span(&file)?;
        let computed = [
            0,
            lt as i64,
            after_gt as i64,
            file.len() as i64 - after_gt as i64,
        ];
        if computed == byte_range {
            lt_after_gt = Some((lt, after_gt));
            break;
        }
        byte_range = computed;
    }
    let Some((lt, after_gt)) = lt_after_gt else {
        return Err(AppError::Pdf(
            "signature byte range did not converge".into(),
        ));
    };

    let mut covered = Vec::with_capacity(lt + file.len() - after_gt);
    covered.extend_from_slice(&file[..lt]);
    covered.extend_from_slice(&file[after_gt..]);
    let hash: [u8; 32] = sha2::Sha256::digest(&covered).into();

    Ok(PreparedRevision {
        file,
        hash,
        contents_hex_start: lt + 1,
        contents_hex_end: after_gt - 1,
    })
}

/// Patch a CMS token into the reserved `/Contents` hex area —
/// zero padding keeps the rest, exactly like PDFBox's
/// `writeExternalSignature`.
pub(crate) fn embed_signature_token(
    mut prepared: PreparedRevision,
    token: &[u8],
) -> AppResult<Vec<u8>> {
    let hex_len = token.len() * 2;
    if hex_len > prepared.contents_hex_end - prepared.contents_hex_start {
        return Err(AppError::Internal(anyhow::anyhow!(
            "Can't write signature, not enough space; the TSA token is {hex_len} hex chars \
             but /Contents reserves {}",
            prepared.contents_hex_end - prepared.contents_hex_start
        )));
    }
    let hex: String = token.iter().map(|b| format!("{b:02X}")).collect();
    prepared.file[prepared.contents_hex_start..prepared.contents_hex_start + hex.len()]
        .copy_from_slice(hex.as_bytes());
    Ok(prepared.file)
}

/// Assemble the incremental revision, fetch the TSA token for the
/// covered-byte hash, embed it, and return the complete signed PDF.
fn timestamped_pdf(prev_bytes: &[u8], document: Document, tsa_url: &str) -> AppResult<Vec<u8>> {
    let prepared = prepare_timestamp_revision(prev_bytes, document)?;
    let token = request_timestamp_token(&prepared.hash, tsa_url)?;
    embed_signature_token(prepared, &token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_matches_preset_forms() {
        // `reqwest::Url` normalisation: empty paths become "/", hosts and
        // schemes lowercase, query/fragment dropped, explicit ports kept.
        assert_eq!(
            normalize_tsa_url("HTTP://Timestamp.DigiCert.COM"),
            "http://timestamp.digicert.com/"
        );
        assert_eq!(
            normalize_tsa_url("https://freetsa.org/tsr?x=1#f"),
            "https://freetsa.org/tsr"
        );
        assert_eq!(
            normalize_tsa_url("http://tsa.example.com:8080/path"),
            "http://tsa.example.com:8080/path"
        );
        assert!(allowed_tsa_urls().contains(&normalize_tsa_url("http://timestamp.digicert.com")));
        assert!(!allowed_tsa_urls().contains(&normalize_tsa_url("http://evil.example.com")));
    }

    #[test]
    fn timestamp_req_encodes_sha256_imprint() {
        let der = build_time_stamp_req(&[0xAB; 32], 0x0102_0304_0506_0708).unwrap();
        // SEQUENCE { INTEGER 1, SEQUENCE { OID sha256, OCTET STRING },
        //             INTEGER nonce, BOOLEAN TRUE }
        assert_eq!(der[0], 0x30);
        let oid_at = der
            .windows(11)
            .position(|w| {
                w == [
                    0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
                ]
            })
            .expect("sha256 OID missing");
        assert!(der[oid_at..].windows(32).any(|w| w == [0xAB; 32]));
        assert!(der.ends_with(&[0x01, 0x01, 0xFF]));
    }

    #[test]
    fn pdf_date_format() {
        let d = pdf_date_now();
        assert!(d.starts_with("D:") && d.len() >= 18);
        assert!(d.contains('+') || d.contains('-'));
    }

    #[test]
    fn int_eq_uint_matches_magnitude() {
        let int = Int::new(&[0x00, 0x80]).unwrap();
        assert!(int_eq_uint(&int, 128));
        let int = Int::new(&[0x7F]).unwrap();
        assert!(int_eq_uint(&int, 127));
        assert!(!int_eq_uint(&int, 128));
    }

    /// The appended revision must parse as a valid incremental update:
    /// AcroForm gains a `/FT /Sig` field whose `/V` is a `/DocTimeStamp`
    /// dict, `/ByteRange` brackets the zero-padded `/Contents`, and the
    /// covered bytes hash to the imprint the TSA was asked for.
    #[test]
    fn timestamped_pdf_structure() {
        let bytes = crate::testkit::Pdf::new().bytes();
        let document = pdfdoc::load("in.pdf", &bytes).unwrap();
        let prepared = prepare_timestamp_revision(&bytes, document).unwrap();

        // The covered bytes are exactly ByteRange[0..1] + [2..3].
        let mut covered = Vec::new();
        covered.extend_from_slice(&prepared.file[..prepared.contents_hex_start - 1]);
        covered.extend_from_slice(&prepared.file[prepared.contents_hex_end + 1..]);
        assert_eq!(sha2::Sha256::digest(&covered)[..], prepared.hash[..]);

        // A small fake token embeds and leaves zero padding behind.
        let token = [0x30, 0x03, 0x02, 0x01, 0x05];
        let expected_hash = prepared.hash;
        let hex_start = prepared.contents_hex_start;
        let hex_end = prepared.contents_hex_end;
        let signed = embed_signature_token(prepared, &token).unwrap();
        let signed_doc = pdfdoc::load("signed.pdf", &signed).unwrap();

        let catalog = signed_doc.catalog().unwrap();
        let acroform = pdfdoc::acroform(&signed_doc).expect("acroform");
        assert_eq!(acroform.get(b"SigFlags").unwrap().as_i64().unwrap(), 3);
        assert!(catalog.get(b"AcroForm").is_ok());

        let fields = acroform.get(b"Fields").unwrap();
        let (_, fields) = signed_doc.dereference(fields).unwrap();
        let lopdf::Object::Array(items) = fields else {
            panic!("fields not an array");
        };
        assert_eq!(items.len(), 1);
        let (_, field) = signed_doc.dereference(&items[0]).unwrap();
        let field = field.as_dict().unwrap();
        assert_eq!(field.get(b"FT").unwrap().as_name().unwrap(), b"Sig");
        assert_eq!(field.get(b"T").unwrap().as_str().unwrap(), b"Signature1");

        let (_, sig_obj) = signed_doc.dereference(field.get(b"V").unwrap()).unwrap();
        let sig = sig_obj.as_dict().unwrap();
        assert_eq!(
            sig.get(b"Type").unwrap().as_name().unwrap(),
            b"DocTimeStamp"
        );
        assert_eq!(
            sig.get(b"SubFilter").unwrap().as_name().unwrap(),
            b"ETSI.RFC3161"
        );

        // /Contents decodes to the token bytes plus zero padding to the
        // reserve — lopdf stores the string value, not the hex text.
        let (_, contents) = signed_doc
            .dereference(sig.get(b"Contents").unwrap())
            .unwrap();
        let lopdf::Object::String(value, _) = contents else {
            panic!("contents not a string");
        };
        let mut expected = token.to_vec();
        expected.resize(RESERVED_CONTENTS_LEN, 0);
        assert_eq!(value.as_slice(), expected.as_slice());

        // /ByteRange covers everything except the <...> token.
        let (_, br) = signed_doc
            .dereference(sig.get(b"ByteRange").unwrap())
            .unwrap();
        let lopdf::Object::Array(br) = br else {
            panic!("bad byterange")
        };
        let nums: Vec<i64> = br.iter().map(|o| o.as_i64().unwrap()).collect();
        assert_eq!(nums[0], 0);
        assert_eq!(nums[1] as usize, hex_start - 1);
        assert_eq!(nums[2] as usize, hex_end + 1);
        assert_eq!(nums[3] as usize + nums[2] as usize, signed.len());

        // The covered hash still verifies against the embedded file.
        let mut covered = Vec::new();
        covered.extend_from_slice(&signed[..nums[1] as usize]);
        covered.extend_from_slice(&signed[nums[2] as usize..]);
        assert_eq!(sha2::Sha256::digest(&covered)[..], expected_hash[..]);
    }
}
