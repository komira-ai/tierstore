//! An exhaustive model check of the put/delete/reconcile protocol of
//! `docs/design/put-without-lock.md` (module [`marks`]) and, as a sanity check of the model
//! itself, of today's per-key entry protocol under write-once (module [`entries`]).
//!
//! A third, independent model (module [`firststart`]) checks the first start of a hot tier that
//! has no `_tiering/format` object, as section 13.1 of the design document proposes it.
//!
//! Nothing here depends on the `tierstore` crate or on any other crate: the protocols are
//! re-stated as small state machines whose every step is one atomic store call (or one local
//! decision), explored in every interleaving by the breadth-first explorer of [`explore`] with
//! 128-bit state fingerprints (the shortest counterexample comes out first). Run it with
//! `cargo run --release -p tierstore-model -- [all|list|literal|corrected|ablation|<id>...] [-v]`.
//!
//! # What is modelled
//!
//! One key. Hot and cold are linearizable maps of version ids with exactly the capabilities
//! measured in section 2.2 of the document: create-only PUT, `If-Match` PUT, plain PUT and a
//! plain, unconditional DELETE. The marker `_tiering/marks/<K>` is a third object with the
//! same primitives; its ETag is a version that changes on every write (versions are renamed in
//! order of appearance, so states that differ only in ETag values are one state). A version
//! id stands for the bytes: copying keeps the id, so tier-independent ETags (Q4) are assumed.
//!
//! Actors: two proxies, each running a script of operations (put, create-only put, `If-Match`
//! put, put at hot `Full`, delete, get, list); an optional third proxy running the previous
//! release; one leader running a copier (with reaper, `CLEARING` finisher and copy-back), an
//! evictor and the stray sweep. Every operation is a sequence of atomic steps: a wave of
//! parallel requests is several steps in any order. A crash of any proxy or of the leader
//! (which a successor takes over at once) is possible between any two steps, up to
//! `max_crashes`. In ghost mode a crashed actor's pending call may land at any later step
//! (the stalled actors of Appendix A).
//!
//! # Properties
//!
//! A tracker ([`lin`]) keeps every order of the operations that the acknowledgements and the
//! observed truth still allow. The truth of the key (hot if present, else cold unless a `gone`
//! marker exists) is observed after every step, so a lost or undone acknowledged operation, a
//! version served after a newer acknowledgement, and a create-only or `If-Match` put that took
//! effect over the wrong version are all rejected (rules A1, A2, A5). Reads and no-effect
//! answers are checked against the values the key held while they ran. The other rules are
//! assertions on a step (A8, A9, A12, A24) or on a state (A7), terminal-state checks (A6, A11,
//! A23) and a liveness check (A4: from every state with no client operation left, the leader's
//! steps can always still reach a state with no marker and cold equal to truth; under strongly
//! fair scheduling of a finite graph that is convergence).
//!
//! # Time
//!
//! The model has no clock. Each timed rule of the document is a guard that enables a step only
//! when the requests the timing argument rules out are gone: a marker with hot absent is
//! cleared as an orphan only when no put has an object write pending; the tombstone hold passes
//! only when no put has a landed marker and a pending object, and no parallel read pair or
//! create-only wave 1 or list cold-then-marker pair is open (`read_window`); both put guards
//! are dropped in the runs that set `puts_unbounded` (a put has no size cap, so no deadline;
//! section 5.1 step 5), where the put's own re-assertion of its marker replaces them; an eviction
//! deletes hot only when no object write is pending and no create-only put that started before
//! the hot object (or, with the correction, its cold copy) is still in flight (`evict_min_age`);
//! the sweep runs only when no proxy is mid-operation (`stray_grace`); a lock is reaped only if
//! its owner is dead, and a waiting writer gives up only behind a dead owner (a live holder
//! releases within `lock_wait`); a hot DELETE that timed out is a ghost call of a live proxy that
//! the proxy outwaits before it releases its lock (`timeouts`, section 5.2). Removing a guard is
//! a wrong variant (`NoHold`, `NoOrphanGuard`, `NoEvictMinAge`, `ReleaseOnTimeout`) that must be
//! rejected.
//!
//! # Not modelled
//!
//! A second key (listing is a one-key list; a `DeleteObjects` is n independent deletes, 5.15;
//! the page boundaries of 5.8 are not modelled). The same-bytes shortcut (5.4a) and the
//! speculative one-wave put (5.1). A replayable body skipping the HEAD under the lock (the
//! model always HEADs). The wave-1 HEADs of a put at `Full` (the cold tag is read under the
//! lock). Multipart parts and presigned puts (they take the put paths). Lost answers of any
//! write but the lock create and the lock CAS. A proxy starts its next operation after the
//! release of the previous one; releases overlap with the other proxy's operations. Ghosts: one
//! pending call per crashed proxy and one for the leader. Multi-operation failures of the stores.
//!
//! # Corrections
//!
//! The runs of this check found counterexamples in the document's protocol as written. The
//! corrections in [`scenarios::FIXES`] remove all of them but one that has none (the cold
//! landing against a hot creator, scenarios `residual/...`): the revised document does not
//! remove it either, it removes the cause (a put at level `Full` no longer lands in cold; only
//! the emergency landing, when the hot store itself answers `Full`, stays, with this residual,
//! section 5.5). The scenarios `doc/...` are the counterexamples of the first draft.
//! [`scenarios::ablation`] shows each correction is needed.
//!
//! The delete of the revised document is the model's: `delete_decides_gone` is the protocol (a
//! lock written with the `gone` it found, a HEAD under the lock, one CAS that writes `gone`, the
//! hot DELETE). The marker read in wave 1 of an overwrite or a delete (5.3, 5.6) is a CAS on an
//! earlier read of the marker, which the model's lock step already allows to fail.

pub mod cli;
pub mod entries;
pub mod explore;
pub mod firststart;
pub mod lin;
pub mod marks;
pub mod scenarios;
