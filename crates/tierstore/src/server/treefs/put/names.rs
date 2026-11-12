//! The names of the put directory: a directory is a key prefix under an allowed one, a file is one
//! component, and a `.part` name may carry the digest its bytes must have.

use std::fmt;

use super::error::PutError;
use crate::server::local_cache::layout::is_digest;

/// The most bytes of one component (`NAME_MAX`).
pub(in crate::server) const MAX_NAME_BYTES: usize = 255;

/// The most components of a directory.
pub(in crate::server) const MAX_DEPTH: usize = 32;

/// The most bytes of a key (the limit of S3).
pub(in crate::server) const MAX_KEY_BYTES: usize = 1024;

/// The ending of a name that is not committed.
pub(super) const PART: &str = ".part";

/// What comes before the hex of an expected digest in a `.part` name.
const DIGEST_MARK: &str = ".sha256-";

/// A SHA-256.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(in crate::server) struct Sha256Digest(pub(super) [u8; 32]);

impl Sha256Digest {
    /// The digest of 64 lower-case hex digits, or none for any other text.
    pub(super) fn from_hex(text: &str) -> Option<Self> {
        if !is_digest(text) {
            return None;
        }
        let mut bytes = [0_u8; 32];
        hex::decode_to_slice(text, &mut bytes).ok()?;
        Some(Self(bytes))
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Checks one component of a directory or a file name.
///
/// A name holds no white space and no control character: a key is one word of the one line of the
/// fill socket's `put` request, so a name that a job chooses can neither split it nor end it.
///
/// # Errors
/// [`PutError::InvalidName`] for an empty name, `.`, `..`, or a name with `/`, NUL, white space or
/// a control character in it; [`PutError::NameTooLong`] for one of more than [`MAX_NAME_BYTES`].
pub(in crate::server) fn check_component(name: &str) -> Result<(), PutError> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(PutError::InvalidName("an empty name, `.` or `..`"));
    }
    if name.bytes().any(|b| b == b'/' || b == 0) {
        return Err(PutError::InvalidName("a name with `/` or NUL in it"));
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(PutError::InvalidName(
            "a name with white space or a control character in it",
        ));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(PutError::NameTooLong);
    }
    Ok(())
}

/// A directory of the put directory: a key prefix under one of the allowed prefixes, without its
/// last `/` (`cas/b/<build>/<nonce>`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::server) struct PutDir(String);

impl PutDir {
    /// The directory `path`, checked: its components (see [`check_component`]), at most
    /// [`MAX_DEPTH`] of them, and `<path>/` at or under one of `prefixes`.
    ///
    /// # Errors
    /// [`PutError::InvalidName`] or [`PutError::NameTooLong`] for a bad component or too many;
    /// [`PutError::NotUnderPrefix`] when no prefix holds it.
    pub(in crate::server) fn new(path: &str, prefixes: &[String]) -> Result<Self, PutError> {
        let dir = Self::parse(path)?;
        let with_slash = format!("{path}/");
        if !prefixes
            .iter()
            .any(|prefix| with_slash.starts_with(prefix.as_str()))
        {
            return Err(PutError::NotUnderPrefix);
        }
        Ok(dir)
    }

    /// The directory `path` with its components checked, whatever the prefixes: a directory the
    /// journal names, which was under a prefix when it was written.
    pub(super) fn parse(path: &str) -> Result<Self, PutError> {
        let mut depth = 0;
        for component in path.split('/') {
            check_component(component)?;
            depth += 1;
        }
        if depth > MAX_DEPTH {
            return Err(PutError::InvalidName("a directory deeper than the limit"));
        }
        Ok(Self(path.to_owned()))
    }

    /// The key prefix, without its last `/`.
    pub(in crate::server) fn as_str(&self) -> &str {
        &self.0
    }

    /// The key of the file `name` of the directory.
    ///
    /// # Errors
    /// [`PutError::NameTooLong`] when the key is over [`MAX_KEY_BYTES`].
    pub(in crate::server) fn key(&self, name: &str) -> Result<String, PutError> {
        let key = format!("{}/{name}", self.0);
        if key.len() > MAX_KEY_BYTES {
            return Err(PutError::NameTooLong);
        }
        Ok(key)
    }
}

/// Checks a list of allowed prefixes: each is a directory (see [`PutDir::parse`]) followed by `/`.
///
/// # Errors
/// The first prefix that is not, by its text.
pub(in crate::server) fn check_prefixes(prefixes: &[String]) -> Result<(), String> {
    for prefix in prefixes {
        let good = prefix
            .strip_suffix('/')
            .is_some_and(|dir| PutDir::parse(dir).is_ok());
        if !good {
            return Err(prefix.clone());
        }
    }
    Ok(())
}

/// Whether `name` is a `.part` name, which a job creates and commits from.
pub(in crate::server) fn is_part(name: &str) -> bool {
    name.ends_with(PART)
}

/// The digest a `.part` name says its bytes must have: the 64 lower-case hex digits of
/// `<anything>.sha256-<hex>.part`. Any other name carries none.
pub(super) fn expected_digest(name: &str) -> Option<Sha256Digest> {
    let stem = name.strip_suffix(PART)?;
    let (_, hex) = stem.rsplit_once(DIGEST_MARK)?;
    Sha256Digest::from_hex(hex)
}
