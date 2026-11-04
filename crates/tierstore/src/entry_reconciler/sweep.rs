//! The stray sweep: a hot object that no entry and no cold object accompanies is adopted, by a
//! pending put entry, so that the next step copies it.

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
    /// cold tier is asked about one object per `1 / stray_rate` seconds, on the loop's own wait. A
    /// failed call ends the sweep, which is due again at the next step.
    pub(super) async fn sweep(&mut self, now: u64, report: &mut StepReport) {
        let page_size = self.config.base.page_size.max(1);
        let grace = millis(self.config.stray_grace);
        let oldest = self.config.base.clock.now().saturating_sub(grace);
        let rate = u64::from(self.config.stray_rate.max(1));
        let pause = Duration::from_millis(1000 / rate);
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
                    Err(Error::NotFound(_)) => self.adopt(&object.key, report).await,
                    Err(error) => return report.failed.push(error),
                }
            }
            if last {
                break;
            }
            after = page.last().map(|object| object.key.clone());
        }
        self.sweep_at = Some(now.saturating_add(millis(self.config.stray_interval)));
    }

    /// Makes the entry of a stray object, unless one exists.
    async fn adopt(&mut self, key: &str, report: &mut StepReport) {
        match self.entries.create(key, Take::Stray).await {
            Ok(Created::Taken(_)) => {
                self.strays_adopted += 1;
                report.strays_adopted += 1;
            }
            Ok(Created::Exists) => {}
            Err(error) => report.failed.push(error),
        }
    }
}
