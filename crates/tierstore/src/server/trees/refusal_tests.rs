//! The statuses of the refusals of the design.

use tonic::{Code, Status};

use super::{Reason, Refusal};

/// Every reason, with the code it is answered with and the name the design gives it.
const TABLE: [(Reason, Code, &str); 34] = [
    (
        Reason::Unauthenticated,
        Code::Unauthenticated,
        "unauthenticated",
    ),
    (
        Reason::AccessDenied,
        Code::PermissionDenied,
        "access denied",
    ),
    (
        Reason::UnsupportedVersion,
        Code::InvalidArgument,
        "unsupported tree version",
    ),
    (
        Reason::UnsupportedEntryName,
        Code::InvalidArgument,
        "unsupported entry name",
    ),
    (
        Reason::MalformedDigest,
        Code::InvalidArgument,
        "malformed digest",
    ),
    (Reason::PathTooLong, Code::InvalidArgument, "path too long"),
    (
        Reason::DuplicatePath,
        Code::InvalidArgument,
        "duplicate path",
    ),
    (
        Reason::FileAndDirectory,
        Code::InvalidArgument,
        "file and directory at one path",
    ),
    (Reason::ReservedKey, Code::InvalidArgument, "reserved key"),
    (
        Reason::SourceUnderLocation,
        Code::InvalidArgument,
        "source under a tree location",
    ),
    (Reason::BadLocation, Code::InvalidArgument, "bad location"),
    (Reason::BadMode, Code::InvalidArgument, "bad mode"),
    (
        Reason::RangeOverflow,
        Code::InvalidArgument,
        "range overflow",
    ),
    (
        Reason::TreeTooLarge,
        Code::InvalidArgument,
        "tree too large",
    ),
    (Reason::TreeTooDeep, Code::InvalidArgument, "tree too deep"),
    (
        Reason::RequestTooLarge,
        Code::InvalidArgument,
        "request too large",
    ),
    (
        Reason::DigestMismatch,
        Code::InvalidArgument,
        "tree digest mismatch",
    ),
    (
        Reason::UnknownChild,
        Code::FailedPrecondition,
        "unknown child tree",
    ),
    (
        Reason::LocationOverlaps,
        Code::AlreadyExists,
        "location overlaps",
    ),
    (Reason::LocationTaken, Code::AlreadyExists, "location taken"),
    (
        Reason::LocationNotEmpty,
        Code::AlreadyExists,
        "location not empty",
    ),
    (
        Reason::TooLargeForCache,
        Code::ResourceExhausted,
        "tree too large for the cache",
    ),
    (
        Reason::TooManyPlacements,
        Code::ResourceExhausted,
        "too many placements",
    ),
    (Reason::OutOfSpace, Code::ResourceExhausted, "out of space"),
    (Reason::SourceMissing, Code::NotFound, "source missing"),
    (Reason::Mismatch, Code::FailedPrecondition, "mismatch"),
    (Reason::Short, Code::FailedPrecondition, "short"),
    (Reason::Upstream, Code::Unavailable, "upstream"),
    (Reason::Busy, Code::Unavailable, "busy"),
    (Reason::SlowDown, Code::Unavailable, "slow down"),
    (
        Reason::CreateTimeout,
        Code::DeadlineExceeded,
        "create timeout",
    ),
    (
        Reason::UnknownPlacement,
        Code::NotFound,
        "unknown placement",
    ),
    (
        Reason::NotAvailable,
        Code::Unimplemented,
        "TreesNotAvailable",
    ),
    (Reason::NotServing, Code::Unavailable, "NotServing"),
];

/// Asserts each refusal of the design is answered with the code that stands for its HTTP status
/// and the message that is its name, exactly, and that naming an entry adds `: ` and the entry.
#[test]
fn each_refusal_is_a_status_with_its_code_and_name() {
    for (reason, code, name) in TABLE {
        let bare = Status::from(Refusal::of(reason));
        assert_eq!((bare.code(), bare.message()), (code, name), "{reason:?}");
        let named = Status::from(Refusal::about(reason, "include/a.h"));
        let message = format!("{name}: include/a.h");
        assert_eq!((named.code(), named.message()), (code, message.as_str()));
    }
}

/// Asserts the names of the refusals are all different, so that a caller can tell them apart.
#[test]
fn the_names_of_the_refusals_differ() {
    let mut names: Vec<&str> = TABLE.iter().map(|(_, _, name)| *name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), TABLE.len());
}
