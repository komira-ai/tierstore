//! The stray sweep: a hot object that no entry and no cold object accompanies is adopted, by a
//! pending put entry, so that the next step copies it. The same sweep aborts the multipart uploads
//! that have stood open in the hot tier for longer than
//! [`EntryReconcilerConfig::abandoned_upload_after`](super::EntryReconcilerConfig): an upload takes
//! no entry until its completion, so nothing else finds one that its client has left.

use std::time::Duration;

use super::{EntryReconciler, StepReport};
use crate::Error;
use crate::deadline::millis;
use crate::entry::{Created, Take};
use crate::journal::RESERVED_PREFIXES;

impl EntryReconciler {
    /// Whether the sweep is due at `now` on the monotonic clock: a new leader sweeps at once, and
    /// after that every [`EntryReconcilerConfig::stray_interval`](super::EntryReconcilerConfig).
    pub(super) fn sweep_due(&self, now: u64) -> bool {
        self.sweep_at.is_none_or(|at| now >= at)
    }

    /// Lists the hot tier and adopts each object older than the grace (by the time the hot tier
    /// gave it, against the wall clock of the settings) that has no cold object and no entry. The
    /// cold tier is asked about one object per `1 / stray_rate` seconds (rounded up to a whole
    /// microsecond), on the loop's own wait. A failed call ends the sweep, which is due again at
    /// the next step.
    pub(super) async fn sweep(&mut self, now: u64, report: &mut StepReport) {
        let page_size = self.config.base.page_size.max(1);
        let grace = millis(self.config.stray_grace);
        let oldest = self.config.base.clock.now().saturating_sub(grace);
        let rate = u64::from(self.config.stray_rate.max(1));
        let pause = Duration::from_micros(1_000_000_u64.div_ceil(rate));
        let mut after: Option<String> = None;
        loop {
            let listing = self.hot.list_page("", after.as_deref(), page_size);
            let page = match listing.await {
                Ok(page) => page,
                Err(error) => return report.failed.push(error),
            };
            let last = page.len() < page_size;
            for object in &page {
                let reserved = RESERVED_PREFIXES
                    .iter()
                    .any(|prefix| object.key.starts_with(prefix));
                if reserved || object.modified > oldest {
                    continue;
                }
                self.config.waiter.sleep(pause).await;
                match self.cold.head(&object.key).await {
                    Ok(_) => {}
                    Err(Error::NotFound(_)) => self.adopt(&object.key, object.size, report).await,
                    Err(error) => return report.failed.push(error),
                }
            }
            if last {
                break;
            }
            after = page.last().map(|object| object.key.clone());
        }
        if let Err(error) = self.abort_abandoned(report).await {
            return report.failed.push(error);
        }
        self.sweep_at = Some(now.saturating_add(millis(self.config.stray_interval)));
    }

    /// Makes the entry of a stray object of `size` bytes, unless one exists, and counts its bytes
    /// as not held by the cold tier until it is copied.
    async fn adopt(&mut self, key: &str, size: u64, report: &mut StepReport) {
        match self.entries.create(key, Take::Stray).await {
            Ok(Created::Taken(_)) => {
                self.tally.lags(key, size);
                self.strays_adopted += 1;
                report.strays_adopted += 1;
            }
            Ok(Created::Exists) => {}
            Err(error) => report.failed.push(error),
        }
    }

    /// Aborts each open upload of the hot tier that was started more than
    /// `abandoned_upload_after` ago by the wall clock of the settings (an upload whose start the
    /// hot tier does not know, which it gives as 0, is left), one per `1 / stray_rate` seconds, on
    /// the loop's own wait. Aborting touches no object and no entry, so it cannot break write-once;
    /// a completion that meets an aborted upload is answered `NoSuchUpload`. A hot tier that does
    /// no multipart uploads has none. `abandoned_upload_after` of zero turns the abort off.
    async fn abort_abandoned(&self, report: &mut StepReport) -> Result<(), Error> {
        let after = millis(self.config.abandoned_upload_after);
        if after == 0 {
            return Ok(());
        }
        let oldest = self.config.base.clock.now().saturating_sub(after);
        let page_size = self.config.base.page_size.max(1);
        let rate = u64::from(self.config.stray_rate.max(1));
        let pause = Duration::from_micros(1_000_000_u64.div_ceil(rate));
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
                let reserved = RESERVED_PREFIXES
                    .iter()
                    .any(|prefix| upload.key.starts_with(prefix));
                if reserved || upload.initiated == 0 || upload.initiated > oldest {
                    continue;
                }
                self.config.waiter.sleep(pause).await;
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
