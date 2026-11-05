//! The values the [`ObjectStore`](crate::ObjectStore) trait speaks in: keys, prefixes,
//! ranges, list pages, capabilities and errors.

use std::fmt;

use crate::key_rules;

/// The key of one object inside one bucket.
///
/// A key follows S3's rules ([`key_rules::s3_client_key`]): any UTF-8 text of 1 to 1024 bytes, except
/// that a segment may not be `.` or `..` (the HTTP client would rewrite the path). The store
/// escapes the key wherever it travels.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey(String);

/// A key prefix for [`ObjectStore::list`](crate::ObjectStore::list): any text of up to 1024
/// bytes ([`key_rules::s3_prefix`]), the empty prefix included.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyPrefix(String);

/// Why a key or prefix was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The key is empty.
    #[error("an object key may not be empty")]
    Empty,
    /// The key is longer than S3 allows.
    #[error("object key has {0} bytes; at most 1024 are allowed")]
    TooLong(usize),
    /// A `/`-separated segment is `.` or `..`, which the HTTP client would rewrite.
    #[error("object key {0:?} has a `.` or `..` segment")]
    Segment(String),
}

impl ObjectKey {
    /// Checks and wraps a key.
    pub fn new(key: impl Into<String>) -> Result<Self, KeyError> {
        let key = key.into();
        key_rules::s3_client_key(&key)?;
        Ok(Self(key))
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl KeyPrefix {
    /// Checks and wraps a prefix. The empty prefix lists the whole bucket.
    pub fn new(prefix: impl Into<String>) -> Result<Self, KeyError> {
        let prefix = prefix.into();
        key_rules::s3_prefix(&prefix)?;
        Ok(Self(prefix))
    }

    /// The prefix as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key `<prefix><name>`.
    pub fn key(&self, name: &str) -> Result<ObjectKey, KeyError> {
        ObjectKey::new(format!("{}{name}", self.0))
    }

    /// The prefix `<prefix><name>`.
    pub fn child(&self, name: &str) -> Result<KeyPrefix, KeyError> {
        KeyPrefix::new(format!("{}{name}", self.0))
    }

    /// Whether `key` starts with this prefix.
    #[must_use]
    pub fn matches(&self, key: &ObjectKey) -> bool {
        key.0.starts_with(&self.0)
    }
}

/// A byte range to read: `len` bytes from `offset`.
///
/// A read returns the bytes of `[offset, offset + len)` that exist: a range running past
/// the end of the object is cut at the end, as S3 does. A range starting at or past the
/// end is [`ObjectStoreError::InvalidRange`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    offset: u64,
    len: u64,
}

impl ByteRange {
    /// The range `[offset, offset + len)`, or `None` if `len` is zero or the end
    /// overflows `u64`.
    #[must_use]
    pub fn new(offset: u64, len: u64) -> Option<Self> {
        (len > 0 && offset.checked_add(len).is_some()).then_some(Self { offset, len })
    }

    /// The first byte.
    #[must_use]
    pub fn offset(self) -> u64 {
        self.offset
    }

    /// The number of bytes asked for.
    #[must_use]
    pub fn size(self) -> u64 {
        self.len
    }

    /// The last byte asked for (inclusive), as an HTTP `Range` names it.
    #[must_use]
    pub fn last(self) -> u64 {
        self.offset + self.len - 1
    }
}

/// How many keys one [`ObjectStore::list`](crate::ObjectStore::list) call may return:
/// 1 to 1000, S3's page limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageSize(u16);

impl PageSize {
    /// The largest page S3 serves.
    pub const MAX: PageSize = PageSize(1000);

    /// A page size of `n`, or `None` outside 1 to 1000.
    #[must_use]
    pub fn new(n: u16) -> Option<Self> {
        (1..=1000).contains(&n).then_some(Self(n))
    }

    /// The size as a number.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Where the next [`ObjectStore::list`](crate::ObjectStore::list) page starts: a token a
/// store issued with a page (opaque: only the store that issued it can read it), or
/// [`ListToken::after`] a key, which every store reads. A token is a position hint, not a
/// capability: a forged one can only resume a listing at a key, or be refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListToken(pub(crate) Token);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Token {
    /// An S3 continuation token.
    Continue(String),
    /// Start after this key (S3's `start-after`). The in-memory store issues these: its
    /// next page starts after the last key of the page before.
    After(ObjectKey),
    /// Start after `after`, continuing the `FsStore` listing `listing`: its pages carry
    /// these, so the next page can be cut from that listing's kept keys. Any other
    /// store reads it as [`Token::After`].
    Kept {
        /// The listing the page came from.
        listing: u64,
        /// The last key of that page.
        after: ObjectKey,
    },
}

impl ListToken {
    /// A listing that starts after `key`: the first key it returns is the least key
    /// under the prefix greater than `key`. `key` need not exist. A rebuild that
    /// stopped part-way resumes from the last key it finished.
    #[must_use]
    pub fn after(key: &ObjectKey) -> Self {
        Self(Token::After(key.clone()))
    }

    /// The token an S3 store issues: its continuation token, opaque to every other store.
    #[must_use]
    #[doc(hidden)]
    pub fn continuation(token: impl Into<String>) -> Self {
        Self(Token::Continue(token.into()))
    }

    /// The token of a page of the local-directory store's listing number `listing`, whose
    /// last key is `after`: the next page is cut from that listing's kept keys. Any other
    /// store reads it as [`ListToken::after`] `after`.
    #[must_use]
    #[doc(hidden)]
    pub fn kept(listing: u64, after: ObjectKey) -> Self {
        Self(Token::Kept { listing, after })
    }

    /// The key a local store's listing resumes after.
    ///
    /// # Errors
    /// [`ObjectStoreError::Protocol`] for a continuation token, which only an S3 store
    /// issues and only it can read.
    #[doc(hidden)]
    pub fn local_after(&self) -> Result<&ObjectKey, ObjectStoreError> {
        match &self.0 {
            Token::After(key) | Token::Kept { after: key, .. } => Ok(key),
            Token::Continue(_) => Err(ObjectStoreError::Protocol(
                "a continuation token from another store".into(),
            )),
        }
    }

    /// The `FsStore` listing this token continues, if it came with one of its pages.
    #[must_use]
    #[doc(hidden)]
    pub fn listing(&self) -> Option<u64> {
        match &self.0 {
            Token::Kept { listing, .. } => Some(*listing),
            Token::After(_) | Token::Continue(_) => None,
        }
    }
}

/// One object in a list page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    /// The object's key.
    pub key: ObjectKey,
    /// The object's size in bytes.
    pub size: u64,
}

/// One page of a listing, in ascending key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListPage {
    /// The objects on this page.
    pub objects: Vec<ObjectInfo>,
    /// Where the next page starts, or `None` if this is the last page.
    pub next: Option<ListToken>,
}

/// What a store says it can do. The conformance suite checks every claim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// `put_new` refuses to replace an existing key ([`ObjectStoreError::AlreadyExists`]).
    /// Without it, `put_new` on an existing key replaces it; callers name keys so that this
    /// never changes the bytes, but the claim is checked when made.
    pub conditional_put: bool,
    /// Object Lock: `put_new` with a retention date makes an object that `delete`
    /// refuses until that date. Required of the store that holds `audit` and of no other.
    pub object_lock: bool,
}

/// Errors from an [`ObjectStore`](crate::ObjectStore). Cloned, so one failed write can
/// be reported to every caller waiting on it.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ObjectStoreError {
    /// No object has this key.
    #[error("object {0} not found")]
    NotFound(ObjectKey),
    /// `put_new` on a key that exists, on a store with conditional writes.
    #[error("object {0} already exists")]
    AlreadyExists(ObjectKey),
    /// The range starts at or past the end of the object.
    #[error("range {range:?} starts past the end of object {key}")]
    InvalidRange {
        /// The object read.
        key: ObjectKey,
        /// The range asked for.
        range: ByteRange,
    },
    /// The object is under Object Lock retention and may not be deleted or replaced yet.
    #[error("object {0} is under retention")]
    Locked(ObjectKey),
    /// The request needs a capability the store does not claim (for example a retention
    /// date on a store without Object Lock). Refused rather than silently weakened.
    #[error("the store does not support {0}")]
    Unsupported(&'static str),
    /// The store answered with an S3 error.
    #[error("store answered {status} {code}: {message}")]
    Service {
        /// The HTTP status.
        status: u16,
        /// The S3 error code (`AccessDenied`, `NoSuchBucket`, ...), or empty.
        code: String,
        /// The store's message.
        message: String,
    },
    /// The store answered something that breaks the S3 contract the crate relies on, such as
    /// the whole object for a ranged read. Never retried into success.
    #[error("store broke the protocol: {0}")]
    Protocol(String),
    /// The request did not complete (connect, timeout, reset). The text carries the
    /// cause and each of its own causes (`request to the store failed: error sending
    /// request for url (...): operation timed out`), so a report of the failure says
    /// which it was. The cause is in the text, not [`std::error::Error::source`], so a
    /// reader that prints the chain does not print it twice.
    #[error("request to the store failed: {}", Causes(.0.as_ref()))]
    Transport(std::sync::Arc<dyn std::error::Error + Send + Sync>),
    /// The body of a [`put_stream`](crate::ObjectStore::put_stream) failed, or gave more
    /// or fewer bytes than declared: nothing was stored. The text says which.
    #[error("the body of a streamed write failed: {0}")]
    Body(String),
}

/// An error and each of its sources, joined by `: `.
struct Causes<'a>(&'a (dyn std::error::Error + 'static));

impl fmt::Display for Causes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut next = self.0.source();
        while let Some(cause) = next {
            write!(f, ": {cause}")?;
            next = cause.source();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts S3's key rules: every text of 1 to 1024 bytes is a key (space, `+`, `%`, `&`, `<`,
    /// `?`, `#`, parentheses, non-ASCII, control characters, `//`, a leading and a trailing `/`),
    /// the empty key and a 1025-byte key are refused (by count of UTF-8 bytes, not characters),
    /// and so is a `.` or `..` segment, which the HTTP client would rewrite, but not a name that
    /// merely contains dots. Replaces the test that pinned the old narrow character set.
    #[test]
    fn keys_follow_s3_rules() {
        for good in [
            "a",
            "cas/ab/cd.seg",
            "a b",
            "a+b",
            "a%2f",
            "a&b<c>",
            "f(x)",
            "caf\u{e9}/\u{65e5}\u{672c}",
            "a?b#c",
            "tab\there\nnewline",
            "/a",
            "a/",
            "a//b",
            "...",
            "a/.b/..c/d..",
            "a/..b",
        ] {
            assert!(ObjectKey::new(good).is_ok(), "{good:?}");
        }
        assert_eq!(ObjectKey::new(""), Err(KeyError::Empty));
        for bad in ["a/./b", "a/../b", ".", "..", "./a", "a/..", "a/."] {
            assert_eq!(
                ObjectKey::new(bad),
                Err(KeyError::Segment(bad.to_owned())),
                "{bad}"
            );
        }
        assert_eq!(
            ObjectKey::new("a".repeat(1025)),
            Err(KeyError::TooLong(1025))
        );
        assert!(ObjectKey::new("a".repeat(1024)).is_ok());
        // 512 two-byte characters are 1024 bytes; one more byte is too many.
        assert!(ObjectKey::new("\u{e9}".repeat(512)).is_ok());
        assert_eq!(
            ObjectKey::new(format!("{}a", "\u{e9}".repeat(512))),
            Err(KeyError::TooLong(1025))
        );
    }

    /// Asserts that a prefix is any text of up to 1024 bytes (the empty prefix, `/`, `a//`, text
    /// with `.` segments or characters a key may hold), that a longer one is refused, that
    /// `matches` is a string prefix test (`p/` does not match `p-other`), and that `key` and
    /// `child` check what they build.
    #[test]
    fn prefixes_join_and_match() {
        let p = KeyPrefix::new("run/1/").unwrap();
        assert_eq!(p.key("x").unwrap().as_str(), "run/1/x");
        assert!(p.matches(&ObjectKey::new("run/1/x").unwrap()));
        assert!(!p.matches(&ObjectKey::new("run/10").unwrap()));
        for ok in ["", "/", "a//", "a/../", "caf\u{e9} (1)/", "a+b%"] {
            assert!(KeyPrefix::new(ok).is_ok(), "{ok:?}");
        }
        assert_eq!(
            KeyPrefix::new("a".repeat(1025)),
            Err(KeyError::TooLong(1025))
        );
        assert_eq!(p.key("../x"), Err(KeyError::Segment("run/1/../x".into())));
        assert_eq!(p.child("a b/").unwrap().as_str(), "run/1/a b/");
        assert_eq!(p.child(&"a".repeat(1024)), Err(KeyError::TooLong(1030)));
    }

    /// Catches a transport failure whose text drops its cause, or the cause's own
    /// cause: "request to the store failed" alone cannot tell a timeout from a reset.
    #[test]
    fn a_transport_failure_says_why() {
        #[derive(Debug, thiserror::Error)]
        #[error("error sending request")]
        struct Outer(#[source] std::io::Error);
        let timed_out = std::io::Error::new(std::io::ErrorKind::TimedOut, "operation timed out");
        let err = ObjectStoreError::Transport(std::sync::Arc::new(Outer(timed_out)));
        assert_eq!(
            err.to_string(),
            "request to the store failed: error sending request: operation timed out"
        );
        assert!(std::error::Error::source(&err).is_none(), "said once");
    }

    /// Catches a cause chain cut short: a connection reset sits three levels below the
    /// transport error (reqwest, hyper, then the I/O error), so a text that prints only
    /// the first cause or two loses "connection reset".
    #[test]
    fn a_transport_failure_says_its_deepest_cause() {
        #[derive(Debug, thiserror::Error)]
        #[error("error sending request")]
        struct Outer(#[source] Middle);
        #[derive(Debug, thiserror::Error)]
        #[error("client error")]
        struct Middle(#[source] std::io::Error);
        let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset");
        let err = ObjectStoreError::Transport(std::sync::Arc::new(Outer(Middle(reset))));
        assert_eq!(
            err.to_string(),
            "request to the store failed: error sending request: client error: connection reset"
        );
    }

    /// Catches: a zero-length or overflowing range, or an off-by-one in the inclusive
    /// end that an HTTP `Range` header carries.
    #[test]
    fn ranges() {
        assert_eq!(ByteRange::new(0, 0), None);
        assert_eq!(ByteRange::new(u64::MAX, 1), None);
        let r = ByteRange::new(10, 5).unwrap();
        assert_eq!((r.offset(), r.size(), r.last()), (10, 5, 14));
        assert_eq!(PageSize::new(0), None);
        assert_eq!(PageSize::new(1001), None);
        assert_eq!(PageSize::new(1000), Some(PageSize::MAX));
    }
}
