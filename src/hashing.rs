//! Incremental hashing that follows the manifest's declared algorithm.
//! Supports blake2b-256 (current publish default) and blake3.

use anyhow::{bail, Result};
use blake2::digest::consts::U32;
use blake2::Blake2b;
use digest::Digest;
use std::path::Path;
use tokio::io::AsyncReadExt;

pub type Blake2b256 = Blake2b<U32>;

pub enum Hasher {
    // Boxed: blake3::Hasher is large; keeps the enum small.
    Blake3(Box<blake3::Hasher>),
    Blake2b(Blake2b256),
}

impl Hasher {
    pub fn new(algo: &str) -> Result<Self> {
        match algo {
            "blake3" => Ok(Hasher::Blake3(Box::new(blake3::Hasher::new()))),
            "blake2b-256" => Ok(Hasher::Blake2b(Blake2b256::new())),
            other => bail!("unsupported hash_algo: {other}"),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Blake3(h) => {
                h.update(data);
            }
            Hasher::Blake2b(h) => Digest::update(h, data),
        }
    }

    pub fn hex(self) -> String {
        match self {
            Hasher::Blake3(h) => h.finalize().to_hex().to_string(),
            Hasher::Blake2b(h) => hex::encode(h.finalize()),
        }
    }
}

/// Hash an existing file on disk with the given algorithm.
pub async fn hash_file(path: &Path, algo: &str) -> Result<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut hasher = Hasher::new(algo)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.hex())
}
