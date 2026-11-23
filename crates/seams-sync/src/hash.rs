//! Content hashing: blake3 over the whole file plus fixed-size blocks.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use blake3::Hasher;
use thiserror::Error;

/// Fixed block size for the current protocol generation. Larger files get more blocks,
/// so partial transfers resume block-wise.
pub const BLOCK_SIZE: u64 = 128 * 1024;

#[derive(Debug, Error)]
pub enum HashError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHash {
    pub whole: [u8; 32],
    /// One hash per 128 KiB block; the last block may be short.
    pub blocks: Vec<[u8; 32]>,
}

pub fn hash_file(path: &Path) -> Result<FileHash, HashError> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);

    let mut whole = Hasher::new();
    let mut block = Hasher::new();
    let mut blocks = Vec::new();
    let mut block_len: u64 = 0;

    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        whole.update(&buf[..read]);

        let mut rest = &buf[..read];
        while !rest.is_empty() {
            let take = (BLOCK_SIZE - block_len).min(rest.len() as u64) as usize;
            block.update(&rest[..take]);
            block_len += take as u64;
            rest = &rest[take..];

            if block_len == BLOCK_SIZE {
                blocks.push(*block.finalize().as_bytes());
                block = Hasher::new();
                block_len = 0;
            }
        }
    }

    if block_len > 0 {
        blocks.push(*block.finalize().as_bytes());
    }

    Ok(FileHash {
        whole: *whole.finalize().as_bytes(),
        blocks,
    })
}
