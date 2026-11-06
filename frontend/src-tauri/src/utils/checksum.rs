use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use indexmap::IndexMap;
use sha2::Digest;

const BUFFER_SIZE: usize = 8192;

#[derive(Debug)]
pub enum ChecksumError {
    Io(io::Error),
    UnsupportedAlgorithm(String),
}

impl std::fmt::Display for ChecksumError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::UnsupportedAlgorithm(algorithm) => {
                write!(formatter, "Unsupported algorithm: {algorithm}")
            }
        }
    }
}

impl std::error::Error for ChecksumError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::UnsupportedAlgorithm(_) => None,
        }
    }
}

impl From<io::Error> for ChecksumError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

enum DigestState {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
}

impl DigestState {
    fn new(algorithm: &str) -> Result<Self, ChecksumError> {
        match algorithm.to_uppercase().as_str() {
            "MD5" => Ok(Self::Md5(md5::Md5::new())),
            "SHA-1" => Ok(Self::Sha1(sha1::Sha1::new())),
            "SHA-256" => Ok(Self::Sha256(sha2::Sha256::new())),
            "SHA-384" => Ok(Self::Sha384(sha2::Sha384::new())),
            "SHA-512" => Ok(Self::Sha512(sha2::Sha512::new())),
            _ => Err(ChecksumError::UnsupportedAlgorithm(algorithm.to_owned())),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Md5(digest) => digest.update(data),
            Self::Sha1(digest) => digest.update(data),
            Self::Sha256(digest) => digest.update(data),
            Self::Sha384(digest) => digest.update(data),
            Self::Sha512(digest) => digest.update(data),
        }
    }

    fn finalize(self) -> Vec<u8> {
        match self {
            Self::Md5(digest) => digest.finalize().to_vec(),
            Self::Sha1(digest) => digest.finalize().to_vec(),
            Self::Sha256(digest) => digest.finalize().to_vec(),
            Self::Sha384(digest) => digest.finalize().to_vec(),
            Self::Sha512(digest) => digest.finalize().to_vec(),
        }
    }
}

enum ChecksumState {
    Crc32(crc32fast::Hasher),
    Adler32(Adler32),
}

impl ChecksumState {
    fn new(algorithm: &str) -> Option<Self> {
        match algorithm.to_uppercase().as_str() {
            "CRC32" => Some(Self::Crc32(crc32fast::Hasher::new())),
            "ADLER32" => Some(Self::Adler32(Adler32::default())),
            _ => None,
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Crc32(checksum) => checksum.update(data),
            Self::Adler32(checksum) => checksum.update(data),
        }
    }

    fn value(self) -> u32 {
        match self {
            Self::Crc32(checksum) => checksum.finalize(),
            Self::Adler32(checksum) => checksum.value(),
        }
    }
}

struct Adler32 {
    a: u32,
    b: u32,
}

impl Default for Adler32 {
    fn default() -> Self {
        Self { a: 1, b: 0 }
    }
}

impl Adler32 {
    fn update(&mut self, data: &[u8]) {
        const MODULUS: u32 = 65_521;
        for &byte in data {
            self.a = (self.a + u32::from(byte)) % MODULUS;
            self.b = (self.b + self.a) % MODULUS;
        }
    }

    fn value(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

pub fn checksum_path(path: impl AsRef<Path>, algorithm: &str) -> Result<String, ChecksumError> {
    checksum(File::open(path)?, algorithm)
}

pub fn checksum<R: Read>(reader: R, algorithm: &str) -> Result<String, ChecksumError> {
    let mut checksums = checksums(reader, &[algorithm])?;
    checksums
        .shift_remove(algorithm)
        .ok_or_else(|| ChecksumError::UnsupportedAlgorithm(algorithm.to_owned()))
}

pub fn checksum_base64_path(
    path: impl AsRef<Path>,
    algorithm: &str,
) -> Result<String, ChecksumError> {
    checksum_base64(File::open(path)?, algorithm)
}

pub fn checksum_base64<R: Read>(reader: R, algorithm: &str) -> Result<String, ChecksumError> {
    let bytes = checksum_bytes(reader, algorithm)?;
    Ok(STANDARD.encode(bytes))
}

pub fn checksums_path(
    path: impl AsRef<Path>,
    algorithms: &[&str],
) -> Result<IndexMap<String, String>, ChecksumError> {
    checksums(File::open(path)?, algorithms)
}

pub fn checksums<R: Read>(
    mut reader: R,
    algorithms: &[&str],
) -> Result<IndexMap<String, String>, ChecksumError> {
    let mut digests = IndexMap::new();
    let mut checksums = IndexMap::new();

    for &algorithm in algorithms {
        if let Some(checksum) = ChecksumState::new(algorithm) {
            checksums.insert(algorithm.to_owned(), checksum);
        } else {
            digests.insert(algorithm.to_owned(), DigestState::new(algorithm)?);
        }
    }

    let mut buffer = [0; BUFFER_SIZE];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        for digest in digests.values_mut() {
            digest.update(&buffer[..count]);
        }
        for checksum in checksums.values_mut() {
            checksum.update(&buffer[..count]);
        }
    }

    let mut results = IndexMap::new();
    for (algorithm, digest) in digests {
        results.insert(algorithm, to_hex(&digest.finalize()));
    }
    for (algorithm, checksum) in checksums {
        results.insert(algorithm, format!("{:08x}", checksum.value()));
    }
    Ok(results)
}

pub fn matches_path(
    path: impl AsRef<Path>,
    algorithm: &str,
    expected: &str,
) -> Result<bool, ChecksumError> {
    matches(File::open(path)?, algorithm, expected)
}

pub fn matches<R: Read>(reader: R, algorithm: &str, expected: &str) -> Result<bool, ChecksumError> {
    Ok(checksum(reader, algorithm)?.eq_ignore_ascii_case(expected))
}

fn checksum_bytes<R: Read>(reader: R, algorithm: &str) -> Result<Vec<u8>, ChecksumError> {
    if let Some(mut checksum) = ChecksumState::new(algorithm) {
        let mut reader = reader;
        let mut buffer = [0; BUFFER_SIZE];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            checksum.update(&buffer[..count]);
        }
        return Ok(checksum.value().to_be_bytes().to_vec());
    }

    let mut digest = DigestState::new(algorithm)?;
    let mut reader = reader;
    let mut buffer = [0; BUFFER_SIZE];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(HEX[usize::from(byte >> 4)] as char);
        result.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{checksum, checksum_base64, checksums, matches, Adler32, ChecksumError};

    const HELLO: &[u8] = b"hello";

    #[test]
    fn computes_digest_and_checksum_formats() {
        assert_eq!(
            checksum(Cursor::new(HELLO), "MD5").unwrap(),
            "5d41402abc4b2a76b9719d911017c592"
        );
        assert_eq!(
            checksum(Cursor::new(HELLO), "SHA-1").unwrap(),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        );
        assert_eq!(
            checksum(Cursor::new(HELLO), "SHA-256").unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            checksum(Cursor::new(HELLO), "SHA-384").unwrap(),
            "59e1748777448c69de6b800d7a33bbfb9ff1b463e44354c3553bcdb9c666fa90125a3c79f90397bdf5f6a13de828684f"
        );
        assert_eq!(
            checksum(Cursor::new(HELLO), "SHA-512").unwrap(),
            "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca72323c3d99ba5c11d7c7acc6e14b8c5da0c4663475c2e5c3adef46f73bcdec043"
        );
        assert_eq!(checksum(Cursor::new(HELLO), "CRC32").unwrap(), "3610a686");
        assert_eq!(checksum(Cursor::new(HELLO), "ADLER32").unwrap(), "062c0215");
    }

    #[test]
    fn encodes_digest_and_unsigned_checksum_as_base64() {
        assert_eq!(
            checksum_base64(Cursor::new(HELLO), "MD5").unwrap(),
            "XUFAKrxLKna5cZ2REBfFkg=="
        );
        assert_eq!(
            checksum_base64(Cursor::new(HELLO), "CRC32").unwrap(),
            "NhCmhg=="
        );
        assert_eq!(
            checksum_base64(Cursor::new([0]), "CRC32").unwrap(),
            "0gLvjQ=="
        );
    }

    #[test]
    fn computes_multiple_algorithms_in_one_ordered_result() {
        let results = checksums(Cursor::new(HELLO), &["MD5", "SHA-256", "CRC32"]).unwrap();
        assert_eq!(
            results.keys().map(String::as_str).collect::<Vec<_>>(),
            ["MD5", "SHA-256", "CRC32"]
        );
        assert_eq!(
            results.get("SHA-256").map(String::as_str),
            Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        );
    }

    #[test]
    fn compares_hex_without_case_sensitivity() {
        assert!(matches(Cursor::new(HELLO), "CRC32", "3610A686").unwrap());
        assert!(!matches(Cursor::new(HELLO), "SHA-1", "00").unwrap());
    }

    #[test]
    fn rejects_unsupported_algorithms() {
        assert!(matches!(
            checksum(Cursor::new(HELLO), "FAKE-ALGO"),
            Err(ChecksumError::UnsupportedAlgorithm(_))
        ));
    }

    #[test]
    fn matches_adler32_reference_and_empty_input() {
        let mut adler32 = Adler32::default();
        adler32.update(HELLO);
        assert_eq!(adler32.value(), 0x062c_0215);
        assert_eq!(checksum(Cursor::new([]), "CRC32").unwrap(), "00000000");
        assert_eq!(checksum(Cursor::new([]), "ADLER32").unwrap(), "00000001");
        assert_eq!(
            checksum(Cursor::new([]), "MD5").unwrap(),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
    }
}
