//! Persistent, Fernet-encrypted cache of the last-applied config-push.
//! Port of `stirling.config.config_cache`.
//!
//! Boot restores the cache so an engine-only restart keeps admin config;
//! sibling workers poll the file's (mtime, size) stamp to adopt a push.
//! The on-disk format is byte-compatible with Python `cryptography`'s
//! Fernet so a cache written by either implementation reads on both.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::engine::general_purpose::URL_SAFE;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::config::{AppSettings, ENGINE_ROOT};
use crate::contracts::config::ConfigPushRequest;

const CACHE_FILENAME: &str = "ai_config_cache.enc";
const KEY_FILENAME: &str = "ai_config_cache.key";
/// Constant salt/info so the same shared secret always derives the same key.
const HKDF_SALT: &[u8] = b"stirling-ai-config-cache/v1/salt";
const HKDF_INFO: &[u8] = b"stirling-ai-config-cache/v1/fernet-key";
/// Fernet spec: version byte, 8-byte big-endian timestamp, 16-byte IV,
/// AES-128-CBC ciphertext, 32-byte HMAC-SHA256 over everything before it.
const FERNET_VERSION: u8 = 0x80;
const IV_LEN: usize = 16;
const HMAC_LEN: usize = 32;
const BLOCK: usize = 16;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type HmacSha256 = Hmac<Sha256>;

fn data_dir() -> PathBuf {
    Path::new(ENGINE_ROOT).join("data")
}

fn derive_key_from_secret(secret: &str) -> [u8; 32] {
    let mut okm = [0u8; 32];
    hkdf::Hkdf::<Sha256>::new(Some(HKDF_SALT), secret.as_bytes())
        .expand(HKDF_INFO, &mut okm)
        .expect("HKDF-SHA256 output length is within the hash limit");
    okm
}

/// Write `payload` atomically (temp file + rename) and owner-only (0600)
/// from the moment it exists — same contract as Python's `_write_private_bytes`.
fn write_private_bytes(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(payload)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).expect("OS RNG unavailable");
    buf
}

/// The Fernet key: HKDF-derived from the shared secret, or a random keyfile
/// when no secret is configured (modest protection only — same warning path
/// as Python).
fn fernet_key(secret: &str, dir: &Path) -> anyhow::Result<[u8; 32]> {
    if !secret.is_empty() {
        return Ok(derive_key_from_secret(secret));
    }
    let key_path = dir.join(KEY_FILENAME);
    if let Ok(raw) = fs::read(&key_path) {
        let trimmed = String::from_utf8_lossy(&raw).trim().to_string();
        if let Ok(decoded) = URL_SAFE.decode(trimmed.as_bytes()) {
            if decoded.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&decoded);
                return Ok(key);
            }
        }
    }
    // Fernet.generate_key() equivalent: urlsafe-b64 of 32 random bytes.
    let key = random_bytes::<32>();
    fs::create_dir_all(dir)?;
    write_private_bytes(&key_path, URL_SAFE.encode(key).as_bytes())?;
    tracing::warn!(
        "STIRLING_ENGINE_SHARED_SECRET is not set; encrypting the AI config cache with a local \
         keyfile at {} (0600, best-effort). This is modest protection only - set a shared secret \
         for HKDF key derivation in any deployment where the cache must be strongly protected.",
        key_path.display()
    );
    Ok(key)
}

fn hmac_sign(signing: &[u8], body: &[u8]) -> [u8; HMAC_LEN] {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(signing).expect("HMAC accepts any key length");
    mac.update(body);
    mac.finalize().into_bytes().into()
}

fn fernet_encrypt(key: &[u8; 32], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let (signing, encryption) = key.split_at(16);
    let iv = random_bytes::<IV_LEN>();

    // Room for the PKCS7 pad block; encrypt_padded_mut returns the ciphertext.
    let mut buf = vec![0u8; plaintext.len() + BLOCK];
    buf[..plaintext.len()].copy_from_slice(plaintext);
    let ciphertext = Aes128CbcEnc::new(encryption.into(), &iv.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
        .map_err(|e| anyhow::anyhow!("config cache encrypt failed: {e}"))?
        .to_vec();

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut body = Vec::with_capacity(1 + 8 + IV_LEN + ciphertext.len());
    body.push(FERNET_VERSION);
    body.extend_from_slice(&timestamp.to_be_bytes());
    body.extend_from_slice(&iv);
    body.extend_from_slice(&ciphertext);
    let tag = hmac_sign(signing, &body);
    body.extend_from_slice(&tag);
    Ok(URL_SAFE.encode(body).into_bytes())
}

fn fernet_decrypt(key: &[u8; 32], token: &[u8]) -> anyhow::Result<Vec<u8>> {
    let decoded = URL_SAFE
        .decode(token)
        .map_err(|e| anyhow::anyhow!("config cache is not valid base64: {e}"))?;
    let (signing, encryption) = key.split_at(16);
    if decoded.len() < 1 + 8 + IV_LEN + BLOCK + HMAC_LEN || decoded[0] != FERNET_VERSION {
        anyhow::bail!("config cache token has an unexpected shape");
    }
    let split = decoded.len() - HMAC_LEN;
    let (body, tag) = decoded.split_at(split);
    if hmac_sign(signing, body)[..] != tag[..] {
        anyhow::bail!("config cache signature mismatch");
    }
    let iv: &[u8; IV_LEN] = body[9..9 + IV_LEN].try_into().expect("IV length checked");
    let mut buf = body[9 + IV_LEN..].to_vec();
    let plaintext = Aes128CbcDec::new(encryption.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow::anyhow!("config cache decrypt failed: {e}"))?;
    Ok(plaintext.to_vec())
}

/// Encrypt and persist the last-applied pushed config, overwriting any prior file.
pub fn save_config(settings: &AppSettings, request: &ConfigPushRequest) -> anyhow::Result<()> {
    save_config_in(&data_dir(), &settings.engine_shared_secret, request)
}

fn save_config_in(dir: &Path, secret: &str, request: &ConfigPushRequest) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    let key = fernet_key(secret, dir)?;
    let token = fernet_encrypt(&key, &serde_json::to_vec(request)?)?;
    write_private_bytes(&dir.join(CACHE_FILENAME), &token)?;
    Ok(())
}

/// Identify the current cache file as (mtime_ns, size), or None when absent;
/// cheap enough to poll.
pub fn cache_stamp() -> Option<(i64, u64)> {
    cache_stamp_in(&data_dir())
}

fn cache_stamp_in(dir: &Path) -> Option<(i64, u64)> {
    let meta = fs::metadata(dir.join(CACHE_FILENAME)).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;
    Some((mtime, meta.len()))
}

/// Load + decrypt the persisted config; returns None (never fails loudly)
/// when absent, corrupt, or the key is wrong.
pub fn load_config(settings: &AppSettings) -> Option<ConfigPushRequest> {
    load_config_in(&data_dir(), &settings.engine_shared_secret)
}

fn load_config_in(dir: &Path, secret: &str) -> Option<ConfigPushRequest> {
    let cache_path = dir.join(CACHE_FILENAME);
    let token = fs::read(&cache_path).ok()?;
    let key = match fernet_key(secret, dir) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("AI config cache key unavailable: {e}");
            return None;
        }
    };
    let payload = match fernet_decrypt(&key, &token) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                "Ignoring unreadable AI config cache at {}: {e}",
                cache_path.display()
            );
            return None;
        }
    };
    match serde_json::from_slice(&payload) {
        Ok(request) => Some(request),
        Err(e) => {
            tracing::warn!(
                "Ignoring unreadable AI config cache at {}: {e}",
                cache_path.display()
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_from_b64(s: &str) -> [u8; 32] {
        let decoded = URL_SAFE.decode(s).unwrap();
        decoded.try_into().unwrap()
    }

    #[test]
    fn decrypts_python_fernet_token() {
        // Generated by Python `cryptography.fernet.Fernet` — the cache must
        // read tokens written by the Python engine during migration.
        let key = key_from_b64("sTeONLr1YrdNFpO8moI_EohpBylCAC2ioj0MmwiRZXc=");
        let token = b"gAAAAABq7o_cQebTMcdyDLOBRcn5pneFpFJcIYfpBnQPGqlYB-15zIuzknU-erDU-N2ZSpJVHmXazg4E6ynIaL0mlDv46QgU66BzTHRBqE05BVmTBm3hqZVOFfaeRog8WWCEJzlGFAAN";
        let plain = fernet_decrypt(&key, token).unwrap();
        assert_eq!(plain, br#"{"models":{"provider":"openai"}}"#);
    }

    #[test]
    fn hkdf_derivation_matches_python() {
        // Fernet(DERIVED).encrypt(b"hello") produced TOKEN2 in Python.
        let derived_key = key_from_b64("Cky4PBcamLHkDFQiAMeCHZqy0tJQoZddwB5R7FhxY2I=");
        assert_eq!(derive_key_from_secret("mysecret"), derived_key);
        let token = b"gAAAAABq7o_c4V98yCOQDxrmTlkOKPpFRBTJ4MHrgEfhzQr3TLQI_C5zo8XWYj7EQnquyEBVQWgDumGnrS2O2u-d6AAi3SfUFA==";
        assert_eq!(fernet_decrypt(&derived_key, token).unwrap(), b"hello");
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = random_bytes::<32>();
        let token = fernet_encrypt(&key, b"payload").unwrap();
        assert_eq!(fernet_decrypt(&key, &token).unwrap(), b"payload");
        // Wrong key must fail the HMAC check, not panic.
        let other = random_bytes::<32>();
        assert!(fernet_decrypt(&other, &token).is_err());
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::contracts::config::{ConfigLimitsSection, ConfigModelsSection, ConfigRagSection};

    fn sample() -> ConfigPushRequest {
        serde_json::from_value(serde_json::json!({
            "models": {
                "provider": "anthropic",
                "smartModel": "claude-haiku-4-5",
                "fastModel": "claude-haiku-4-5",
                "smartMaxTokens": 8192,
                "fastMaxTokens": 2048,
                "apiKey": "secret-key-value",
                "baseUrl": ""
            },
            "rag": {
                "embeddingProvider": "voyageai",
                "embeddingModel": "voyage-4",
                "embeddingApiKey": "embed-secret",
                "embeddingBaseUrl": "",
                "topK": 20,
                "maxSearches": 5
            },
            "limits": {"maxPages": 200, "maxCharacters": 200000, "modelMaxConcurrency": 32}
        }))
        .unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stirling-cache-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_with_shared_secret() {
        let dir = tmpdir("secret");
        let req = sample();
        save_config_in(&dir, "the-shared-secret", &req).unwrap();
        // HKDF-from-secret path: no keyfile is written.
        assert!(!dir.join(KEY_FILENAME).exists());
        let loaded = load_config_in(&dir, "the-shared-secret").unwrap();
        assert_eq!(serde_json::to_value(&loaded).unwrap(), serde_json::to_value(&req).unwrap());
        // Secrets are encrypted at rest.
        let raw = fs::read(dir.join(CACHE_FILENAME)).unwrap();
        assert!(!raw.windows(b"secret-key-value".len()).any(|w| w == b"secret-key-value"));
    }

    #[test]
    fn roundtrip_with_keyfile_when_no_secret() {
        let dir = tmpdir("keyfile");
        let req = sample();
        save_config_in(&dir, "", &req).unwrap();
        assert!(dir.join(KEY_FILENAME).exists());
        assert!(load_config_in(&dir, "").is_some());
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tmpdir("missing");
        assert!(load_config_in(&dir, "s").is_none());
    }

    #[test]
    fn load_corrupt_returns_none() {
        let dir = tmpdir("corrupt");
        fs::write(dir.join(CACHE_FILENAME), b"not-a-valid-fernet-token").unwrap();
        assert!(load_config_in(&dir, "a-secret").is_none());
    }

    #[test]
    fn wrong_key_returns_none() {
        let dir = tmpdir("wrongkey");
        save_config_in(&dir, "secret-a", &sample()).unwrap();
        assert!(load_config_in(&dir, "secret-b").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cache_and_keyfile_are_owner_only() {
        let dir = tmpdir("perms");
        save_config_in(&dir, "", &sample()).unwrap();
        for name in [CACHE_FILENAME, KEY_FILENAME] {
            let mode = fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{name} is {mode:o}");
        }
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = tmpdir("tmpfile");
        save_config_in(&dir, "the-shared-secret", &sample()).unwrap();
        save_config_in(&dir, "the-shared-secret", &sample()).unwrap();
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        assert!(load_config_in(&dir, "the-shared-secret").is_some());
    }

    #[test]
    fn cache_stamp_tracks_writes() {
        let dir = tmpdir("stamp");
        assert!(cache_stamp_in(&dir).is_none());
        save_config_in(&dir, "the-shared-secret", &sample()).unwrap();
        let first = cache_stamp_in(&dir);
        assert!(first.is_some());

        let mut bigger = sample();
        bigger.models.smart_model =
            "claude-haiku-4-5-with-a-much-longer-name-so-the-size-differs".to_string();
        save_config_in(&dir, "the-shared-secret", &bigger).unwrap();
        assert_ne!(cache_stamp_in(&dir), first);
    }
}
