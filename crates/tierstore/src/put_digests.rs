//! What a streamed put knows of its body before the body comes: the digests its request named
//! ([`BodyDigests`]), handed with the kind of the put ([`PutKind`]) to
//! [`Backend::put_stream_digested`](crate::Backend::put_stream_digested), which sends them for its
//! store to check; and what a backend did about the objects its store kept of puts that got no
//! answer ([`ShortObjects`], [`Backend::short_objects`](crate::Backend::short_objects)).

use crate::PutClaim;

/// The digests of a body that its request named before the body (`Content-MD5`, and the SHA-256
/// of `x-amz-content-sha256` or `x-amz-checksum-sha256`), which the front holds the body to as it
/// passes. A store that checks a body against them refuses a body that is not them, one cut short
/// included, and keeps nothing of it. None for a body whose request named none, or named one only
/// after the body (the trailer of an `aws-chunked` body).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BodyDigests {
    /// The MD5 of the body.
    pub md5: Option<[u8; 16]>,
    /// The SHA-256 of the body.
    pub sha256: Option<[u8; 32]>,
}

/// How a streamed put of [`Backend::put_stream_digested`](crate::Backend::put_stream_digested)
/// writes: as [`Backend::put_stream`](crate::Backend::put_stream),
/// [`Backend::put_stream_hot_only`](crate::Backend::put_stream_hot_only) or
/// [`Backend::put_stream_replacing`](crate::Backend::put_stream_replacing).
#[derive(Clone, Copy, Debug)]
pub enum PutKind<'a> {
    /// Create-only.
    Creates,
    /// Create-only, in the first tier of a backend with two.
    CreatesHotOnly,
    /// S3's plain `PUT`, with what the request says of its body.
    Replaces(&'a PutClaim),
}

/// What a backend over an S3 store did after its writes that failed, since it was made: about the
/// objects the store kept of its single PUTs that got no answer, one look at the key after each
/// such PUT (`objstore-s3`'s `s3/cut_put.rs`); and about the multipart uploads whose abort failed
/// (`s3/cleanup.rs`). Both run in tasks of their own, after the write has given its error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShortObjects {
    /// Objects of the wrong length the PUT had made, deleted.
    pub removed: u64,
    /// Objects of the wrong length the PUT had not made (one there before it began, or another
    /// writer's), left as they are.
    pub left: u64,
    /// Looks that could not be made or finished (in their deadline), so an object of the wrong
    /// length may have stayed.
    pub failed: u64,
    /// Looks not made: as many were running as may run at once.
    pub skipped: u64,
    /// The multipart uploads whose abort failed, kept to be aborted again when the store answers:
    /// the size of that list now, a gauge.
    pub failed_aborts: u64,
    /// Failed aborts dropped from that list (it was full, or the abort was tried every time it
    /// may be), left to the bucket's lifecycle rule.
    pub aborts_dropped: u64,
}
