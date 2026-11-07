//! The stray sweep over markers (`docs/design/put-without-lock.md`, 5.12): a hot object that no
//! marker accompanies, and that the cold tier does not hold as a copy, gets a `DIRTY` marker (M12)
//! so that the copier copies it. It repairs the two crash windows of the invariant I7 (an object
//! landed and its marker did not), and generalises "hot with no cold" to "hot with no marker and a
//! cold that is not a copy".
//!
//! The sweep is a merge-join of two sorted listings, the hot tier's objects and the names in the
//! marks bucket, a page of each at a time. A hot object older than `stray_grace` (by the hot
//! tier's time of its last write against the wall clock, as the entry sweep counts it) and
//! outside the reserved prefixes, with no marker, is looked up in the cold tier, one object per
//! `1 / stray_rate` seconds on the sweep's own waiter. It is adopted when
//!
//! - the cold tier has no object, or one of another size, or
//! - both tags are plain MD5s (32 hex digits) and differ, or
//! - the sizes are equal and the tags cannot be compared (a `dir:` hot tier tags with SHA-256, a
//!   multipart object with `md5-N`) but the leader has a record of the pair of tags it copied that
//!   no longer has the hot object's current tag: a change. A mismatch hiding in the rest is found
//!   by the compare of the eviction (5.10).
//!
//! No time of either store is compared with the other's: the hot and the cold tier are separate
//! clusters, and a cold clock ahead of the hot clock by more than the real gap would hide an
//! uncopied overwrite.
//!
//! The same pass aborts the multipart uploads of the hot tier that were started more than
//! `abandoned_upload_after` ago (7 days; zero turns it off), because a put that dies no longer
//! holds a lock the leader's reap could abort its uploads under (5.13). The cold tier's uploads
//! are left to the client.
//!
//! It runs at takeover and then every `stray_interval`, and reads the format object first: under
//! a format that does not say `active-marks` at epoch 3 or later and name the marks bucket the
//! sweep was given (the condition of a put and of the copier) it does nothing.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crate::deadline::millis;
use crate::entry::found;
use crate::format::marks_run_in;
use crate::journal::is_reserved;
use crate::marks_evict::CopyRecords;
use crate::{Backend, Clock, Creation, Error, Listed, MarkBirth, Markers, Meta, Sleeper};

/// The settings of the sweep: `stray_grace`, `stray_interval`, `stray_rate`,
/// `abandoned_upload_after` and the size of a listing page of the design (section 9), with the
/// defaults it gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepConfig {
    /// A hot object must have been written this long ago to be looked at (600 s).
    pub stray_grace: Duration,
    /// The time between two sweeps after the one at takeover (24 h).
    pub stray_interval: Duration,
    /// The most cold lookups a second (100).
    pub stray_rate: u32,
    /// A hot-tier multipart upload started this long ago is aborted (7 days); zero never aborts.
    pub abandoned_upload_after: Duration,
    /// The objects asked for in one page of a listing (1000, the `marks_page` of the design).
    pub page_size: usize,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            stray_grace: Duration::from_secs(600),
            stray_interval: Duration::from_secs(24 * 3600),
            stray_rate: 100,
            abandoned_upload_after: Duration::from_secs(7 * 24 * 3600),
            page_size: 1000,
        }
    }
}

/// What one [`StraySweep::run`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Whether the format opened writes on markers in the sweep's marks bucket. If not, nothing
    /// else was done.
    pub active: bool,
    /// The hot objects that got a `DIRTY` marker.
    pub adopted: usize,
    /// The abandoned hot-tier multipart uploads aborted.
    pub uploads_aborted: usize,
    /// The calls that failed, in the order they did. A failed listing or lookup ends the sweep,
    /// which is due again at the next call.
    pub failed: Vec<Error>,
}

/// The sorted names of a bucket, a page at a time, read forward to the first one at or after a
/// key.
struct Names<'a> {
    store: &'a dyn Backend,
    page_size: usize,
    after: Option<String>,
    page: VecDeque<String>,
    done: bool,
}

impl<'a> Names<'a> {
    fn new(store: &'a dyn Backend, page_size: usize) -> Self {
        Self {
            store,
            page_size,
            after: None,
            page: VecDeque::new(),
            done: false,
        }
    }

    /// Whether the bucket has a name equal to `key`. The calls must ask for ascending keys.
    async fn has(&mut self, key: &str) -> Result<bool, Error> {
        loop {
            while let Some(front) = self.page.front() {
                if front.as_str() < key {
                    self.page.pop_front();
                } else {
                    return Ok(front == key);
                }
            }
            if self.done {
                return Ok(false);
            }
            let listing = self
                .store
                .list_page("", self.after.as_deref(), self.page_size)
                .await?;
            self.done = listing.len() < self.page_size;
            self.after = listing.last().map(|object| object.key.clone());
            self.page = listing.into_iter().map(|object| object.key).collect();
        }
    }
}

/// Whether `tag` is a plain MD5: 32 hex digits, which no multipart tag (`md5-N`) and no SHA-256
/// tag is.
fn plain_md5(tag: &str) -> bool {
    tag.len() == 32 && tag.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The clocks and the wait of the sweep.
#[derive(Clone)]
pub struct SweepTime {
    /// The wall clock the hot tier's times agree with.
    pub clock: Arc<dyn Clock>,
    /// The leader's monotonic clock.
    pub mono: Arc<dyn Clock>,
    /// What the sweep waits on between lookups.
    pub waiter: Arc<dyn Sleeper>,
}

/// The leader's stray sweep over markers.
pub struct StraySweep {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    marks: Arc<dyn Backend>,
    bucket: String,
    markers: Markers,
    records: Arc<CopyRecords>,
    time: SweepTime,
    config: SweepConfig,
    /// When the sweep runs next, on `mono`; `None` for now.
    sweep_at: Option<u64>,
}

impl StraySweep {
    /// The sweep of the leader whose markers are `markers`, kept in the bucket `marks` (the one
    /// `markers` is over), which the format record names `bucket`.
    #[must_use]
    pub fn new(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        marks: Arc<dyn Backend>,
        bucket: &str,
        markers: Markers,
        records: Arc<CopyRecords>,
        time: SweepTime,
        config: SweepConfig,
    ) -> Self {
        Self {
            hot,
            cold,
            marks,
            bucket: bucket.to_owned(),
            markers,
            records,
            time,
            config,
            sweep_at: None,
        }
    }

    /// The leader took over: the next [`run`](Self::run) sweeps.
    pub fn took_over(&mut self) {
        self.sweep_at = None;
    }

    /// Whether a sweep is due now: at takeover, and every `stray_interval` after one.
    #[must_use]
    pub fn due(&self) -> bool {
        self.sweep_at.is_none_or(|at| self.time.mono.now() >= at)
    }

    fn pause(&self) -> Duration {
        let rate = u64::from(self.config.stray_rate.max(1));
        Duration::from_micros(1_000_000_u64.div_ceil(rate))
    }

    /// Sweeps if the format opens writes on markers in this sweep's marks bucket.
    pub async fn run(&mut self) -> SweepReport {
        let mut report = SweepReport::default();
        match marks_run_in(self.hot.as_ref(), &self.bucket).await {
            Ok(true) => report.active = true,
            Ok(false) => return report,
            Err(error) => {
                report.failed.push(error);
                return report;
            }
        }
        if let Err(error) = self.adopt_strays(&mut report).await {
            report.failed.push(error);
            return report;
        }
        if let Err(error) = self.abort_abandoned(&mut report).await {
            report.failed.push(error);
            return report;
        }
        let interval = millis(self.config.stray_interval);
        self.sweep_at = Some(self.time.mono.now().saturating_add(interval));
        report
    }

    /// The merge-join: each hot object old enough, outside the reserved prefixes and with no
    /// marker is looked up in the cold tier and adopted if it is not a copy of it.
    async fn adopt_strays(&self, report: &mut SweepReport) -> Result<(), Error> {
        let page_size = self.config.page_size.max(1);
        let oldest = self
            .time
            .clock
            .now()
            .saturating_sub(millis(self.config.stray_grace));
        let mut names = Names::new(self.marks.as_ref(), page_size);
        let mut after: Option<String> = None;
        loop {
            let page = self.hot.list_page("", after.as_deref(), page_size).await?;
            for object in &page {
                if is_reserved(&object.key) || object.modified > oldest {
                    continue;
                }
                if names.has(&object.key).await? {
                    continue;
                }
                self.time.waiter.sleep(self.pause()).await;
                let cold = found(self.cold.head(&object.key).await)?;
                if cold.is_none_or(|cold| self.is_stray(object, &cold)) {
                    match self.markers.create(&object.key, MarkBirth::Dirty).await {
                        Ok(Creation::Made(_)) => report.adopted += 1,
                        Ok(Creation::Exists) => {}
                        Err(error) => report.failed.push(error),
                    }
                }
            }
            if page.len() < page_size {
                return Ok(());
            }
            after = page.last().map(|object| object.key.clone());
        }
    }

    /// Whether the cold object `cold` is not a copy of the hot object `hot` (see the module).
    fn is_stray(&self, hot: &Listed, cold: &Meta) -> bool {
        if hot.size != cold.size {
            return true;
        }
        if plain_md5(&hot.tag) && plain_md5(&cold.tag) {
            return hot.tag != cold.tag;
        }
        self.records
            .get(&hot.key)
            .is_some_and(|record| record.hot != hot.tag)
    }

    /// Aborts each open upload of the hot tier that was started more than `abandoned_upload_after`
    /// ago by the wall clock (one whose start the hot tier does not know, which it gives as 0, is
    /// left), one per `1 / stray_rate` seconds. A hot tier that does no multipart uploads has
    /// none.
    async fn abort_abandoned(&self, report: &mut SweepReport) -> Result<(), Error> {
        let after = millis(self.config.abandoned_upload_after);
        if after == 0 {
            return Ok(());
        }
        let oldest = self.time.clock.now().saturating_sub(after);
        let page_size = self.config.page_size.max(1);
        let mut marker: Option<(String, String)> = None;
        loop {
            let at = marker
                .as_ref()
                .map(|(key, id)| (key.as_str(), Some(id.as_str())));
            let page = match self.hot.list_uploads("", at, page_size).await {
                Ok(page) => page,
                Err(Error::Unsupported(_)) => return Ok(()),
                Err(error) => return Err(error),
            };
            for upload in &page {
                if is_reserved(&upload.key) || upload.initiated == 0 || upload.initiated > oldest {
                    continue;
                }
                self.time.waiter.sleep(self.pause()).await;
                match self.hot.abort_upload(&upload.key, &upload.id).await {
                    Ok(()) => report.uploads_aborted += 1,
                    // Completed or aborted by its client meanwhile.
                    Err(Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            if page.len() < page_size {
                return Ok(());
            }
            marker = page.last().map(|up| (up.key.clone(), up.id.clone()));
        }
    }
}
