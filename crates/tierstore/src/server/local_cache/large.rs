//! The S3 front's large objects: a GET that misses goes to the fetch engine
//! (`docs/design/fetch-cache-serve.md`, section 9; section 18, row T12).
//!
//! A cache serves its objects from `obj/`, filled whole. With a [`LargeObjects`] attached
//! ([`LocalCache::route_large`]), a GET through the S3 front ([`Backend::open_read`]) of a key the
//! cache keeps, which neither `obj/` nor an entry by extents holds, asks first what the object
//! is. The answer comes from the front's book of the objects it has met, else from `LOCATE` (one
//! call of the engine's [`Upstream`]):
//! - **Small** (at most `whole_fill_limit`, [`CacheConfig::whole_limit`]): nothing changes; the
//!   read takes the path of `obj/`.
//! - **Large** (the book holds the generation, or `LOCATE` names it: every location carries one): the object is the pack `PackId = (key, ETag)` with no cell digests, and the read is
//!   served from that pack's entry through the [`Engine`], which sends the generation as
//!   `If-Match` on every part from the first: no part starts without it. The `ETag` the client is
//!   told is that generation.
//! - **Not routable**: a location without a size, or with no source. The read takes the path of
//!   `obj/`, as before.
//!
//! A ranged GET is served from the `present` cells of the pack's entry, which may be partial (a
//! `Partial` entry of section 9). A cell that is present is read at once, and the engine is not
//! asked for it. The cells of the read that are not present are ensured at `demand` class a
//! window at a time ([`AHEAD_CELLS`]) and waited for. Every cell is checked again after its read
//! (the reader protocol of 5.1), and the cells go out in order, one in memory at a time. A cell
//! that went away between its look and its read is waited for again.
//!
//! What the client is told: the object's size and `ETag` (the generation, 32 lowercase hex
//! digits) come from the same answer as the bytes, before the first byte. A definitive miss is a
//! `404`, remembered for [`MISS_TTL`] (10.6 step 4). A miss that is not definitive, an error of
//! the upstream, and a fill that fails or is lost are a `503`, and the book forgets the object, so
//! the next read asks again. A write of the key through this cache (a put, a delete, a completed
//! upload) forgets what the book knew of it too.

mod book;
mod cells;

#[cfg(test)]
mod book_tests;
#[cfg(test)]
mod cells_tests;
#[cfg(test)]
mod front_tests;
#[cfg(test)]
mod routing_tests;
#[cfg(test)]
pub(super) mod testing;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream::{self, StreamExt as _};
use tierstore_fetch_api::engine::{Engine, PackSpec};
use tierstore_fetch_api::entry::EntryStore;
use tierstore_fetch_api::ids::{HolderId, JobId, PackId, PackKey, PackRoot};
use tierstore_fetch_api::page::CELL_SHIFT;
use tierstore_fetch_api::upstream::UpstreamError;
use tierstore_fetch_api::upstream::{Located, Upstream};

use super::stream::eager;
use super::{Inner, LocalCache, locked};
use crate::{Error, Meta, Opened, Span, Window};

pub(crate) use book::KNOWN_LIMIT;
use book::{Book, Known};
use cells::Cells;

/// The bytes of a cell (1 MiB).
const CELL: u64 = 1 << CELL_SHIFT;

/// How many cells of a read are ensured ahead of the one being served: a read of a whole object
/// of many GiB asks the engine for 64 MiB at a time, so a slow client does not pull the object
/// into the cache much faster than it reads it.
pub(crate) const AHEAD_CELLS: u32 = 64;

/// How long a definitive miss is answered `404` without asking again (10.6 step 4).
pub(crate) const MISS_TTL: Duration = Duration::from_secs(5);

/// The bit every job of the S3 front has set, so that none of its jobs is a mount's (4.5: the
/// deficit round robin runs across jobs).
pub(crate) const S3_JOB: u64 = 1 << 63;

/// How many times a cell is waited for when it went away between its wait and its read.
const READ_TRIES: u32 = 3;

/// What the S3 front's large objects are served with: the engine, the upstream it reads and the
/// entry store it fills.
#[derive(Clone)]
pub struct LargeParts {
    /// The fetch engine: every part of a large object goes through it.
    pub engine: Arc<dyn Engine>,
    /// The engine's upstream: `LOCATE`.
    pub upstream: Arc<dyn Upstream>,
    /// The engine's entry store, from which the front reads the `present` cells.
    pub store: Arc<dyn EntryStore>,
    /// The holder whose hold keeps an entry while the front reads it (7).
    pub holder: HolderId,
}

/// What the large-object path did, counted.
#[derive(Default)]
pub(crate) struct LargeCounters {
    routed: AtomicU64,
    small: AtomicU64,
    unroutable: AtomicU64,
    known: AtomicU64,
    locates: AtomicU64,
    misses: AtomicU64,
    unavailable: AtomicU64,
    fill_failures: AtomicU64,
}

/// The counts of [`LargeCounters`] at one moment.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LargeCounts {
    /// Reads served through the engine.
    pub routed: u64,
    /// Reads of small objects, sent to the path of `obj/`.
    pub small: u64,
    /// Reads of an object the front could not route (a location without a size or a source).
    pub unroutable: u64,
    /// Reads whose object the book knew.
    pub known: u64,
    /// `LOCATE` calls.
    pub locates: u64,
    /// Reads answered `404` from a definitive miss, asked or remembered.
    pub misses: u64,
    /// Reads answered `503` before their first byte: a miss that is not definitive, or an error
    /// of the upstream.
    pub unavailable: u64,
    /// Reads whose fill failed or was lost while they waited.
    pub fill_failures: u64,
}

#[cfg(test)]
impl LargeCounters {
    /// The counts now.
    #[must_use]
    pub(crate) fn counts(&self) -> LargeCounts {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        LargeCounts {
            routed: get(&self.routed),
            small: get(&self.small),
            unroutable: get(&self.unroutable),
            known: get(&self.known),
            locates: get(&self.locates),
            misses: get(&self.misses),
            unavailable: get(&self.unavailable),
            fill_failures: get(&self.fill_failures),
        }
    }
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// The S3 front's path for large objects (see the module).
pub struct LargeObjects {
    parts: LargeParts,
    book: Mutex<Book>,
    jobs: AtomicU64,
    counters: LargeCounters,
}

/// What the front found an object to be.
enum Identity {
    /// Its generation and size.
    Known(Known),
    /// No size, or no source: not for the engine.
    Unroutable,
}

impl LargeObjects {
    /// The path over `parts`, with an empty book that holds at most [`KNOWN_LIMIT`] objects and
    /// as many misses.
    #[must_use]
    pub fn new(parts: LargeParts) -> Self {
        Self::with_limit(parts, KNOWN_LIMIT)
    }

    /// The path over `parts`, with a book of at most `limit` objects and as many misses (at
    /// least one of each): past it, the least recently used is forgotten.
    #[must_use]
    pub(crate) fn with_limit(parts: LargeParts, limit: usize) -> Self {
        Self {
            parts,
            book: Mutex::new(Book::new(limit)),
            jobs: AtomicU64::new(0),
            counters: LargeCounters::default(),
        }
    }

    /// What the path did.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn counters(&self) -> &LargeCounters {
        &self.counters
    }

    /// A job of its own for one read (4.5).
    fn job(&self) -> JobId {
        JobId(S3_JOB | self.jobs.fetch_add(1, Ordering::Relaxed))
    }

    /// Forgets what the book knows of `key`: its generation, and a miss.
    pub(super) fn forget(&self, key: &str) {
        locked(&self.book).forget(key);
    }

    /// What `key` is, at `now` (milliseconds of the cache's clock).
    /// Also the book's epoch before any call: what the call finds is put in the book only if no
    /// write forgot anything meanwhile.
    async fn identify(&self, key: &str, now: u64) -> Result<(Identity, u64), Error> {
        let epoch = {
            let mut book = locked(&self.book);
            let epoch = book.epoch();
            if let Some(known) = book.known(key) {
                bump(&self.counters.known);
                return Ok((Identity::Known(known), epoch));
            }
            if book.missed(key, now) {
                bump(&self.counters.misses);
                return Err(Error::NotFound(key.to_owned()));
            }
            epoch
        };
        bump(&self.counters.locates);
        let answer = self.parts.upstream.locate(&PackKey(key.to_owned())).await;
        let location = match answer {
            Ok(Located::Found(location)) => location,
            Ok(Located::DefinitiveMiss) => {
                let until = now.saturating_add(millis(MISS_TTL));
                locked(&self.book).miss_at(key, until, epoch);
                bump(&self.counters.misses);
                return Err(Error::NotFound(key.to_owned()));
            }
            Ok(Located::NotDefinitive(why)) => {
                bump(&self.counters.unavailable);
                return Err(Error::Unavailable(format!(
                    "{key}: the storage tier's answer is not definitive yet ({why:?})"
                )));
            }
            Err(error) => {
                bump(&self.counters.unavailable);
                return Err(Error::Unavailable(format!("{key}: LOCATE failed: {error}")));
            }
        };
        let Some(size) = location.size else {
            return Ok((Identity::Unroutable, epoch));
        };
        if location.sources.is_empty() {
            return Ok((Identity::Unroutable, epoch));
        }
        let known = Known {
            generation: location.generation,
            size,
            modified: now,
        };
        Ok((Identity::Known(known), epoch))
    }

    /// The read of `span` of `key` through the engine, or `None` when the object is not one for
    /// it (small, or not routable). `limit` is the cache's `whole_fill_limit`, `now` its clock.
    async fn open(
        self: &Arc<Self>,
        key: &str,
        span: Option<Span>,
        limit: u64,
        now: u64,
    ) -> Result<Option<Opened>, Error> {
        let (identity, epoch) = self.identify(key, now).await?;
        let Identity::Known(known) = identity else {
            bump(&self.counters.unroutable);
            return Ok(None);
        };
        let size = known.size;
        if size <= limit {
            bump(&self.counters.small);
            return Ok(None);
        }
        bump(&self.counters.routed);
        let window = Window::of(span, size);
        let (start, end) = match window {
            Window::Whole => (0, size),
            Window::Part(range) => (range.offset(), range.offset() + range.size()),
            Window::Unsatisfiable => (0, 0),
        };
        let job = self.job();
        locked(&self.book).learn_at(key, known.clone(), epoch);
        let body = if matches!(window, Window::Unsatisfiable) {
            stream::empty().boxed()
        } else {
            // The cells are ensured now, so the engine fetches them while the first is sent.
            let cells = Cells::new(self.clone(), known.spec(key), (start, end), job).stream();
            eager(cells).await?
        };
        Ok(Some(Opened {
            meta: known.meta(),
            window,
            body,
        }))
    }
}

/// `duration` in milliseconds.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Known {
    /// The pack of the object: `(key, ETag)` with no cell digests (section 9).
    fn spec(&self, key: &str) -> PackSpec {
        PackSpec {
            id: PackId {
                key: PackKey(key.to_owned()),
                root: PackRoot::Etag(self.generation),
            },
            size: self.size,
            digests: None,
        }
    }

    /// What the front tells the client of it.
    fn meta(&self) -> Meta {
        Meta {
            size: self.size,
            tag: self.generation.to_string(),
            modified: self.modified,
        }
    }
}

/// An upstream's error as the front's: a `503`, whatever it is. A source that does not hold the
/// object after `LOCATE` named it is a lost location (4.6), not a definitive miss (10.6 step 3),
/// so it is never a `404`.
fn upstream_error(key: &str, error: &UpstreamError) -> Error {
    Error::Unavailable(format!("{key}: {error}"))
}

impl LocalCache {
    /// Routes the S3 front's GETs of large objects that miss to `large` (see the module
    /// `large`). Once only: whether this call attached it (`false` if one was attached already).
    pub fn route_large(&self, large: Arc<LargeObjects>) -> bool {
        self.inner.large.set(large).is_ok()
    }

    /// The large-object path, if one is attached.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn large(&self) -> Option<&Arc<LargeObjects>> {
        self.inner.large.get()
    }
}

impl Inner {
    /// The read of `span` of `key` through the engine, if a large-object path is attached, the
    /// cache keeps the key and holds none of it, and the object is large; else `None`, and the
    /// read takes the path of `obj/`.
    pub(super) async fn open_large(
        &self,
        key: &str,
        span: Option<Span>,
    ) -> Result<Option<Opened>, Error> {
        let Some(large) = self.large.get() else {
            return Ok(None);
        };
        let Some(name) = self.served_name(key) else {
            return Ok(None);
        };
        {
            let index = locked(&self.index);
            if index.get(&name.hex).is_some() || index.partials.get(&name.hex).is_some() {
                return Ok(None);
            }
        }
        let now = self.clock.now();
        large.open(key, span, self.config.whole_limit, now).await
    }

    /// Forgets what the large-object path knows of `key`, if one is attached.
    pub(super) fn forget_large(&self, key: &str) {
        if let Some(large) = self.large.get() {
            large.forget(key);
        }
    }
}
