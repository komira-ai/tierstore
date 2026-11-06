//! The pass of the leader over the markers of the put-without-lock design
//! (`docs/design/put-without-lock.md`, 5.9, 5.13 and 5.14), which a step does in place of the pass
//! over the entries when the format says `active-marks` and the reconciler has the marks bucket
//! and its name ([`EntryReconciler::with_marks`]) and the name is the one the format records. Under
//! any other phase, or with another name, nothing here runs: the pass over the markers is inert
//! until the activation.
//!
//! A pass lists the marks bucket in pages of [`EntryReconcilerConfig::marks_page`], reads the
//! markers of a page (one GET each, which is the version `a` of the marker) and works them on a
//! pool of [`EntryReconcilerConfig::copy_concurrency`], the `DIRTY` ones first (the module
//! `copier`), then the locks and clearings of owners that departed or whose lease ran out (the
//! module `reap`). A `WANT` marker, which asks for a copy-back, and the eviction and the stray
//! sweep that make locks of the leader's own are the work of the next pull request: this pass
//! leaves a `WANT` as it is.
//!
//! The waits of the design (`orphan_after`, `lock_lease`, `tombstone_hold`) are not sleeps. The
//! leader notes the version of each marker it lists and when, on its monotonic clock, it first
//! saw that version ([`MarkBooks`]); a pass that finds the version unchanged for long enough acts,
//! and until then the marker is left. A version that changed starts the count again, and so does a
//! new leader, which counts a wait up to one wait late and never early.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures::stream::{self, StreamExt};

use super::{EntryReconciler, StepReport};
use crate::deadline::millis;
use crate::format::{Format, FormatPhase};
use crate::journal::PROXIES_PREFIX;
use crate::{Backend, Error, Listed, MarkSeen, MarkerState, Markers, MarkersConfigError};

/// The leader's handle on the marks bucket: the backend, to list it, the calls on the markers
/// over the same backend, and the name the format record must give for the pass to run.
pub(super) struct MarkSide {
    pub(super) store: Arc<dyn Backend>,
    pub(super) markers: Markers,
    pub(super) bucket: String,
}

/// What a pass remembers of the markers it lists, by key. Both maps are the leader's own: a new
/// leader starts them afresh.
#[derive(Default)]
pub(super) struct MarkBooks {
    /// The version (content tag) of each marker as last listed, and when the leader first saw it
    /// so, on its monotonic clock.
    seen: BTreeMap<String, (String, u64)>,
    /// The markers whose cold object the leader deleted or overwrote while the marker said
    /// `gone`: the version of the marker and when, on the monotonic clock, the cold write was
    /// made. The marker is cleared only once `tombstone_hold` has passed since.
    holds: BTreeMap<String, (String, Hold)>,
}

/// A hold on a `gone` marker: the cold object was deleted or overwritten at `at` on the monotonic
/// clock, and the marker is cleared only once `tombstone_hold` has passed since. For an
/// overwrite it also holds the tags of the pair written, which say, for as long as the marker
/// stands unchanged (no overwrite of hot can have happened meanwhile, for that moves the marker),
/// that cold has the bytes of hot, so that the pass after the hold does not copy again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Hold {
    /// When the cold object was written, on the monotonic clock.
    pub(super) at: u64,
    /// The tags of the hot and the cold object after a copy.
    pub(super) tags: Option<(String, String)>,
}

/// What a pass knows of one marker beyond its body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Look {
    /// The marker has stood unchanged for `orphan_after`.
    pub(super) stale: bool,
    /// The marker has stood unchanged for `lock_lease`: a holder that lives is presumed stalled.
    pub(super) expired: bool,
    /// The hold on this version of the marker, if the leader noted one.
    pub(super) held: Option<Hold>,
}

impl MarkBooks {
    /// Notes the versions of a page listed at `now`: a new or changed version is first seen now.
    pub(super) fn observe(&mut self, page: &[Listed], now: u64) {
        for object in page {
            let same = self
                .seen
                .get(&object.key)
                .is_some_and(|(tag, _)| *tag == object.tag);
            if !same {
                self.seen
                    .insert(object.key.clone(), (object.tag.clone(), now));
            }
        }
    }

    /// What is known of the marker `key` at version `tag`, at `now`, given the waits in
    /// milliseconds.
    pub(super) fn look(&self, key: &str, tag: &str, now: u64, orphan: u64, lease: u64) -> Look {
        let age = self
            .seen
            .get(key)
            .filter(|(seen, _)| seen == tag)
            .map_or(0, |(_, first)| now.saturating_sub(*first));
        let held = self
            .holds
            .get(key)
            .filter(|(held, _)| held == tag)
            .map(|(_, hold)| hold.clone());
        Look {
            stale: age >= orphan,
            expired: age >= lease,
            held,
        }
    }

    /// Notes the hold on the marker `key` at version `tag`.
    pub(super) fn hold(&mut self, key: &str, tag: &str, hold: Hold) {
        self.holds.insert(key.to_owned(), (tag.to_owned(), hold));
    }

    /// Forgets the markers that a complete listing did not find.
    pub(super) fn retain(&mut self, listed: &BTreeSet<String>) {
        self.seen.retain(|key, _| listed.contains(key));
        self.holds.retain(|key, _| listed.contains(key));
    }
}

/// What the work on one marker came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Marked {
    /// Nothing is done with a marker in this state by this release (a `WANT`).
    Left,
    /// The marker waits: for `orphan_after`, for the hold, or for its owner or its lease.
    Pinned,
    /// The marker moved under the pass, or the cold object did: the key is looked at again at the
    /// next pass.
    Moved,
    /// The cold object was deleted or overwritten under a marker that says `gone`: the hold
    /// starts.
    Held(Hold),
    /// The marker was cleared and removed.
    Cleared,
    /// A lock or a clearing was reaped.
    Reaped,
}

impl EntryReconciler {
    /// Gives the reconciler the marks bucket of the hot store, `store`, named `bucket` (the name
    /// the format record must carry for the markers to be worked), and the calls on its
    /// markers as the actor named by the holder.
    ///
    /// # Errors
    /// [`MarkersConfigError`] if `config.markers` does not hold together.
    pub fn with_marks(
        mut self,
        store: Arc<dyn Backend>,
        bucket: impl Into<String>,
    ) -> Result<Self, MarkersConfigError> {
        let base = &self.config.base;
        let markers = Markers::new(
            Arc::clone(&store),
            base.holder.as_str(),
            self.config.markers.clone(),
            Arc::clone(&base.mono),
            Arc::clone(&base.sleeper),
        )?;
        let bucket = bucket.into();
        self.marks = Some(Arc::new(MarkSide {
            store,
            markers,
            bucket,
        }));
        Ok(self)
    }

    /// The marks bucket, if the markers are worked under `format`: it says `active-marks`, and
    /// names the marks bucket this reconciler has.
    pub(super) fn runs_marks(&self, format: &Format) -> Option<Arc<MarkSide>> {
        let named = format.marks_bucket.as_deref();
        let side = self.marks.as_ref()?;
        let active = format.phase == FormatPhase::ActiveMarks;
        (active && named == Some(side.bucket.as_str())).then(|| Arc::clone(side))
    }

    /// The steps of a step over the markers, once the format says `active-marks` and the lease is
    /// held.
    pub(super) async fn mark_steps(
        &mut self,
        side: Arc<MarkSide>,
        mut report: StepReport,
    ) -> StepReport {
        report.held = true;
        let now = self.config.base.mono.now();
        match self.hot.list(PROXIES_PREFIX).await {
            Ok(listed) => self.registrations.observe(now, &listed),
            Err(error) => {
                report.failed.push(error);
                return report;
            }
        }
        if let Some(listed) = self.mark_pages(&side, now, &mut report).await {
            self.mark_books.retain(&listed);
            self.retries.retain(|key, _| listed.contains(key));
            self.tally.keep_lagging(&listed);
        }
        self.tend_usage(now, &mut report).await;
        self.publish_usage(now, &mut report).await;
        report
    }

    /// Lists the marks bucket a page at a time and works each page. Gives the keys listed, or
    /// `None` if a listing failed (the keys not reached are then not forgotten).
    async fn mark_pages(
        &mut self,
        side: &MarkSide,
        now: u64,
        report: &mut StepReport,
    ) -> Option<BTreeSet<String>> {
        let size = self.config.marks_page.max(1);
        let mut listed = BTreeSet::new();
        let mut after: Option<String> = None;
        loop {
            let page = match side.store.list_page("", after.as_deref(), size).await {
                Ok(page) => page,
                Err(error) => {
                    report.failed.push(error);
                    return None;
                }
            };
            let last = page.len() < size;
            after = page.last().map(|object| object.key.clone());
            listed.extend(page.iter().map(|object| object.key.clone()));
            self.mark_page(side, page, now, report).await;
            if last {
                return Some(listed);
            }
        }
    }

    /// Reads the markers of a page and works them: `DIRTY` first, then the others.
    async fn mark_page(
        &mut self,
        side: &MarkSide,
        page: Vec<Listed>,
        now: u64,
        report: &mut StepReport,
    ) {
        self.mark_books.observe(&page, now);
        let waiting = |object: &Listed| {
            self.retries
                .get(&object.key)
                .is_some_and(|retry| retry.until > now)
        };
        let (backed_off, due): (Vec<Listed>, Vec<Listed>) = page.into_iter().partition(waiting);
        report.backed_off += backed_off.len();
        let width = self.config.copy_concurrency.max(1);
        let reads = stream::iter(due).map(|object| async move {
            let read = side.markers.read(&object.key).await;
            (object.key, read)
        });
        let read: Vec<_> = reads.buffer_unordered(width).collect().await;
        let mut items = Vec::new();
        let mut failed = Vec::new();
        for (key, read) in read {
            match read {
                Ok(Some(seen)) => items.push(seen),
                Ok(None) => self.tally.caught_up(&key),
                Err(error) => failed.push((key, error)),
            }
        }
        items.sort_by_key(|seen| seen.marker.st != MarkerState::Dirty);
        let orphan = millis(self.config.orphan_after);
        let lease = millis(self.config.markers.lock_lease);
        let looks: Vec<Look> = items
            .iter()
            .map(|seen| {
                let (key, tag) = (&seen.marker.key, &seen.tag);
                self.mark_books.look(key, tag, now, orphan, lease)
            })
            .collect();
        let this = &*self;
        let steps = stream::iter(items.into_iter().zip(looks)).map(|(seen, look)| async move {
            let marked = this.mark_one(side, &seen, look, now).await;
            (seen, marked)
        });
        let worked: Vec<_> = steps.buffer_unordered(width).collect().await;
        for (key, error) in failed {
            self.fail(key, error, report);
        }
        for (seen, marked) in worked {
            let key = seen.marker.key;
            match marked {
                Ok(marked) => {
                    self.retries.remove(&key);
                    match marked {
                        Marked::Left => {}
                        Marked::Pinned | Marked::Moved => report.pinned += 1,
                        Marked::Held(hold) => {
                            self.mark_books.hold(&key, &seen.tag, hold);
                            report.changed += 1;
                        }
                        Marked::Cleared | Marked::Reaped => report.changed += 1,
                    }
                }
                Err(error) => self.fail(key, error, report),
            }
        }
    }

    /// Does the step of the design for the marker `seen` in the state it is in.
    async fn mark_one(
        &self,
        side: &MarkSide,
        seen: &MarkSeen,
        look: Look,
        now: u64,
    ) -> Result<Marked, Error> {
        match seen.marker.st {
            MarkerState::Dirty => self.mark_dirty(side, seen, look, now).await,
            MarkerState::Want => Ok(Marked::Left),
            MarkerState::Lock | MarkerState::Clearing | MarkerState::Other(_) => {
                self.mark_held(side, seen, look, now).await
            }
        }
    }
}
