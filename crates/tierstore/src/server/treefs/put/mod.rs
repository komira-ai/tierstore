//! The staged-file store of the put directory: the core that the FUSE frontend (Linux) and the NFS
//! frontend (macOS) of `<mountpoint>/put/` will share. It has no protocol in it and no upload: a
//! file a job writes under `put/<key prefix>/` is staged here, and its commit, a `rename` of a
//! `.part` name to a final name, hands the caller the staged file, its key and its SHA-256 to put
//! upstream (the fill socket's `put` verb), and then records the answer.
//!
//! The rules, from the design of the put directory (`docs/design/jobs-on-jobscheduler-and-tierstore.md`
//! of komira-ai/kbf, sections 3, 5 and 7); the store knows nothing of what the objects hold:
//!
//! - Only a directory under one of the allowed key prefixes takes files ([`names::PutDir`]); its
//!   components are UTF-8 names of 1 to 255 bytes, never `.` or `..`, and at most
//!   [`names::MAX_DEPTH`] of them; a key is at most [`names::MAX_KEY_BYTES`] bytes.
//! - A job creates only `.part` names (create-only), writes them, and either removes one (the
//!   abort) or renames it. A rename to another `.part` name is a change of the name alone, which
//!   records the expected digest when the new name is `<dst>.sha256-<64 hex>.part`. A rename to a
//!   name that is not `.part` is the commit: it is refused while a handle of the file is open, and
//!   the bytes must hash to the expected digest of the source name, if it has one.
//! - The bytes go to a file of the stage directory named by an internal number, never by the key,
//!   so that a file system that folds case cannot merge two keys. A SHA-256 runs inline while the
//!   writes are sequential; any other write makes the digest unknown, and the commit reads the file
//!   once to hash it.
//! - What is staged, renamed, committed and removed is appended to a journal in the stage directory
//!   ([`journal`]), which a start replays ([`replay`]), so that a restarted process serves the
//!   staged files of the one before. A committed name stays visible for a while, for the job's
//!   `stat`, with no bytes.
//! - Quotas: a create is refused (`ENOSPC`) while the stage's volume has less free space than set,
//!   and (`EDQUOT`) when the directory holds as many names as allowed; a write that would take an
//!   object or a directory over its bytes is refused (`EFBIG`, `EDQUOT`). Each limit is unset by
//!   default.
//! - The sweep ([`sweep`]) removes the staged files older than the stage TTL, the committed names
//!   older than the committed TTL, and the files a start found with no journal entry, ten minutes
//!   after that start.
//!
//! [`error::PutError::errno`] is the errno a frontend answers for each refusal.

mod commit;
mod error;
mod hasher;
mod journal;
mod names;
mod replay;
mod store;
mod sweep;

#[cfg(test)]
mod error_tests;
#[cfg(test)]
mod hasher_tests;
#[cfg(test)]
mod journal_tests;
#[cfg(test)]
mod names_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod store_commit_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod sweep_tests;
#[cfg(test)]
mod testing;
