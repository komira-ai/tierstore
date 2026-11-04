//! A hot/cold object store over S3-compatible backends.
//!
//! The crate knows nothing of what is stored: objects are immutable byte strings under string
//! keys, and it has no crate of the workspace it lives in among its normal dependencies, so that
//! it can move to a repository of its own. The one exception is the feature `s3` (off by
//! default), which adds `S3Backend`, an adapter over the S3 client of `objstore-s3`; its tests
//! use two crates of the workspace as dev-dependencies, which stay behind.
//!
//! - [`Backend`] is the minimal API of an S3-compatible store: a create-only put (it fails
//!   with [`Error::AlreadyExists`] where the key exists, as `If-None-Match: *` does), a put
//!   that replaces an object only if it has a given content tag (`If-Match`, for the few
//!   small objects the crate rewrites), get with an optional byte range, head (size, content
//!   tag and time of last write), delete, and list by prefix, whole or a page at a time. Its
//!   methods are async, as the object store clients of the workspace are, and boxed so that a
//!   backend is used as `dyn Backend`. It also has the four calls of a multipart upload
//!   (create, upload a part from a stream of chunks, complete, abort), which a backend need
//!   not override: the provided ones answer [`Error::Unsupported`]. A backend whose store
//!   must know the length of a part before it is sent overrides
//!   [`Backend::upload_part_with_len`] too, which the S3 front calls with the `Content-Length`
//!   of the request. A backend can also write a reservation, an object that is marked as one
//!   ([`Meta::reservation`]): create-only like a put ([`Backend::put_reservation`]), or in place
//!   of an object with a given content tag ([`Backend::put_reservation_if_match`]); a plain
//!   [`Backend::put_if_match`] of it makes a real object. Nothing in the crate writes or reads
//!   one yet.
//! - [`MemoryBackend`] is an in-memory backend whose times come from a [`TestClock`] that a
//!   test sets by hand, and [`FaultyBackend`] wraps a backend to fail or delay chosen calls
//!   and to count them (per kind of call, and per kind and key): both are for tests, as are
//!   [`GateBackend`], which parks a chosen call until a test releases it, [`Mono`], a monotonic
//!   clock a test advances, and [`RecordingSleeper`], which records the waits a loop asks for
//!   through [`Sleeper`] and returns at once. [`DirBackend`] keeps a bucket in a directory of the
//!   local file system, parts of uploads included, which are streamed to files.
//! - `S3Backend` (feature `s3`, in the module `s3`) is a bucket of an S3-compatible store as
//!   a [`Backend`]: a create-only put is `If-None-Match: *`, a put that replaces is
//!   `If-Match`, a get is a ranged GET, a head gives the `ETag` as the content tag, and a
//!   listing follows the continuation tokens. It does the multipart calls too: the chunks of
//!   a part are sent to the store as they arrive, never collected, and a completion is
//!   create-only.
//! - [`self_test`] asks a backend the questions its guarantees rest on (a create-only put that
//!   refuses a key that exists, a replace on the right content tag only, a ranged read that
//!   gives the range, a head, a listing that pages, and on the cold tier a create-only
//!   completion) under a probe key in `_tiering/selftest/`, and names the first one it answers
//!   wrongly (a [`SelfTestError::Violation`]; a store it could not ask is
//!   [`SelfTestError::Unreachable`], and one that is full is not in doubt). The `tierstore`
//!   binary runs it on both tiers and refuses to start if one answers wrongly.
//!   [`settle_conditional_delete`] then asks a tier once how it does [`Backend::delete_if_match`]
//!   (an S3 tier: `If-Match` on the DELETE where the store enforces it, else a read of the tag
//!   and a delete) and never fails the start.
//! - [`Proxy`] is a store over a hot and a cold backend that is itself a [`Backend`]. Any
//!   number of proxies (one per node, each with its own id) work over the same two backends,
//!   and none needs state for correctness. A get reads the hot tier, else the cold tier, and
//!   copies what it found in the cold tier back into the hot tier; a head does not. A put is
//!   refused if the key exists in either tier, is written to the hot tier, and is
//!   acknowledged only after an entry for it is durable in a journal object
//!   `_journal/<proxy_id>/<seq>` in the hot tier. A delete removes the object from both tiers
//!   and journals the delete. A listing merges the two tiers. When the hot tier is full the
//!   proxy writes new objects straight to the cold tier. A multipart upload goes straight to
//!   the cold tier, part by part, never staged in the proxy: it creates the object there only
//!   when the upload is completed (create-only: a key that exists in either tier is refused),
//!   and journals nothing, as a put to the cold tier of a full hot tier does not; a read copies
//!   the object into the hot tier.
//! - A proxy can heartbeat ([`Proxy::start_heartbeat`], [`Proxy::stop_heartbeat`]): it rewrites its
//!   registration `_tiering/proxies/<proxy_id>` every [`ProxyConfig::heartbeat`], and each beat the
//!   store acknowledges moves its `live_until` on, on a monotonic clock of its own, to the time
//!   the request was sent plus [`ProxyConfig::departure`] less [`ProxyConfig::deadline_margin`]
//!   ([`Proxy::is_live`]). [`Bounded`] gives the calls a holder makes a limit, and a call that
//!   destroys starts only if its [`Deadline`] is at least the margin away. No request path asks
//!   for either yet.
//! - [`Reconciler`] is the one runner, guarded by a lease object in the hot tier. It tails
//!   the journals, copies each put from the hot tier to the cold tier (in parallel, bounded),
//!   applies the deletes, records how far it got in a checkpoint per proxy, evicts the
//!   oldest copied objects from the hot tier when it is over its high-water mark, deletes old
//!   journal objects, and publishes the hot tier's usage and the copy lag. It never lists
//!   the data prefixes of either tier: it lists `_tiering/proxies/` and the journal prefix of
//!   each proxy, and reads the journal objects, the checkpoints, the lease and the usage. It also
//!   removes the registration of a proxy that heartbeated, left no journal object and has not
//!   changed for [`ReconcilerConfig::registration_retention`] by its own monotonic clock; by
//!   default that is zero and nothing is removed.
//! - [`Entries`] are the calls on the journal entry of one key, `_entries/<sha256 of the key>` in
//!   the hot tier: a create-only create, a replace on the content tag for each [`Step`] of the
//!   entry's life, a delete, and a read-back by nonce for a write whose answer was lost. The
//!   body is an [`Entry`]. So far only the puts of a [`Proxy`] whose format is active and the
//!   [`EntryReconciler`] call them.
//! - The [`EntryReconciler`] works on the entries in place of the journals: it holds the lease,
//!   lists `_entries/`, copies each landed put to the cold tier after it has taken the entry,
//!   finishes the deletes handed over to it, reaps the entries of owners that departed, and adopts
//!   hot objects that have no entry and no cold object. It does nothing until `_tiering/format`
//!   says `active`, and no binary starts it yet.
//! - [`CachedStore`] answers repeated reads of an object's ranges from RAM in front of any
//!   backend, typically a [`Proxy`]. It is a cache of the node it runs on, not state.
//!
//! The keys under `_journal/` and `_tiering/` belong to the proxy and the
//! reconciler: a client get, head, put, delete or upload there is refused by the [`Proxy`], and
//! a listing of either tier leaves them out. The S3 front refuses each supported call on them itself,
//! before the operation's own processing (a request it refuses as unsupported, such as one with a
//! `versionId`, is answered so first); the local cache refuses them as well, since its upstream
//! may be a plain bucket.
//!
//! Every proxy, reconciler and cache takes its settings as constructor parameters.
//!
//! With the `server` feature the crate also has the `server` module, the S3 API front of a
//! [`Proxy`] (the calls of S3 clients, signed with SigV4, answered from the two tiers, with
//! request bodies and answers that must make progress and checksums that are verified), and the
//! `tierstore` binary, which runs a proxy, that front and a [`Reconciler`] in one process
//! (`cargo build -p tierstore --features server`), over memory, directory or S3 tiers; the
//! feature turns `s3` on.
//!
//! The same feature adds a read-only OCI distribution front, over HTTPS or over plain HTTP on
//! loopback (`--oci-listen`, `server::oci`), which serves the objects under a key prefix by
//! their SHA-256 digest, so that a container engine pulls an image from the tiers.

#![forbid(unsafe_code)]

mod backend;
mod bounded;
#[cfg(test)]
mod bounded_tests;
#[cfg(test)]
mod build_commit;
#[cfg(test)]
mod build_version;
mod cache;
mod clock;
#[cfg(test)]
mod clock_tests;
mod deadline;
#[cfg(test)]
mod deadline_tests;
mod dir;
mod entry;
mod entry_reconciler;
#[cfg(test)]
mod entry_tests;
mod error;
mod faulty;
#[cfg(test)]
mod faulty_tests;
mod format;
#[cfg(test)]
mod format_tests;
mod gate;
#[cfg(test)]
mod gate_tests;
mod journal;
#[cfg(test)]
mod journal_tests;
mod lease;
#[cfg(test)]
mod lease_tests;
mod memory;
mod metrics;
#[cfg(test)]
mod metrics_names_tests;
mod reconciler;
mod registration;
#[cfg(test)]
mod registration_tests;
mod registrations;
#[cfg(test)]
mod registrations_tests;
mod reservation;
#[cfg(feature = "s3")]
pub mod s3;
mod selftest;
#[cfg(test)]
mod selftest_tests;
#[cfg(feature = "server")]
pub mod server;
mod sleeper;
#[cfg(test)]
mod sleeper_tests;
mod state;
mod tiered;

pub use backend::{Backend, ByteRange, Listed, Meta, OpenUpload, PartBody, ReadBody, UploadsAfter};
pub use bounded::Bounded;
pub use cache::CachedStore;
pub use clock::{Clock, Mono, MonotonicClock, SystemClock};
pub use deadline::Deadline;
pub use dir::DirBackend;
pub use entry::{
    Created, Entries, Entry, EntryOp, EntryState, Outcome, Seen, Step, Take, decode_entry,
    encode_entry, entry_key,
};
pub use entry_reconciler::{EntryReconciler, EntryReconcilerConfig, StepReport};
pub use error::Error;
pub use faulty::{FaultyBackend, Op};
pub use gate::{Gate, GateBackend, Phase};
pub use journal::{JournalEntry, JournalOp, decode_journal, encode_journal};
pub use memory::{MemoryBackend, TestClock};
pub use reconciler::{PassReport, Reconciler, ReconcilerConfig};
#[cfg(feature = "s3")]
pub use s3::S3Backend;
pub use selftest::{
    Check, Note, SelfTestError, SelfTestFailure, SelfTestOptions, SweepBudget, Swept, self_test,
    settle_conditional_delete, sweep_probes,
};
pub use sleeper::{RecordingSleeper, Sleeper, TokioSleeper};
pub use tiered::{Proxy, ProxyConfig};
