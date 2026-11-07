//! The read of a key the hot tier does not have, when markers may be in force
//! (`docs/design/put-without-lock.md`, 5.7): the marker of the key is read in parallel with the
//! cold read, and the cold object is the answer only if the marker does not say the key is deleted.
//!
//! Which phases of the format read the marker is [`FormatState::reads_markers`](super::pause); in
//! the others (the entry protocol's `active`, and a store with no format object) a hot miss is
//! the cold read alone and the marks bucket is not asked. The hot tier is asked what it was before
//! this module, with one difference: the phase has to be known before the marker can be read in
//! parallel with the cold read, so a miss reads the format object (kept for `format_refresh`)
//! before the cold read. A head, a ranged read and a read of a key neither tier has did not read
//! it before; a whole-object read of a cold key read it after the cold read, to ask for a
//! copy-back on an entry.
//!
//! - **The pair.** `GET marks/K` and the cold read are started together. When both have answered,
//!   `gone = true` means the key is deleted: the hot tier is asked once more (a put may have landed
//!   since the first miss), and what it gives is the answer, or absent if it still has nothing; the
//!   cold answer is dropped. Any other marker, or none, leaves the cold answer (an object, absent,
//!   or its error) as the answer. A marker that cannot be read fails the read (the hot store is
//!   failing, and a `gone` marker may be behind it).
//! - **The window.** A pair whose two answers took longer than `read_window` between the start of
//!   both and the end of the later is dropped and read again, at most `read_retries` times more,
//!   and then the read is [`Error::Unavailable`]: the copier clears a `gone` marker only
//!   `tombstone_hold` after the cold delete, and a pair no longer than `read_window` cannot pair a
//!   cold answer that was current before a delete with a marker read after the marker was cleared.
//! - **The ask for a copy-back.** A whole-object read that the cold tier answered, for a key with
//!   no marker, in the phase `active-marks` at epoch 3 or later with the marks bucket this proxy has
//!   (`ProxyConfig::marks_name`, the condition of a put), asks for `WANT` (M10): a create-only write in a task
//!   of its own, below the `Critical` level, for an object of at most `copy_back_max_bytes`, and
//!   at most once in `want_retry` for each key from this proxy, from a set of at most
//!   `want_recent_limit` keys kept in memory, so that a cold-only key read a thousand times a second
//!   makes one create a minute and the set makes no store request. Its failure is ignored.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{Inner, locked};
use crate::deadline::millis;
use crate::{Error, MarkBirth, Markers};

/// The settings of the reads and listings that read markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkReads {
    /// `read_window`: the longest a hot-miss pair, or the cold-then-marker reads of a listing page,
    /// may take before it is read again (5 s). Below `tombstone_hold`.
    pub read_window: Duration,
    /// `read_retries`: how many times such a read is read again before it is answered
    /// [`Error::Unavailable`] (3).
    pub read_retries: u32,
    /// `list_marker_pages`: how many more pages of markers one round of a listing reads to cover
    /// the page of keys it merged (4).
    pub list_marker_pages: u32,
    /// `want_retry`: how long a proxy waits before it asks again for the copy-back of a key (60 s).
    pub want_retry: Duration,
    /// `want_recent_limit`: how many keys the proxy remembers it asked for (100000).
    pub want_recent_limit: usize,
    /// `copy_back_max_bytes`: the largest object a read asks to have copied back (64 MiB).
    pub copy_back_max_bytes: u64,
}

impl Default for MarkReads {
    fn default() -> Self {
        Self {
            read_window: Duration::from_secs(5),
            read_retries: 3,
            list_marker_pages: 4,
            want_retry: Duration::from_secs(60),
            want_recent_limit: 100_000,
            copy_back_max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// How a read that missed the hot tier asks for a copy-back, if it asks.
pub(super) enum CopyBack {
    /// Markers are not read in this phase: the entry protocol's ask, if its format is active.
    ByEntry,
    /// None: the key has a marker, or the markers are not in force.
    No,
    /// By creating `WANT` through these markers: the key has no marker and the markers are in
    /// force for this proxy (it may write them).
    ByMarker(Arc<Markers>),
}

/// What a read that missed the hot tier came to.
pub(super) enum Missed<T> {
    /// The cold answer is the answer.
    Cold { found: T, copy_back: CopyBack },
    /// The marker said the key is deleted, and the hot tier has it since.
    Hot(T),
}

/// The keys this proxy asked a copy-back of lately, and when.
#[derive(Default)]
pub(super) struct Wants {
    state: Mutex<Asked>,
}

#[derive(Default)]
struct Asked {
    /// The key and the reading of the monotonic clock at its last ask.
    at: HashMap<String, u64>,
    /// When the set was last cleared of the asks that are past `want_retry`.
    pruned: Option<u64>,
}

impl Wants {
    /// Whether an ask of `key` at `now` (the monotonic clock, in milliseconds) may be made, and
    /// if so notes it. Not if the key was asked within `want_retry`, and not if the set is full of
    /// asks that are still within it: a set that is full is cleaned of the others at most once in
    /// `want_retry`, so that a full set costs a read no scan.
    fn allow(&self, key: &str, now: u64, config: &MarkReads) -> bool {
        let retry = millis(config.want_retry);
        let recent = |at: &u64| now.saturating_sub(*at) < retry;
        let mut asked = locked(&self.state);
        if asked.at.get(key).is_some_and(recent) {
            return false;
        }
        let known = asked.at.contains_key(key);
        if !known && asked.at.len() >= config.want_recent_limit {
            if asked
                .pruned
                .is_none_or(|at| now.saturating_sub(at) >= retry)
            {
                asked.at.retain(|_, at| recent(at));
                asked.pruned = Some(now);
            }
            if asked.at.len() >= config.want_recent_limit {
                return false;
            }
        }
        asked.at.insert(key.to_owned(), now);
        true
    }
}

impl Inner {
    /// The markers, if a read that missed the hot tier reads them now: this proxy has a marks
    /// bucket and the format object, as of a read at most `format_refresh` old, names a phase from
    /// which markers can be in force. Also whether this proxy may write them: the format opens
    /// writes on markers (`active-marks` at epoch 3 or later) in the marks bucket this proxy has
    /// (`ProxyConfig::marks_name`), as for a put, so that two names are never in use at once.
    async fn markers_to_read(&self) -> Option<(&Arc<Markers>, bool)> {
        let markers = self.markers.as_ref()?;
        let state = self.format_state().await;
        let mine = self.config.marks_name.as_deref();
        let may_write = state
            .marks
            .as_deref()
            .is_some_and(|recorded| mine == Some(recorded));
        state.reads_markers().then_some((markers, may_write))
    }

    /// Whether a listing reads markers, and the markers it reads them through.
    pub(super) async fn markers_for_listing(&self) -> Option<&Arc<Markers>> {
        self.markers_to_read().await.map(|(markers, _)| markers)
    }

    /// Reads `key` after the hot tier missed it: the cold read `cold`, in parallel with the marker
    /// where markers are read, and `hot_again`, the hot read once more, if the marker says the key
    /// is deleted. See the module.
    ///
    /// # Errors
    /// What `cold` gives when its answer is the answer; the error of the marker read; what
    /// `hot_again` gives if the marker says the key is deleted ([`Error::NotFound`] if the hot
    /// tier still has nothing); [`Error::Unavailable`] if every pair took longer than the window.
    pub(super) async fn miss_read<T, C, CF, H, HF>(
        &self,
        key: &str,
        cold: C,
        hot_again: H,
    ) -> Result<Missed<T>, Error>
    where
        C: Fn() -> CF,
        CF: Future<Output = Result<T, Error>>,
        H: FnOnce() -> HF,
        HF: Future<Output = Result<T, Error>>,
    {
        let Some((markers, in_force)) = self.markers_to_read().await else {
            let found = cold().await?;
            return Ok(Missed::Cold {
                found,
                copy_back: CopyBack::ByEntry,
            });
        };
        let config = &self.config.mark_reads;
        let window = millis(config.read_window);
        let pairs = config.read_retries.saturating_add(1);
        for _ in 0..pairs {
            let started = self.config.mono.now();
            let (marker, read) = futures::join!(markers.read(key), cold());
            if self.config.mono.now().saturating_sub(started) > window {
                continue;
            }
            return match marker? {
                Some(seen) if seen.marker.gone => hot_again().await.map(Missed::Hot),
                Some(_) => read.map(|found| Missed::Cold {
                    found,
                    copy_back: CopyBack::No,
                }),
                None => read.map(|found| Missed::Cold {
                    found,
                    copy_back: if in_force {
                        CopyBack::ByMarker(Arc::clone(markers))
                    } else {
                        CopyBack::No
                    },
                }),
            };
        }
        let why = format!(
            "the hot and cold tiers answered a read of {key} further apart than read_window \
             ({window} ms), {pairs} times"
        );
        Err(Error::Unavailable(why))
    }

    /// Asks for the copy-back of `key`, an object of `size` bytes the cold tier answered a whole
    /// read of, the way `how` says (the caller has seen that the level is below `Critical`).
    pub(super) async fn ask_copy_back_of(self: &Arc<Self>, how: CopyBack, key: &str, size: u64) {
        match how {
            CopyBack::ByEntry => {
                if self.entries_active().await {
                    self.ask_copy_back(key);
                }
            }
            CopyBack::ByMarker(markers) => self.ask_want(markers, key, size),
            CopyBack::No => {}
        }
    }

    /// Asks for the copy-back of `key`, an object of `size` bytes the cold tier answered a whole
    /// read of, by creating its `WANT` marker in a task of its own (M10): the read never waits for
    /// it, and its failure is ignored. Not for an object above `copy_back_max_bytes`, nor for a key
    /// this proxy asked for within `want_retry`.
    pub(super) fn ask_want(&self, markers: Arc<Markers>, key: &str, size: u64) {
        let config = &self.config.mark_reads;
        if size > config.copy_back_max_bytes
            || !self.wants.allow(key, self.config.mono.now(), config)
        {
            return;
        }
        let key = key.to_owned();
        drop(tokio::spawn(async move {
            let _ = markers.create(&key, MarkBirth::Want).await;
        }));
    }
}

#[cfg(test)]
#[path = "mark_reads_tests.rs"]
mod tests;
