//! The fill of a range of a pack, the flights of the fills, and the move of an entry that is
//! whole into `obj/`.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read as _;
use std::os::unix::fs::{FileExt as _, PermissionsExt as _};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::future::{BoxFuture, OptionFuture, Shared};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::{ByteRange, Meta};

use super::super::fill::{Published, publish, tagged};
use super::super::layout::{Name, Origin, Sidecar, is_digest};
use super::super::metrics::{add, bump};
use super::super::{Inner, LocalCache, locked};
use super::counts::ExtentCounters;
use super::error::RangeError;
use super::files::{self, ExtentSidecar};
use super::table::Object;

/// The most chunks of one range fill asked of the upstream at once. A range of several chunks (a
/// large file's span, a group of a tree's spans) is read by that many ranged GETs at a time, taken
/// in the order of their offsets, so that one stream's rate does not bound the fill; the bytes are
/// still hashed and written in order, and a fill holds at most this many chunks in memory.
pub(super) const CHUNKS_AT_ONCE: usize = 4;

/// The task of a read of one chunk, aborted when it is dropped before it ends: a fill that stops
/// leaves no read of its running.
struct Aborting<T>(JoinHandle<T>);

impl<T> Drop for Aborting<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A range of a pack, and the SHA-256 its bytes must have.
#[derive(Clone, Copy, Debug)]
pub struct RangeWant<'a> {
    /// The key of the pack, an object of the upstream.
    pub pack: &'a str,
    /// What the caller knows of the pack (its HEAD): the size and tag the entry is made of. None
    /// to have the cache ask the upstream, once, for an entry it has none of.
    pub known: Option<&'a Meta>,
    /// The first byte of the range.
    pub offset: u64,
    /// The number of bytes, above 0.
    pub len: u64,
    /// The SHA-256 the bytes must have, as sixty-four lower-case hex digits.
    pub sha256: &'a str,
}

/// One part of a range of a pack: a span of it, and the SHA-256 its bytes must have.
#[derive(Clone, Copy, Debug)]
pub struct RangePart<'a> {
    /// The first byte of the part.
    pub offset: u64,
    /// The number of bytes, above 0.
    pub len: u64,
    /// The SHA-256 the bytes must have, as sixty-four lower-case hex digits.
    pub sha256: &'a str,
}

/// Parts of a pack to fill in one read: the bytes from the first part's start to the last
/// part's end.
#[derive(Clone, Copy, Debug)]
pub struct PartsWant<'a> {
    /// The key of the pack, an object of the upstream.
    pub pack: &'a str,
    /// What the caller knows of the pack, as for [`RangeWant::known`].
    pub known: Option<&'a Meta>,
    /// The parts, in any order; they may leave gaps between them, which are read and not kept.
    pub parts: &'a [RangePart<'a>],
}

/// What a flight is of, owned, so that its task holds it: the range read, and the parts of it
/// recorded, each if its bytes hash to its digest (a fill of one range is one part, the range).
#[derive(Clone, Debug)]
pub(super) struct Wanted {
    pub(super) key: String,
    pub(super) known: Option<Meta>,
    pub(super) start: u64,
    pub(super) end: u64,
    /// The digest of a fill of one range; for a fill of parts, `parts:` and the SHA-256 of what
    /// the parts are, so that only a fill of the same parts joins it.
    pub(super) sha256: String,
    /// `(start, end, sha256)` of each part.
    pub(super) parts: Vec<(u64, u64, String)>,
}

/// A fill of a range that is running, shared by the requests for it.
type Flight = Shared<BoxFuture<'static, Result<(), RangeError>>>;

/// The pack, the first byte, the end and the digest of a flight.
type FlightKey = (String, u64, u64, String);

/// The flights, the counters and the lock of the writes of sidecars.
#[derive(Debug, Default)]
pub(in crate::server::local_cache) struct ExtentState {
    pub(super) flights: Mutex<HashMap<FlightKey, Flight>>,
    pub counters: ExtentCounters,
    /// Held while a sidecar is written, so that the last to write has the latest ranges.
    writing: Mutex<()>,
}

impl ExtentState {
    /// Whether a fill of a range is running.
    pub(in crate::server::local_cache) fn filling(&self) -> bool {
        !locked(&self.flights).is_empty()
    }

    /// Puts a flight of a range in the table, or takes it out: a fill of a range that is running,
    /// for a test of what waits for the writes of the cache.
    #[cfg(test)]
    pub(in crate::server::local_cache) fn set_test_flight(&self, on: bool) {
        let key = ("pack".to_owned(), 0, 1, "digest".to_owned());
        let mut flights = locked(&self.flights);
        if on {
            let flight = futures::future::ready(Ok(())).boxed().shared();
            let _ = flights.insert(key, flight);
        } else {
            let _ = flights.remove(&key);
        }
    }
}

/// What a request for a range found among the flights.
enum Entered {
    /// A flight of the same range and digest is running: its end is the answer.
    Joined(Flight),
    /// A flight of an overlapping range is running: wait for it, then look again.
    Behind(Flight),
    /// This request started the flight.
    Started(Flight),
}

/// A flight's place in the table, taken out when it is dropped (the fill ended, panicked or was
/// cancelled).
struct Place {
    inner: Arc<Inner>,
    key: FlightKey,
}

impl Drop for Place {
    fn drop(&mut self) {
        let _ = locked(&self.inner.extents.flights).remove(&self.key);
    }
}

/// A fill's hold of an entry: the entry is not evicted while one is held, and an entry that has
/// no range when the last hold ends is removed.
struct Holding {
    inner: Arc<Inner>,
    hex: String,
    serial: u64,
}

impl Drop for Holding {
    fn drop(&mut self) {
        let mut index = locked(&self.inner.index);
        if index.extent_release(&self.hex, self.serial) {
            let _ = index.extent_drop(&self.hex);
            files::remove(&self.inner.config.path, &self.hex);
        }
    }
}

impl LocalCache {
    /// Keeps `[offset, offset + len)` of the pack in the cache, read from the upstream and
    /// hashed on its way to the disk, as a range of the pack's entry by extents, if the bytes
    /// hash to `sha256`. Gives at once if the range is in the cache already, and shares the fill
    /// of the same range with a request that comes while it runs.
    ///
    /// # Errors
    /// Why nothing was kept: see [`RangeError`].
    pub async fn fill_range(&self, want: &RangeWant<'_>) -> Result<(), RangeError> {
        self.inner.fill_range(want).await
    }

    /// Keeps each of the parts of the pack in the cache, read from the upstream in one pass from
    /// the first part's start to the last part's end and hashed part by part on its way to the
    /// disk: a part whose bytes hash to its digest is recorded as filled, and the bytes between
    /// the parts are read and not kept. Gives at once if every part is in the cache; a fill of the
    /// same parts that is running is joined, and one of an overlapping range is waited for.
    ///
    /// # Errors
    /// Why not every part was kept: see [`RangeError`]. [`RangeError::Mismatch`] if a part's
    /// bytes did not hash to its digest, after the parts that did are recorded.
    pub async fn fill_parts(&self, want: &PartsWant<'_>) -> Result<(), RangeError> {
        self.inner.fill_parts(want).await
    }
}

impl Inner {
    /// See [`LocalCache::fill_range`].
    async fn fill_range(self: &Arc<Self>, want: &RangeWant<'_>) -> Result<(), RangeError> {
        if !self.read_write() {
            return Err(RangeError::ReadOnly);
        }
        let name = self.cacheable(want.pack).ok_or(RangeError::NotKept)?;
        let digest = is_digest(want.sha256);
        let end = want.offset.checked_add(want.len);
        let Some(end) = end.filter(|_| want.len > 0 && digest) else {
            return Err(RangeError::Empty);
        };
        let wanted = Wanted {
            key: want.pack.to_owned(),
            known: want.known.cloned(),
            start: want.offset,
            end,
            sha256: want.sha256.to_owned(),
            parts: vec![(want.offset, end, want.sha256.to_owned())],
        };
        self.fill_wanted(&name, &wanted).await
    }

    /// See [`LocalCache::fill_parts`].
    async fn fill_parts(self: &Arc<Self>, want: &PartsWant<'_>) -> Result<(), RangeError> {
        if !self.read_write() {
            return Err(RangeError::ReadOnly);
        }
        let name = self.cacheable(want.pack).ok_or(RangeError::NotKept)?;
        let mut parts = Vec::with_capacity(want.parts.len());
        let mut whole = Sha256::new();
        for part in want.parts {
            let end = part.offset.checked_add(part.len);
            let Some(end) = end.filter(|_| part.len > 0 && is_digest(part.sha256)) else {
                return Err(RangeError::Empty);
            };
            whole.update(format!("{} {end} {}\n", part.offset, part.sha256));
            parts.push((part.offset, end, part.sha256.to_owned()));
        }
        let start = parts.iter().map(|part| part.0).min();
        let end = parts.iter().map(|part| part.1).max();
        let (Some(start), Some(end)) = (start, end) else {
            return Err(RangeError::Empty);
        };
        let wanted = Wanted {
            key: want.pack.to_owned(),
            known: want.known.cloned(),
            start,
            end,
            sha256: format!("parts:{}", hex::encode(whole.finalize())),
            parts,
        };
        self.fill_wanted(&name, &wanted).await
    }

    /// Fills what `wanted` names unless it is held: the flight of it, joined if one runs.
    async fn fill_wanted(self: &Arc<Self>, name: &Name, wanted: &Wanted) -> Result<(), RangeError> {
        loop {
            if self.wanted_held(name, wanted).await? {
                bump(&self.extents.counters.held);
                return Ok(());
            }
            match self.range_flight(name, wanted) {
                Entered::Joined(running) => {
                    bump(&self.extents.counters.joins);
                    return running.await;
                }
                Entered::Behind(running) => drop(running.await),
                Entered::Started(started) => return started.await,
            }
        }
    }

    /// Whether every part of `wanted` is in the cache, each counted as used.
    async fn wanted_held(&self, name: &Name, wanted: &Wanted) -> Result<bool, RangeError> {
        for (start, end, _) in &wanted.parts {
            if !self.range_held(name, *start, *end).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether `[start, end)` of the pack `name` is in the cache, whole in `obj/` or in an entry
    /// by extents, counted as used.
    async fn range_held(&self, name: &Name, start: u64, end: u64) -> Result<bool, RangeError> {
        let whole = locked(&self.index).get(&name.hex).map(|entry| entry.size);
        if let Some(size) = whole {
            if end > size {
                return Err(RangeError::Past);
            }
            if self.present(name).await {
                return Ok(true);
            }
        }
        let mut index = locked(&self.index);
        let Some(entry) = index.partials.get(&name.hex) else {
            return Ok(false);
        };
        if end > entry.object.size {
            return Err(RangeError::Past);
        }
        let covered = entry.map.covers(start, end);
        if covered {
            let _ = index.extent_used(&name.hex, self.clock.now());
        }
        Ok(covered)
    }

    /// The flight of the range: the one that runs, or the one this request starts. The task is
    /// spawned after the table is unlocked, with the flight's place made before, so that a spawn
    /// that drops the task at once drops the place where it does not lock the table.
    fn range_flight(self: &Arc<Self>, name: &Name, wanted: &Wanted) -> Entered {
        let key = (
            name.hex.clone(),
            wanted.start,
            wanted.end,
            wanted.sha256.clone(),
        );
        let (sender, receiver) = oneshot::channel::<JoinHandle<Result<(), RangeError>>>();
        let counting = Arc::clone(self);
        let done: Flight = async move {
            // No task is sent if the spawn is dropped at once; a task that panicked has no end.
            let task = OptionFuture::from(receiver.await.ok()).await;
            let ended = task
                .and_then(Result::ok)
                .unwrap_or(Err(RangeError::Aborted));
            if ended == Err(RangeError::Aborted) {
                counting.extents.counters.failed(&RangeError::Aborted);
            }
            ended
        }
        .boxed()
        .shared();
        {
            let mut flights = locked(&self.extents.flights);
            if let Some(running) = flights.get(&key) {
                return Entered::Joined(running.clone());
            }
            let overlaps = |(hex, start, end, _): &FlightKey| {
                *hex == name.hex && *start < wanted.end && wanted.start < *end
            };
            if let Some((_, running)) = flights.iter().find(|(other, _)| overlaps(other)) {
                return Entered::Behind(running.clone());
            }
            let _ = flights.insert(key.clone(), done.clone());
        }
        let place = Place {
            inner: Arc::clone(self),
            key,
        };
        let (inner, name, wanted) = (Arc::clone(self), name.clone(), wanted.clone());
        let task = tokio::spawn(async move {
            let _place = place;
            inner.range_fill(&name, &wanted).await
        });
        // The receiver is in the flight that is in the table, and in the caller: it is there.
        let _ = sender.send(task);
        Entered::Started(done)
    }

    /// The fill of a flight; a fill that failed is counted here, once, whoever waits.
    pub(super) async fn range_fill(
        self: &Arc<Self>,
        name: &Name,
        wanted: &Wanted,
    ) -> Result<(), RangeError> {
        let ended = self.range_carry_out(name, wanted).await;
        if let Err(why) = &ended {
            self.extents.counters.failed(why);
        }
        ended
    }

    /// The permit, the entry and the hold of it, and the fill.
    async fn range_carry_out(
        self: &Arc<Self>,
        name: &Name,
        wanted: &Wanted,
    ) -> Result<(), RangeError> {
        let Some(_permit) = self.socket_permit().await else {
            return Err(RangeError::Busy);
        };
        // A range that was filled while this waited for its permit is held.
        if self.wanted_held(name, wanted).await? {
            return Ok(());
        }
        let generation = self.generation();
        let object = self.extent_object(name, wanted).await?;
        if wanted.end > object.size {
            return Err(RangeError::Past);
        }
        // None: the pack became whole while this looked, so there is nothing to fill.
        let Some(serial) = locked(&self.index).extent_hold(&name.hex, &object) else {
            return Ok(());
        };
        let hold = Holding {
            inner: Arc::clone(self),
            hex: name.hex.clone(),
            serial,
        };
        let ended = self
            .extent_write(name, wanted, (&object, serial), generation)
            .await;
        drop(hold);
        ended
    }

    /// What the entry is made of: the object the entry has, or `known`, or the upstream's HEAD.
    /// An entry of another version than `known` says is removed, to be made again.
    async fn extent_object(&self, name: &Name, wanted: &Wanted) -> Result<Object, RangeError> {
        let held = locked(&self.index)
            .partials
            .get(&name.hex)
            .map(|entry| entry.object.clone());
        let meta = match (&wanted.known, &held) {
            (Some(known), _) => known.clone(),
            (None, Some(held)) => meta_of(held),
            (None, None) => self.upstream_head(&wanted.key).await?,
        };
        tagged(&wanted.key, &meta)?;
        if held.is_some_and(|held| !self.same_version(&meta_of(&held), &meta)) {
            self.drop_partial(&name.hex);
        }
        Ok(Object {
            key: wanted.key.clone(),
            size: meta.size,
            tag: meta.tag,
            modified: meta.modified,
        })
    }

    /// Reads the range from the upstream into the entry's file, and records it as filled if it
    /// hashes to what was asked.
    async fn extent_write(
        self: &Arc<Self>,
        name: &Name,
        wanted: &Wanted,
        (object, serial): (&Object, u64),
        generation: u64,
    ) -> Result<(), RangeError> {
        let hex = &name.hex;
        // The hold keeps the entry from being evicted; a delete that removes it is noticed at the
        // end, when the range is recorded.
        let gaps = locked(&self.index)
            .partials
            .get(hex)
            .map(|entry| entry.map.missing(wanted.start, wanted.end))
            .unwrap_or_default();
        let need: u64 = gaps.iter().map(|(start, end)| end - start).sum();
        let Some(reservation) = self.reserve(need) else {
            bump(&self.counters.fills_skipped);
            return Err(RangeError::NoSpace);
        };
        if !self.owns(generation) {
            return Err(RangeError::ReadOnly);
        }
        let file = self.extent_file(hex, object.size).await?;
        let mut hashers: Vec<Sha256> = wanted.parts.iter().map(|_| Sha256::new()).collect();
        let step = self.config.fill_chunk.max(1);
        // The chunks in order, up to `CHUNKS_AT_ONCE` asked at once, each read on a task of its
        // own so that it goes on while a chunk before it is hashed and written; a failure drops
        // the others, which aborts their reads.
        let (start, end) = (wanted.start, wanted.end);
        let starts = std::iter::successors(Some(start), |at| at.checked_add(step))
            .take_while(move |at| *at < end);
        let mut chunks = futures::stream::iter(starts)
            .map(|at| {
                let want = step.min(end - at);
                let (inner, key) = (Arc::clone(self), wanted.key.clone());
                let mut read = Aborting(tokio::spawn(async move {
                    inner.upstream_get(&key, ByteRange::new(at, want)).await
                }));
                async move {
                    // A read that panicked is the fill's panic, as it was when the fill read
                    // itself; nothing but this future aborts the task, and only by its drop.
                    let got = (&mut read.0).await.unwrap_or_else(|ended| {
                        std::panic::resume_unwind(
                            ended
                                .try_into_panic()
                                .unwrap_or_else(|e| Box::new(e.to_string())),
                        )
                    });
                    (at, want, got)
                }
            })
            .buffered(CHUNKS_AT_ONCE);
        while let Some((at, want, chunk)) = chunks.next().await {
            let chunk = chunk?;
            if chunk.len() as u64 != want {
                return Err(RangeError::Short);
            }
            if !self.owns(generation) {
                return Err(RangeError::ReadOnly);
            }
            let until = at + want;
            for ((start, end, _), hasher) in wanted.parts.iter().zip(&mut hashers) {
                let (from, to) = ((*start).max(at), (*end).min(until));
                if from < to {
                    hasher.update(&chunk[(from - at) as usize..(to - at) as usize]);
                }
            }
            let piece = (Arc::clone(&file), gaps.clone(), at, chunk);
            blocking(move || write_gaps(&piece.0, &piece.1, piece.2, &piece.3)).await?;
        }
        let checked: Vec<(u64, u64)> = wanted
            .parts
            .iter()
            .zip(hashers)
            .filter(|((_, _, sha256), hasher)| hex::encode(hasher.clone().finalize()) == *sha256)
            .map(|((start, end, _), _)| (*start, *end))
            .collect();
        if checked.is_empty() {
            return Err(RangeError::Mismatch);
        }
        let (mut added, mut whole) = (0, false);
        for part in &checked {
            let Some((more, now_whole)) = locked(&self.index).extent_add(hex, serial, *part) else {
                return Err(RangeError::Changed);
            };
            added += more;
            whole |= now_whole;
        }
        drop(reservation);
        bump(&self.extents.counters.fills);
        add(&self.extents.counters.fill_bytes, added);
        self.extent_save(hex, generation).await?;
        if whole {
            self.move_whole(name, (object, serial), generation).await;
        }
        self.relieve().await;
        if checked.len() < wanted.parts.len() {
            return Err(RangeError::Mismatch);
        }
        Ok(())
    }

    /// The data file of the entry, opened for writing, made as a sparse file of `size` bytes if
    /// it is not there.
    async fn extent_file(&self, hex: &str, size: u64) -> Result<Arc<File>, RangeError> {
        let (root, hex) = (self.config.path.clone(), hex.to_owned());
        let file = blocking(move || -> std::io::Result<File> {
            files::make(&root, &hex)?;
            let path = files::data_of(&root, &hex);
            let file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
            if file.metadata()?.len() != size {
                file.set_len(size)?;
            }
            Ok(file)
        })
        .await?;
        Ok(Arc::new(file))
    }

    /// Writes the sidecar of the entry as the index has it now, by a rename, after the data.
    pub(super) async fn extent_save(
        self: &Arc<Self>,
        hex: &str,
        generation: u64,
    ) -> Result<(), RangeError> {
        let (inner, hex) = (Arc::clone(self), hex.to_owned());
        blocking(move || -> std::io::Result<()> {
            let _writing = locked(&inner.extents.writing);
            let sidecar = {
                let index = locked(&inner.index);
                index.partials.get(&hex).map(|entry| ExtentSidecar {
                    key: entry.object.key.clone(),
                    size: entry.object.size,
                    tag: entry.object.tag.clone(),
                    modified: entry.object.modified,
                    extents: entry.map.pairs(),
                })
            };
            // An entry that is gone has no sidecar to write; neither has a cache that lost the lock.
            let Some(sidecar) = sidecar.filter(|_| inner.owns(generation)) else {
                return Ok(());
            };
            let root = &inner.config.path;
            let (new, meta) = (files::meta_new_of(root, &hex), files::meta_of(root, &hex));
            std::fs::write(&new, serde_json::to_vec(&sidecar)?)?;
            std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o644))?;
            std::fs::rename(&new, &meta)
        })
        .await
    }

    /// Moves the entry, which is filled whole, into `obj/` as the object of its key: checked
    /// against the upstream's version (and the hash a key named by its content has), hashed,
    /// hard-linked under the name `digest-v1` gives it, and taken out of `extents-v1/`. An entry
    /// that cannot be moved now stays where it is, whole, and serves its ranges.
    async fn move_whole(
        self: &Arc<Self>,
        name: &Name,
        (object, serial): (&Object, u64),
        generation: u64,
    ) {
        let hex = &name.hex;
        let checked = match self.upstream_head(&object.key).await {
            Ok(now) => self.same_version(&now, &meta_of(object)),
            Err(_) => return,
        };
        if !checked {
            self.drop_partial(hex);
            return;
        }
        let from = files::data_of(&self.config.path, hex);
        let hashing = tokio::task::spawn_blocking({
            let from = from.clone();
            move || hash_file(&from)
        });
        let Ok(Ok(digest)) = hashing.await else {
            return;
        };
        if name
            .content
            .as_ref()
            .is_some_and(|content| *content != digest)
        {
            self.drop_partial(hex);
            return;
        }
        let sidecar = Sidecar {
            key: object.key.clone(),
            size: object.size,
            sha256: digest,
            tag: object.tag.clone(),
            modified: object.modified,
            origin: Origin::Object,
        };
        let published = self.publish_whole(hex, &from, &sidecar, generation).await;
        let kept = (object.size, object.tag.clone(), object.modified);
        #[cfg(test)]
        self.run_before_insert();
        let mut index = locked(&self.index);
        // Taken out, or replaced, while the file was hashed: what was linked is not the entry's.
        let same = index
            .partials
            .get(hex)
            .is_some_and(|entry| entry.serial == serial);
        match published {
            Ok(Published::New) if same => {
                index.insert_from(hex, kept, Some(self.clock.now()), Origin::Object);
                let _ = index.extent_take(hex);
                files::remove(&self.config.path, hex);
                bump(&self.extents.counters.promotions);
            }
            Ok(Published::New) => self.remove_linked(hex),
            Ok(Published::Existed) if same => {
                let _ = index.extent_take(hex);
                files::remove(&self.config.path, hex);
            }
            Ok(Published::Existed | Published::Lost) | Err(_) => {}
        }
    }

    /// Links the file of the entry into `obj/` with its sidecar, on a thread that may block.
    async fn publish_whole(
        self: &Arc<Self>,
        hex: &str,
        from: &std::path::Path,
        sidecar: &Sidecar,
        generation: u64,
    ) -> std::io::Result<Published> {
        let (inner, hex, from) = (Arc::clone(self), hex.to_owned(), from.to_owned());
        let text = serde_json::to_vec(sidecar)?;
        let publishing = tokio::task::spawn_blocking(move || {
            // The mode of an object of `obj/`, set on the file before it has a second name.
            std::fs::set_permissions(&from, std::fs::Permissions::from_mode(0o444))?;
            let root = &inner.config.path;
            let temporary = files::meta_new_of(root, &hex);
            let owns = || inner.owns(generation);
            publish(&*inner.disk, root, &hex, (&from, &temporary), &text, &owns)
        });
        publishing.await.map_err(std::io::Error::other)?
    }

    /// Takes the entry out, with its pins, and removes its files: it is gone from the cache.
    pub(in crate::server::local_cache) fn drop_partial(&self, hex: &str) {
        let mut index = locked(&self.index);
        if index.extent_drop(hex).is_some() {
            files::remove(&self.config.path, hex);
        }
    }

    /// Takes the entry out and removes its files, leaving its pins to the whole object that
    /// replaced it.
    pub(in crate::server::local_cache) fn retire_partial(&self, hex: &str) {
        let mut index = locked(&self.index);
        if index.extent_take(hex).is_some() {
            files::remove(&self.config.path, hex);
        }
    }

    /// Removes the entry for the eviction pass unless a tree pins it or a fill writes to it;
    /// says whether it did.
    pub(in crate::server::local_cache) fn evict_partial(&self, hex: &str) -> bool {
        let mut index = locked(&self.index);
        let gone = index.extent_evict(hex).is_some();
        if gone {
            files::remove(&self.config.path, hex);
            let _ = self
                .extents
                .counters
                .evictions
                .fetch_add(1, Ordering::Relaxed);
        }
        gone
    }
}

/// Runs `job` on a thread that may block; a job that panicked is `aborted`.
async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, RangeError> {
    let done = tokio::task::spawn_blocking(job).await;
    done.map_or(Err(RangeError::Aborted), |ended| Ok(ended?))
}

/// The HEAD answer an entry's object stands for.
fn meta_of(object: &Object) -> Meta {
    Meta {
        size: object.size,
        tag: object.tag.clone(),
        modified: object.modified,
    }
}

/// Writes the parts of `chunk`, which is the bytes from `at` of the object, that lie in the
/// `gaps` (the parts of the range that no verified extent holds): a byte that a verified extent
/// holds is never written over.
fn write_gaps(file: &File, gaps: &[(u64, u64)], at: u64, chunk: &[u8]) -> std::io::Result<()> {
    let end = at + chunk.len() as u64;
    for (start, stop) in gaps {
        let (from, to) = ((*start).max(at), (*stop).min(end));
        if from < to {
            let piece = &chunk[usize::try_from(from - at).unwrap_or(0)..];
            let len = usize::try_from(to - from).unwrap_or(0);
            file.write_all_at(&piece[..len], from)?;
        }
    }
    Ok(())
}

/// The SHA-256 of the file at `path`, as lower-case hex, read once.
fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hex::encode(hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}
