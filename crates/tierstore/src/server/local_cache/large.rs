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
//! - **Large, generation known** (the book holds it, or `LOCATE` names it, as the storage tier's
//!   does): the object is the pack `PackId = (key, ETag)` with no cell digests, and the read is
//!   served from that pack's entry through the [`Engine`], which sends the generation as
//!   `If-Match` on every part from the first: no part starts without it.
//! - **Large, generation unknown** (a location without one: an object of the legacy upstream,
//!   silo): the front sends **the first part alone**, a GET of the first cells of the read with no
//!   `If-Match`. Its `ETag` is the generation; from then on the object is the pack `(key, ETag)`
//!   as above, so every later part carries `If-Match`. The first part's bytes are the head of the
//!   answer, and the rest comes from the entry. An overwrite on silo between the two is a `412` to
//!   the engine's parts: the fill fails and no byte of the other object is served.
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
mod alone_tests;
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
use tierstore_fetch_api::ids::{ByteRange as PackRange, Class, HolderId, JobId, PackId, PackKey};
use tierstore_fetch_api::ids::{PackRoot, Rank};
use tierstore_fetch_api::page::CELL_SHIFT;
use tierstore_fetch_api::upstream::UpstreamError;
use tierstore_fetch_api::upstream::{GetRequest, Located, PartStream, Source, Upstream};

use super::stream::eager;
use super::{Inner, LocalCache, locked};
use crate::{Error, Meta, Opened, ReadBody, Span, Window};

pub(crate) use book::KNOWN_LIMIT;
use book::{Book, Known};
use cells::Cells;

/// The bytes of a cell (1 MiB).
const CELL: u64 = 1 << CELL_SHIFT;

/// The most cells the first part alone asks for: the engine's largest first piece, 4 MiB (4.4).
pub(crate) const ALONE_CELLS: u64 = 4;

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
    /// The fetch engine: every part of a large object but the first part alone goes through it.
    pub engine: Arc<dyn Engine>,
    /// The engine's upstream: `LOCATE`, and the first part alone of an object with no known
    /// generation.
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
    alone: AtomicU64,
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
    /// First parts sent alone.
    pub alone: u64,
    /// Reads answered `404` from a definitive miss, asked or remembered.
    pub misses: u64,
    /// Reads answered `503` before their first byte: a miss that is not definitive, or an error
    /// of the upstream (a first part alone that its source does not hold among them).
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
            alone: get(&self.alone),
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
    /// Its size, and the source its first part alone is read from: no generation is known.
    Unpinned {
        /// The size.
        size: u64,
        /// The location's first source.
        source: Source,
    },
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
        let identity = match (location.generation, location.sources.into_iter().next()) {
            (Some(generation), _) => Identity::Known(Known {
                generation,
                size,
                modified: now,
            }),
            (None, Some(source)) => Identity::Unpinned { size, source },
            (None, None) => Identity::Unroutable,
        };
        Ok((identity, epoch))
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
        let (size, pinned) = match identity {
            Identity::Known(known) => (known.size, Ok(known)),
            Identity::Unpinned { size, source } => (size, Err(source)),
            Identity::Unroutable => {
                bump(&self.counters.unroutable);
                return Ok(None);
            }
        };
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
        let (known, head, from) = match pinned {
            Ok(known) => (known, None, start),
            Err(source) => {
                let alone = self.alone(key, (size, source), (start, end), now, job);
                let (known, head, from) = alone.await?;
                (known, Some(head), from)
            }
        };
        locked(&self.book).learn_at(key, known.clone(), epoch);
        // The rest is ensured now, so the engine fetches it while the head is sent.
        let rest = if from < end {
            Cells::new(self.clone(), known.spec(key), (from, end), job).stream()
        } else {
            stream::empty().boxed()
        };
        let body: ReadBody = match head {
            Some(head) => head.chain(rest).boxed(),
            None => rest,
        };
        let body = if matches!(window, Window::Unsatisfiable) {
            stream::empty().boxed()
        } else {
            eager(body).await?
        };
        Ok(Some(Opened {
            meta: known.meta(),
            window,
            body,
        }))
    }

    /// The first part alone of `key`, an object of `size` bytes from `source` with no known
    /// generation, for a read of `start..end` (`0..0` for none): a GET of its first cells (at most
    /// [`ALONE_CELLS`], to the end of the read) with no `If-Match`. Gives what the object is (the
    /// part's `ETag` is its generation), the part's bytes that the read wants, and where the read
    /// goes on from.
    async fn alone(
        &self,
        key: &str,
        (size, source): (u64, Source),
        (start, end): (u64, u64),
        now: u64,
        job: JobId,
    ) -> Result<(Known, ReadBody, u64), Error> {
        let first = start - start % CELL;
        // A read of no bytes (an unsatisfiable range) asks for one, for the `ETag` alone.
        let stop = if end > start {
            end.div_ceil(CELL)
                .saturating_mul(CELL)
                .min(first + ALONE_CELLS * CELL)
                .min(size)
        } else {
            first + 1
        };
        bump(&self.counters.alone);
        let request = GetRequest {
            key: PackKey(key.to_owned()),
            source,
            range: PackRange {
                offset: first,
                len: stop - first,
            },
            generation: None,
            class: Class::Demand,
            job,
            rank: Rank(0),
        };
        let response = self.parts.upstream.get(request).await.map_err(|error| {
            bump(&self.counters.unavailable);
            upstream_error(key, &error)
        })?;
        let known = Known {
            generation: response.generation,
            size,
            modified: now,
        };
        let take = stop.min(end).saturating_sub(start);
        let head = cut(key, response.body, start - first, take);
        Ok((known, head, stop.max(start)))
    }
}

/// The bytes `skip..skip + take` of a part's `body`, as the front's chunks: the read ends with an
/// error when the body fails or ends before them.
fn cut(key: &str, body: PartStream, skip: u64, take: u64) -> ReadBody {
    struct Cut {
        key: String,
        body: PartStream,
        at: u64,
        skip: u64,
        end: u64,
    }
    let cut = Cut {
        key: key.to_owned(),
        body,
        at: 0,
        skip,
        end: skip + take,
    };
    stream::unfold(Some(cut), |state| async move {
        let mut cut = state?;
        while cut.at < cut.end {
            let piece = match cut.body.next().await {
                Some(Ok(piece)) => piece,
                Some(Err(error)) => return Some((Err(upstream_error(&cut.key, &error)), None)),
                None => {
                    let text = format!("{}: the first part ended at byte {}", cut.key, cut.at);
                    return Some((Err(Error::Unavailable(text)), None));
                }
            };
            let (low, high) = (cut.at, cut.at + piece.len() as u64);
            cut.at = high;
            let (from, to) = (cut.skip.max(low), cut.end.min(high));
            if from < to {
                // Both are inside the piece, whose length is a `usize`.
                let piece = piece.slice((from - low) as usize..(to - low) as usize);
                return Some((Ok(piece), Some(cut)));
            }
        }
        None
    })
    .boxed()
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
            upload_part_size: 0,
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
