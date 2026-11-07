//! Port of `CertSignController` (`POST /api/v1/security/cert-sign`):
//! digitally sign a PDF with an uploaded certificate, producing a CMS
//! (PKCS#7) `adbe.pkcs7.detached` signature embedded through a lopdf
//! incremental update — the same machinery `timestamp-pdf` uses.
//!
//! Deliberate divergences from Java:
//! - `certType=JKS` is rejected: the JKS keystore format has no Rust
//!   counterpart here.
//! - `certType=SERVER` reports the same "not available in this edition"
//!   error Java gives when `ServerCertificateService` has no bean — the
//!   only implementation is proprietary.
//! - `WINDOWS_STORE`/`PKCS11` report the desktop-only error; hardware
//!   signing stays on the desktop/Java path.
//! - PEM accepts PKCS#8 (`PRIVATE KEY` / `ENCRYPTED PRIVATE KEY`) and
//!   unencrypted PKCS#1 RSA / SEC1 EC keys. OpenSSL-legacy encrypted PEM
//!   (`Proc-Type`/`DEK-Info`) is rejected.
//! - PKCS#12 supports the PKCS#12-KDF PBE algorithms (3DES/RC2-CBC) and
//!   PBES2 (PBKDF2 + DES/3DES/AES-CBC). RC4 PBE bags and RC2 inside
//!   PBES2 are rejected.
//! - The PKCS#12 MAC is not verified; a wrong password surfaces as a
//!   decryption failure. For PBES2 the KDF password is tried as the
//!   PKCS#12 BMPString form first, then raw — implementations disagree
//!   on which RFC 7292 mandates.
//! - `alias` selects a PKCS#12 key bag by its `friendlyName` attribute;
//!   Java resolves keystore aliases, which for PKCS#12 are the same names.
//! - The certificate matching the private key's public half is moved to
//!   the front of the embedded chain; Java trusts keystore order.

use std::collections::HashMap;

use axum::response::Response;
use base64::Engine as _;
use cms::builder::{SignedDataBuilder, SignerInfoBuilder};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::ContentInfo;
use cms::encrypted_data::EncryptedData;
use cms::signed_data::{EncapsulatedContentInfo, SignerIdentifier};
use der::asn1::{
    BitString, BmpString, ObjectIdentifier, OctetString, PrintableStringRef, TeletexStringRef,
    Utf8StringRef,
};
use der::{Any, Decode, Encode, Sequence, Tagged};
use lopdf::{dictionary, Dictionary, Document, Object, Stream, StringFormat};
use pkcs8::PrivateKeyInfo;
use signature::{Keypair, Signer};
use spki::{AlgorithmIdentifierOwned, DynSignatureAlgorithmIdentifier, SignatureBitStringEncoding};
use x509_cert::certificate::Certificate;

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::pdf_response;
use crate::handlers::visual::embed_image;
use crate::pdfdoc;
use crate::signature::oid;
use crate::timestamp::{
    embed_signature_token, prepare_signed_revision, pdf_date_now, AppearanceFactory,
    RESERVED_CONTENTS_LEN,
};
use crate::upload::{first_upload, output_name, param, param_bool, param_parse, Upload};

const OID_DATA: &str = "1.2.840.113549.1.7.1";
const OID_ENCRYPTED_DATA: &str = "1.2.840.113549.1.7.6";
const OID_RSA_ENCRYPTION: &str = "1.2.840.113549.1.1.1";
const OID_ID_EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
const OID_ED25519: &str = "1.3.101.112";
const OID_SHA256_WITH_RSA: &str = "1.2.840.113549.1.1.11";
const OID_ECDSA_SHA256: &str = "1.2.840.10045.4.3.2";
const OID_ECDSA_SHA384: &str = "1.2.840.10045.4.3.3";
const OID_SHA256_DIGEST: &str = "2.16.840.1.101.3.4.2.1";
const OID_P256: &str = "1.2.840.10045.3.1.7";
const OID_P384: &str = "1.3.132.0.34";
const OID_CN: &str = "2.5.4.3";
const OID_NULL: [u8; 2] = [0x05, 0x00];

// PKCS#12 / PBES2 algorithm identifiers.
const OID_PBES2: &str = "1.2.840.113549.1.5.13";
const OID_PBKDF2: &str = "1.2.840.113549.1.5.12";
const OID_DES_EDE3_CBC: &str = "1.2.840.113549.3.7";
const OID_DES_CBC: &str = "1.3.14.3.2.7";
const OID_RC2_CBC: &str = "1.2.840.113549.3.2";
const OID_AES128_CBC: &str = "2.16.840.1.101.3.4.1.2";
const OID_AES192_CBC: &str = "2.16.840.1.101.3.4.1.22";
const OID_AES256_CBC: &str = "2.16.840.1.101.3.4.1.42";
const OID_HMAC_SHA1: &str = "1.2.840.113549.2.7";
const OID_HMAC_SHA224: &str = "1.2.840.113549.2.8";
const OID_HMAC_SHA256: &str = "1.2.840.113549.2.9";
const OID_HMAC_SHA384: &str = "1.2.840.113549.2.10";
const OID_HMAC_SHA512: &str = "1.2.840.113549.2.11";
const OID_HMAC_SHA224_NIST: &str = "2.16.840.1.101.3.4.2.4";
const OID_HMAC_SHA256_NIST: &str = "2.16.840.1.101.3.4.2.1";
const OID_HMAC_SHA384_NIST: &str = "2.16.840.1.101.3.4.2.2";
const OID_HMAC_SHA512_NIST: &str = "2.16.840.1.101.3.4.2.3";
const OID_FRIENDLY_NAME: &str = "1.2.840.113549.1.9.20";
const OID_LOCAL_KEY_ID: &str = "1.2.840.113549.1.9.21";

// PKCS#12-KDF PBE OIDs (params: salt + iterations).
const PBE_RC4_128: &str = "1.2.840.113549.1.12.1.1";
const PBE_RC4_40: &str = "1.2.840.113549.1.12.1.2";
const PBE_3DES_CBC: &str = "1.2.840.113549.1.12.1.3";
const PBE_2KEY_3DES_CBC: &str = "1.2.840.113549.1.12.1.4";
const PBE_RC2_128_CBC: &str = "1.2.840.113549.1.12.1.5";
const PBE_RC2_40_CBC: &str = "1.2.840.113549.1.12.1.6";

const SIGNATURE_RECT: [i64; 4] = [0, 0, 200, 50];
const APPEARANCE_HEIGHT: f64 = 50.0;
const FONT_SIZE: f64 = 10.0;
const LEADING: f64 = 15.0;

static LOGO_PNG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../app/core/src/main/resources/static/images/signature.png"
));

// ---------------------------------------------------------------------------
// Endpoint
// ---------------------------------------------------------------------------

/// `POST /api/v1/security/cert-sign` —
/// `CertSignController.signPDFWithCert`: CMS-sign the PDF with an uploaded
/// certificate and return the incrementally-updated file.
#[stirling_macros::endpoint(files = "privateKeyFile,certFile,p12File,jksFile")]
fn cert_sign(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let document = pdfdoc::load(&upload.name, &upload.bytes)?;

    let cert_type = param(&params, "certType", "");
    if cert_type.trim().is_empty() {
        return Err(AppError::BadRequest(
            "certificate type options are not specified".into(),
        ));
    }
    let identity = signing_identity(
        cert_type,
        &extras,
        param(&params, "password", ""),
        param(&params, "alias", ""),
    )?;

    let show_signature = param_bool(&params, "showSignature");
    // showLogo defaults to true; only an explicit false-ish value disables it.
    let show_logo = !param_is_false(&params, "showLogo");
    let reason = param(&params, "reason", "");
    let location = param(&params, "location", "");
    let name = param(&params, "name", "");
    // Java binds pageNumber as Integer defaulting to 1 and converts to
    // 0-based only for the visible signature.
    let page_number: i64 = param_parse(&params, "pageNumber", 1)?;

    let sig_dict = signature_dictionary(reason, location, name);
    let widget = if show_signature {
        let page_count = pdfdoc::page_ids(&document).len();
        let page_index = resolve_signature_page(page_number, page_count)?;
        let cn = leaf_common_name(&identity.chain[0]).unwrap_or_default();
        let sign_date = java_date_string();
        let reason_owned = (!reason.is_empty()).then(|| reason.to_string());
        Some((
            SIGNATURE_RECT,
            Box::new(move |doc: &mut Document| {
                appearance_stream(doc, &cn, &sign_date, reason_owned.as_deref(), show_logo)
            }) as Box<AppearanceFactory<'_>>,
            page_index,
        ))
    } else {
        None
    };

    let page_index = widget.as_ref().map(|(_, _, i)| *i).unwrap_or(0);
    let prepared = prepare_signed_revision(
        &upload.bytes,
        document,
        sig_dict,
        widget.map(|(rect, factory, _)| (rect, factory)),
        page_index,
    )?;

    let cms = build_cms_signed_data(&identity, &prepared.hash)?;
    let signed = embed_signature_token(prepared, &cms)?;
    Ok(pdf_response(
        signed,
        &output_name(Some(&upload.name), "_signed.pdf"),
    ))
}

/// `param_bool` inverted for parameters that default to true (`showLogo`).
fn param_is_false(params: &[(String, String)], name: &str) -> bool {
    params
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .is_some_and(|(_, v)| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "false" | "0" | "off" | "no"
            )
        })
}

/// `CertSignController.resolveSignaturePage`: pageNumber is 1-based in the
/// request; the widget needs a real page.
fn resolve_signature_page(page_number: i64, page_count: usize) -> AppResult<usize> {
    let index = page_number - 1;
    if index < 0 || index as usize >= page_count {
        return Err(AppError::BadRequest(format!(
            "Invalid argument: pageNumber {} - the visible signature needs a page from 1 to {page_count}",
            index + 1
        )));
    }
    Ok(index as usize)
}

/// The signature dictionary PDFBox's `PDSignature` produces for a regular
/// signature: `/Type /Sig`, PPKLite filter, detached PKCS#7 subfilter,
/// optional name/location/reason, `/M` sign date, reserved `/Contents`.
fn signature_dictionary(reason: &str, location: &str, name: &str) -> Dictionary {
    let mut sig = Dictionary::new();
    sig.set("Type", "Sig");
    sig.set("Filter", "Adobe.PPKLite");
    sig.set("SubFilter", "adbe.pkcs7.detached");
    for (key, value) in [
        ("Name", name),
        ("Location", location),
        ("Reason", reason),
    ] {
        if !value.is_empty() {
            sig.set(key, Object::string_literal(value));
        }
    }
    sig.set("M", Object::string_literal(pdf_date_now()));
    sig.set(
        "Contents",
        Object::String(vec![0u8; RESERVED_CONTENTS_LEN], StringFormat::Hexadecimal),
    );
    sig.set(
        "ByteRange",
        Object::Array(vec![Object::Integer(0); 4]),
    );
    sig
}

// ---------------------------------------------------------------------------
// Signing keys + CMS
// ---------------------------------------------------------------------------

/// Uniform signature output for the CMS builder: the bytes that land in
/// `SignerInfo.signature` — raw for RSA/Ed25519, DER `Ecdsa-Sig-Value`
/// for ECDSA (what `signature.rs` verification expects).
struct CmsSignature(Vec<u8>);

impl SignatureBitStringEncoding for CmsSignature {
    fn to_bitstring(&self) -> der::Result<BitString> {
        BitString::new(0, self.0.clone())
    }
}

/// Verifying-key counterpart kept only to satisfy `Keypair`; never used
/// for verification here.
#[derive(Clone)]
enum AnyVerifyingKey {
    Rsa(rsa::RsaPublicKey),
    P256(p256::ecdsa::VerifyingKey),
    P384(p384::ecdsa::VerifyingKey),
    Ed25519(ed25519_dalek::VerifyingKey),
}

/// Private key material reduced to the algorithms Java's
/// `resolveSignatureAlgorithm` emits for software keystores: RSA PKCS#1
/// v1.5 with SHA-256, ECDSA with SHA-256, Ed25519.
enum SigningKey {
    Rsa(rsa::pkcs1v15::SigningKey<sha2::Sha256>),
    P256(p256::ecdsa::SigningKey),
    P384(p384::ecdsa::SigningKey),
    Ed25519(Ed25519Signer),
}

/// `ed25519_dalek::Signature` does not implement
/// `SignatureBitStringEncoding`, so Ed25519 goes through this wrapper —
/// the CMS builder only needs `Signer<CmsSignature>` + algorithm id.
struct Ed25519Signer(ed25519_dalek::SigningKey);

impl Keypair for Ed25519Signer {
    type VerifyingKey = ed25519_dalek::VerifyingKey;

    fn verifying_key(&self) -> Self::VerifyingKey {
        self.0.verifying_key()
    }
}

impl DynSignatureAlgorithmIdentifier for Ed25519Signer {
    fn signature_algorithm_identifier(&self) -> spki::Result<AlgorithmIdentifierOwned> {
        Ok(AlgorithmIdentifierOwned {
            oid: oid(OID_ED25519),
            parameters: None,
        })
    }
}

impl Signer<CmsSignature> for Ed25519Signer {
    fn try_sign(&self, msg: &[u8]) -> signature::Result<CmsSignature> {
        Ok(CmsSignature(self.0.sign(msg).to_bytes().to_vec()))
    }
}

impl Keypair for SigningKey {
    type VerifyingKey = AnyVerifyingKey;

    fn verifying_key(&self) -> Self::VerifyingKey {
        match self {
            Self::Rsa(k) => AnyVerifyingKey::Rsa(k.as_ref().to_public_key()),
            Self::P256(k) => AnyVerifyingKey::P256(*k.verifying_key()),
            Self::P384(k) => AnyVerifyingKey::P384(*k.verifying_key()),
            Self::Ed25519(k) => AnyVerifyingKey::Ed25519(k.verifying_key()),
        }
    }
}

impl DynSignatureAlgorithmIdentifier for SigningKey {
    fn signature_algorithm_identifier(&self) -> spki::Result<AlgorithmIdentifierOwned> {
        match self {
            // rsa's SigningKey has no DynSignatureAlgorithmIdentifier;
            // BC emits sha256WithRSAEncryption with NULL parameters.
            Self::Rsa(_) => Ok(AlgorithmIdentifierOwned {
                oid: oid(OID_SHA256_WITH_RSA),
                parameters: Some(Any::from_der(&OID_NULL).expect("NULL is valid DER")),
            }),
            // ecdsa::SigningKey carries no signature-algorithm id; Java
            // emits ecdsa-with-SHA256/384 with absent parameters (RFC 5758).
            Self::P256(_) => Ok(AlgorithmIdentifierOwned {
                oid: oid(OID_ECDSA_SHA256),
                parameters: None,
            }),
            Self::P384(_) => Ok(AlgorithmIdentifierOwned {
                oid: oid(OID_ECDSA_SHA384),
                parameters: None,
            }),
            Self::Ed25519(k) => k.signature_algorithm_identifier(),
        }
    }
}

impl Signer<CmsSignature> for SigningKey {
    fn try_sign(&self, msg: &[u8]) -> signature::Result<CmsSignature> {
        match self {
            Self::Rsa(k) => k
                .try_sign(msg)
                .map(|s| CmsSignature(signature::SignatureEncoding::to_bytes(&s).to_vec())),
            Self::P256(k) => k.try_sign(msg).and_then(|s| {
                s.to_der()
                    .map(|d| CmsSignature(d.to_bytes().to_vec()))
                    .map_err(|_| signature::Error::new())
            }),
            Self::P384(k) => k.try_sign(msg).and_then(|s| {
                s.to_der()
                    .map(|d| CmsSignature(d.to_bytes().to_vec()))
                    .map_err(|_| signature::Error::new())
            }),
            Self::Ed25519(k) => k.try_sign(msg),
        }
    }
}

/// SPKI DER of the public half, used to pick the chain's leaf certificate.
fn public_key_spki_der(key: &SigningKey) -> Option<Vec<u8>> {
    use spki::EncodePublicKey as _;
    match key.verifying_key() {
        AnyVerifyingKey::Rsa(rsa) => {
            // rsaEncryption AlgorithmIdentifier + BIT STRING PKCS#1 key.
            let pkcs1 = rsa::pkcs1::EncodeRsaPublicKey::to_pkcs1_der(&rsa).ok()?;
            let bit_string = BitString::from_bytes(pkcs1.as_bytes()).ok()?;
            #[derive(Sequence)]
            struct SpkiRsa {
                algorithm: AlgorithmIdentifierOwned,
                subject_public_key: BitString,
            }
            SpkiRsa {
                algorithm: AlgorithmIdentifierOwned {
                    oid: oid(OID_RSA_ENCRYPTION),
                    parameters: Some(Any::from_der(&OID_NULL).ok()?),
                },
                subject_public_key: bit_string,
            }
            .to_der()
            .ok()
        }
        AnyVerifyingKey::P256(k) => k.to_public_key_der().ok().map(|d| d.to_vec()),
        AnyVerifyingKey::P384(k) => k.to_public_key_der().ok().map(|d| d.to_vec()),
        AnyVerifyingKey::Ed25519(k) => {
            // ed25519-dalek has no EncodePublicKey; SPKI is
            // SEQUENCE { OID 1.3.101.112, BIT STRING(32-byte key) }.
            #[derive(Sequence)]
            struct SpkiEd25519 {
                algorithm: AlgorithmIdentifierOwned,
                subject_public_key: BitString,
            }
            SpkiEd25519 {
                algorithm: AlgorithmIdentifierOwned {
                    oid: oid(OID_ED25519),
                    parameters: None,
                },
                subject_public_key: BitString::from_bytes(k.as_bytes()).ok()?,
            }
            .to_der()
            .ok()
            .map(|d| d.to_vec())
        }
    }
}

struct SigningIdentity {
    key: SigningKey,
    /// Certificate chain, leaf (key-matching) certificate first — what BC
    /// embeds in the CMS.
    chain: Vec<Certificate>,
}

/// Move the certificate whose SPKI matches the signing key to the front;
/// the rest keep file order as the chain.
fn order_chain_by_key(mut chain: Vec<Certificate>, key: &SigningKey) -> Vec<Certificate> {
    let Some(spki) = public_key_spki_der(key) else {
        return chain;
    };
    if let Some(pos) = chain.iter().position(|c| {
        c.tbs_certificate
            .subject_public_key_info
            .to_der()
            .map(|d| d == spki)
            .unwrap_or(false)
    }) {
        let leaf = chain.remove(pos);
        chain.insert(0, leaf);
    }
    chain
}

/// SHA-256 `AlgorithmIdentifier` with NULL parameters, matching BC's
/// `DigestAlgorithmIdentifier` output.
fn sha256_alg_id() -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid: oid(OID_SHA256_DIGEST),
        parameters: Some(Any::from_der(&OID_NULL).expect("NULL is valid DER")),
    }
}

/// `CreateSignatureBase.sign`: CMS SignedData with signed attributes
/// (contentType, messageDigest, signingTime) over the ByteRange-covered
/// bytes, detached content, SHA-256 throughout like Java's defaults.
fn build_cms_signed_data(identity: &SigningIdentity, covered_hash: &[u8; 32]) -> AppResult<Vec<u8>> {
    let leaf = &identity.chain[0];
    let tbs = &leaf.tbs_certificate;
    let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
        issuer: tbs.issuer.clone(),
        serial_number: tbs.serial_number.clone(),
    });

    let eci = EncapsulatedContentInfo {
        econtent_type: oid(OID_DATA),
        econtent: None,
    };
    let mut signer_builder = SignerInfoBuilder::new(
        &identity.key,
        sid,
        sha256_alg_id(),
        &eci,
        Some(&covered_hash[..]),
    )
    .map_err(|e| AppError::Internal(anyhow::anyhow!("CMS signer init: {e}")))?;
    signer_builder
        .add_signed_attribute(
            cms::builder::create_signing_time_attribute()
                .map_err(|e| AppError::Internal(anyhow::anyhow!("CMS signingTime: {e}")))?,
        )
        .map_err(|e| AppError::Internal(anyhow::anyhow!("CMS attribute: {e}")))?;

    let mut data_builder = SignedDataBuilder::new(&eci);
    data_builder
        .add_digest_algorithm(sha256_alg_id())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("CMS digest alg: {e}")))?;
    for cert in &identity.chain {
        data_builder
            .add_certificate(CertificateChoices::Certificate(cert.clone()))
            .map_err(|e| AppError::Internal(anyhow::anyhow!("CMS certificate: {e}")))?;
    }
    data_builder
        .add_signer_info(signer_builder)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("CMS signer info: {e}")))?;
    data_builder
        .build()
        .and_then(|ci| ci.to_der().map_err(Into::into))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("CMS encode: {e}")))
}

// ---------------------------------------------------------------------------
// Identity loading by certType
// ---------------------------------------------------------------------------

fn extra<'a>(extras: &'a [(String, Upload)], name: &str) -> Option<&'a Upload> {
    extras.iter().find(|(n, _)| n == name).map(|(_, u)| u)
}

fn signing_identity(
    cert_type: &str,
    extras: &[(String, Upload)],
    password: &str,
    alias: &str,
) -> AppResult<SigningIdentity> {
    match cert_type {
        "PEM" => identity_from_pem(extras, password),
        "PKCS12" | "PFX" => identity_from_pkcs12(extras, password, alias),
        "JKS" => Err(AppError::BadRequest(
            "JKS keystores are not supported by the Rust backend".into(),
        )),
        "SERVER" => Err(AppError::BadRequest(
            "Server certificate service is not available in this edition".into(),
        )),
        "WINDOWS_STORE" | "PKCS11" => Err(AppError::BadRequest(
            "Hardware-backed signing is only available in the Stirling PDF desktop app".into(),
        )),
        other => Err(AppError::BadRequest(format!(
            "Invalid argument: certificate type: {other}"
        ))),
    }
}

fn require_extra<'a>(
    extras: &'a [(String, Upload)],
    name: &str,
    label: &str,
) -> AppResult<&'a Upload> {
    extra(extras, name).ok_or_else(|| {
        AppError::BadRequest(format!("Invalid argument: {label} - {name} file is required"))
    })
}

// -- PEM -----------------------------------------------------------------

fn identity_from_pem(extras: &[(String, Upload)], password: &str) -> AppResult<SigningIdentity> {
    let key_file = require_extra(extras, "privateKeyFile", "PEM private key")?;
    let cert_file = require_extra(extras, "certFile", "PEM certificate")?;

    let key = private_key_from_pem(&key_file.bytes, password)?;
    let chain = order_chain_by_key(cert_chain_from_pem(&cert_file.bytes)?, &key);
    Ok(SigningIdentity { key, chain })
}

/// Java accepts PEM or DER for the certificate file; a PEM bundle may hold
/// the whole chain.
fn cert_chain_from_pem(bytes: &[u8]) -> AppResult<Vec<Certificate>> {
    if let Ok(chain) = Certificate::load_pem_chain(bytes) {
        if !chain.is_empty() {
            return Ok(chain);
        }
    }
    if let Ok(cert) = Certificate::from_der(bytes) {
        return Ok(vec![cert]);
    }
    Err(AppError::BadRequest(
        "Invalid argument: PEM certificate - the file could not be read as a certificate".into(),
    ))
}

/// One PEM block: label + DER payload.
struct PemBlock {
    label: String,
    der: Vec<u8>,
}

fn pem_blocks(bytes: &[u8]) -> Vec<PemBlock> {
    let text = String::from_utf8_lossy(bytes);
    let mut blocks = Vec::new();
    let mut rest = text.as_ref();
    while let Some(begin) = rest.find("-----BEGIN ") {
        let after_begin = &rest[begin + "-----BEGIN ".len()..];
        let Some(label_end) = after_begin.find("-----") else {
            break;
        };
        let label = after_begin[..label_end].trim().to_string();
        let body_start = begin + "-----BEGIN ".len() + label_end + 5;
        let end_marker = format!("-----END {label}-----");
        let Some(end) = rest[body_start..].find(&end_marker) else {
            break;
        };
        let body = &rest[body_start..body_start + end];
        if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(
            body.chars().filter(|c| !c.is_whitespace()).collect::<String>(),
        ) {
            blocks.push(PemBlock { label, der });
        }
        rest = &rest[body_start + end + end_marker.len()..];
    }
    blocks
}

fn private_key_from_pem(bytes: &[u8], password: &str) -> AppResult<SigningKey> {
    let blocks = pem_blocks(bytes);
    let key_labels = [
        "PRIVATE KEY",
        "ENCRYPTED PRIVATE KEY",
        "RSA PRIVATE KEY",
        "EC PRIVATE KEY",
    ];
    let Some(block) = blocks.iter().find(|b| key_labels.contains(&b.label.as_str())) else {
        // A DER PKCS#8 PrivateKeyInfo is also plausible input.
        if let Ok(pki) = PrivateKeyInfo::from_der(bytes) {
            return signer_from_private_key_info(&pki);
        }
        return Err(AppError::BadRequest(
            "Invalid argument: PEM private key - no private key found in the file".into(),
        ));
    };
    match block.label.as_str() {
        "PRIVATE KEY" => {
            let pki = PrivateKeyInfo::from_der(&block.der).map_err(|e| {
                AppError::BadRequest(format!("Invalid argument: PEM private key: {e}"))
            })?;
            signer_from_private_key_info(&pki)
        }
        "ENCRYPTED PRIVATE KEY" => {
            if password.is_empty() {
                return Err(AppError::BadRequest(
                    "Invalid argument: PEM private key - the key is encrypted, so its password is required"
                        .into(),
                ));
            }
            let encrypted = pkcs8::EncryptedPrivateKeyInfo::from_der(&block.der).map_err(|e| {
                AppError::BadRequest(format!("Invalid argument: PEM private key: {e}"))
            })?;
            let doc = encrypted.decrypt(password.as_bytes()).map_err(|_| {
                AppError::BadRequest(
                    "Invalid argument: password - the PEM private key password is incorrect or missing"
                        .into(),
                )
            })?;
            let pki = PrivateKeyInfo::from_der(doc.as_bytes()).map_err(|e| {
                AppError::BadRequest(format!("Invalid argument: PEM private key: {e}"))
            })?;
            signer_from_private_key_info(&pki)
        }
        "RSA PRIVATE KEY" => {
            use rsa::pkcs1::DecodeRsaPrivateKey as _;
            let rsa_key = rsa::RsaPrivateKey::from_pkcs1_der(&block.der).map_err(|e| {
                AppError::BadRequest(format!("Invalid argument: PEM private key: {e}"))
            })?;
            Ok(SigningKey::Rsa(rsa::pkcs1v15::SigningKey::from(rsa_key)))
        }
        "EC PRIVATE KEY" => ec_signing_key_from_sec1(&block.der),
        _ => unreachable!(),
    }
}

fn ec_signing_key_from_sec1(der: &[u8]) -> AppResult<SigningKey> {
    use sec1::DecodeEcPrivateKey as _;
    if let Ok(key) = p256::ecdsa::SigningKey::from_sec1_der(der) {
        return Ok(SigningKey::P256(key));
    }
    if let Ok(key) = p384::ecdsa::SigningKey::from_sec1_der(der) {
        return Ok(SigningKey::P384(key));
    }
    Err(AppError::BadRequest(
        "Invalid argument: PEM private key - could not read the EC key".into(),
    ))
}

/// `CreateSignatureBase` constructor: PKCS#8 `PrivateKeyInfo` → the
/// concrete signing key, keyed on the algorithm OID.
fn signer_from_private_key_info(pki: &PrivateKeyInfo<'_>) -> AppResult<SigningKey> {
    let alg = pki.algorithm.oid.to_string();
    match alg.as_str() {
        OID_RSA_ENCRYPTION => rsa::pkcs1v15::SigningKey::<sha2::Sha256>::try_from(pki.clone())
            .map(SigningKey::Rsa)
            .map_err(|e| {
                AppError::BadRequest(format!("Invalid argument: PEM private key: {e}"))
            }),
        OID_ID_EC_PUBLIC_KEY => {
            let curve = pki
                .algorithm
                .parameters
                .and_then(|p| p.decode_as::<ObjectIdentifier>().ok())
                .map(|o| o.to_string());
            match curve.as_deref() {
                Some(OID_P256) => p256::ecdsa::SigningKey::try_from(pki.clone())
                    .map(SigningKey::P256)
                    .map_err(|e| {
                        AppError::BadRequest(format!("Invalid argument: PEM private key: {e}"))
                    }),
                Some(OID_P384) => p384::ecdsa::SigningKey::try_from(pki.clone())
                    .map(SigningKey::P384)
                    .map_err(|e| {
                        AppError::BadRequest(format!("Invalid argument: PEM private key: {e}"))
                    }),
                _ => Err(AppError::BadRequest(format!(
                    "Invalid argument: PEM private key - unsupported EC curve {curve:?}"
                ))),
            }
        }
        OID_ED25519 => {
            // RFC 8410: privateKey is an OCTET STRING wrapping the
            // 32-byte seed; ed25519-dalek has no PKCS#8 TryFrom.
            let seed = der::asn1::OctetString::from_der(pki.private_key.as_bytes())
                .map_err(|e| AppError::BadRequest(format!("Invalid argument: PEM private key: {e}")))?;
            let bytes: &[u8] = seed.as_bytes();
            let arr: [u8; 32] = bytes.try_into().map_err(|_| {
                AppError::BadRequest(
                    "Invalid argument: PEM private key - Ed25519 seed must be 32 bytes".into(),
                )
            })?;
            Ok(SigningKey::Ed25519(Ed25519Signer(
                ed25519_dalek::SigningKey::from_bytes(&arr),
            )))
        }
        _ => Err(AppError::BadRequest(format!(
            "Invalid argument: PEM private key - unsupported key algorithm {alg}"
        ))),
    }
}

// -- PKCS#12 ---------------------------------------------------------------

/// One decrypted SafeBag: either key material or a certificate.
enum P12Bag {
    /// `keyBag` (plain PKCS#8) or `pkcs8ShroudedKeyBag` (encrypted).
    Key {
        pki_der: Vec<u8>,
        shrouded: bool,
        friendly_name: Option<String>,
    },
    Cert {
        der: Vec<u8>,
        friendly_name: Option<String>,
    },
}

/// PKCS#12 `PFX` → decrypted `SafeContents` bags. ContentInfo entries may
/// be plain `data` or `encryptedData` (PBE); both are unwrapped here.
fn pkcs12_safe_bags(bytes: &[u8], password: &str) -> AppResult<Vec<P12Bag>> {
    let pfx = pkcs12::pfx::Pfx::from_der(bytes)
        .map_err(|e| AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}")))?;
    let auth_safe_bytes = content_bytes(&pfx.auth_safe.content)?;
    let auth_safe = decode_sequence_of::<ContentInfo>(&auth_safe_bytes)
        .map_err(|e| AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}")))?;

    let mut bags = Vec::new();
    for content_info in &auth_safe {
        let oid = content_info.content_type.to_string();
        let inner = content_bytes(&content_info.content)?;
        match oid.as_str() {
            OID_DATA => collect_safe_contents(&inner, password, &mut bags)?,
            OID_ENCRYPTED_DATA => {
                let encrypted = EncryptedData::from_der(&inner).map_err(|e| {
                    AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}"))
                })?;
                let plain = decrypt_encrypted_data(&encrypted, password)?;
                collect_safe_contents(&plain, password, &mut bags)?;
            }
            _ => {
                return Err(AppError::BadRequest(format!(
                    "PKCS12: unsupported content type {oid}"
                )))
            }
        }
    }
    Ok(bags)
}

/// `ContentInfo.eContent` for `data` is an OCTET STRING wrapping the
/// payload; other types carry their structure directly.
fn content_bytes(content: &Any) -> AppResult<Vec<u8>> {
    if content.tag() == der::Tag::OctetString {
        Ok(content.value().to_vec())
    } else {
        content
            .to_der()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("CMS content encode: {e}")))
    }
}

/// `SEQUENCE OF T` decode: the Any captures the SEQUENCE TLV, its value
/// is the concatenated element encodings.
fn decode_sequence_of<T>(bytes: &[u8]) -> der::Result<Vec<T>>
where
    T: for<'a> Decode<'a>,
{
    use der::Reader as _;
    let any = Any::from_der(bytes)?;
    let mut reader = der::SliceReader::new(any.value())?;
    let mut out = Vec::new();
    while !reader.is_finished() {
        out.push(reader.decode::<T>()?);
    }
    Ok(out)
}

fn collect_safe_contents(
    bytes: &[u8],
    password: &str,
    bags: &mut Vec<P12Bag>,
) -> AppResult<()> {
    let safe_contents = decode_sequence_of::<pkcs12::safe_bag::SafeBag>(bytes)
        .map_err(|e| AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}")))?;
    for safe_bag in &safe_contents {
        let (friendly_name, local_key_id) = bag_attributes(safe_bag);
        let _ = local_key_id;
        // bag_value holds the full TLV of the `[0] EXPLICIT` wrapper;
        // unwrap it to reach the bag's inner structure.
        let inner = bag_inner_der(&safe_bag.bag_value)?;
        let bag_oid = safe_bag.bag_id.to_string();
        if bag_oid == pkcs12::PKCS_12_KEY_BAG_OID.to_string()
            || bag_oid == pkcs12::PKCS_12_PKCS8_KEY_BAG_OID.to_string()
        {
            bags.push(P12Bag::Key {
                pki_der: inner,
                shrouded: bag_oid == pkcs12::PKCS_12_PKCS8_KEY_BAG_OID.to_string(),
                friendly_name,
            });
        } else if bag_oid == pkcs12::PKCS_12_CERT_BAG_OID.to_string() {
            let cert_bag = pkcs12::cert_type::CertBag::from_der(&inner).map_err(|e| {
                AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}"))
            })?;
            if cert_bag.cert_id.to_string() == pkcs12::PKCS_12_X509_CERT_OID.to_string() {
                bags.push(P12Bag::Cert {
                    der: cert_bag.cert_value.as_bytes().to_vec(),
                    friendly_name,
                });
            }
        }
    }
    Ok(())
}

/// SafeBag.bag_value is the `[0] EXPLICIT` wrapper TLV; its content is the
/// inner structure's TLV.
fn bag_inner_der(bag_value: &[u8]) -> AppResult<Vec<u8>> {
    let any = Any::from_der(bag_value)
        .map_err(|e| AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}")))?;
    if !matches!(
        any.tag(),
        der::Tag::ContextSpecific {
            constructed: true,
            number: der::TagNumber::N0
        }
    ) {
        return Err(AppError::BadRequest(
            "Invalid argument: PKCS12 keystore - malformed safe bag".into(),
        ));
    }
    Ok(any.value().to_vec())
}

/// PKCS12 `bagAttributes`: `friendlyName` (BMPString) and `localKeyId`
/// (octet string) attribute values.
fn bag_attributes(bag: &pkcs12::safe_bag::SafeBag) -> (Option<String>, Option<Vec<u8>>) {
    let mut friendly_name = None;
    let mut local_key_id = None;
    let Some(attrs) = &bag.bag_attributes else {
        return (None, None);
    };
    for attr in attrs.iter() {
        let oid = attr.oid.to_string();
        for value in attr.values.iter() {
            match oid.as_str() {
                OID_FRIENDLY_NAME => {
                    if let Ok(bmp) = value.decode_as::<BmpString>() {
                        friendly_name = Some(bmp_string_to_string(&bmp));
                    }
                }
                OID_LOCAL_KEY_ID => {
                    if let Ok(octets) = value.decode_as::<OctetString>() {
                        local_key_id = Some(octets.as_bytes().to_vec());
                    }
                }
                _ => {}
            }
        }
    }
    (friendly_name, local_key_id)
}

fn bmp_string_to_string(bmp: &BmpString) -> String {
    let units: Vec<u16> = bmp
        .as_bytes()
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

fn identity_from_pkcs12(
    extras: &[(String, Upload)],
    password: &str,
    alias: &str,
) -> AppResult<SigningIdentity> {
    let p12 = require_extra(extras, "p12File", "PKCS12 keystore")?;
    let bags = pkcs12_safe_bags(&p12.bytes, password)?;

    let mut key_bags: Vec<&P12Bag> = bags
        .iter()
        .filter(|b| matches!(b, P12Bag::Key { .. }))
        .collect();
    if key_bags.is_empty() {
        return Err(AppError::BadRequest(
            "Invalid argument: PKCS12 keystore - no private key found".into(),
        ));
    }
    let chosen = if alias.is_empty() {
        key_bags.remove(0)
    } else {
        *key_bags
            .iter()
            .find(|b| matches!(b, P12Bag::Key { friendly_name: Some(n), .. } if n == alias))
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "Invalid argument: alias - no certificate named {alias} in the PKCS12 keystore"
                ))
            })?
    };
    let P12Bag::Key {
        pki_der,
        shrouded,
        ..
    } = chosen
    else {
        unreachable!()
    };

    // The bag value is either plain PKCS#8 or an EncryptedPrivateKeyInfo;
    // decrypt first so the borrow stays inside this scope.
    let decrypted;
    let pki_der: &[u8] = if *shrouded {
        decrypted = decrypt_shrouded_keybag(pki_der, password)?;
        &decrypted
    } else {
        pki_der
    };
    let pki = PrivateKeyInfo::from_der(pki_der)
        .map_err(|e| AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}")))?;
    let key = signer_from_private_key_info(&pki)?;

    let mut certs: Vec<Certificate> = Vec::new();
    for bag in &bags {
        let P12Bag::Cert { der, .. } = bag else { continue };
        if let Ok(cert) = Certificate::from_der(der) {
            certs.push(cert);
        }
    }
    if certs.is_empty() {
        return Err(AppError::BadRequest(
            "Invalid argument: PKCS12 keystore - no certificate found".into(),
        ));
    }
    let chain = order_chain_by_key(certs, &key);
    Ok(SigningIdentity { key, chain })
}

/// `pkcs8ShroudedKeyBag`: an `EncryptedPrivateKeyInfo`. Parsed with the
/// pkcs12 crate's type because pkcs8's decoder rejects the PKCS#12-KDF
/// PBE OIDs (1.2.840.113549.1.12.1.x). Returns the plaintext PKCS#8 DER.
fn decrypt_shrouded_keybag(der: &[u8], password: &str) -> AppResult<Vec<u8>> {
    let encrypted = pkcs12::pbe_params::EncryptedPrivateKeyInfo::from_der(der).map_err(|e| {
        AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}"))
    })?;
    let alg = encrypted.encryption_algorithm.oid.to_string();
    let ciphertext = encrypted.encrypted_data.as_bytes();
    let params_der = encrypted
        .encryption_algorithm
        .parameters
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("PKCS12: missing PBE parameters".into()))?
        .to_der()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("PBE params encode: {e}")))?;

    match alg.as_str() {
        OID_PBES2 => {
            let pbes2 = pkcs12::pbe_params::Pbes2Params::from_der(&params_der).map_err(|e| {
                AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}"))
            })?;
            decrypt_pbes2(&pbes2, password, ciphertext)
        }
        PBE_3DES_CBC | PBE_2KEY_3DES_CBC | PBE_RC2_128_CBC | PBE_RC2_40_CBC => {
            let pbe = pkcs12::pbe_params::Pkcs12PbeParams::from_der(&params_der).map_err(|e| {
                AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}"))
            })?;
            decrypt_pkcs12_pbe(&alg, &pbe, password, ciphertext)
        }
        _ => Err(AppError::BadRequest(format!(
            "PKCS12: unsupported key encryption algorithm {alg}"
        ))),
    }
}

// -- PKCS#12 / PBES2 PBE decryption ----------------------------------------

/// Password → BMPString bytes + double-NUL terminator: the form the
/// PKCS#12 KDF consumes and the first form PBES2-in-PKCS12 is tried with.
fn p12_password_bytes(password: &str) -> AppResult<Vec<u8>> {
    let bmp = BmpString::from_utf8(password)
        .map_err(|e| AppError::BadRequest(format!("invalid password encoding: {e}")))?;
    let mut bytes = bmp.as_bytes().to_vec();
    bytes.extend_from_slice(&[0, 0]);
    Ok(bytes)
}

fn decrypt_encrypted_data(ed: &EncryptedData, password: &str) -> AppResult<Vec<u8>> {
    let alg = ed.enc_content_info.content_enc_alg.oid.to_string();
    let params = ed
        .enc_content_info
        .content_enc_alg
        .parameters
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("PKCS12: missing PBE parameters".into()))?;
    let params_der = params
        .to_der()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("PBE params encode: {e}")))?;
    let ciphertext = ed
        .enc_content_info
        .encrypted_content
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("PKCS12: missing encrypted content".into()))?
        .as_bytes()
        .to_vec();

    match alg.as_str() {
        OID_PBES2 => {
            let pbes2 = pkcs12::pbe_params::Pbes2Params::from_der(&params_der).map_err(|e| {
                AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}"))
            })?;
            decrypt_pbes2(&pbes2, password, &ciphertext)
        }
        PBE_3DES_CBC | PBE_2KEY_3DES_CBC | PBE_RC2_128_CBC | PBE_RC2_40_CBC => {
            let pbe = pkcs12::pbe_params::Pkcs12PbeParams::from_der(&params_der).map_err(|e| {
                AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}"))
            })?;
            decrypt_pkcs12_pbe(&alg, &pbe, password, &ciphertext)
        }
        PBE_RC4_128 | PBE_RC4_40 => Err(AppError::BadRequest(
            "PKCS12: RC4-encrypted keystores are not supported".into(),
        )),
        _ => Err(AppError::BadRequest(format!(
            "PKCS12: unsupported encryption algorithm {alg}"
        ))),
    }
}

/// PBES2-params → PBKDF2 key + CBC decryption. The KDF password is tried
/// as the PKCS#12 BMPString form, then raw.
fn decrypt_pbes2(
    pbes2: &pkcs12::pbe_params::Pbes2Params,
    password: &str,
    ciphertext: &[u8],
) -> AppResult<Vec<u8>> {
    let kdf_oid = pbes2.kdf.oid.to_string();
    if kdf_oid != OID_PBKDF2 {
        return Err(AppError::BadRequest(format!(
            "PKCS12: unsupported PBES2 KDF {kdf_oid}"
        )));
    }
    let kdf_params = pbes2
        .kdf
        .parameters
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("PKCS12: missing PBKDF2 parameters".into()))?
        .to_der()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("PBKDF2 params encode: {e}")))?;
    let pbkdf2 = pkcs12::pbe_params::Pbkdf2Params::from_der(&kdf_params).map_err(|e| {
        AppError::BadRequest(format!("Invalid argument: PKCS12 keystore: {e}"))
    })?;

    let enc_oid = pbes2.encryption.oid.to_string();
    let (key_len, iv_len) = cipher_dims(&enc_oid)?;
    let key_len = pbkdf2
        .key_length
        .map(|l| l as usize)
        .unwrap_or(key_len);
    if key_len != match enc_oid.as_str() {
        OID_DES_CBC => 8,
        OID_DES_EDE3_CBC => 24,
        OID_AES128_CBC => 16,
        OID_AES192_CBC => 24,
        OID_AES256_CBC => 32,
        _ => key_len,
    } {
        // A declared keyLength that disagrees with the cipher is malformed.
        return Err(AppError::BadRequest(
            "PKCS12: PBKDF2 keyLength does not match the encryption scheme".into(),
        ));
    }

    let iv = pbes2
        .encryption
        .parameters
        .as_ref()
        .and_then(|p| p.decode_as::<OctetString>().ok())
        .map(|o| o.as_bytes().to_vec())
        .ok_or_else(|| AppError::BadRequest("PKCS12: missing encryption IV".into()))?;
    if iv.len() != iv_len {
        return Err(AppError::BadRequest(format!(
            "PKCS12: encryption IV has {} bytes, expected {iv_len}",
            iv.len()
        )));
    }

    let prf_oid = pbkdf2.prf.oid.to_string();
    let salt = pbkdf2.salt.as_bytes();
    let iterations = pbkdf2.iteration_count;
    let bmp_password = p12_password_bytes(password)?;
    let raw_password = password.as_bytes().to_vec();
    for pw in [&bmp_password, &raw_password] {
        let key = pbkdf2_derive(&prf_oid, pw, salt, iterations, key_len)?;
        if let Ok(plain) = cbc_decrypt(&enc_oid, &key, &iv, ciphertext) {
            return Ok(plain);
        }
    }
    Err(AppError::BadRequest(
        "Invalid argument: password - the PKCS12 keystore password is incorrect or missing"
            .into(),
    ))
}

/// `(key_len, iv_len)` for the PBES2 encryption schemes we support.
fn cipher_dims(oid: &str) -> AppResult<(usize, usize)> {
    match oid {
        OID_DES_CBC => Ok((8, 8)),
        OID_DES_EDE3_CBC => Ok((24, 8)),
        OID_AES128_CBC => Ok((16, 16)),
        OID_AES192_CBC => Ok((24, 16)),
        OID_AES256_CBC => Ok((32, 16)),
        OID_RC2_CBC => Err(AppError::BadRequest(
            "PKCS12: RC2-encrypted keystores are not supported".into(),
        )),
        _ => Err(AppError::BadRequest(format!(
            "PKCS12: unsupported encryption algorithm {oid}"
        ))),
    }
}

fn pbkdf2_derive(
    prf_oid: &str,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    len: usize,
) -> AppResult<Vec<u8>> {
    fn derive<D>(password: &[u8], salt: &[u8], iterations: u32, len: usize) -> Vec<u8>
    where
        D: sha2::digest::Digest,
    {
        let mut out = vec![0u8; len];
        pbkdf2::pbkdf2::<hmac::Hmac<D>>(password, salt, iterations, &mut out)
            .expect("pbkdf2 output buffer is a multiple of the digest size");
        out
    }
    Ok(match prf_oid {
        OID_HMAC_SHA1 => derive::<sha1::Sha1>(password, salt, iterations, len),
        OID_HMAC_SHA224 | OID_HMAC_SHA224_NIST => {
            derive::<sha2::Sha224>(password, salt, iterations, len)
        }
        OID_HMAC_SHA256 | OID_HMAC_SHA256_NIST => {
            derive::<sha2::Sha256>(password, salt, iterations, len)
        }
        OID_HMAC_SHA384 | OID_HMAC_SHA384_NIST => {
            derive::<sha2::Sha384>(password, salt, iterations, len)
        }
        OID_HMAC_SHA512 | OID_HMAC_SHA512_NIST => {
            derive::<sha2::Sha512>(password, salt, iterations, len)
        }
        _ => {
            return Err(AppError::BadRequest(format!(
                "PKCS12: unsupported PBKDF2 PRF {prf_oid}"
            )))
        }
    })
}

/// PKCS#12-KDF PBE (pbeWithSHA1And*): key and IV both come from the
/// PKCS#12 KDF with the BMPString password.
fn decrypt_pkcs12_pbe(
    alg: &str,
    params: &pkcs12::pbe_params::Pkcs12PbeParams,
    password: &str,
    ciphertext: &[u8],
) -> AppResult<Vec<u8>> {
    use pkcs12::kdf::{derive_key_utf8, Pkcs12KeyType};
    let salt = params.salt.as_bytes();
    let iterations = params.iterations;
    let (key_len, iv_len) = match alg {
        PBE_3DES_CBC => (24, 8),
        PBE_2KEY_3DES_CBC => (16, 8),
        PBE_RC2_128_CBC => (16, 8),
        PBE_RC2_40_CBC => (5, 8),
        _ => unreachable!(),
    };
    let key = derive_key_utf8::<sha1::Sha1>(
        password,
        salt,
        Pkcs12KeyType::EncryptionKey,
        iterations,
        key_len,
    )
    .map_err(|e| AppError::Internal(anyhow::anyhow!("PKCS12 KDF: {e}")))?;
    let iv = derive_key_utf8::<sha1::Sha1>(
        password,
        salt,
        Pkcs12KeyType::Iv,
        iterations,
        iv_len,
    )
    .map_err(|e| AppError::Internal(anyhow::anyhow!("PKCS12 KDF: {e}")))?;

    match alg {
        PBE_3DES_CBC => cbc_decrypt_raw::<des::TdesEde3>(&key, &iv, ciphertext),
        PBE_2KEY_3DES_CBC => cbc_decrypt_raw::<des::TdesEde2>(&key, &iv, ciphertext),
        _ => cbc_decrypt_raw::<rc2::Rc2>(&key, &iv, ciphertext),
    }
}

fn cbc_decrypt_raw<C>(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> AppResult<Vec<u8>>
where
    C: cbc::cipher::KeyIvInit + cbc::cipher::BlockCipher,
{
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut};
    let mut buf = ciphertext.to_vec();
    let mut decryptor = cbc::Decryptor::<C>::new_from_slices(key, iv)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("invalid PBE key/iv length")))?;
    let plain = decryptor
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| {
            AppError::BadRequest(
                "Invalid argument: password - the PKCS12 keystore password is incorrect or missing"
                    .into(),
            )
        })?;
    Ok(plain.to_vec())
}

fn cbc_decrypt(oid: &str, key: &[u8], iv: &[u8], ciphertext: &[u8]) -> AppResult<Vec<u8>> {
    match oid {
        OID_DES_CBC => cbc_decrypt_raw::<des::Des>(key, iv, ciphertext),
        OID_DES_EDE3_CBC => cbc_decrypt_raw::<des::TdesEde3>(key, iv, ciphertext),
        OID_AES128_CBC => cbc_decrypt_raw::<aes::Aes128>(key, iv, ciphertext),
        OID_AES192_CBC => cbc_decrypt_raw::<aes::Aes192>(key, iv, ciphertext),
        OID_AES256_CBC => cbc_decrypt_raw::<aes::Aes256>(key, iv, ciphertext),
        _ => Err(AppError::BadRequest(format!(
            "PKCS12: unsupported encryption algorithm {oid}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Visible signature appearance
// ---------------------------------------------------------------------------

/// `CertSignController.createVisibleSignature`: a 200×50 Form XObject —
/// logo at 8% scale with Multiply blend + 50% alpha, then Times-Bold
/// "Signed by {CN}", the sign date, and the optional reason.
fn appearance_stream(
    doc: &mut Document,
    cn: &str,
    sign_date: &str,
    reason: Option<&str>,
    show_logo: bool,
) -> AppResult<Stream> {
    let mut resources = Dictionary::new();
    let mut xobjects = Dictionary::new();
    let mut content = String::new();

    if show_logo {
        let logo = embed_image(doc, LOGO_PNG)?;
        let (w, h) = (logo.width as f64, logo.height as f64);
        xobjects.set("Im1", Object::Reference(logo.id));
        // Java: q, ExtGState(Multiply, ca=0.5), cm scale(0.08), drawImage
        // at (100,0) — i.e. the unit image scaled by its pixel size under
        // the 0.08 CTM.
        content.push_str("q /GS1 gs 0.08 0 0 0.08 0 0 cm ");
        content.push_str(&format!(
            "q {w} 0 0 {h} 100 0 cm /Im1 Do Q Q\n"
        ));
    }

    let first_y = APPEARANCE_HEIGHT - LEADING;
    let mut lines = vec![format!("Signed by {cn}"), sign_date.to_string()];
    if let Some(reason) = reason.filter(|r| !r.trim().is_empty()) {
        lines.push(reason.to_string());
    }
    content.push_str(&format!(
        "BT /F1 {FONT_SIZE} Tf 0 g {FONT_SIZE} {first_y} Td {LEADING} TL"
    ));
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            content.push_str(" T*");
        }
        content.push_str(&format!(" ({}) Tj", pdf_literal(line)));
    }
    content.push_str(" ET\n");

    resources.set(
        "Font",
        dictionary! {
            "F1" => dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => "Times-Bold",
                "Encoding" => "WinAnsiEncoding",
            }
        },
    );
    if !xobjects.is_empty() {
        resources.set("XObject", Object::Dictionary(xobjects));
    }
    let mut ext_gstate = Dictionary::new();
    ext_gstate.set("BM", "Multiply");
    ext_gstate.set("ca", Object::Real(0.5f32));
    resources.set(
        "ExtGState",
        dictionary! { "GS1" => Object::Dictionary(ext_gstate) },
    );

    let mut dict = Dictionary::new();
    dict.set("Type", "XObject");
    dict.set("Subtype", "Form");
    dict.set("FormType", 1i64);
    dict.set(
        "BBox",
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(SIGNATURE_RECT[2]),
            Object::Integer(SIGNATURE_RECT[3]),
        ]),
    );
    dict.set("Resources", Object::Dictionary(resources));
    Ok(Stream::new(dict, content.into_bytes()))
}

/// PDF literal-string escaping for the WinAnsi-encoded appearance text:
/// backslash/parens escaped, characters outside the byte range become `?`.
fn pdf_literal(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            c if (c as u32) < 0x20 || (c as u32) > 0xFF => out.push('?'),
            c => out.push(c),
        }
    }
    out
}

/// `signature.getSignDate().getTime().toString()` — `java.util.Date`'s
/// "EEE MMM dd HH:mm:ss zzz yyyy" display format.
fn java_date_string() -> String {
    chrono::Local::now().format("%a %b %d %H:%M:%S %Z %Y").to_string()
}

/// The `CN` RDN of the leaf certificate's subject, for the "Signed by"
/// line — `X500Name.getRDNs(CN)[0]` in Java.
fn leaf_common_name(cert: &Certificate) -> Option<String> {
    for rdn in &cert.tbs_certificate.subject.0 {
        for atv in rdn.0.iter() {
            if atv.oid.to_string() == OID_CN {
                if let Some(s) = directory_string(&atv.value) {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn directory_string(any: &Any) -> Option<String> {
    match any.tag() {
        der::Tag::Utf8String => Utf8StringRef::try_from(any)
            .ok()
            .map(|s| s.as_str().to_string()),
        der::Tag::PrintableString => PrintableStringRef::try_from(any)
            .ok()
            .map(|s| s.as_str().to_string()),
        der::Tag::TeletexString => TeletexStringRef::try_from(any)
            .ok()
            .map(|s| s.as_str().to_string()),
        der::Tag::BmpString => any
            .decode_as::<BmpString>()
            .ok()
            .map(|b| bmp_string_to_string(&b)),
        _ => None,
    }
}
