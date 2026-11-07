//! Port of Java's `ValidateSignatureController` +
//! `CertificateValidationService`: CMS/PKCS#7 signature extraction from PDF
//! signature dictionaries, cryptographic verification, RFC 3161 document
//! timestamps, and a PKIX-lite certificate path check against the trust
//! anchors `security.validation.trust` configures.
//!
//! Deliberate divergences from Java:
//! - Revocation (OCSP/CRL/AIA fetch) is not implemented; `revocation.mode`
//!   other than `none` keeps Java's soft-fail reporting shape but performs
//!   no network lookups.
//! - `useAATL`/`useEUTL` trust lists are not downloaded; enabling them logs
//!   a warning and contributes no anchors.
//! - `serverAsAnchor` has no Rust counterpart (no server TLS certificate).

use std::collections::HashSet;
use std::time::SystemTime;

use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::ContentInfo;
use cms::signed_data::{SignedData, SignerIdentifier, SignerInfo};
use der::asn1::{ObjectIdentifier, OctetString, Uint};
use der::{Any, Decode, DecodePem, Encode, Sequence};
use rsa::traits::PublicKeyParts as _;
use serde_json::{json, Value};
use sha2::Digest as _;
use signature::hazmat::PrehashVerifier;
use spki::AlgorithmIdentifierOwned;
use x509_cert::certificate::Certificate;
use x509_cert::ext::pkix::{
    AuthorityKeyIdentifier, BasicConstraints, KeyUsage, SubjectKeyIdentifier,
};
use x509_cert::time::Time as CertTime;

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::pdfdoc;
use crate::settings;
use crate::upload::{Upload, FILE_FIELD};

// ---------------------------------------------------------------------------
// Algorithm identifiers
// ---------------------------------------------------------------------------

const RSA_ENCRYPTION: &str = "1.2.840.113549.1.1.1";
const SHA1_WITH_RSA: &str = "1.2.840.113549.1.1.5";
const SHA256_WITH_RSA: &str = "1.2.840.113549.1.1.11";
const SHA384_WITH_RSA: &str = "1.2.840.113549.1.1.12";
const SHA512_WITH_RSA: &str = "1.2.840.113549.1.1.13";
const RSASSA_PSS: &str = "1.2.840.113549.1.1.10";
const ECDSA_SHA1: &str = "1.2.840.10045.4.1";
const ECDSA_SHA224: &str = "1.2.840.10045.4.3.1";
const ECDSA_SHA256: &str = "1.2.840.10045.4.3.2";
const ECDSA_SHA384: &str = "1.2.840.10045.4.3.3";
const ECDSA_SHA512: &str = "1.2.840.10045.4.3.4";
const ED25519: &str = "1.3.101.112";
const EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
const P256_OID: &str = "1.2.840.10045.3.1.7";
const P384_OID: &str = "1.3.132.0.34";
const ID_MGF1: &str = "1.2.840.113549.1.1.8";
const OID_SHA1: &str = "1.3.14.3.2.26";

const OID_MESSAGE_DIGEST_ATTR: &str = "1.2.840.113549.1.9.4";
const OID_SIGNING_TIME_ATTR: &str = "1.2.840.113549.1.9.5";
const OID_TIMESTAMP_TOKEN_ATTR: &str = "1.2.840.113549.1.9.16.2.14";
const OID_SIGNED_DATA: &str = "1.2.840.113549.1.7.2";
const OID_TST_INFO: &str = "1.2.840.113549.1.9.16.1.4";

const SUBFILTER_RFC3161: &str = "ETSI.RFC3161";

fn oid(s: &str) -> ObjectIdentifier {
    ObjectIdentifier::new_unwrap(s)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DigestKind {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl DigestKind {
    fn hash(self, msg: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => sha1::Sha1::digest(msg).to_vec(),
            Self::Sha224 => sha2::Sha224::digest(msg).to_vec(),
            Self::Sha256 => sha2::Sha256::digest(msg).to_vec(),
            Self::Sha384 => sha2::Sha384::digest(msg).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(msg).to_vec(),
        }
    }
}

fn digest_for_oid(oid: &ObjectIdentifier) -> Option<DigestKind> {
    Some(match oid.to_string().as_str() {
        "1.3.14.3.2.26" => DigestKind::Sha1,
        "2.16.840.1.101.3.4.2.4" => DigestKind::Sha224,
        "2.16.840.1.101.3.4.2.1" => DigestKind::Sha256,
        "2.16.840.1.101.3.4.2.2" => DigestKind::Sha384,
        "2.16.840.1.101.3.4.2.3" => DigestKind::Sha512,
        _ => return None,
    })
}

/// The digest a signature/certificate `AlgorithmIdentifier` OID implies;
/// `None` for algorithms carrying their digest elsewhere
/// (`rsaEncryption`, `rsassaPss`, `id-Ed25519`).
fn implied_digest(sig_alg: &str) -> Option<DigestKind> {
    Some(match sig_alg {
        SHA1_WITH_RSA => DigestKind::Sha1,
        SHA256_WITH_RSA => DigestKind::Sha256,
        SHA384_WITH_RSA => DigestKind::Sha384,
        SHA512_WITH_RSA => DigestKind::Sha512,
        ECDSA_SHA1 => DigestKind::Sha1,
        ECDSA_SHA224 => DigestKind::Sha224,
        ECDSA_SHA256 => DigestKind::Sha256,
        ECDSA_SHA384 => DigestKind::Sha384,
        ECDSA_SHA512 => DigestKind::Sha512,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Public keys + signature verification
// ---------------------------------------------------------------------------

enum PublicKey {
    Rsa(rsa::RsaPublicKey),
    P256(p256::ecdsa::VerifyingKey),
    P384(p384::ecdsa::VerifyingKey),
    Ed25519(ed25519_dalek::VerifyingKey),
}

fn public_key_from_spki(spki: &spki::SubjectPublicKeyInfoOwned) -> Result<PublicKey, String> {
    let alg = spki.algorithm.oid.to_string();
    let bits = spki
        .subject_public_key
        .as_bytes()
        .ok_or("subjectPublicKey has unused bits")?
        .to_vec();
    match alg.as_str() {
        RSA_ENCRYPTION => rsa::pkcs1::DecodeRsaPublicKey::from_pkcs1_der(&bits)
            .map(PublicKey::Rsa)
            .map_err(|e| format!("RSA public key decode: {e}")),
        EC_PUBLIC_KEY => {
            let curve = alg_param_oid(&spki.algorithm)
                .ok_or("EC parameters missing")?
                .to_string();
            match curve.as_str() {
                P256_OID => p256::ecdsa::VerifyingKey::from_sec1_bytes(&bits)
                    .map(PublicKey::P256)
                    .map_err(|e| format!("P-256 public key decode: {e}")),
                P384_OID => p384::ecdsa::VerifyingKey::from_sec1_bytes(&bits)
                    .map(PublicKey::P384)
                    .map_err(|e| format!("P-384 public key decode: {e}")),

                _ => Err(format!("unsupported EC curve {curve}")),
            }
        }
        ED25519 => {
            let arr: [u8; 32] = bits
                .try_into()
                .map_err(|_| "Ed25519 public key must be 32 bytes".to_string())?;
            ed25519_dalek::VerifyingKey::from_bytes(&arr)
                .map(PublicKey::Ed25519)
                .map_err(|e| format!("Ed25519 public key decode: {e}"))
        }
        _ => Err(format!("unsupported public key algorithm {alg}")),
    }
}

/// `RSASSA-PSS-params` (RFC 4055): context-tagged IMPLICIT fields, each
/// DEFAULTed — absent means the RFC default (SHA-1, MGF1-SHA1, salt 20,
/// trailer 1), applied at the use site.
#[derive(Sequence)]
struct RsaPssParams {
    #[asn1(context_specific = "0", tag_mode = "IMPLICIT", optional = "true")]
    hash_algorithm: Option<AlgorithmIdentifierOwned>,
    #[asn1(context_specific = "1", tag_mode = "IMPLICIT", optional = "true")]
    mask_gen_algorithm: Option<AlgorithmIdentifierOwned>,
    #[asn1(context_specific = "2", tag_mode = "IMPLICIT", optional = "true")]
    salt_length: Option<Uint>,
    #[asn1(context_specific = "3", tag_mode = "IMPLICIT", optional = "true")]
    trailer_field: Option<Uint>,
}

fn uint_value(v: &Uint) -> Option<usize> {
    v.as_bytes().iter().try_fold(0usize, |acc, b| {
        acc.checked_mul(256)?.checked_add(*b as usize)
    })
}

/// `AlgorithmIdentifier.parameters` is `ANY`; an OID-typed parameter decodes
/// straight back to an `ObjectIdentifier`.
fn alg_param_oid(alg: &AlgorithmIdentifierOwned) -> Option<ObjectIdentifier> {
    let params = alg.parameters.as_ref()?;
    let der = params.to_der().ok()?;
    ObjectIdentifier::from_der(&der).ok()
}

/// `maskGenAlgorithm.parameters` is a `HashAlgorithm` — an
/// `AlgorithmIdentifier` whose OID names the MGF1 hash.
fn mgf1_hash(alg: &AlgorithmIdentifierOwned) -> Result<DigestKind, String> {
    if alg.oid.to_string() != ID_MGF1 {
        return Err(format!("unsupported MGF algorithm {}", alg.oid));
    }
    let Some(params) = &alg.parameters else {
        return Ok(DigestKind::Sha1);
    };
    let der = params
        .to_der()
        .map_err(|e| format!("MGF params encode: {e}"))?;
    let hash_alg =
        AlgorithmIdentifierOwned::from_der(&der).map_err(|e| format!("MGF params decode: {e}"))?;
    digest_for_oid(&hash_alg.oid).ok_or_else(|| format!("unsupported MGF1 hash {}", hash_alg.oid))
}

/// `Ok(true)` verified, `Ok(false)` signature mismatch, `Err` when the
/// algorithm is unsupported or malformed. `digest_alg` is the CMS
/// `digestAlgorithm`, consulted only when the signature algorithm does not
/// imply one (`rsaEncryption`, `rsassaPss`).
fn verify_signature(
    key: &PublicKey,
    sig_alg: &AlgorithmIdentifierOwned,
    digest_alg: Option<&AlgorithmIdentifierOwned>,
    message: &[u8],
    signature: &[u8],
) -> Result<bool, String> {
    let alg = sig_alg.oid.to_string();
    match alg.as_str() {
        SHA1_WITH_RSA | SHA256_WITH_RSA | SHA384_WITH_RSA | SHA512_WITH_RSA => {
            let digest = implied_digest(&alg).unwrap();
            let PublicKey::Rsa(rsa_key) = key else {
                return Err("RSA signature algorithm with non-RSA key".into());
            };
            verify_rsa_pkcs1(rsa_key, digest, message, signature)
        }
        RSA_ENCRYPTION => {
            let digest_alg = digest_alg.ok_or("missing digestAlgorithm")?;
            let digest = digest_for_oid(&digest_alg.oid)
                .ok_or_else(|| format!("unsupported digest algorithm {}", digest_alg.oid))?;
            let PublicKey::Rsa(rsa_key) = key else {
                return Err("rsaEncryption signature with non-RSA key".into());
            };
            verify_rsa_pkcs1(rsa_key, digest, message, signature)
        }
        RSASSA_PSS => {
            let PublicKey::Rsa(rsa_key) = key else {
                return Err("RSASSA-PSS signature with non-RSA key".into());
            };
            verify_rsa_pss(rsa_key, sig_alg, digest_alg, message, signature)
        }
        ECDSA_SHA1 | ECDSA_SHA224 | ECDSA_SHA256 | ECDSA_SHA384 | ECDSA_SHA512 => {
            let digest = implied_digest(&alg).unwrap();
            let hashed = digest.hash(message);
            match key {
                PublicKey::P256(k) => {
                    let sig = p256::ecdsa::DerSignature::from_der(signature)
                        .map_err(|e| format!("ECDSA signature decode: {e}"))?;
                    Ok(k.verify_prehash(&hashed, &sig).is_ok())
                }
                PublicKey::P384(k) => {
                    let sig = p384::ecdsa::DerSignature::from_der(signature)
                        .map_err(|e| format!("ECDSA signature decode: {e}"))?;
                    Ok(k.verify_prehash(&hashed, &sig).is_ok())
                }
                _ => Err("ECDSA signature algorithm with non-EC key".into()),
            }
        }
        ED25519 => {
            let PublicKey::Ed25519(k) = key else {
                return Err("Ed25519 signature with non-Ed25519 key".into());
            };
            let sig_bytes: [u8; 64] = signature
                .try_into()
                .map_err(|_| "Ed25519 signature must be 64 bytes".to_string())?;
            let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
            Ok(k.verify_strict(message, &sig).is_ok())
        }
        _ => Err(format!("unsupported signature algorithm {alg}")),
    }
}

fn verify_rsa_pkcs1(
    key: &rsa::RsaPublicKey,
    digest: DigestKind,
    message: &[u8],
    signature: &[u8],
) -> Result<bool, String> {
    let hashed = digest.hash(message);
    macro_rules! scheme {
        ($d:ty) => {
            rsa::pkcs1v15::Pkcs1v15Sign::new::<$d>()
        };
    }
    let result = match digest {
        DigestKind::Sha1 => key.verify(scheme!(sha1::Sha1), &hashed, signature),
        DigestKind::Sha224 => key.verify(scheme!(sha2::Sha224), &hashed, signature),
        DigestKind::Sha256 => key.verify(scheme!(sha2::Sha256), &hashed, signature),
        DigestKind::Sha384 => key.verify(scheme!(sha2::Sha384), &hashed, signature),
        DigestKind::Sha512 => key.verify(scheme!(sha2::Sha512), &hashed, signature),
    };
    Ok(result.is_ok())
}

fn verify_rsa_pss(
    key: &rsa::RsaPublicKey,
    sig_alg: &AlgorithmIdentifierOwned,
    digest_alg: Option<&AlgorithmIdentifierOwned>,
    message: &[u8],
    signature: &[u8],
) -> Result<bool, String> {
    // RFC 4055: absent `parameters` means every default applies; inside
    // the params each DEFAULTed field falls back the same way. The CMS
    // `digestAlgorithm` only stands in when no params are present at all.
    let mut hash_oid = digest_alg.map(|a| a.oid);
    let mut mgf = DigestKind::Sha1;
    let mut salt_length = 20usize;
    if let Some(params) = &sig_alg.parameters {
        let der = params
            .to_der()
            .map_err(|e| format!("PSS params encode: {e}"))?;
        let pss = RsaPssParams::from_der(&der).map_err(|e| format!("PSS params decode: {e}"))?;
        hash_oid = Some(
            pss.hash_algorithm
                .as_ref()
                .map(|a| a.oid)
                .unwrap_or_else(|| oid(OID_SHA1)),
        );
        if let Some(mgf_alg) = &pss.mask_gen_algorithm {
            mgf = mgf1_hash(mgf_alg)?;
        }
        if let Some(tf) = &pss.trailer_field {
            if uint_value(tf) != Some(1) {
                return Err("unsupported PSS trailerField".into());
            }
        }
        if let Some(sl) = &pss.salt_length {
            salt_length = uint_value(sl).ok_or_else(|| "PSS saltLength too large".to_string())?;
        }
    }
    let digest = match &hash_oid {
        Some(o) => {
            digest_for_oid(o).ok_or_else(|| format!("unsupported PSS hash algorithm {o}"))?
        }
        None => DigestKind::Sha1,
    };
    // `rsa::pss::Pss` only supports MGF1 keyed with the message hash, so a
    // differing MGF hash is rejected rather than approximated.
    if mgf != digest {
        return Err("unsupported MGF1 hash for RSASSA-PSS".into());
    }
    let hashed = digest.hash(message);
    let salt = salt_length;
    macro_rules! verify {
        ($d:ty) => {
            key.verify(rsa::pss::Pss::new_with_salt::<$d>(salt), &hashed, signature)
        };
    }
    let result = match digest {
        DigestKind::Sha1 => verify!(sha1::Sha1),
        DigestKind::Sha224 => verify!(sha2::Sha224),
        DigestKind::Sha256 => verify!(sha2::Sha256),
        DigestKind::Sha384 => verify!(sha2::Sha384),
        DigestKind::Sha512 => verify!(sha2::Sha512),
    };
    Ok(result.is_ok())
}

// ---------------------------------------------------------------------------
// X.509 certificate model
// ---------------------------------------------------------------------------

struct CertInfo {
    cert: Certificate,
    tbs_der: Vec<u8>,
    der: Vec<u8>,
    subject: Vec<u8>,
    issuer: Vec<u8>,
    key: Option<PublicKey>,
    key_alg: String,
    is_ca: bool,
    has_key_usage: bool,
    key_cert_sign: bool,
    skid: Option<Vec<u8>>,
    akid: Option<AuthorityKeyIdentifier>,
    subject_name: String,
    issuer_name: String,
    serial_hex: String,
    not_before: SystemTime,
    not_after: SystemTime,
}

impl CertInfo {
    fn parse(der: &[u8]) -> Result<Self, String> {
        let cert = Certificate::from_der(der).map_err(|e| format!("certificate decode: {e}"))?;
        let tbs = &cert.tbs_certificate;
        let tbs_der = tbs
            .to_der()
            .map_err(|e| format!("tbsCertificate encode: {e}"))?
            .to_vec();
        let subject = tbs.subject.to_der().map_err(|e| e.to_string())?.to_vec();
        let issuer = tbs.issuer.to_der().map_err(|e| e.to_string())?.to_vec();
        let key = public_key_from_spki(&tbs.subject_public_key_info).ok();
        let key_alg = tbs.subject_public_key_info.algorithm.oid.to_string();

        let is_ca = tbs
            .get::<BasicConstraints>()
            .ok()
            .flatten()
            .is_some_and(|(_, bc)| bc.ca);
        let has_key_usage = tbs.get::<KeyUsage>().ok().flatten().is_some();
        let key_cert_sign = tbs
            .get::<KeyUsage>()
            .ok()
            .flatten()
            .is_some_and(|(_, ku)| ku.key_cert_sign());
        let skid = tbs
            .get::<SubjectKeyIdentifier>()
            .ok()
            .flatten()
            .map(|(_, id)| id.0.as_bytes().to_vec());
        let akid = tbs
            .get::<AuthorityKeyIdentifier>()
            .ok()
            .flatten()
            .map(|(_, id)| id);

        Ok(Self {
            subject_name: tbs.subject.to_string(),
            issuer_name: tbs.issuer.to_string(),
            // BigInteger.toString(16): no leading zeros, no 0x prefix.
            serial_hex: {
                let bytes = tbs.serial_number.as_bytes();
                let start = bytes
                    .iter()
                    .position(|b| *b != 0)
                    .unwrap_or(bytes.len().saturating_sub(1));
                bytes[start..].iter().map(|b| format!("{b:02x}")).collect()
            },
            not_before: tbs.validity.not_before.to_system_time(),
            not_after: tbs.validity.not_after.to_system_time(),
            cert,
            tbs_der,
            der: der.to_vec(),
            subject,
            issuer,
            key,
            key_alg,
            is_ca,
            has_key_usage,
            key_cert_sign,
            skid,
            akid,
        })
    }

    fn verify_own_signature(&self, issuer_key: &PublicKey) -> Result<bool, String> {
        verify_signature(
            issuer_key,
            &self.cert.signature_algorithm,
            None,
            &self.tbs_der,
            self.cert.signature.as_bytes().unwrap_or(&[]),
        )
    }

    fn key_size(&self) -> usize {
        match &self.key {
            Some(PublicKey::Rsa(k)) => k.n().bits(),
            Some(PublicKey::P256(_)) => 256,
            Some(PublicKey::P384(_)) => 384,
            Some(PublicKey::Ed25519(_)) => 256,
            None => 0,
        }
    }

    fn sig_alg_name(&self) -> String {
        signature_algorithm_name(&self.cert.signature_algorithm.oid.to_string())
    }

    fn key_usages(&self) -> Vec<String> {
        type UsagePredicate = fn(&KeyUsage) -> bool;
        const LABELS: [(&str, UsagePredicate); 9] = [
            ("Digital Signature", |k| k.digital_signature()),
            ("Non-Repudiation", |k| k.non_repudiation()),
            ("Key Encipherment", |k| k.key_encipherment()),
            ("Data Encipherment", |k| k.data_encipherment()),
            ("Key Agreement", |k| k.key_agreement()),
            ("Certificate Signing", |k| k.key_cert_sign()),
            ("CRL Signing", |k| k.crl_sign()),
            ("Encipher Only", |k| k.encipher_only()),
            ("Decipher Only", |k| k.decipher_only()),
        ];
        self.cert
            .tbs_certificate
            .get::<KeyUsage>()
            .ok()
            .flatten()
            .map(|(_, ku)| {
                LABELS
                    .iter()
                    .filter(|(_, f)| f(&ku))
                    .map(|(label, _)| label.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn validity_ok(&self, at: SystemTime) -> bool {
        at >= self.not_before && at <= self.not_after
    }
}

fn signature_algorithm_name(oid: &str) -> String {
    match oid {
        "1.2.840.113549.1.1.4" => "MD5withRSA".into(),
        SHA1_WITH_RSA => "SHA1withRSA".into(),
        "1.2.840.113549.1.1.6" => "SHA224withRSA".into(),
        SHA256_WITH_RSA => "SHA256withRSA".into(),
        SHA384_WITH_RSA => "SHA384withRSA".into(),
        SHA512_WITH_RSA => "SHA512withRSA".into(),
        RSASSA_PSS => "SHA256withRSAandMGF1".into(),
        ECDSA_SHA1 => "SHA1withECDSA".into(),
        ECDSA_SHA224 => "SHA224withECDSA".into(),
        ECDSA_SHA256 => "SHA256withECDSA".into(),
        ECDSA_SHA384 => "SHA384withECDSA".into(),
        ECDSA_SHA512 => "SHA512withECDSA".into(),
        ED25519 => "Ed25519".into(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Trust anchors
// ---------------------------------------------------------------------------

struct Anchor {
    subject: Vec<u8>,
    key: PublicKey,
}

fn anchors_from_settings(custom_cert: Option<&Certificate>) -> Vec<Anchor> {
    let validation = &settings::current().security.validation;
    let mut anchors = Vec::new();

    if let Some(cert) = custom_cert {
        if let Ok(der) = cert.to_der() {
            if let Ok(info) = CertInfo::parse(&der) {
                if let Some(key) = info.key {
                    anchors.push(Anchor {
                        subject: info.subject.clone(),
                        key,
                    });
                }
            }
        }
    }

    if validation.trust.use_system_trust {
        let result = rustls_native_certs::load_native_certs();
        for err in &result.errors {
            tracing::warn!("native trust store: {err}");
        }
        for cert in result.certs {
            if let Ok(info) = CertInfo::parse(cert.as_ref()) {
                if let Some(key) = info.key {
                    anchors.push(Anchor {
                        subject: info.subject.clone(),
                        key,
                    });
                }
            }
        }
    }

    if validation.trust.use_mozilla_bundle {
        for ta in webpki_roots::TLS_SERVER_ROOTS.iter() {
            if let Ok(spki) =
                spki::SubjectPublicKeyInfoOwned::from_der(ta.subject_public_key_info.as_ref())
            {
                if let Ok(key) = public_key_from_spki(&spki) {
                    anchors.push(Anchor {
                        subject: ta.subject.as_ref().to_vec(),
                        key,
                    });
                }
            }
        }
    }

    if validation.trust.use_aatl || validation.trust.use_eutl {
        tracing::warn!(
            "useAATL/useEUTL are enabled but the Rust backend does not download trust lists"
        );
    }

    anchors
}

// ---------------------------------------------------------------------------
// Certificate path building (PKIX-lite)
// ---------------------------------------------------------------------------

/// Builds a certification path from `signer` through `pool` (the CMS
/// certificate set) to a trust anchor, returning pool indices in path
/// order (leaf first) or an error message.
fn build_cert_path(
    signer: &CertInfo,
    pool: &[CertInfo],
    anchors: &[Anchor],
    at: SystemTime,
) -> Result<Vec<usize>, String> {
    let signer_idx = pool
        .iter()
        .position(|c| c.der == signer.der)
        .ok_or("signer certificate not in CMS certificate set")?;
    let mut path = vec![signer_idx];
    let mut current = signer;

    for _ in 0..10 {
        // Anchor hop: current is issued by a trust anchor whose public key
        // verifies current's signature. Self-signed trust anchors match
        // through the same rule.
        let anchor_link = anchors.iter().any(|anchor| {
            anchor.subject == current.issuer
                && current.verify_own_signature(&anchor.key) == Ok(true)
        });
        if anchor_link {
            for (pos, &idx) in path.iter().enumerate() {
                let cert = &pool[idx];
                if !cert.validity_ok(at) {
                    return Err(format!(
                        "certificate {} is not valid at the validation time",
                        cert.subject_name
                    ));
                }
                if pos > 0 && !cert.is_ca {
                    return Err(format!("{} is not a CA certificate", cert.subject_name));
                }
            }
            return Ok(path);
        }

        // Otherwise the issuer must be another CMS certificate.
        let mut candidates: Vec<usize> = pool
            .iter()
            .enumerate()
            .filter(|(_, c)| c.subject == current.issuer)
            .map(|(i, _)| i)
            .collect();
        if let Some(akid) = &current.akid {
            if let Some(key_id) = &akid.key_identifier {
                let kid = key_id.as_bytes();
                let narrowed: Vec<usize> = candidates
                    .iter()
                    .copied()
                    .filter(|&i| pool[i].skid.as_deref() == Some(kid))
                    .collect();
                if !narrowed.is_empty() {
                    candidates = narrowed;
                }
            }
        }

        let mut advanced = false;
        for idx in candidates {
            if path.contains(&idx) {
                continue;
            }
            let issuer_cert = &pool[idx];
            let Some(issuer_key) = &issuer_cert.key else {
                continue;
            };
            if issuer_cert.is_ca
                && (!issuer_cert.has_key_usage || issuer_cert.key_cert_sign)
                && issuer_cert.validity_ok(at)
                && current.verify_own_signature(issuer_key) == Ok(true)
            {
                path.push(idx);
                current = issuer_cert;
                advanced = true;
                break;
            }
        }
        if !advanced {
            return Err(format!(
                "unable to find a valid certification path to a trust anchor for {}",
                current.subject_name
            ));
        }
    }
    Err("certificate path too long".into())
}

// ---------------------------------------------------------------------------
// RFC 3161 / CMS timestamp helpers
// ---------------------------------------------------------------------------

#[derive(Sequence)]
struct MessageImprint {
    hash_algorithm: AlgorithmIdentifierOwned,
    hashed_message: OctetString,
}

#[derive(Sequence)]
struct TstInfo {
    #[allow(dead_code)]
    version: der::asn1::Int,
    #[allow(dead_code)]
    policy: ObjectIdentifier,
    message_imprint: MessageImprint,
    #[allow(dead_code)]
    serial_number: der::asn1::Int,
    gen_time: CertTime,
    #[allow(dead_code)]
    #[asn1(optional = "true")]
    accuracy: Option<Any>,
    #[allow(dead_code)]
    #[asn1(optional = "true")]
    ordering: Option<bool>,
    #[allow(dead_code)]
    #[asn1(optional = "true")]
    nonce: Option<der::asn1::Int>,
    #[allow(dead_code)]
    #[asn1(context_specific = "0", tag_mode = "EXPLICIT", optional = "true")]
    tsa: Option<Any>,
    #[allow(dead_code)]
    #[asn1(context_specific = "1", optional = "true")]
    extensions: Option<Any>,
}

fn signed_data_from_content_info(content_info: &ContentInfo) -> Option<SignedData> {
    if content_info.content_type != oid(OID_SIGNED_DATA) {
        return None;
    }
    let der = content_info.content.to_der().ok()?;
    SignedData::from_der(&der).ok()
}

/// The TSTInfo carried as eContent of a document timestamp.
fn tst_info_from_signed_data(data: &SignedData) -> Option<TstInfo> {
    if data.encap_content_info.econtent_type != oid(OID_TST_INFO) {
        return None;
    }
    let econtent = data.encap_content_info.econtent.as_ref()?;
    let der = econtent.to_der().ok()?;
    let octets = OctetString::from_der(&der).ok()?;
    TstInfo::from_der(octets.as_bytes()).ok()
}

/// RFC 3161 `id-aa-timeStampToken` unsigned attribute → token genTime.
fn timestamp_token_gen_time(attrs: Option<&x509_cert::attr::Attributes>) -> Option<SystemTime> {
    let attrs = attrs?;
    let attr = attrs
        .iter()
        .find(|a| a.oid == oid(OID_TIMESTAMP_TOKEN_ATTR))?;
    let value = attr.values.iter().next()?;
    let content_info = ContentInfo::from_der(&value.to_der().ok()?).ok()?;
    let signed = signed_data_from_content_info(&content_info)?;
    tst_info_from_signed_data(&signed).map(|t| t.gen_time.to_system_time())
}

/// `CertificateValidationService.extractValidationTime`: document timestamp
/// → TSA timestamp attribute → signingTime → current.
fn validation_time(
    signer_info: &SignerInfo,
    document_timestamp: Option<SystemTime>,
) -> (SystemTime, &'static str) {
    if let Some(t) = document_timestamp {
        return (t, "document-timestamp");
    }
    if let Some(t) = timestamp_token_gen_time(signer_info.unsigned_attrs.as_ref()) {
        return (t, "timestamp");
    }
    if let Some(attrs) = &signer_info.signed_attrs {
        if let Some(attr) = attrs.iter().find(|a| a.oid == oid(OID_SIGNING_TIME_ATTR)) {
            if let Some(value) = attr.values.iter().next() {
                if let Ok(der) = value.to_der() {
                    if let Ok(time) = CertTime::from_der(&der) {
                        return (time.to_system_time(), "signing-time");
                    }
                }
            }
        }
    }
    (SystemTime::now(), "current")
}

// ---------------------------------------------------------------------------
// PDF signature dictionaries
// ---------------------------------------------------------------------------

struct PdfSignature {
    byte_range: [i64; 4],
    contents: Vec<u8>,
    sub_filter: Option<String>,
    name: Option<String>,
    sign_date: Option<String>,
    reason: Option<String>,
    location: Option<String>,
}

fn text_field(dict: &lopdf::Dictionary, doc: &lopdf::Document, key: &[u8]) -> Option<String> {
    let object = dict.get(key).ok()?;
    let (_, resolved) = doc.dereference(object).ok()?;
    lopdf::decode_text_string(resolved).ok()
}

/// `/ByteRange` as four offsets; any malformed entry yields zeros, which
/// downstream coverage checks treat as unverifiable.
fn sig_byte_range(dict: &lopdf::Dictionary, doc: &lopdf::Document) -> [i64; 4] {
    let parsed = (|| {
        let br = dict.get(b"ByteRange").ok()?;
        let (_, resolved) = doc.dereference(br).ok()?;
        let lopdf::Object::Array(items) = resolved else {
            return None;
        };
        if items.len() != 4 {
            return None;
        }
        let mut out = [0i64; 4];
        for (i, item) in items.iter().enumerate() {
            let (_, v) = doc.dereference(item).ok()?;
            out[i] = v
                .as_i64()
                .ok()
                .or_else(|| v.as_float().ok().map(|f| f as i64))?;
        }
        Some(out)
    })();
    parsed.unwrap_or([0; 4])
}

fn collect_signature_dicts(
    doc: &lopdf::Document,
    object: &lopdf::Object,
    visited: &mut HashSet<lopdf::ObjectId>,
    depth: usize,
    out: &mut Vec<lopdf::Dictionary>,
) -> AppResult<()> {
    if depth > 100 {
        return Ok(());
    }
    let Some(dict) = pdfdoc::dict_owned(doc, object)? else {
        return Ok(());
    };
    let is_sig = dict
        .get(b"FT")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .is_some_and(|n| n == b"Sig");
    if is_sig {
        if let Some(sig) = dict
            .get(b"V")
            .ok()
            .and_then(|v| doc.dereference(v).ok())
            .and_then(|(_, o)| o.as_dict().ok())
        {
            out.push(sig.clone());
        }
    }
    let Some(id) = object.as_reference().ok() else {
        return Ok(());
    };
    if !visited.insert(id) {
        return Ok(());
    }
    let Ok(kids) = dict.get(b"Kids") else {
        return Ok(());
    };
    let Ok((_, resolved)) = doc.dereference(kids) else {
        return Ok(());
    };
    let lopdf::Object::Array(items) = resolved else {
        return Ok(());
    };
    for kid in items {
        collect_signature_dicts(doc, kid, visited, depth + 1, out)?;
    }
    Ok(())
}

fn extract_signatures(doc: &lopdf::Document) -> AppResult<Vec<PdfSignature>> {
    let mut dicts = Vec::new();
    if let Some(form) = pdfdoc::acroform(doc) {
        if let Ok(fields) = form.get(b"Fields") {
            let (_, resolved) = doc
                .dereference(fields)
                .map_err(|e| AppError::BadRequest(format!("invalid PDF: {e}")))?;
            if let lopdf::Object::Array(items) = resolved {
                let mut visited = HashSet::new();
                for item in items {
                    collect_signature_dicts(doc, item, &mut visited, 0, &mut dicts)?;
                }
            }
        }
    }

    let mut out = Vec::new();
    for dict in dicts {
        let byte_range = sig_byte_range(&dict, doc);
        let contents = dict
            .get(b"Contents")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, r)| match r {
                lopdf::Object::String(bytes, _) => Some(bytes.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let sub_filter = dict
            .get(b"SubFilter")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned());
        out.push(PdfSignature {
            byte_range,
            contents,
            sub_filter,
            name: text_field(&dict, doc, b"Name"),
            sign_date: text_field(&dict, doc, b"M"),
            reason: text_field(&dict, doc, b"Reason"),
            location: text_field(&dict, doc, b"Location"),
        });
    }
    Ok(out)
}

/// The bytes a signature's ByteRange covers: `[a, a+b)` and `[c, c+d)`
/// concatenated — the detached CMS content.
fn signed_content(file: &[u8], byte_range: &[i64; 4]) -> Option<Vec<u8>> {
    let [a, b, c, d] = *byte_range;
    let (a, b, c, d) = (a as usize, b as usize, c as usize, d as usize);
    let first = file.get(a..a.checked_add(b)?)?;
    let second = file.get(c..c.checked_add(d)?)?;
    let mut out = Vec::with_capacity(first.len() + second.len());
    out.extend_from_slice(first);
    out.extend_from_slice(second);
    Some(out)
}

// ---------------------------------------------------------------------------
// Validation result JSON
// ---------------------------------------------------------------------------

fn empty_result(covers: bool) -> Value {
    // Every field Java's `SignatureValidationResult` serializes, so the
    // JSON shape is stable even on early-exit paths.
    json!({
        "valid": false,
        "chainValid": false,
        "trustValid": false,
        "chainValidationError": Value::Null,
        "certPathLength": 0,
        "notExpired": false,
        "coversEntireDocument": covers,
        "revocationChecked": false,
        "revocationStatus": "not-checked",
        "validationTimeSource": Value::Null,
        "signerName": Value::Null,
        "signatureDate": Value::Null,
        "reason": Value::Null,
        "location": Value::Null,
        "errorMessage": Value::Null,
        "issuerDN": Value::Null,
        "subjectDN": Value::Null,
        "serialNumber": Value::Null,
        "validFrom": Value::Null,
        "validUntil": Value::Null,
        "signatureAlgorithm": Value::Null,
        "keySize": 0,
        "version": Value::Null,
        "keyUsages": [],
        "selfSigned": false,
    })
}

fn java_date(st: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = st.into();
    dt.format("%a %b %d %H:%M:%S %Z %Y").to_string()
}

fn parse_pdf_date_offset(raw: &str) -> Option<i64> {
    let pos = raw.find(['O', 'o', '+', '-', 'Z', 'z'])?;
    let zone = &raw[pos..];
    if zone.starts_with('Z') || zone.starts_with('z') {
        return Some(0);
    }
    let zone = zone
        .strip_prefix('O')
        .or_else(|| zone.strip_prefix('o'))
        .unwrap_or(zone);
    let (sign, rest) = zone.split_at(1);
    let sign = if sign == "-" { -1i64 } else { 1i64 };
    let digits: String = rest
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(4)
        .collect();
    if digits.len() < 4 {
        return None;
    }
    let hours: i64 = digits[0..2].parse().ok()?;
    let mins: i64 = digits[2..4].parse().ok()?;
    Some(sign * (hours * 3600 + mins * 60))
}

fn pdf_date_to_system_time(raw: &str) -> Option<SystemTime> {
    let digits: String = raw
        .trim_start_matches("D:")
        .trim_start_matches("d:")
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(14)
        .collect();
    if digits.len() < 14 {
        return None;
    }
    let y: i32 = digits[0..4].parse().ok()?;
    let mo: u32 = digits[4..6].parse().ok()?;
    let d: u32 = digits[6..8].parse().ok()?;
    let h: u32 = digits[8..10].parse().ok()?;
    let mi: u32 = digits[10..12].parse().ok()?;
    let s: u32 = digits[12..14].parse().ok()?;
    let naive = chrono::NaiveDate::from_ymd_opt(y, mo, d)?.and_hms_opt(h, mi, s)?;
    let offset_secs = parse_pdf_date_offset(raw).unwrap_or(0);
    let utc = naive - chrono::Duration::seconds(offset_secs);
    let secs = utc.and_utc().timestamp();
    if secs < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

/// Java reports `signDate.getTime().toString()` — epoch milliseconds.
fn pdf_date_millis(raw: &str) -> Option<String> {
    pdf_date_to_system_time(raw).map(|st| {
        st.duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis().to_string())
            .unwrap_or_else(|_| "0".into())
    })
}

// ---------------------------------------------------------------------------
// Endpoint
// ---------------------------------------------------------------------------

/// `POST /api/v1/security/validate-signature` —
/// `ValidateSignatureController.validateSignature`: validates each PDF
/// signature dictionary's CMS signature and certificate chain, returning
/// `List<SignatureValidationResult>` as JSON.
#[stirling_macros::endpoint(files = "certFile")]
fn validate_signature(
    _state: AppState,
    _query: std::collections::HashMap<String, String>,
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
    _params: Vec<(String, String)>,
) -> AppResult<axum::Json<Value>> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let file_bytes: &[u8] = &upload.bytes;
    let file_len = upload.bytes.byte_len() as i64;

    let custom_cert = extras
        .iter()
        .find(|(field, _)| field == "certFile")
        .map(|(_, u)| parse_custom_cert(&u.bytes))
        .transpose()?;

    let signatures = extract_signatures(&document)?;

    // Document coverage: the furthest any signature's ByteRange reaches
    // must reach EOF, else bytes were appended after signing.
    let max_covered = signatures
        .iter()
        .map(|s| s.byte_range[2] + s.byte_range[3])
        .max()
        .unwrap_or(0);
    let document_covered = max_covered <= 0 || max_covered >= file_len;

    let revocation_enabled = !settings::current()
        .security
        .validation
        .revocation
        .mode
        .eq_ignore_ascii_case("none");

    let results = signatures
        .iter()
        .map(|sig| {
            validate_one_signature(
                file_bytes,
                sig,
                document_covered,
                custom_cert.as_ref(),
                revocation_enabled,
            )
        })
        .collect();
    Ok(axum::Json(Value::Array(results)))
}

fn parse_custom_cert(bytes: &[u8]) -> AppResult<Certificate> {
    // Java accepts PEM or DER.
    if let Ok(cert) = Certificate::from_pem(bytes) {
        return Ok(cert);
    }
    if let Ok(chain) = Certificate::load_pem_chain(bytes) {
        if let Some(cert) = chain.into_iter().next() {
            return Ok(cert);
        }
    }
    Certificate::from_der(bytes)
        .map_err(|e| AppError::BadRequest(format!("invalid certificate file format: {e}")))
}

fn validate_one_signature(
    file_bytes: &[u8],
    sig: &PdfSignature,
    document_covered: bool,
    custom_cert: Option<&Certificate>,
    revocation_enabled: bool,
) -> Value {
    let mut result = empty_result(document_covered);

    macro_rules! fail {
        ($msg:expr) => {{
            result["errorMessage"] = json!(format!("Signature validation failed: {}", $msg));
            return result;
        }};
    }

    // /Contents is zero-padded to its reserved length; trailing padding is
    // stripped before DER decode (BC parses from a stream for the same
    // reason).
    let mut cms_bytes = sig.contents.clone();
    while cms_bytes.last() == Some(&0) {
        cms_bytes.pop();
    }
    let content_info = match ContentInfo::from_der(&cms_bytes) {
        Ok(ci) => ci,
        Err(e) => fail!(format!("CMS parse error: {e}")),
    };
    let signed_data = match signed_data_from_content_info(&content_info) {
        Some(sd) => sd,
        None => fail!("signature is not a CMS SignedData"),
    };

    let mut pool: Vec<CertInfo> = Vec::new();
    if let Some(certs) = &signed_data.certificates {
        for choice in certs.0.iter() {
            if let CertificateChoices::Certificate(cert) = choice {
                if let Ok(der) = cert.to_der() {
                    if let Ok(info) = CertInfo::parse(&der) {
                        pool.push(info);
                    }
                }
            }
        }
    }
    let anchors = anchors_from_settings(custom_cert);

    // Document timestamp (PAdES-LTV): the signed content lives inside the
    // CMS as a TSTInfo, and the signature covers that — not the ByteRange.
    let is_doc_timestamp = sig.sub_filter.as_deref() == Some(SUBFILTER_RFC3161);
    let detached = signed_content(file_bytes, &sig.byte_range);
    let (content, doc_ts) = if is_doc_timestamp {
        let tst = tst_info_from_signed_data(&signed_data);
        let content = signed_data
            .encap_content_info
            .econtent
            .as_ref()
            .and_then(|e| e.to_der().ok())
            .and_then(|d| OctetString::from_der(&d).ok())
            .map(|o| o.as_bytes().to_vec())
            .unwrap_or_default();
        (content, tst.map(|t| t.gen_time.to_system_time()))
    } else {
        match detached {
            Some(c) => (c, None),
            None => fail!("ByteRange outside file bounds"),
        }
    };

    if is_doc_timestamp {
        // The TSTInfo's message imprint must equal the digest of the signed
        // byte range — otherwise the token attests some other document.
        let imprint_ok = tst_info_from_signed_data(&signed_data)
            .and_then(|t| {
                let kind = digest_for_oid(&t.message_imprint.hash_algorithm.oid)?;
                let covered = signed_content(file_bytes, &sig.byte_range).unwrap_or_default();
                Some(kind.hash(&covered) == t.message_imprint.hashed_message.as_bytes())
            })
            .unwrap_or(false);
        if !imprint_ok {
            result["errorMessage"] = json!("Timestamp message imprint does not match the document");
            return result;
        }
    }

    for signer_info in signed_data.signer_infos.0.iter() {
        validate_signer(
            &mut result,
            signer_info,
            &pool,
            &anchors,
            &content,
            is_doc_timestamp,
            doc_ts,
            sig,
            revocation_enabled,
        );
    }

    result
}

#[allow(clippy::too_many_arguments)]
fn validate_signer(
    result: &mut Value,
    signer_info: &SignerInfo,
    pool: &[CertInfo],
    anchors: &[Anchor],
    content: &[u8],
    is_doc_timestamp: bool,
    doc_ts: Option<SystemTime>,
    sig: &PdfSignature,
    revocation_enabled: bool,
) {
    let signer_cert = match &signer_info.sid {
        SignerIdentifier::IssuerAndSerialNumber(isn) => find_cert_by_issuer_serial(pool, isn),
        SignerIdentifier::SubjectKeyIdentifier(skid) => pool
            .iter()
            .find(|c| c.skid.as_deref() == Some(skid.0.as_bytes())),
    };
    let Some(signer_cert) = signer_cert else {
        result["valid"] = json!(false);
        result["errorMessage"] = json!("Signer certificate not found in CMS");
        populate_cert_fields(result, None, sig, doc_ts, is_doc_timestamp);
        return;
    };
    let Some(signer_key) = &signer_cert.key else {
        result["valid"] = json!(false);
        result["errorMessage"] = json!(format!(
            "unsupported signer key algorithm {}",
            signer_cert.key_alg
        ));
        populate_cert_fields(result, Some(signer_cert), sig, doc_ts, is_doc_timestamp);
        return;
    };

    // Signature input: with signed attributes it covers their DER encoding
    // (the SET tag becomes the IMPLICIT [0] tag); otherwise the content.
    let message = if let Some(attrs) = &signer_info.signed_attrs {
        let digest_alg = &signer_info.digest_alg;
        let Some(kind) = digest_for_oid(&digest_alg.oid) else {
            result["valid"] = json!(false);
            result["errorMessage"] =
                json!(format!("unsupported digest algorithm {}", digest_alg.oid));
            populate_cert_fields(result, Some(signer_cert), sig, doc_ts, is_doc_timestamp);
            return;
        };
        let expected = kind.hash(content);
        let md_ok = attrs.iter().any(|a| {
            a.oid == oid(OID_MESSAGE_DIGEST_ATTR)
                && a.values.iter().any(|v| {
                    v.to_der()
                        .ok()
                        .and_then(|d| OctetString::from_der(&d).ok())
                        .is_some_and(|o| o.as_bytes() == expected.as_slice())
                })
        });
        if !md_ok {
            result["valid"] = json!(false);
            result["errorMessage"] = json!("messageDigest attribute does not match content");
            populate_cert_fields(result, Some(signer_cert), sig, doc_ts, is_doc_timestamp);
            return;
        }
        // RFC 5652 §5.4: the signature input is the DER encoding of the
        // SignedAttrs SET OF — the [0] IMPLICIT tag on the wire is replaced
        // by the universal SET tag, which is what `to_der` already emits.
        match attrs.to_der() {
            Ok(d) => d,
            Err(_) => {
                result["valid"] = json!(false);
                result["errorMessage"] = json!("signed attributes encode failed");
                populate_cert_fields(result, Some(signer_cert), sig, doc_ts, is_doc_timestamp);
                return;
            }
        }
    } else {
        content.to_vec()
    };

    match verify_signature(
        signer_key,
        &signer_info.signature_algorithm,
        Some(&signer_info.digest_alg),
        &message,
        signer_info.signature.as_bytes(),
    ) {
        Ok(true) => result["valid"] = json!(true),
        Ok(false) => result["valid"] = json!(false),
        Err(e) => {
            result["valid"] = json!(false);
            result["errorMessage"] = json!(format!("Signature validation failed: {e}"));
            populate_cert_fields(result, Some(signer_cert), sig, doc_ts, is_doc_timestamp);
            return;
        }
    }

    let (at, source) = validation_time(signer_info, doc_ts);
    let chain = if anchors.is_empty() {
        Err("No trust anchors available".to_string())
    } else {
        build_cert_path(signer_cert, pool, anchors, at)
    };
    match chain {
        Ok(path) => {
            result["chainValid"] = json!(true);
            result["trustValid"] = json!(true);
            result["certPathLength"] = json!(path.len());
        }
        Err(e) => {
            result["chainValid"] = json!(false);
            result["trustValid"] = json!(false);
            result["chainValidationError"] = json!(e);
        }
    }

    result["notExpired"] = json!(signer_cert.validity_ok(at));
    result["revocationChecked"] = json!(revocation_enabled);
    result["revocationStatus"] = json!(if !revocation_enabled {
        "not-checked"
    } else if result["chainValid"].as_bool() == Some(true) {
        "good"
    } else {
        match result["chainValidationError"].as_str() {
            Some(e) if e.to_lowercase().contains("revocation") => {
                if e.to_lowercase().contains("unable to check") {
                    "soft-fail"
                } else {
                    "revoked"
                }
            }
            _ => "unknown",
        }
    });
    result["validationTimeSource"] = json!(source);

    populate_cert_fields(result, Some(signer_cert), sig, doc_ts, is_doc_timestamp);
}

fn populate_cert_fields(
    result: &mut Value,
    cert: Option<&CertInfo>,
    sig: &PdfSignature,
    doc_ts: Option<SystemTime>,
    is_doc_timestamp: bool,
) {
    result["signerName"] = json!(sig.name);
    result["signatureDate"] = if is_doc_timestamp {
        doc_ts.map(java_date)
    } else {
        sig.sign_date.as_deref().and_then(pdf_date_millis)
    }
    .map(Value::from)
    .unwrap_or(Value::Null);
    result["reason"] = json!(sig.reason);
    result["location"] = json!(sig.location);
    let Some(cert) = cert else {
        return;
    };
    result["issuerDN"] = json!(cert.issuer_name);
    result["subjectDN"] = json!(cert.subject_name);
    result["serialNumber"] = json!(cert.serial_hex);
    result["validFrom"] = json!(java_date(cert.not_before));
    result["validUntil"] = json!(java_date(cert.not_after));
    result["signatureAlgorithm"] = json!(cert.sig_alg_name());
    result["keySize"] = json!(cert.key_size());
    // Java serializes `String.valueOf(cert.getVersion())` — a string.
    result["version"] = json!((cert.cert.tbs_certificate.version as u8 + 1).to_string());
    result["keyUsages"] = json!(cert.key_usages());
    // Java's isSelfSigned also verifies the self-signature; a cert whose
    // issuer equals its subject but fails self-verification is reported
    // signed-by-its-issuer-name only.
    result["selfSigned"] = json!(
        cert.subject == cert.issuer
            && cert
                .key
                .as_ref()
                .and_then(|k| cert.verify_own_signature(k).ok())
                .unwrap_or(false)
    );
}

fn find_cert_by_issuer_serial<'a>(
    pool: &'a [CertInfo],
    isn: &IssuerAndSerialNumber,
) -> Option<&'a CertInfo> {
    let issuer_der = isn.issuer.to_der().ok()?.to_vec();
    pool.iter().find(|c| {
        c.issuer == issuer_der
            && c.cert
                .tbs_certificate
                .serial_number
                .to_der()
                .is_ok_and(|d| d == isn.serial_number.to_der().unwrap_or_default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate_fixture(path: &str) -> Vec<Value> {
        validate_fixture_with_cert(path, None)
    }

    fn validate_fixture_with_cert(path: &str, cert_path: Option<&str>) -> Vec<Value> {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let doc = pdfdoc::load("fixture.pdf", &bytes).unwrap();
        let sigs = extract_signatures(&doc).unwrap();
        assert!(!sigs.is_empty(), "{path}: no signature dictionaries found");
        let file_len = bytes.len() as i64;
        let max_covered = sigs
            .iter()
            .map(|s| s.byte_range[2] + s.byte_range[3])
            .max()
            .unwrap_or(0);
        let covered = max_covered <= 0 || max_covered >= file_len;
        let custom_cert = cert_path.map(|p| {
            let pem = std::fs::read(p).unwrap_or_else(|e| panic!("{p}: {e}"));
            Certificate::from_pem(&pem).unwrap_or_else(|e| panic!("{p}: {e}"))
        });
        sigs.iter()
            .map(|s| validate_one_signature(&bytes, s, covered, custom_cert.as_ref(), false))
            .collect()
    }

    const TIMESTAMPED: &str = "../app/core/src/test/resources/timestamp/doc-timestamped.pdf";
    const TIMESTAMPED_TAMPERED: &str =
        "../app/core/src/test/resources/timestamp/doc-timestamped-tampered.pdf";

    // Mirrors `DocumentTimestampValidationTest`: a real FreeTSA RFC 3161
    // document timestamp validates, a byte-flipped copy fails the imprint.
    // The FreeTSA root is not in the Mozilla TLS bundle or typical system
    // stores, so the fixture's chain is anchored through the endpoint's
    // `certFile` custom-trust-anchor path — the same mechanism a deployment
    // uses to trust a private or timestamping CA.
    #[test]
    fn document_timestamp_validates() {
        let results =
            validate_fixture_with_cert(TIMESTAMPED, Some("tests/fixtures/freetsa-root.pem"));
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r["valid"], json!(true), "{r}");
        assert_eq!(r["errorMessage"], Value::Null, "{r}");
        assert_eq!(r["validationTimeSource"], json!("document-timestamp"));
        assert!(r["signatureDate"].is_string(), "{r}");
        assert!(
            r["subjectDN"]
                .as_str()
                .unwrap_or("")
                .contains("freetsa.org"),
            "{r}"
        );
        assert_eq!(r["coversEntireDocument"], json!(true));
    }

    #[test]
    fn tampered_document_timestamp_fails_imprint() {
        let results = validate_fixture(TIMESTAMPED_TAMPERED);
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r["valid"], json!(false), "{r}");
        assert_eq!(
            r["errorMessage"],
            json!("Timestamp message imprint does not match the document")
        );
    }

    #[test]
    fn rsa_pss_params_decode() {
        // RFC 4055 RSASSA-PSS-params: [0] sha256, [1] mgf1-sha256,
        // [2] salt 32, [3] trailer 1 — all context-tagged IMPLICIT.
        #[rustfmt::skip]
        let der: &[u8] = &[
            0x30, 0x31,
            0xA0, 0x0D, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00,
            0xA1, 0x1A, 0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x08,
            0x30, 0x0D, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00,
            0x82, 0x01, 0x20,
            0x83, 0x01, 0x01,
        ];
        let pss = RsaPssParams::from_der(der).unwrap();
        assert_eq!(
            pss.hash_algorithm.unwrap().oid,
            oid("2.16.840.1.101.3.4.2.1")
        );
        let mgf = pss.mask_gen_algorithm.unwrap();
        assert_eq!(mgf1_hash(&mgf), Ok(DigestKind::Sha256));
        assert_eq!(uint_value(pss.salt_length.as_ref().unwrap()), Some(32));
        assert_eq!(uint_value(pss.trailer_field.as_ref().unwrap()), Some(1));

        // Every field DEFAULTed → encoded as an empty SEQUENCE.
        let empty = RsaPssParams::from_der(&[0x30, 0x00]).unwrap();
        assert!(empty.hash_algorithm.is_none());
        assert!(empty.mask_gen_algorithm.is_none());
        assert!(empty.salt_length.is_none());
        assert!(empty.trailer_field.is_none());
    }

    #[test]
    fn pdf_date_parsing() {
        let st = pdf_date_to_system_time("D:20240101120000+02'00'").unwrap();
        assert_eq!(
            st.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
            1704103200
        );
        assert_eq!(
            pdf_date_millis("D:20240101120000Z").as_deref(),
            Some("1704110400000")
        );
        assert!(pdf_date_to_system_time("not a date").is_none());
    }
}
