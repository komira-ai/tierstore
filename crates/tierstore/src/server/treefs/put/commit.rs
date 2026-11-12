//! The commit: a rename of a `.part` name to a final one. [`StagedStore::rename`] checks the file and
//! hashes it, and hands the caller a [`Commit`]: the key, the SHA-256, the size and the staged file
//! to put upstream (the fill socket's `put` verb, create-only, with the checksum). The caller ends
//! it with the answer, [`Commit::finish`]. Until then the file can be neither written, removed,
//! swept, nor committed again, and no other file is committed to the same key; a commit that is
//! dropped unfinished leaves the file staged as it was.

use std::path::PathBuf;
use std::sync::Arc;

use super::error::PutError;
use super::hasher::hash_file;
use super::journal::Record;
use super::names::{PutDir, Sha256Digest, expected_digest};
use super::store::{Entry, StagedStore};
use crate::server::treefs::locked;

/// Why a commit is refused while a handle of its file is open.
pub(in crate::server) const HANDLE_OPEN: &str = "a handle of the file is open";

/// The answer of the upstream to the put of a commit (the `put` verb's reply).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum PutOutcome {
    /// The object was created (`{"ok":true,"created":true}`).
    Created,
    /// The key existed with the same SHA-256 (`{"ok":true,"created":false}`): a put made again
    /// after its answer was lost.
    AlreadyStored,
    /// The key exists with other bytes (`exists_different`).
    ExistsDifferent,
    /// The upstream refused the bytes for their checksum (`checksum`).
    Checksum,
    /// The upstream refused the object as too large (`toolarge`).
    TooLarge,
    /// The put did not finish by the commit deadline (`upstream`).
    Upstream,
}

/// A commit whose upload is the caller's to make.
#[derive(Debug)]
pub(in crate::server) struct Commit<'a> {
    /// The store.
    store: &'a StagedStore,
    /// The number of the staged file.
    id: u64,
    /// The directory.
    dir: PutDir,
    /// The committed name.
    name: String,
    /// `<dir>/<name>`.
    key: String,
    /// The SHA-256 of the bytes.
    sha256: Sha256Digest,
    /// The bytes.
    size: u64,
    /// Whether the commit was recorded, so that the drop leaves it.
    done: bool,
}

impl Commit<'_> {
    /// The key to put.
    pub(in crate::server) fn key(&self) -> &str {
        &self.key
    }

    /// The SHA-256 of the bytes, which the put sends as their checksum.
    pub(in crate::server) fn sha256(&self) -> Sha256Digest {
        self.sha256
    }

    /// The bytes.
    pub(in crate::server) fn size(&self) -> u64 {
        self.size
    }

    /// The staged file, which the put reads.
    pub(in crate::server) fn path(&self) -> PathBuf {
        self.store.stage_path(self.id)
    }

    /// Records the answer of the put. A created or already stored object becomes the committed
    /// name, shown with its digest and size until the committed TTL, which replaces a committed
    /// name it had; the staged file is removed. Any other answer leaves the file staged, so that
    /// the same rename can be made again.
    ///
    /// # Errors
    /// [`PutError::ExistsDifferent`], [`PutError::Checksum`], [`PutError::TooLarge`] or
    /// [`PutError::Upstream`], for those answers.
    pub(in crate::server) fn finish(mut self, outcome: PutOutcome) -> Result<(), PutError> {
        match outcome {
            PutOutcome::Created | PutOutcome::AlreadyStored => {}
            PutOutcome::ExistsDifferent => return Err(PutError::ExistsDifferent),
            PutOutcome::Checksum => return Err(PutError::Checksum),
            PutOutcome::TooLarge => return Err(PutError::TooLarge),
            PutOutcome::Upstream => return Err(PutError::Upstream),
        }
        self.done = true;
        let store = self.store;
        let mut table = locked(&store.table);
        let _ = table.take(self.id);
        if let Some(earlier) = table.id_of(&self.dir, &self.name) {
            let _ = table.take(earlier);
            table.note(&Record::Remove { id: earlier });
        }
        let ms = store.clock.now();
        table.note(&Record::Commit {
            id: self.id,
            name: self.name.clone(),
            sha256: self.sha256.to_string(),
            size: self.size,
            ms,
        });
        table.insert(
            self.id,
            Entry {
                dir: self.dir.as_str().to_owned(),
                name: self.name.clone(),
                ms,
                size: self.size,
                committed: Some(self.sha256),
            },
        );
        drop(table);
        store.remove_stage_file(self.id);
        Ok(())
    }
}

impl Drop for Commit<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = locked(&self.store.table).uploading.remove(&self.id);
        }
    }
}

impl StagedStore {
    /// Starts the commit of the staged file `src` of `dir` as `dst`, the key `key`.
    pub(super) fn begin_commit(
        &self,
        dir: &PutDir,
        src: &str,
        dst: &str,
        key: String,
    ) -> Result<Commit<'_>, PutError> {
        let (id, body, size) = {
            let mut table = locked(&self.table);
            let id = table.id_of(dir, src).ok_or(PutError::NotFound)?;
            if table.uploading.contains_key(&id) {
                return Err(PutError::Busy("a commit of the file is running"));
            }
            if table.handles.values().any(|of| *of == id) {
                return Err(PutError::Busy(HANDLE_OPEN));
            }
            if table.uploading.values().any(|of| *of == key) {
                return Err(PutError::Busy("a commit of the key is running"));
            }
            let body = Arc::clone(table.bodies.get(&id).ok_or(PutError::NotFound)?);
            let size = table.entries.get(&id).ok_or(PutError::NotFound)?.size;
            let _ = table.uploading.insert(id, key.clone());
            (id, body, size)
        };
        // From here on, a failure drops the commit, which ends the upload it marked.
        let mut commit = Commit {
            store: self,
            id,
            dir: dir.clone(),
            name: dst.to_owned(),
            key,
            sha256: Sha256Digest([0; 32]),
            size,
            done: false,
        };
        let body = locked(&body);
        let (sha256, size) = match body.hash.digest(size) {
            Some(digest) => (digest, size),
            None => hash_file(&body.file)?,
        };
        drop(body);
        commit.sha256 = sha256;
        commit.size = size;
        if let Some(expected) = expected_digest(src).filter(|expected| *expected != sha256) {
            return Err(PutError::DigestMismatch {
                expected,
                actual: sha256,
            });
        }
        if self.settings.max_object_bytes.is_some_and(|max| size > max) {
            return Err(PutError::TooLarge);
        }
        Ok(commit)
    }
}
