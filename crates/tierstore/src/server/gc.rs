//! The garbage-collection key: the file format of a scoped key (see `scoped`) that lists, heads
//! and deletes under one prefix. A GC key is one instance of a scoped key: the server reads a
//! `--gc-key-file` as a scoped key with a list, a delete and an abort right on the prefix of the
//! file, and no read and no write right. A key file cannot give a list or an abort right: only a GC key has them. Nothing in the server tells the two apart after the file is read.
//!
//! A server has any number of them (`--gc-key-file`, one file per key, mode 0600, the file
//! itself and owned by the user the server runs as):
//!
//! ```json
//! {"access_key_id": "pack-gc", "secret_access_key": "...", "prefix": "cas/b/"}
//! ```
//!
//! All three fields are required and no other is allowed. The prefix ends in `/` and is not
//! `_journal/` or `_tiering/` or under them or a prefix of them. The key may then do what a
//! scoped key with a list, a delete and an abort right on `[prefix]` may: ListObjectsV2 and
//! ListMultipartUploads with a `prefix` parameter inside the prefix, HeadObject, DeleteObject, and
//! AbortMultipartUpload of an upload of a key longer than the prefix. Everything else, GetObject
//! included, is `403 AccessDenied`, answered before the body is read, with the refusals and their
//! order of `scoped`. The key is refused on a presigned URL (the URL's key must be a presigning
//! key, or a key that reads).
//!
//! A delete through the proxy removes the object from the cold tier and finishes it on the entry
//! of the key, as for any key; through a cache with `write_through` it drops the cache's copy of the object and then
//! deletes it at the upstream (there is no entry). **A delete by a GC key is not create-only: it is the only way an object leaves
//! `cas/b/`**, since the presigning key can only create, and the full key is not meant for the
//! frontend.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use super::ServerConfig;
use super::keys::{AccessKey, read_private_text};
use super::presign::{PresignError, validate_prefix};
use super::scoped::Scopes;

/// The flag whose files hold the GC keys.
const FLAG: &str = "--gc-key-file";

/// A key that lists, heads and deletes under one prefix.
#[derive(Clone, PartialEq, Eq)]
pub struct GcKey {
    key: AccessKey,
    prefix: String,
    scopes: Arc<Scopes>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    access_key_id: String,
    secret_access_key: String,
    prefix: String,
}

impl GcKey {
    /// The key with this id and secret, limited to `prefix`.
    ///
    /// # Errors
    /// [`PresignError::Key`] for an empty id or secret, an id with a `/`, `,`, `=` or a character
    /// that is not printable ASCII, and a prefix that does not end in `/` or is not a valid
    /// prefix (see [`validate_prefix`]).
    pub fn new(id: &str, secret: &str, prefix: &str) -> Result<Self, PresignError> {
        check_credentials(id, secret)?;
        if validate_prefix(prefix).is_err() || !prefix.ends_with('/') {
            return Err(PresignError::Key(
                "the prefix is not a valid prefix ending in /",
            ));
        }
        Ok(Self {
            key: AccessKey::new(id, secret),
            prefix: prefix.to_owned(),
            scopes: Arc::new(Scopes::listing_and_deleting(prefix)),
        })
    }

    /// The key in the file `path`: JSON of exactly `access_key_id`, `secret_access_key` and
    /// `prefix`, mode 0600 or tighter, the file itself, owned by the user the server runs as.
    ///
    /// # Errors
    /// What is wrong, led by `flag` and the path. No message carries the secret.
    pub fn from_file(flag: &str, path: &Path) -> Result<Self, String> {
        let fail = |why: &str| format!("{flag} {}: {why}", path.display());
        let text = read_private_text(flag, path)?;
        // The error of the parser is dropped: for a value of the wrong type it quotes the value.
        let parsed = serde_json::from_str::<KeyFile>(&text);
        let file = parsed
            .map_err(|_| fail("not JSON of exactly access_key_id, secret_access_key and prefix"))?;
        Self::new(&file.access_key_id, &file.secret_access_key, &file.prefix)
            .map_err(|why| fail(&why.to_string()))
    }

    /// The id of the key.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.key.id
    }

    /// The prefix the key is limited to.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The secret of the key.
    pub(super) fn secret(&self) -> &str {
        &self.key.secret
    }

    /// The rights of the key, as a scoped key's, to hand to the role of a request.
    pub(super) fn scopes(&self) -> Arc<Scopes> {
        Arc::clone(&self.scopes)
    }
}

impl fmt::Debug for GcKey {
    /// Shows the id and the prefix and never the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcKey")
            .field("id", &self.key.id)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl ServerConfig {
    /// The GC key with this id, if the server has one.
    pub(super) fn gc_key(&self, id: &str) -> Option<&GcKey> {
        self.gc_keys.iter().find(|key| key.id() == id)
    }
}

/// The GC keys in the files of the flag `--gc-key-file`, in the order of the flags.
///
/// # Errors
/// The first file that cannot be used, as [`GcKey::from_file`] says.
pub(super) fn gc_keys(paths: &[PathBuf]) -> Result<Vec<GcKey>, String> {
    paths
        .iter()
        .map(|path| GcKey::from_file(FLAG, path))
        .collect()
}

/// The ids of the GC keys, for the check that no two keys of a server share one.
pub(super) fn ids(keys: &[GcKey]) -> Vec<&str> {
    keys.iter().map(GcKey::id).collect()
}

/// Refuses a server in which two keys have one id: a request names its key by id alone.
///
/// # Errors
/// What is wrong, naming the id (never a secret).
pub(super) fn ensure_distinct(others: &[&str], gc: &[GcKey]) -> Result<(), String> {
    ensure_ids_distinct(others, gc.iter().map(GcKey::id))
}

/// Refuses an id in `ids` that is in `others` or is given twice in `ids`.
///
/// # Errors
/// What is wrong, naming the id (never a secret).
pub(super) fn ensure_ids_distinct<'a>(
    others: &[&str],
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    let mut seen: std::collections::BTreeSet<&str> = others.iter().copied().collect();
    for id in ids {
        if !seen.insert(id) {
            return Err(format!(
                "the access key id {id:?} is used twice among the key flags"
            ));
        }
    }
    Ok(())
}

/// Refuses an empty id or secret, and an id with a `/`, `,`, `=` or a character that is not
/// printable ASCII: an id is part of a credential scope.
///
/// # Errors
/// [`PresignError::Key`].
pub(super) fn check_credentials(id: &str, secret: &str) -> Result<(), PresignError> {
    let plain = |c: char| c.is_ascii_graphic() && !"/,=".contains(c);
    if id.is_empty() || secret.is_empty() || !id.chars().all(plain) {
        return Err(PresignError::Key(
            "the id or the secret is empty, or the id is odd",
        ));
    }
    Ok(())
}
