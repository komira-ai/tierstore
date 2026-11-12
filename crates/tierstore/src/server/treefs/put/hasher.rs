//! The SHA-256 of a staged file: inline while the writes are sequential, else read once at commit.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt as _;

use sha2::{Digest as _, Sha256};

use super::names::Sha256Digest;

/// The bytes the commit reads at a time to hash a file whose digest is not known.
const READ_CHUNK: usize = 1 << 20;

/// The SHA-256 of the bytes written from offset 0 on, one after the other, or nothing once a write
/// was not the next one.
#[derive(Clone, Default)]
pub(super) struct InlineHash {
    /// The hash of the first `hashed` bytes, while every write has been the next.
    state: Option<Sha256>,
    /// The bytes hashed.
    hashed: u64,
}

impl InlineHash {
    /// A hash of no bytes, which the writes go on from.
    pub(super) fn new() -> Self {
        Self {
            state: Some(Sha256::new()),
            hashed: 0,
        }
    }

    /// A hash that is not known: a file a start found staged.
    pub(super) fn unknown() -> Self {
        Self::default()
    }

    /// Takes in a write of `bytes` at `offset`. A write that is not at the end of what was hashed
    /// makes the digest unknown for good; a write of nothing changes nothing.
    pub(super) fn wrote(&mut self, offset: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match &mut self.state {
            Some(state) if offset == self.hashed => {
                state.update(bytes);
                self.hashed += bytes.len() as u64;
            }
            _ => self.state = None,
        }
    }

    /// The digest of a file of `size` bytes, if every one of them was hashed in order.
    pub(super) fn digest(&self, size: u64) -> Option<Sha256Digest> {
        let state = self.state.as_ref().filter(|_| self.hashed == size)?;
        Some(Sha256Digest(state.clone().finalize().into()))
    }
}

/// The SHA-256 and the length of the whole of `file`, read once from offset 0 to its end in chunks
/// of [`READ_CHUNK`] bytes: never the whole file in memory.
///
/// # Errors
/// What a read said.
pub(super) fn hash_file(file: &File) -> io::Result<(Sha256Digest, u64)> {
    let mut state = Sha256::new();
    let mut buffer = vec![0_u8; READ_CHUNK];
    let mut offset = 0_u64;
    loop {
        let read = file.read_at(&mut buffer, offset)?;
        if read == 0 {
            return Ok((Sha256Digest(state.finalize().into()), offset));
        }
        state.update(&buffer[..read]);
        offset += read as u64;
    }
}
