//! Checksum support: MD5, SHA-1, SHA-256, SHA-512 and BLAKE3.
//!
//! Hashing is done off the async worker threads (`spawn_blocking`) and reports
//! progress so a 20 GiB verification does not look like a freeze.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};

/// Supported digest algorithms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgo {
    Md5,
    Sha1,
    Sha256,
    Sha512,
    Blake3,
}

impl HashAlgo {
    pub fn as_str(&self) -> &'static str {
        match self {
            HashAlgo::Md5 => "md5",
            HashAlgo::Sha1 => "sha1",
            HashAlgo::Sha256 => "sha256",
            HashAlgo::Sha512 => "sha512",
            HashAlgo::Blake3 => "blake3",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "md5" => Ok(HashAlgo::Md5),
            "sha1" | "sha-1" => Ok(HashAlgo::Sha1),
            "sha256" | "sha-256" | "sha2" => Ok(HashAlgo::Sha256),
            "sha512" | "sha-512" => Ok(HashAlgo::Sha512),
            "blake3" => Ok(HashAlgo::Blake3),
            other => Err(Error::UnknownHashAlgo(other.to_string())),
        }
    }

    /// Length of the hex digest.
    pub fn hex_len(&self) -> usize {
        match self {
            HashAlgo::Md5 => 32,
            HashAlgo::Sha1 => 40,
            HashAlgo::Sha256 => 64,
            HashAlgo::Sha512 => 128,
            HashAlgo::Blake3 => 64,
        }
    }

    /// Guess the algorithm from a bare hex string's length.
    pub fn guess_from_hex(hex: &str) -> Option<Self> {
        let hex = hex.trim();
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        match hex.len() {
            32 => Some(HashAlgo::Md5),
            40 => Some(HashAlgo::Sha1),
            64 => Some(HashAlgo::Sha256),
            128 => Some(HashAlgo::Sha512),
            _ => None,
        }
    }
}

/// A digest the user expects the finished file to have.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedHash {
    pub algo: HashAlgo,
    /// Lower-case hex.
    pub hex: String,
}

impl ExpectedHash {
    pub fn new(algo: HashAlgo, hex: impl Into<String>) -> Result<Self> {
        let hex = hex.into().trim().to_ascii_lowercase();
        if hex.len() != algo.hex_len() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::InvalidHashSpec(format!(
                "{} expects {} hex characters, got {}",
                algo.as_str(),
                algo.hex_len(),
                hex.len()
            )));
        }
        Ok(ExpectedHash { algo, hex })
    }

    /// Accepts `sha256:9f86d0...`, `SHA-256 9f86d0...` or a bare hex string
    /// (algorithm inferred from its length).
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        let (algo_part, hex_part) = match spec.split_once(':') {
            Some((a, h)) => (Some(a), h),
            None => match spec.split_once(' ') {
                Some((a, h)) => (Some(a), h),
                None => (None, spec),
            },
        };
        let hex = hex_part.trim();
        let algo = match algo_part {
            Some(a) => HashAlgo::parse(a)?,
            None => HashAlgo::guess_from_hex(hex).ok_or_else(|| {
                Error::InvalidHashSpec(format!(
                    "cannot infer an algorithm from a {0}-character digest; write it as \
                     `sha256:{1}`",
                    hex.len(),
                    hex
                ))
            })?,
        };
        ExpectedHash::new(algo, hex)
    }

    pub fn display(&self) -> String {
        let short: String = self.hex.chars().take(12).collect();
        format!("{}:{short}...", self.algo.as_str())
    }

    pub fn matches(&self, actual_hex: &str) -> bool {
        self.hex.eq_ignore_ascii_case(actual_hex.trim())
    }
}

/// Trait object wrapper so all five algorithms share one code path even though
/// they come from crates built on two different `digest` versions.
pub trait IncrementalHasher: Send {
    fn update(&mut self, data: &[u8]);
    fn finish_hex(self: Box<Self>) -> String;
    fn algo(&self) -> HashAlgo;
}

macro_rules! digest_hasher {
    ($name:ident, $ty:ty, $algo:expr) => {
        struct $name(<$ty as DigestShim>::Inner);

        impl IncrementalHasher for $name {
            fn update(&mut self, data: &[u8]) {
                <$ty as DigestShim>::update(&mut self.0, data);
            }
            fn finish_hex(self: Box<Self>) -> String {
                <$ty as DigestShim>::finish_hex(self.0)
            }
            fn algo(&self) -> HashAlgo {
                $algo
            }
        }
    };
}

/// Bridges both `digest` 0.10 (sha1) and `digest` 0.11 (sha2, md-5).
trait DigestShim {
    type Inner;
    fn new() -> Self::Inner;
    fn update(inner: &mut Self::Inner, data: &[u8]);
    fn finish_hex(inner: Self::Inner) -> String;
}

struct Sha256Digest;
impl DigestShim for Sha256Digest {
    type Inner = sha2::Sha256;
    fn new() -> Self::Inner {
        use sha2::Digest as _;
        sha2::Sha256::new()
    }
    fn update(inner: &mut Self::Inner, data: &[u8]) {
        use sha2::Digest as _;
        inner.update(data);
    }
    fn finish_hex(inner: Self::Inner) -> String {
        use sha2::Digest as _;
        hex::encode(inner.finalize())
    }
}

struct Sha512Digest;
impl DigestShim for Sha512Digest {
    type Inner = sha2::Sha512;
    fn new() -> Self::Inner {
        use sha2::Digest as _;
        sha2::Sha512::new()
    }
    fn update(inner: &mut Self::Inner, data: &[u8]) {
        use sha2::Digest as _;
        inner.update(data);
    }
    fn finish_hex(inner: Self::Inner) -> String {
        use sha2::Digest as _;
        hex::encode(inner.finalize())
    }
}

struct Md5Digest;
impl DigestShim for Md5Digest {
    type Inner = md5::Md5;
    fn new() -> Self::Inner {
        use md5::Digest as _;
        md5::Md5::new()
    }
    fn update(inner: &mut Self::Inner, data: &[u8]) {
        use md5::Digest as _;
        inner.update(data);
    }
    fn finish_hex(inner: Self::Inner) -> String {
        use md5::Digest as _;
        hex::encode(inner.finalize())
    }
}

struct Sha1Digest;
impl DigestShim for Sha1Digest {
    type Inner = sha1::Sha1;
    fn new() -> Self::Inner {
        use sha1::Digest as _;
        sha1::Sha1::new()
    }
    fn update(inner: &mut Self::Inner, data: &[u8]) {
        use sha1::Digest as _;
        inner.update(data);
    }
    fn finish_hex(inner: Self::Inner) -> String {
        use sha1::Digest as _;
        hex::encode(inner.finalize())
    }
}

digest_hasher!(Sha256Hasher, Sha256Digest, HashAlgo::Sha256);
digest_hasher!(Sha512Hasher, Sha512Digest, HashAlgo::Sha512);
digest_hasher!(Md5Hasher, Md5Digest, HashAlgo::Md5);
digest_hasher!(Sha1Hasher, Sha1Digest, HashAlgo::Sha1);

struct Blake3Hasher(blake3::Hasher);

impl IncrementalHasher for Blake3Hasher {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    fn finish_hex(self: Box<Self>) -> String {
        self.0.finalize().to_hex().to_string()
    }
    fn algo(&self) -> HashAlgo {
        HashAlgo::Blake3
    }
}

/// Create a streaming hasher for `algo`.
pub fn hasher(algo: HashAlgo) -> Box<dyn IncrementalHasher> {
    match algo {
        HashAlgo::Md5 => Box::new(Md5Hasher(<Md5Digest as DigestShim>::new())),
        HashAlgo::Sha1 => Box::new(Sha1Hasher(<Sha1Digest as DigestShim>::new())),
        HashAlgo::Sha256 => Box::new(Sha256Hasher(<Sha256Digest as DigestShim>::new())),
        HashAlgo::Sha512 => Box::new(Sha512Hasher(<Sha512Digest as DigestShim>::new())),
        HashAlgo::Blake3 => Box::new(Blake3Hasher(blake3::Hasher::new())),
    }
}

/// Hash a whole file. Progress is written to `progress_bytes` and the operation
/// aborts early when `cancel` fires.
pub async fn hash_file(
    path: impl AsRef<Path>,
    algo: HashAlgo,
    progress: Option<Arc<AtomicU64>>,
    cancel: Option<CancellationToken>,
) -> Result<String> {
    let path = path.as_ref().to_path_buf();
    let span = tokio::task::spawn_blocking(move || {
        let mut file = File::open(&path).map_err(|e| Error::io(&path, e))?;
        let total = file.metadata().map(|m| m.len()).ok();
        let mut h = hasher(algo);
        let mut buf = vec![0u8; 1024 * 1024];
        let mut done: u64 = 0;
        loop {
            if let Some(token) = &cancel {
                if token.is_cancelled() {
                    return Err(Error::Cancelled);
                }
            }
            let n = file.read(&mut buf).map_err(|e| Error::io(&path, e))?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            done += n as u64;
            if let Some(p) = &progress {
                p.store(done, Ordering::Relaxed);
            }
            let _ = total;
        }
        Ok(h.finish_hex())
    });
    span.await.map_err(|e| Error::Other(format!("hash task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_specs() {
        let h = ExpectedHash::parse(
            "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        )
        .unwrap();
        assert_eq!(h.algo, HashAlgo::Sha256);
        // bare hex, length-inferred
        let h2 = ExpectedHash::parse("d41d8cd98f00b204e9800998ecf8427e").unwrap();
        assert_eq!(h2.algo, HashAlgo::Md5);
        assert!(ExpectedHash::parse("sha256:deadbeef").is_err());
        assert!(ExpectedHash::parse("nope").is_err());
    }

    #[tokio::test]
    async fn hashes_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("hello.txt");
        std::fs::write(&f, b"hello").unwrap();
        let hex = hash_file(&f, HashAlgo::Sha256, None, None).await.unwrap();
        assert_eq!(
            hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let md5 = hash_file(&f, HashAlgo::Md5, None, None).await.unwrap();
        assert_eq!(md5, "5d41402abc4b2a76b9719d911017c592");
        let sha1 = hash_file(&f, HashAlgo::Sha1, None, None).await.unwrap();
        assert_eq!(sha1, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
        let b3 = hash_file(&f, HashAlgo::Blake3, None, None).await.unwrap();
        assert_eq!(b3.len(), 64);
    }
}
