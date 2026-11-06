//! Ports of `PasswordController`'s `/add-password` and `/remove-password`
//! endpoints on lopdf's `EncryptionState` / `Document::encrypt`.
//!
//! Java behaviour reproduced:
//! - `add-password`: `StandardProtectionPolicy` with user/owner passwords,
//!   the eight `prevent*` flags as `AccessPermission` bits, `keyLength`
//!   {40,128,256} (default 256) → RC4-40 / AES-128 / AES-256.
//! - `remove-password`: full removal when the stored `/P` grants owner-level
//!   access (all eight permission bits); otherwise re-encryption preserving
//!   `/P`, key length and AES-vs-RC4 choice, with a random owner password
//!   and an empty user password — exactly PDFBox's re-`protect` branch.
//! - `PdfSessionPermissions.requireOwner` (`security.honorOwnerPasswords`
//!   defaults to true): protection changes on encrypted sources require
//!   owner-role authentication, and signed PDFs are refused.
//!
//! Subset vs Java:
//! - Selective removal fields on remove-password (`ownerPassword`,
//!   `retainedOpeningPassword`, and `sourcePassword` outside `removeAll`)
//!   and `preserveExisting` on an already-encrypted file are rejected
//!   rather than approximated.
//! - `prepare-pdf-session` decrypts for the interactive workflow after
//!   `PdfSessionPermissions.requireOperation` gates on the endpoint's
//!   required permission; `restore-pdf-protection` stays on Java because
//!   re-encrypting with the source's original key material needs
//!   JPDFium's `PdfSecurity.copyEncryption`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use axum::response::Response;
use lopdf::encryption::crypt_filters::{Aes128CryptFilter, Aes256CryptFilter, CryptFilter};
use lopdf::{Document, EncryptionState, EncryptionVersion, Object, Permissions};
use rand::RngExt as _;

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::pdf_response;
use crate::pdfdoc;
use crate::upload::{first_upload, output_name, param, param_bool, Upload};

/// `/P` bits for the eight `AccessPermission` checks `isOwnerPermission`
/// performs: print, modify, extract, annotate, fill-form, accessibility,
/// assemble, print-faithful (PDF bits 3-6 and 9-12).
const OWNER_PERMISSION_MASK: u32 = 0xF3C;

/// Last-occurrence-wins text param; `None` when the field was not sent,
/// matching the Java models' `null` for absent multipart fields.
fn param_opt<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// lopdf authenticates during the read but leaves `/Encrypt` in the
/// trailer; `Document::encrypt` refuses a document whose trailer still
/// names a security handler, and a saved file would keep the stale entry.
fn strip_encryption(doc: &mut Document) {
    let Some(encrypt) = doc.trailer.remove(b"Encrypt") else {
        return;
    };
    if let Ok(id) = encrypt.as_reference() {
        doc.objects.remove(&id);
    }
}

/// The credential that actually opened the document: lopdf tries the
/// empty password before the supplied one, so "" wins when it
/// authenticates — the same order `CustomPDFDocumentFactory` loaders use.
fn opening_credential<'a>(doc: &Document, supplied: &'a str) -> &'a str {
    if doc.authenticate_password("").is_ok() {
        ""
    } else {
        supplied
    }
}

/// `PdfSessionPermissions.requireOwner` with `honorOwnerPasswords=true`
/// (the `ApplicationProperties.Security` default): encrypted sources need
/// owner-role authentication before protection may change.
fn require_owner(doc: &Document, credential: &str) -> AppResult<()> {
    if doc.is_encrypted() && doc.authenticate_owner_password(credential).is_err() {
        return Err(AppError::Forbidden(
            "changing PDF protection requires the permissions password".into(),
        ));
    }
    Ok(())
}

fn refuse_signed(doc: &Document) -> AppResult<()> {
    if pdfdoc::signature_dict_exists(doc)? {
        return Err(AppError::Forbidden(
            "changing protection on signed PDFs is not supported".into(),
        ));
    }
    Ok(())
}

/// `AccessPermission` bits as lopdf `Permissions`: same PDF bit positions,
/// cleared where a `prevent*` flag is set.
fn requested_permissions(params: &[(String, String)]) -> Permissions {
    let mut permissions = Permissions::all();
    if param_bool(params, "preventAssembly") {
        permissions.remove(Permissions::ASSEMBLABLE);
    }
    if param_bool(params, "preventExtractContent") {
        permissions.remove(Permissions::COPYABLE);
    }
    if param_bool(params, "preventExtractForAccessibility") {
        permissions.remove(Permissions::COPYABLE_FOR_ACCESSIBILITY);
    }
    if param_bool(params, "preventFillInForm") {
        permissions.remove(Permissions::FILLABLE);
    }
    if param_bool(params, "preventModify") {
        permissions.remove(Permissions::MODIFIABLE);
    }
    if param_bool(params, "preventModifyAnnotations") {
        permissions.remove(Permissions::ANNOTABLE);
    }
    if param_bool(params, "preventPrinting") {
        permissions.remove(Permissions::PRINTABLE);
    }
    if param_bool(params, "preventPrintingFaithful") {
        permissions.remove(Permissions::PRINTABLE_IN_HIGH_QUALITY);
    }
    permissions
}

/// lopdf's R2–R4 key derivation reads the trailer `/ID`; PDFs without one
/// (legal but rare — PDFBox generates an ID on save) would fail
/// encryption. Seed a random one like `COSWriter` does.
fn ensure_file_id(doc: &mut Document) {
    if doc.trailer.get(b"ID").is_ok() {
        return;
    }
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    let id = Object::String(bytes.to_vec(), lopdf::StringFormat::Hexadecimal);
    doc.trailer.set("ID", Object::Array(vec![id.clone(), id]));
}

fn aes256_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::rng().fill(&mut key);
    key
}

fn random_owner_password() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn aes_crypt_filters(filter: Arc<dyn CryptFilter>) -> BTreeMap<Vec<u8>, Arc<dyn CryptFilter>> {
    BTreeMap::from([(b"StdCF".to_vec(), filter)])
}

fn v4_state(
    doc: &Document,
    owner_password: &str,
    user_password: &str,
    permissions: Permissions,
) -> Result<EncryptionState, lopdf::Error> {
    EncryptionState::try_from(EncryptionVersion::V4 {
        document: doc,
        encrypt_metadata: true,
        crypt_filters: aes_crypt_filters(Arc::new(Aes128CryptFilter)),
        stream_filter: b"StdCF".to_vec(),
        string_filter: b"StdCF".to_vec(),
        owner_password,
        user_password,
        permissions,
    })
}

fn v5_state(
    owner_password: &str,
    user_password: &str,
    permissions: Permissions,
) -> Result<EncryptionState, lopdf::Error> {
    let key = aes256_key();
    EncryptionState::try_from(EncryptionVersion::V5 {
        encrypt_metadata: true,
        crypt_filters: aes_crypt_filters(Arc::new(Aes256CryptFilter)),
        file_encryption_key: &key,
        stream_filter: b"StdCF".to_vec(),
        string_filter: b"StdCF".to_vec(),
        owner_password,
        user_password,
        permissions,
    })
}

/// `StandardProtectionPolicy` equivalent for the add-password endpoint:
/// 40 → RC4-40 (V1/R2), 128 → AES-128 (V4/R4), 256 → AES-256 (V5/R6),
/// matching PDFBox's defaults.
fn encryption_state(
    doc: &Document,
    key_length: u32,
    owner_password: &str,
    user_password: &str,
    permissions: Permissions,
) -> AppResult<EncryptionState> {
    let state = match key_length {
        40 => EncryptionState::try_from(EncryptionVersion::V1 {
            document: doc,
            owner_password,
            user_password,
            permissions,
        }),
        128 => v4_state(doc, owner_password, user_password, permissions),
        256 => v5_state(owner_password, user_password, permissions),
        other => {
            return Err(AppError::BadRequest(format!(
                "unsupported encryption key length: {other}"
            )))
        }
    };
    state.map_err(|e| AppError::Pdf(format!("encryption setup failed: {e}")))
}

/// The `/Encrypt` dictionary's `/Length` (bits, default 40 like
/// `PDEncryption.getLength`) and `/V` for the permission-preserving
/// re-encryption in remove-password.
fn source_key_length(doc: &Document) -> i64 {
    pdfdoc::encrypt_dict(doc)
        .and_then(|enc| {
            let (_, resolved) = doc.dereference(enc.get(b"Length").ok()?).ok()?;
            resolved.as_i64().ok()
        })
        .unwrap_or(40)
}

fn source_version(doc: &Document) -> i64 {
    pdfdoc::encrypt_dict(doc)
        .and_then(|enc| {
            let (_, resolved) = doc.dereference(enc.get(b"V").ok()?).ok()?;
            resolved.as_i64().ok()
        })
        .unwrap_or(0)
}

/// Re-encryption for remove-password's restricted-permission branch:
/// PDFBox re-`protect`s with a random 32-byte-hex owner password, empty
/// user password, the stored `/P` bits, the stored key length, and AES
/// iff the source `/V` is 4 or later.
fn preserved_state(
    doc: &Document,
    permissions_bits: u32,
    key_length: i64,
    prefer_aes: bool,
) -> AppResult<EncryptionState> {
    let permissions = Permissions::from_bits_retain(permissions_bits as u64);
    let owner_password = random_owner_password();
    let state = match key_length {
        40 => EncryptionState::try_from(EncryptionVersion::V1 {
            document: doc,
            owner_password: &owner_password,
            user_password: "",
            permissions,
        }),
        128 if prefer_aes => v4_state(doc, &owner_password, "", permissions),
        256 => v5_state(&owner_password, "", permissions),
        n if (40..=128).contains(&n) => EncryptionState::try_from(EncryptionVersion::V2 {
            document: doc,
            owner_password: &owner_password,
            user_password: "",
            key_length: n as usize,
            permissions,
        }),
        other => {
            return Err(AppError::BadRequest(format!(
                "unsupported source encryption key length: {other}"
            )))
        }
    };
    state.map_err(|e| AppError::Pdf(format!("encryption setup failed: {e}")))
}

/// `POST /api/v1/security/add-password` — encrypts the document with the
/// requested opening/owner passwords and permission restrictions.
#[stirling_macros::endpoint]
fn add_password(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;

    let password = param(&params, "password", "");
    let owner_password = param(&params, "ownerPassword", "");
    let source_password = param_opt(&params, "sourcePassword");
    let preserve_existing = param_bool(&params, "preserveExisting");
    let key_length = param(&params, "keyLength", "256")
        .trim()
        .parse::<u32>()
        .map_err(|_| AppError::BadRequest("keyLength must be an integer".into()))?;

    let explicit_protection = source_password.is_some() || preserve_existing;

    // Java authenticates an encrypted source with sourcePassword (or the
    // legacy empty credential); lopdf accepts either role, and the owner
    // check below enforces the same gate `requireOwner` applies.
    let mut document =
        pdfdoc::load_with_password(&upload.name, &upload.bytes, source_password.unwrap_or(""))?;
    let was_encrypted = document.is_encrypted();

    if explicit_protection || was_encrypted {
        let credential = opening_credential(&document, source_password.unwrap_or(""));
        require_owner(&document, credential)?;
        refuse_signed(&document)?;
    }

    if preserve_existing && was_encrypted {
        return Err(AppError::BadRequest(
            "preserveExisting on an encrypted file is not yet supported by the Rust backend".into(),
        ));
    }

    let permissions = requested_permissions(&params);
    let restricted = permissions != Permissions::all();

    if explicit_protection {
        if restricted && (owner_password.is_empty() || owner_password == password) {
            return Err(AppError::BadRequest(
                "restrictions require a nonempty permissions password different from the opening password"
                    .into(),
            ));
        }
        if owner_password.is_empty() && password.is_empty() {
            return Err(AppError::BadRequest(
                "enter a password for the new protection".into(),
            ));
        }
    }
    if !matches!(key_length, 40 | 128 | 256) {
        return Err(AppError::BadRequest(
            "unsupported encryption key length".into(),
        ));
    }

    strip_encryption(&mut document);
    ensure_file_id(&mut document);
    let state = encryption_state(&document, key_length, owner_password, password, permissions)?;
    document
        .encrypt(&state)
        .map_err(|e| AppError::Pdf(format!("encryption failed: {e}")))?;

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    let suffix = if owner_password.is_empty() && password.is_empty() {
        "_permissions.pdf"
    } else {
        "_passworded.pdf"
    };
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), suffix),
    ))
}

/// `POST /api/v1/security/remove-password` — decrypts the document;
/// restricted-permission sources are re-encrypted with a random owner
/// password and an empty user password, preserving `/P`, key length and
/// cipher choice, matching PDFBox's `protect` fallback.
#[stirling_macros::endpoint]
fn remove_password(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;

    let remove_all = param_bool(&params, "removeAll");
    let password = param(&params, "password", "");

    let mut document = if remove_all {
        // Java's removeAll authenticates with sourcePassword when given,
        // else password, then requires owner access and refuses signed
        // files; the selective fields do not apply.
        let credential = param_opt(&params, "sourcePassword").unwrap_or(password);
        let document = pdfdoc::load_with_password(&upload.name, &upload.bytes, credential)?;
        require_owner(&document, opening_credential(&document, credential))?;
        refuse_signed(&document)?;
        document
    } else {
        for field in ["ownerPassword", "sourcePassword", "retainedOpeningPassword"] {
            if param_opt(&params, field).is_some() {
                return Err(AppError::BadRequest(format!(
                    "selective password removal ({field}) is not yet supported by the Rust backend"
                )));
            }
        }
        pdfdoc::load_with_password(&upload.name, &upload.bytes, password)?
    };

    if !document.is_encrypted() {
        pdfdoc::touch_loaded_info(&mut document);
        let bytes = pdfdoc::save(&mut document)?;
        return Ok(pdf_response(
            bytes,
            &output_name(Some(&upload.name), "_password_removed.pdf"),
        ));
    }

    // Java reads the stored permission bits before stripping: owner-level
    // access (all eight bits) means full removal, otherwise the document
    // is re-protected so the restrictions survive.
    let owner_level = pdfdoc::encrypt_permissions(&document)
        .is_some_and(|p| p & OWNER_PERMISSION_MASK == OWNER_PERMISSION_MASK);
    if owner_level || remove_all {
        strip_encryption(&mut document);
    } else {
        let permissions = pdfdoc::encrypt_permissions(&document).unwrap_or(0);
        let key_length = source_key_length(&document);
        let prefer_aes = source_version(&document) >= 4;
        strip_encryption(&mut document);
        ensure_file_id(&mut document);
        let state = preserved_state(&document, permissions, key_length, prefer_aes)?;
        document
            .encrypt(&state)
            .map_err(|e| AppError::Pdf(format!("encryption failed: {e}")))?;
    }

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_password_removed.pdf"),
    ))
}

/// `POST /api/v1/security/prepare-pdf-session` — produces the decrypted
/// working copy the interactive editor operates on. `PdfSessionPermissions
/// .requireOperation` is reproduced: the `endpoint` parameter must name an
/// allow-listed operation, signed protected PDFs are refused, and when
/// `security.honorOwnerPasswords` applies a user-authenticated document
/// additionally needs the permission the operation requires.
///
/// Divergence: Java's `isOwnerAuthenticated` reads which credential
/// PDFBox recorded at load; here owner role is re-tested with the
/// credential that actually opened the document (`opening_credential`),
/// so a password matching both roles counts as owner-authenticated.
#[stirling_macros::endpoint]
fn prepare_pdf_session(
    _state: AppState,
    query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let password = param_opt(&params, "password").unwrap_or("");
    // `@RequestParam` binds query strings and form fields alike.
    let endpoint = param_opt(&params, "endpoint")
        .map(str::to_owned)
        .or_else(|| query.get("endpoint").cloned())
        .ok_or_else(|| AppError::BadRequest("endpoint is required".into()))?;

    let mut document = pdfdoc::load_with_password(&upload.name, &upload.bytes, password)?;
    let credential = opening_credential(&document, password);
    require_operation(&document, &endpoint, credential)?;

    // `setAllSecurityToBeRemoved(true)`: the working copy is plaintext,
    // so the stale trailer entry must go before saving.
    strip_encryption(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    let name = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    Ok(pdf_response(bytes, name))
}

/// `AccessPermission` bit masks (`1 << (bit - 1)`): modify = bit 4,
/// extract = bit 5, assemble = bit 11.
const PERMISSION_MODIFY: u32 = 0x8;
const PERMISSION_EXTRACT: u32 = 0x10;
const PERMISSION_ASSEMBLE: u32 = 0x400;

fn permission_allows(doc: &Document, mask: u32) -> bool {
    pdfdoc::encrypt_permissions(doc).is_some_and(|p| p & mask != 0)
}

/// `/Encrypt` `/R`, the revision `PDEncryption.getRevision` reports;
/// 0 when absent, which never equals the revision-2 special case.
fn encryption_revision(doc: &Document) -> i64 {
    pdfdoc::encrypt_dict(doc)
        .and_then(|d| d.get(b"R").ok().and_then(|o| o.as_i64().ok()))
        .unwrap_or(0)
}

fn require_operation(doc: &Document, endpoint: &str, credential: &str) -> AppResult<()> {
    // The allow-list is evaluated before the encryption checks, so an
    // unknown endpoint is refused even for unencrypted input.
    let permitted = match endpoint {
        "/api/v1/general/rotate-pdf"
        | "/api/v1/general/remove-pages"
        | "/api/v1/general/rearrange-pages"
        | "/api/v1/general/merge-pdfs" => {
            // Revision 2 predates the assemble bit, so Java substitutes
            // canModify there.
            if pdfdoc::is_encrypted(doc) && encryption_revision(doc) == 2 {
                permission_allows(doc, PERMISSION_MODIFY)
            } else {
                permission_allows(doc, PERMISSION_ASSEMBLE)
            }
        }
        "/api/v1/general/split-pages"
        | "/api/v1/general/split-pdf-by-sections"
        | "/api/v1/general/split-by-size-or-count"
        | "/api/v1/general/split-pdf-by-chapters"
        | "/api/v1/general/split-for-poster-print"
        | "/api/v1/misc/auto-split-pdf" => permission_allows(doc, PERMISSION_EXTRACT),
        "/api/v1/misc/compress-pdf" => permission_allows(doc, PERMISSION_MODIFY),
        _ => {
            return Err(AppError::BadRequest(
                "This operation cannot preserve PDF protection".into(),
            ));
        }
    };
    if !pdfdoc::is_encrypted(doc) {
        return Ok(());
    }
    if pdfdoc::signature_dict_exists(doc)? {
        return Err(AppError::Forbidden(
            "Signed protected PDFs support viewing only".into(),
        ));
    }
    // `honorOwnerPasswords` defaults to true in ApplicationProperties.
    if doc.authenticate_owner_password(credential).is_err() && !permitted {
        return Err(AppError::Forbidden(
            "This operation requires the permissions password".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    /// The add-password pipeline minus HTTP: encrypt a one-page fixture at
    /// `key_length` and return the saved bytes.
    fn encrypted_pdf(key_length: u32, owner: &str, user: &str) -> Vec<u8> {
        let mut pdf = testkit::Pdf::new();
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        ensure_file_id(&mut doc);
        let state = encryption_state(&doc, key_length, owner, user, Permissions::all()).unwrap();
        doc.encrypt(&state).unwrap();
        pdfdoc::save(&mut doc).unwrap()
    }

    #[test]
    fn encrypted_pdf_opens_with_user_and_owner_passwords() {
        for key_length in [40, 128, 256] {
            let bytes = encrypted_pdf(key_length, "owner-secret", "user-secret");
            for credential in ["user-secret", "owner-secret"] {
                let doc = pdfdoc::load_with_password("t.pdf", &bytes, credential).unwrap();
                assert_eq!(doc.get_pages().len(), 1);
            }
            assert!(pdfdoc::load_with_password("t.pdf", &bytes, "wrong").is_err());
        }
    }

    #[test]
    fn encrypted_pdf_stores_requested_permission_bits() {
        let mut pdf = testkit::Pdf::new();
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        ensure_file_id(&mut doc);
        let permissions = Permissions::all() - Permissions::PRINTABLE;
        let state = encryption_state(&doc, 256, "owner", "user", permissions).unwrap();
        doc.encrypt(&state).unwrap();
        let bytes = pdfdoc::save(&mut doc).unwrap();

        // Raw load keeps the trailer /Encrypt dictionary readable.
        let raw = Document::load_mem(&bytes).unwrap();
        let stored = pdfdoc::encrypt_permissions(&raw).unwrap();
        assert_eq!(stored as u64 & permissions.bits(), permissions.bits());
        assert_eq!(stored & OWNER_PERMISSION_MASK, OWNER_PERMISSION_MASK & !0x4);
    }

    #[test]
    fn strip_encryption_allows_reencryption() {
        let bytes = encrypted_pdf(128, "owner", "user");
        let mut doc = pdfdoc::load_with_password("t.pdf", &bytes, "user").unwrap();
        strip_encryption(&mut doc);
        assert!(!pdfdoc::is_encrypted(&doc));
        ensure_file_id(&mut doc);
        let state = encryption_state(&doc, 40, "o2", "u2", Permissions::all()).unwrap();
        doc.encrypt(&state).unwrap();
        let bytes = pdfdoc::save(&mut doc).unwrap();
        let reopened = pdfdoc::load_with_password("t.pdf", &bytes, "u2").unwrap();
        assert_eq!(reopened.get_pages().len(), 1);
    }
}
