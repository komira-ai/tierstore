//! The requests of the fill socket: one line each, parsed here and run by [`serve`](super::serve).
//!
//! ```text
//! fill <sha256> <size> from <key> @ <offset>   a blob inside the upstream object <key>
//! fill <sha256>                                the whole object cas/l/<sha256>
//! touch <sha256> [<sha256> ...]                these blobs were used now (no answer)
//! pin <mount> <tree> <sha256> [<sha256> ...]   keep these blobs for the tree of the mount
//! unpin <mount> <tree>                         take away every pin of the tree of the mount
//! pins-live <mount> [<tree> ...]               keep the pins of these trees of the mount only
//! put <key> <sha256> <size> from <path>         a create-only put of the file at <path>
//! place <mount> <path>                         where the path of the mount is among the placements
//! mounted                                      the paths of the placements' fs locations
//! flat <placement>                             the tree of a placement, flattened for the mount
//! image <placement>                            where the mount reads the tree of a placement
//! span <sha256> <size> from <pack> @ <offset>  the range of a pack that is a file of a tree
//! unplace <mount> <placement>                  the mount no longer holds the placement
//! places-live <mount> [<placement> ...]        the mount holds these placements and no other
//! started <path>                              the mount first looked into the tree at <path>
//! attached <outcome> <ms>                      the mount attached a tree on a lookup, or did not
//! fetched <cache> <filled> <failed> <ms> <n>.. where the manifests of a read of a tree came from
//! ```
//!
//! A digest is sixty-four lower-case hex digits; a number is decimal digits only; a mount and a
//! tree are each one word of at most [`MAX_NAME`] visible ASCII characters (the boot id of the
//! mount, and the digest of the tree). `<key>` is the
//! rest of the line between `from ` and the last ` @ `, so a key may hold spaces; it is not
//! under a prefix the proxy keeps for itself. The `<key>` of a `put` is one word (no spaces), with
//! the same rules, and its `<path>` is the rest of the line after `from `, an absolute path. The
//! `<path>` of a `place` is the rest of the line after the mount: names of a tree joined by `/`,
//! none at either end, the first not one of the mount's own (`trees`, `views`, `put`); so is the
//! `<path>` of a `started`, the rest of the line after the verb. A
//! `<placement>` is thirty-two lower-case hex digits. A `span` has the words of a `fill` of a blob.
//! The words of an `attached` and of a `fetched` are those of
//! [`Attach`](crate::server::local_cache::attaches::Attach) and
//! [`Fetches`](crate::server::local_cache::attaches::Fetches): an outcome (`ok`, `absent`,
//! `failed`) and a number, and four numbers and one for each bucket of the fill histogram.

use std::path::PathBuf;

use crate::TreeDigest;
use crate::journal::is_reserved;
use crate::server::local_cache::attaches::{Attach, Fetches};
use crate::server::local_cache::layout::is_digest;
use crate::server::local_cache::trees::{Location, PlacementId, placement_id};

/// The longest line a request may be, in bytes: a `touch` of about a thousand digests.
pub(super) const MAX_LINE: usize = 64 * 1024;

/// The longest name of a mount or a tree, in bytes.
const MAX_NAME: usize = 128;

/// The longest key of an S3 object, in bytes.
const MAX_KEY: usize = 1024;

/// A request that is understood.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Request {
    /// Fill the blob of `size` bytes at `offset` of the upstream object `from`.
    Blob {
        sha256: String,
        size: u64,
        from: String,
        offset: u64,
    },
    /// Fill the whole object `cas/l/<sha256>`.
    Manifest { sha256: String },
    /// Count these blobs as used now; only the digests that are digests are kept.
    Touch(Vec<String>),
    /// Pin these blobs for `tree` of `mount`.
    Pin {
        mount: String,
        tree: String,
        blobs: Vec<String>,
    },
    /// Take away every pin of `tree` of `mount`.
    Unpin { mount: String, tree: String },
    /// Keep the pins of `trees` of `mount` and take away every other pin.
    PinsLive { mount: String, trees: Vec<String> },
    /// Put the `size` bytes of the file at `path`, which are to hash to `sha256`, as the object
    /// `key`, create-only.
    Put {
        key: String,
        sha256: TreeDigest,
        size: u64,
        path: PathBuf,
    },
    /// A question of the mount about the placements of the node.
    Mount(MountRequest),
    /// The mount first looked into the tree at the fs location `path`: the start of the command
    /// of the run the path is a source of (`local_cache/starts.rs`).
    Started { path: String },
    /// The mount attached a tree on a lookup, or could not (`local_cache/attaches.rs`).
    Attached(Attach),
    /// Where the manifests of a read of a tree came from (`local_cache/attaches.rs`).
    Fetched(Fetches),
}

/// A request of the mount about the placements of the node (module `mount`).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MountRequest {
    /// Where the path `path` of the mount `mount` is among the fs locations of the placements; a
    /// placement found there is held for the mount.
    Place { mount: String, path: String },
    /// The paths of the fs locations of the placements.
    Mounted,
    /// The tree of the placement, flattened for the mount.
    Flat { placement: PlacementId },
    /// Where the mount reads the tree of the placement: the image of the tree, or the tree
    /// flattened.
    Image { placement: PlacementId },
    /// The `size` bytes at `offset` of the pack `pack`, which are to hash to `sha256`, in the
    /// cache: where they are, filled first if the cache does not hold them.
    Span {
        sha256: TreeDigest,
        size: u64,
        pack: String,
        offset: u64,
    },
    /// The mount `mount` no longer holds the placement.
    Unplace {
        mount: String,
        placement: PlacementId,
    },
    /// The mount `mount` holds the placements `keep` and no other.
    PlacesLive {
        mount: String,
        keep: Vec<PlacementId>,
    },
}

/// Why a line is not a request. The text follows `refused ` in the answer.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum Refusal {
    /// The line is not text.
    #[error("the request is not text")]
    NotText,
    /// The first word is not one the socket answers.
    #[error("unknown request")]
    Unknown,
    /// A request that has not the words of its form.
    #[error("malformed request")]
    Malformed,
    /// A digest that is not sixty-four lower-case hex digits.
    #[error("not a sha256 digest")]
    Digest,
    /// A size or an offset that is not decimal digits that fit 64 bits.
    #[error("not a number")]
    Number,
    /// A blob of no bytes: an empty file needs no fill.
    #[error("size is zero")]
    Empty,
    /// The blob ends past what 64 bits count.
    #[error("offset and size pass 2^64")]
    Overflow,
    /// A key that is empty or longer than an S3 key.
    #[error("not a usable key")]
    Key,
    /// A key under a prefix the proxy keeps for itself.
    #[error("reserved key")]
    Reserved,
    /// A mount or a tree that is empty, longer than [`MAX_NAME`] or not visible ASCII.
    #[error("not a usable name")]
    Name,
    /// A path that is not absolute.
    #[error("not an absolute path")]
    Path,
    /// A path of the mount that is not names joined by `/`, or begins with a name of the mount's
    /// own.
    #[error("not a path of the mount")]
    MountPath,
    /// A placement id that is not thirty-two lower-case hex digits.
    #[error("not a placement id")]
    Placement,
}

/// The digest `text`, or why it is not one.
fn digest(text: &str) -> Result<String, Refusal> {
    if is_digest(text) {
        Ok(text.to_owned())
    } else {
        Err(Refusal::Digest)
    }
}

/// The mount or tree name `text`, or why it is not one.
fn name(text: &str) -> Result<String, Refusal> {
    if text.is_empty() || text.len() > MAX_NAME || !text.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Refusal::Name);
    }
    Ok(text.to_owned())
}

/// The number `text` writes in decimal digits (no sign, no space).
fn number(text: &str) -> Result<u64, Refusal> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Refusal::Number);
    }
    text.parse().map_err(|_| Refusal::Number)
}

/// The request a line is, without its newline.
///
/// # Errors
/// Why the line is not a request: see [`Refusal`]. A `touch` is never refused: it is not
/// answered, so what is not a digest in it is passed over.
pub(super) fn parse(line: &[u8]) -> Result<Request, Refusal> {
    let text = std::str::from_utf8(line).map_err(|_| Refusal::NotText)?;
    let (verb, rest) = text.split_once(' ').unwrap_or((text, ""));
    match verb {
        "fill" => fill(rest),
        "touch" => Ok(Request::Touch(
            rest.split(' ')
                .filter(|word| is_digest(word))
                .map(str::to_owned)
                .collect(),
        )),
        "pin" => pin(rest),
        "put" => put(rest),
        "place" => {
            let (mount, path) = rest.split_once(' ').ok_or(Refusal::Malformed)?;
            Location::fs(path).map_err(|_| Refusal::MountPath)?;
            Ok(Request::Mount(MountRequest::Place {
                mount: name(mount)?,
                path: path.to_owned(),
            }))
        }
        "started" => {
            Location::fs(rest).map_err(|_| Refusal::MountPath)?;
            Ok(Request::Started {
                path: rest.to_owned(),
            })
        }
        "attached" => Attach::parse(rest)
            .map(Request::Attached)
            .ok_or(Refusal::Malformed),
        "fetched" => Fetches::parse(rest)
            .map(Request::Fetched)
            .ok_or(Refusal::Malformed),
        "mounted" => (text == "mounted")
            .then_some(Request::Mount(MountRequest::Mounted))
            .ok_or(Refusal::Malformed),
        "flat" => Ok(Request::Mount(MountRequest::Flat {
            placement: placement(rest)?,
        })),
        "image" => Ok(Request::Mount(MountRequest::Image {
            placement: placement(rest)?,
        })),
        "span" => {
            let (sha256, size, pack, offset) = blob(rest)?;
            Ok(Request::Mount(MountRequest::Span {
                sha256: TreeDigest::from_hex(&sha256).ok_or(Refusal::Digest)?,
                size,
                pack,
                offset,
            }))
        }
        "places-live" => {
            let (mount, ids) = rest.split_once(' ').unwrap_or((rest, ""));
            let ids = ids.split(' ').filter(|word| !word.is_empty());
            Ok(Request::Mount(MountRequest::PlacesLive {
                mount: name(mount)?,
                keep: ids.map(placement).collect::<Result<_, _>>()?,
            }))
        }
        "unplace" => {
            let (mount, id) = rest.split_once(' ').ok_or(Refusal::Malformed)?;
            Ok(Request::Mount(MountRequest::Unplace {
                mount: name(mount)?,
                placement: placement(id)?,
            }))
        }
        "unpin" => {
            let (mount, tree) = rest.split_once(' ').ok_or(Refusal::Malformed)?;
            Ok(Request::Unpin {
                mount: name(mount)?,
                tree: name(tree)?,
            })
        }
        "pins-live" => {
            let (mount, trees) = rest.split_once(' ').unwrap_or((rest, ""));
            let trees = trees.split(' ').filter(|word| !word.is_empty());
            Ok(Request::PinsLive {
                mount: name(mount)?,
                trees: trees.map(name).collect::<Result<_, _>>()?,
            })
        }
        _ => Err(Refusal::Unknown),
    }
}

/// The request `pin <rest>` is.
fn pin(rest: &str) -> Result<Request, Refusal> {
    let (mount, rest) = rest.split_once(' ').ok_or(Refusal::Malformed)?;
    let (tree, blobs) = rest.split_once(' ').ok_or(Refusal::Malformed)?;
    Ok(Request::Pin {
        mount: name(mount)?,
        tree: name(tree)?,
        blobs: blobs.split(' ').map(digest).collect::<Result<_, _>>()?,
    })
}

/// The object key `text`, or why it cannot be written: empty, longer than an S3 key, or under a
/// prefix the proxy keeps for itself.
fn key(text: &str) -> Result<String, Refusal> {
    if text.is_empty() || text.len() > MAX_KEY {
        return Err(Refusal::Key);
    }
    if is_reserved(text) {
        return Err(Refusal::Reserved);
    }
    Ok(text.to_owned())
}

/// The request `put <rest>` is.
fn put(rest: &str) -> Result<Request, Refusal> {
    let mut words = rest.splitn(4, ' ');
    let (Some(target), Some(sha256), Some(size), Some(source)) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(Refusal::Malformed);
    };
    let path = source.strip_prefix("from ").ok_or(Refusal::Malformed)?;
    let sha256 = TreeDigest::from_hex(sha256).ok_or(Refusal::Digest)?;
    let size = number(size)?;
    let key = key(target)?;
    if !path.starts_with('/') {
        return Err(Refusal::Path);
    }
    Ok(Request::Put {
        key,
        sha256,
        size,
        path: PathBuf::from(path),
    })
}

/// The placement id `text`, or why it is not one.
fn placement(text: &str) -> Result<PlacementId, Refusal> {
    placement_id(text).ok_or(Refusal::Placement)
}

/// The request `fill <rest>` is.
fn fill(rest: &str) -> Result<Request, Refusal> {
    if !rest.contains(' ') {
        return Ok(Request::Manifest {
            sha256: digest(rest)?,
        });
    }
    let (sha256, size, from, offset) = blob(rest)?;
    Ok(Request::Blob {
        sha256,
        size,
        from,
        offset,
    })
}

/// The digest, size, key and offset of `<sha256> <size> from <key> @ <offset>`.
fn blob(rest: &str) -> Result<(String, u64, String, u64), Refusal> {
    let (sha256, source) = rest.split_once(' ').ok_or(Refusal::Malformed)?;
    let sha256 = digest(sha256)?;
    let (size, source) = source.split_once(' ').ok_or(Refusal::Malformed)?;
    let source = source.strip_prefix("from ").ok_or(Refusal::Malformed)?;
    let (from, offset) = source.rsplit_once(" @ ").ok_or(Refusal::Malformed)?;
    let (size, offset) = (number(size)?, number(offset)?);
    if size == 0 {
        return Err(Refusal::Empty);
    }
    if offset.checked_add(size).is_none() {
        return Err(Refusal::Overflow);
    }
    Ok((sha256, size, key(from)?, offset))
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
