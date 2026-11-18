//! What a case reports when a store does not conform: [`Violation`], the calls it names
//! ([`Call`]) and the refusals a case requires ([`Refusal`]), with the checks that build them.

use std::fmt;

use objstore_s3::{ByteRange, KeyError, KeyPrefix, ObjectInfo, ObjectKey, ObjectStoreError};

use crate::Case;

/// A call of the [`objstore_s3::ObjectStore`] trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    /// `put_new`.
    PutNew,
    /// `put_stream`.
    PutStream,
    /// `get_range`.
    GetRange,
    /// `len` (HEAD).
    Len,
    /// `delete`.
    Delete,
    /// `list`.
    List,
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Call::PutNew => "put_new",
            Call::PutStream => "put_stream",
            Call::GetRange => "get_range",
            Call::Len => "len",
            Call::Delete => "delete",
            Call::List => "list",
        })
    }
}

/// A refusal a case requires of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// [`ObjectStoreError::AlreadyExists`]: a write to a used key.
    AlreadyExists,
    /// [`ObjectStoreError::NotFound`]: a read of an absent key.
    NotFound,
    /// [`ObjectStoreError::InvalidRange`]: a range that starts at or past the end.
    InvalidRange,
    /// [`ObjectStoreError::Unsupported`]: a retention date without Object Lock.
    Unsupported,
    /// [`ObjectStoreError::Body`]: a streamed body that ends short, runs long or fails.
    Body,
}

impl Refusal {
    /// Whether `error` is this refusal.
    #[must_use]
    pub fn is(self, error: &ObjectStoreError) -> bool {
        matches!(
            (self, error),
            (Refusal::AlreadyExists, ObjectStoreError::AlreadyExists(_))
                | (Refusal::NotFound, ObjectStoreError::NotFound(_))
                | (Refusal::InvalidRange, ObjectStoreError::InvalidRange { .. })
                | (Refusal::Unsupported, ObjectStoreError::Unsupported(_))
                | (Refusal::Body, ObjectStoreError::Body(_))
        )
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::AlreadyExists => "AlreadyExists",
            Refusal::NotFound => "NotFound",
            Refusal::InvalidRange => "InvalidRange",
            Refusal::Unsupported => "Unsupported",
            Refusal::Body => "Body",
        })
    }
}

/// A claim of [`objstore_s3::Capabilities`] the suite cannot run with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// The store does not claim conditional writes: it is not create-only.
    NoConditionalPut,
    /// The store claims Object Lock, which no case checks.
    ObjectLock,
}

/// How a store broke the contract, in the first place a case found it.
#[derive(Clone, Debug, thiserror::Error)]
pub enum Violation {
    /// A key or prefix the case builds under the run's root was refused (a root too long).
    #[error("{case}: a key under the root is refused: {error}")]
    Key {
        /// The case.
        case: Case,
        /// Why.
        error: KeyError,
    },
    /// The store's claims do not suit the suite.
    #[error("{case}: {}", match claim {
        Claim::NoConditionalPut => "the store does not claim conditional writes; the suite is of create-only stores",
        Claim::ObjectLock => "the store claims Object Lock, which the suite does not check",
    })]
    Claim {
        /// The case.
        case: Case,
        /// The claim.
        claim: Claim,
    },
    /// A call that had to succeed failed.
    #[error("{case}: {call} of {key} failed: {error}")]
    Failed {
        /// The case.
        case: Case,
        /// The call.
        call: Call,
        /// The key (the prefix, for a listing).
        key: String,
        /// What the store answered.
        error: ObjectStoreError,
    },
    /// A call that had to be refused succeeded.
    #[error("{case}: {call} of {key} succeeded; {want} was required")]
    Accepted {
        /// The case.
        case: Case,
        /// The call.
        call: Call,
        /// The key.
        key: ObjectKey,
        /// The refusal required.
        want: Refusal,
    },
    /// A call that had to be refused was refused for another reason.
    #[error("{case}: {call} of {key} was refused with \"{error}\"; {want} was required")]
    WrongRefusal {
        /// The case.
        case: Case,
        /// The call.
        call: Call,
        /// The key.
        key: ObjectKey,
        /// The refusal required.
        want: Refusal,
        /// What the store answered.
        error: ObjectStoreError,
    },
    /// A read gave other bytes than the key holds.
    #[error(
        "{case}: {key} [{}, {}): read {got} bytes, expected {want}{}",
        range.offset(),
        range.offset() + range.size(),
        Diff(*first_difference)
    )]
    Bytes {
        /// The case.
        case: Case,
        /// The key.
        key: ObjectKey,
        /// The range read.
        range: ByteRange,
        /// The number of bytes expected.
        want: u64,
        /// The number read.
        got: u64,
        /// The first offset (from the start of the read) where the bytes differ, if within both.
        first_difference: Option<u64>,
    },
    /// HEAD gave another length, or an object where there is none, or none where there is one.
    #[error("{case}: len of {key} is {}, expected {}", Len(*got), Len(*want))]
    Length {
        /// The case.
        case: Case,
        /// The key.
        key: ObjectKey,
        /// The length expected (`None`: no object).
        want: Option<u64>,
        /// The length answered.
        got: Option<u64>,
    },
    /// A listing of a prefix is not exactly its live keys and sizes in key order.
    #[error(
        "{case}: the listing of {:?} is [{}], expected [{}]",
        prefix.as_str(),
        Listed(got),
        Listed(want)
    )]
    Listing {
        /// The case.
        case: Case,
        /// The prefix listed.
        prefix: KeyPrefix,
        /// The live keys and sizes, in key order.
        want: Vec<ObjectInfo>,
        /// What the listing gave.
        got: Vec<ObjectInfo>,
    },
    /// A page held more keys than its `max_keys`.
    #[error("{case}: a page of {:?} holds {got} keys, more than its max_keys {max_keys}", prefix.as_str())]
    Page {
        /// The case.
        case: Case,
        /// The prefix listed.
        prefix: KeyPrefix,
        /// The page size asked for.
        max_keys: u16,
        /// The keys on the page.
        got: usize,
    },
    /// Concurrent writers of one key: more succeeded than the contract allows, or fewer than it
    /// requires even counting those answered `409`.
    #[error(
        "{case}: {created} of {writers} concurrent writers of {key} succeeded and {conflicted} were \
         answered 409, expected {want}"
    )]
    Race {
        /// The case.
        case: Case,
        /// The key.
        key: ObjectKey,
        /// The writers.
        writers: usize,
        /// The writers that succeeded.
        created: usize,
        /// The writers answered `409` (`ConditionalRequestConflict`).
        conflicted: usize,
        /// The number that had to succeed.
        want: usize,
    },
}

/// `, first difference at <n>`, if there is one.
struct Diff(Option<u64>);

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(at) => write!(f, ", first difference at {at}"),
            None => Ok(()),
        }
    }
}

/// A length, or `absent`.
struct Len(Option<u64>);

impl fmt::Display for Len {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(n) => write!(f, "{n}"),
            None => f.write_str("absent"),
        }
    }
}

/// `key (size), ...`.
struct Listed<'a>(&'a [ObjectInfo]);

impl fmt::Display for Listed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, o) in self.0.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            write!(f, "{sep}{} ({})", o.key, o.size)?;
        }
        Ok(())
    }
}

/// The violation of a call of `case` on `key` that had to succeed and failed with an error.
pub(crate) fn failed(
    case: Case,
    call: Call,
    key: &impl fmt::Display,
) -> impl FnOnce(ObjectStoreError) -> Violation {
    let key = key.to_string();
    move |error| Violation::Failed {
        case,
        call,
        key,
        error,
    }
}

/// `Ok` if `result` is the refusal `want`, else the violation.
pub(crate) fn refused<T>(
    case: Case,
    call: Call,
    key: &ObjectKey,
    want: Refusal,
    result: Result<T, ObjectStoreError>,
) -> Result<(), Violation> {
    let key = key.clone();
    match result {
        Err(error) if want.is(&error) => Ok(()),
        Err(error) => Err(Violation::WrongRefusal {
            case,
            call,
            key,
            want,
            error,
        }),
        Ok(_) => Err(Violation::Accepted {
            case,
            call,
            key,
            want,
        }),
    }
}

/// `Ok` if `got` equals `want`, else the violation (`range` is the range read).
pub(crate) fn same_bytes(
    case: Case,
    key: &ObjectKey,
    range: ByteRange,
    want: &[u8],
    got: &[u8],
) -> Result<(), Violation> {
    if want == got {
        return Ok(());
    }
    let first_difference = want.iter().zip(got).position(|(a, b)| a != b);
    Err(Violation::Bytes {
        case,
        key: key.clone(),
        range,
        want: want.len() as u64,
        got: got.len() as u64,
        first_difference: first_difference.map(|at| at as u64),
    })
}

/// `Ok` if `got` equals `want`, else the violation.
pub(crate) fn same_len(
    case: Case,
    key: &ObjectKey,
    want: Option<u64>,
    got: Option<u64>,
) -> Result<(), Violation> {
    if want == got {
        return Ok(());
    }
    Err(Violation::Length {
        case,
        key: key.clone(),
        want,
        got,
    })
}
