//! The rules a kind of store puts on the names of its objects: one function per kind, all
//! here, so that a rule is read in one place and a store of another kind (Google Cloud Storage,
//! a file system) sits beside S3's with its own functions and its own [`KeyError`] arms, not
//! behind a trait.
//!
//! # S3's rules
//!
//! [`s3_key`] and [`s3_prefix`] are the rules of Amazon S3: a key is any UTF-8 text of 1 to
//! 1024 bytes. Space, `+`, `%`, `&`, `<`, `?`, `#`, parentheses, non-ASCII text, `//`, a leading
//! or trailing `/` and the control characters are all legal, and the client escapes them (path
//! escaping for SigV4 in `s3::sigv4`, XML escaping read back in `s3::xml`). A prefix is any
//! text of up to 1024 bytes, empty included.
//!
//! One rule is narrower than S3's, and it is the client's, not the service's ([`s3_client_key`]): a segment
//! that is exactly `.` or `..` is refused. S3 accepts such keys, but the URL type of the HTTP
//! client (`reqwest`, through `url`) removes those segments from a path before the request is
//! made, whatever way they are written (`..`, `%2e%2e`), so the request would name another
//! object. Refusing the key says so; sending it would read or overwrite a neighbour.

use crate::types::KeyError;

/// Longest key S3 accepts, in bytes.
pub const S3_MAX_KEY_BYTES: usize = 1024;

/// Checks `key` against S3's rules (see the module doc): not empty, at most 1024 bytes. A `.` or
/// `..` segment is a legal S3 key, so it is legal here; [`s3_client_key`] is the stricter rule of
/// this crate's client.
///
/// # Errors
/// [`KeyError::Empty`] or [`KeyError::TooLong`].
pub fn s3_key(key: &str) -> Result<(), KeyError> {
    if key.is_empty() {
        return Err(KeyError::Empty);
    }
    s3_prefix(key)
}

/// Checks `key` against S3's rules and the one rule of this crate's S3 client (see the module
/// doc): no `.` or `..` segment. This is what [`ObjectKey::new`](crate::ObjectKey::new) applies.
///
/// # Errors
/// [`KeyError::Empty`], [`KeyError::TooLong`], or [`KeyError::Segment`].
pub fn s3_client_key(key: &str) -> Result<(), KeyError> {
    s3_key(key)?;
    if key.split('/').any(|s| s == "." || s == "..") {
        return Err(KeyError::Segment(key.to_owned()));
    }
    Ok(())
}

/// Checks `prefix` against S3's rules (see the module doc). A prefix travels in the query, so
/// it may hold `.` and `..` segments; any text that is not too long is one.
///
/// # Errors
/// [`KeyError::TooLong`].
pub fn s3_prefix(prefix: &str) -> Result<(), KeyError> {
    if prefix.len() > S3_MAX_KEY_BYTES {
        return Err(KeyError::TooLong(prefix.len()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that S3's own rule takes a `.` or `..` segment and the client's rule does not.
    #[test]
    fn only_the_client_refuses_dot_segments() {
        assert_eq!(s3_key("a/../b"), Ok(()));
        assert_eq!(
            s3_client_key("a/./b"),
            Err(KeyError::Segment("a/./b".into()))
        );
        assert_eq!(s3_client_key(""), Err(KeyError::Empty));
        assert_eq!(s3_key(&"k".repeat(1025)), Err(KeyError::TooLong(1025)));
    }
}
