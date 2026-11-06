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
//!   of the request.
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
//! - [`Proxy`] is a store over a hot and a cold backend that is itself a [`Backend`]. Any
//!   number of proxies (one per node, each with its own id) work over the same two backends,
//!   and none needs state for correctness. A get reads the hot tier, else the cold tier; a head
//!   does too. A put is refused if the key exists in either tier, is written to the hot tier, and
//!   is acknowledged when the journal entry of its key (below) says so. A delete removes the
//!   object from the cold tier at once and finishes on the entry. A listing merges the two tiers.
//!   When the hot tier is full the proxy writes new objects straight to the cold tier. A multipart
//!   upload is made in the hot tier whenever it has room, and in the cold tier only when it is
//!   full, part by part, never staged in the proxy: the tier is chosen when the upload is
//!   created and is named in the upload id the client holds. The object is made there only when
//!   the upload is completed (create-only: a key that exists in either tier is refused), on the
//!   entry of the key, and the reconciler copies it to the cold tier like any put. A read that finds an object in the cold tier asks
//!   for its copy into the hot tier on an entry and does not copy it itself. Writes are made
//!   only while the format object says `{"epoch": 2, "phase": "active"}`; before that they are
//!   answered with [`Error::Unavailable`] and reads are served.
//! - A proxy can heartbeat ([`Proxy::start_heartbeat`], [`Proxy::stop_heartbeat`]): it rewrites its
//!   registration `_tiering/proxies/<proxy_id>` every [`ProxyConfig::heartbeat`], and each beat the
//!   store acknowledges moves its `live_until` on, on a monotonic clock of its own, to the time
//!   the request was sent plus [`ProxyConfig::departure`] less [`ProxyConfig::deadline_margin`]
//!   ([`Proxy::is_live`]). [`Bounded`] gives the calls a holder makes a limit, and a call that
//!   destroys starts only if its [`Deadline`] is at least the margin away. The puts and deletes
//!   ask for both; no read path does.
//! - [`Entries`] are the calls on the journal entry of one key, `_entries/<sha256 of the key>` in
//!   the hot tier: a create-only create, a replace on the content tag for each [`Step`] of the
//!   entry's life, a delete, and a read-back by nonce for a write whose answer was lost. The
//!   body is an [`Entry`]. The puts, the deletes and the cold reads of a [`Proxy`] and the
//!   [`EntryReconciler`] call them. The entry is both the lock on the key while one actor
//!   changes it and the record the reconciler works from.
//! - The [`EntryReconciler`] is the one runner, guarded by a lease object in the hot tier: it
//!   holds the lease, lists `_entries/`, copies each landed put to the cold tier after it has
//!   taken the entry, finishes the deletes handed over to it, copies the cold object of each
//!   copy-back a read asked for into the hot tier, evicts the oldest copied objects from the hot
//!   tier when it is over its high-water mark, publishes the hot tier's usage and the copy lag,
//!   reaps the entries of owners that departed, and adopts hot objects that have no entry and no
//!   cold object. It does nothing until `_tiering/format` says `active`. [`ReconcilerConfig`] is
//!   the part of its settings that is the lease, the pool of copies and the water marks.
//! - [`Markers`] are the calls on the marker of one key, the object named by the key in the marks
//!   bucket of the hot tier: a create-only create ([`MarkBirth`]), a change on the content tag for
//!   each [`MarkMove`] of the transition table, a remove, a read, a wait for a lock to end, and the
//!   lease a holder keeps its lock alive by. The body is a [`Marker`]. Only the leader's work below calls
//!   them, and nothing the server runs calls that yet.
//! - [`MarkEvictor`], [`CopyBacker`] and [`StraySweep`] are the leader's eviction, copy-back and
//!   stray sweep over markers (`docs/design/put-without-lock.md`, 5.10 to 5.12), and [`WantRequests`]
//!   is the ask a proxy's cold read makes for a copy-back. [`LeaderLease`] is the lease they ask the
//!   margin of, and [`CopyRecords`] the pairs of tags the copier will record for the eviction and the
//!   sweep. Each pass reads `_tiering/format` and does nothing unless it says `active-marks`; the
//!   leader's step that calls the first three comes with the pull requests 7 and 10 of the design, and
//!   the hot-miss read that calls the ask with pull request 3.
//! - [`CachedStore`] answers repeated reads of an object's ranges from RAM in front of any
//!   backend, typically a [`Proxy`]. It is a cache of the node it runs on, not state.
//!
//! The keys under `_journal/` (the per-node journals of the releases before the entries, which
//! this release does not apply; their formats stay readable as bytes, see [`decode_journal`]), `_entries/` and `_tiering/` belong to the proxy and the
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
//! `tierstore` binary, which runs a proxy, that front and an [`EntryReconciler`] in one process
//! (`cargo build -p tierstore --features server`), over memory, directory or S3 tiers; the
//! feature turns `s3` on.
//!
//! The same feature adds an OCI distribution front (read-only unless it asks for keys), over HTTPS or over plain HTTP on
//! loopback (an `oci` interface, `server::oci`), which serves the objects under a key prefix by
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
mod lease;
#[cfg(test)]
mod lease_tests;
mod marker;
#[cfg(test)]
mod marker_tests;
mod marks;
mod marks_copy_back;
#[cfg(test)]
mod marks_copy_back_tests;
mod marks_evict;
#[cfg(test)]
mod marks_evict_tests;
mod marks_leader;
#[cfg(test)]
mod marks_leader_tests;
#[cfg(test)]
mod marks_moves_tests;
mod marks_sweep;
#[cfg(test)]
mod marks_sweep_tests;
#[cfg(test)]
mod marks_tests;
#[cfg(test)]
mod marks_wait_tests;
#[cfg(test)]
mod marks_want_tests;
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
#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "server")]
pub mod server;
mod sleeper;
#[cfg(test)]
mod sleeper_tests;
mod tiered;

pub use backend::{
    Backend, ByteRange, Listed, Meta, OpenUpload, Opened, PartBody, PutAnswer, ReadBody, Span,
    UploadedPart, UploadsAfter, Window,
};
pub use bounded::Bounded;
pub use cache::CachedStore;
pub use clock::{Clock, Mono, MonotonicClock, SystemClock};
pub use deadline::Deadline;
pub use dir::DirBackend;
pub use entry::{
    Attempt, Created, Entries, Entry, EntryOp, EntryState, Outcome, Seen, Step, Take, Unsettled,
    decode_entry, encode_entry, entry_key,
};
pub use entry_reconciler::{EntryReconciler, EntryReconcilerConfig, StepReport};
pub use error::Error;
pub use faulty::{FaultyBackend, Op};
pub use gate::{Gate, GateBackend, Phase};
pub use journal::{
    Checkpoint, JournalEntry, JournalOp, decode_checkpoint, decode_journal, encode_checkpoint,
    encode_journal,
};
pub use marker::{MARKER_VERSION, Marker, MarkerState, MarkerWhy, decode_marker, encode_marker};
pub use marks::{
    Creation, MarkBirth, MarkMove, MarkSeen, Markers, MarkersConfig, MarkersConfigError, Waited,
    Written,
};
pub use marks_copy_back::{
    ColdRead, CopiedBack, CopyBackConfig, CopyBackReport, CopyBacker, Skip, WantRequests, Wanted,
};
pub use marks_evict::{CopyRecord, CopyRecords, EvictConfig, EvictReport, Evicted, MarkEvictor};
pub use marks_leader::LeaderLease;
pub use marks_sweep::{StraySweep, SweepConfig, SweepReport, SweepTime};
pub use memory::{MemoryBackend, TestClock};
pub use reconciler::ReconcilerConfig;
#[cfg(feature = "s3")]
pub use s3::S3Backend;
pub use sleeper::{RecordingSleeper, Sleeper, TokioSleeper};
pub use tiered::{DeleteWait, Proxy, ProxyConfig, StepRetry};
