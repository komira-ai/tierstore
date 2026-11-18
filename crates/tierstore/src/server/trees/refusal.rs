//! Every refusal the design names, and the gRPC status each is answered with.
//!
//! The design (`node-local-trees.md`, sections 3.2, 5.4 and 12) gives each refusal a name and an
//! HTTP status. The status here is the code that stands for that HTTP status:
//!
//! | HTTP | Code | Reasons |
//! |---|---|---|
//! | 400 | `INVALID_ARGUMENT` | the checks of 3.2 that look at the request alone |
//! | 400, the child is created first | `FAILED_PRECONDITION` | `unknown child tree`; `mismatch`, `short` (422) |
//! | 401 | `UNAUTHENTICATED` | no key, an unknown key, a bad signature |
//! | 403 | `PERMISSION_DENIED` | a key that lacks the right |
//! | 404 | `NOT_FOUND` | `source missing`, `unknown placement` |
//! | 409 | `ALREADY_EXISTS` | `location overlaps`, `location taken`, `location not empty` |
//! | 429, 507 | `RESOURCE_EXHAUSTED` | too many placements, no room in the cache, no space |
//! | 501 | `UNIMPLEMENTED` | `TreesNotAvailable` |
//! | 502, 503 | `UNAVAILABLE` | `upstream`, `busy`, `slow down`, and `NotServing` |
//! | 504 | `DEADLINE_EXCEEDED` | `create timeout` |
//!
//! The message of a status is the name, and, when the refusal names an entry, a key or a location,
//! `: ` and that, so a caller reads which entry failed.

use std::fmt;

use tonic::{Code, Status};

/// The name of a refusal.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the refusals of the checks of the tree core, which lands in the next change; the tests of this module make each one"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    /// The request is not signed by a key of the server, or the signature is wrong.
    Unauthenticated,
    /// The key is the server's and lacks the right for the call.
    AccessDenied,
    /// `v` is not 1 (3.2).
    UnsupportedVersion,
    /// A name of an entry is empty, `.`, `..`, or has a `/` or a NUL.
    UnsupportedEntryName,
    /// A digest is not 32 bytes.
    MalformedDigest,
    /// A path or a `target` is too long.
    PathTooLong,
    /// Two entries have one path.
    DuplicatePath,
    /// A file path is also the directory prefix of another entry.
    FileAndDirectory,
    /// A source key is under a reserved prefix.
    ReservedKey,
    /// A source key is under a placement of this node.
    SourceUnderLocation,
    /// A location is not well formed.
    BadLocation,
    /// A mode is not 0 to 07777.
    BadMode,
    /// `from.offset + size` does not fit in 64 bits.
    RangeOverflow,
    /// More entries than `trees.max_entries`, or more bytes than `trees.max_bytes`.
    TreeTooLarge,
    /// A path has more names than `trees.max_depth`.
    TreeTooDeep,
    /// The request is longer than `trees.max_request_bytes`.
    RequestTooLarge,
    /// The client's own digest is not the server's.
    DigestMismatch,
    /// A `tree` entry names a tree this node does not have.
    UnknownChild,
    /// The location is, is inside or holds a placement of this node.
    LocationOverlaps,
    /// Another tree is placed at the location.
    LocationTaken,
    /// An ordinary object exists under an `s3` location.
    LocationNotEmpty,
    /// The new bytes do not fit under `local_cache.pin_limit`.
    TooLargeForCache,
    /// `trees.max_placements` placements are alive.
    TooManyPlacements,
    /// The disk is full.
    OutOfSpace,
    /// A source object is missing.
    SourceMissing,
    /// A blob's bytes do not hash to the entry's `sha256`.
    Mismatch,
    /// A source object is shorter than `offset + size`.
    Short,
    /// The upstream failed or timed out.
    Upstream,
    /// The fill pool or queue is full.
    Busy,
    /// The listener is a read-only instance (does not hold the cache directory lock).
    SlowDown,
    /// `trees.create_timeout` passed.
    CreateTimeout,
    /// No placement has this id.
    UnknownPlacement,
    /// The node has no `[local_cache]`, or `trees.enabled` is false.
    NotAvailable,
    /// The interface is up and its node has no tree core yet.
    NotServing,
}

impl Reason {
    /// The code of the status that answers the reason.
    pub(crate) const fn code(self) -> Code {
        match self {
            Self::Unauthenticated => Code::Unauthenticated,
            Self::AccessDenied => Code::PermissionDenied,
            Self::UnsupportedVersion
            | Self::UnsupportedEntryName
            | Self::MalformedDigest
            | Self::PathTooLong
            | Self::DuplicatePath
            | Self::FileAndDirectory
            | Self::ReservedKey
            | Self::SourceUnderLocation
            | Self::BadLocation
            | Self::BadMode
            | Self::RangeOverflow
            | Self::TreeTooLarge
            | Self::TreeTooDeep
            | Self::RequestTooLarge
            | Self::DigestMismatch => Code::InvalidArgument,
            Self::UnknownChild | Self::Mismatch | Self::Short => Code::FailedPrecondition,
            Self::LocationOverlaps | Self::LocationTaken | Self::LocationNotEmpty => {
                Code::AlreadyExists
            }
            Self::TooLargeForCache | Self::TooManyPlacements | Self::OutOfSpace => {
                Code::ResourceExhausted
            }
            Self::SourceMissing | Self::UnknownPlacement => Code::NotFound,
            Self::Upstream | Self::Busy | Self::SlowDown | Self::NotServing => Code::Unavailable,
            Self::CreateTimeout => Code::DeadlineExceeded,
            Self::NotAvailable => Code::Unimplemented,
        }
    }

    /// The name of the reason, as the design writes it.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::AccessDenied => "access denied",
            Self::UnsupportedVersion => "unsupported tree version",
            Self::UnsupportedEntryName => "unsupported entry name",
            Self::MalformedDigest => "malformed digest",
            Self::PathTooLong => "path too long",
            Self::DuplicatePath => "duplicate path",
            Self::FileAndDirectory => "file and directory at one path",
            Self::ReservedKey => "reserved key",
            Self::SourceUnderLocation => "source under a tree location",
            Self::BadLocation => "bad location",
            Self::BadMode => "bad mode",
            Self::RangeOverflow => "range overflow",
            Self::TreeTooLarge => "tree too large",
            Self::TreeTooDeep => "tree too deep",
            Self::RequestTooLarge => "request too large",
            Self::DigestMismatch => "tree digest mismatch",
            Self::UnknownChild => "unknown child tree",
            Self::LocationOverlaps => "location overlaps",
            Self::LocationTaken => "location taken",
            Self::LocationNotEmpty => "location not empty",
            Self::TooLargeForCache => "tree too large for the cache",
            Self::TooManyPlacements => "too many placements",
            Self::OutOfSpace => "out of space",
            Self::SourceMissing => "source missing",
            Self::Mismatch => "mismatch",
            Self::Short => "short",
            Self::Upstream => "upstream",
            Self::Busy => "busy",
            Self::SlowDown => "slow down",
            Self::CreateTimeout => "create timeout",
            Self::UnknownPlacement => "unknown placement",
            Self::NotAvailable => "TreesNotAvailable",
            Self::NotServing => "NotServing",
        }
    }
}

/// A refusal: a [`Reason`], and what it is about (an entry's path, a key, a location), if it is
/// about one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    reason: Reason,
    about: Option<String>,
}

impl Refusal {
    /// The refusal for `reason`, about nothing in particular.
    pub(crate) const fn of(reason: Reason) -> Self {
        Self {
            reason,
            about: None,
        }
    }

    /// The refusal for `reason`, about `what`.
    pub(crate) fn about(reason: Reason, what: impl Into<String>) -> Self {
        Self {
            reason,
            about: Some(what.into()),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.about {
            Some(about) => write!(f, "{}: {about}", self.reason.name()),
            None => f.write_str(self.reason.name()),
        }
    }
}

impl From<Refusal> for Status {
    fn from(refusal: Refusal) -> Self {
        Status::new(refusal.reason.code(), refusal.to_string())
    }
}
