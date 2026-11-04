//! The last step of a pass while writes are paused: when the format object says `drain`, the
//! reconciler that holds the lease writes `drained` once every journal it knows is applied to its
//! end (see the module `format`).

use tokio::time::Instant;

use super::{PassReport, Reconciler};
use crate::Backend;
use crate::format::{Format, FormatPhase, format_body, read_format_tagged};
use crate::journal::FORMAT_KEY;

impl Reconciler {
    /// Forgets since when `drain` was seen: the lease is not held, or the format is not `drain`.
    pub(super) fn forget_drain(&mut self) {
        self.draining = None;
    }

    /// Writes `drained` over `drain` if the format says `drain` and has said so for
    /// [`ReconcilerConfig::drain_settle`](super::ReconcilerConfig::drain_settle) (so that every
    /// proxy has read it and stopped writing journals), and the pass had no failure: every journal
    /// object listed was read and applied, and each checkpoint stored, since a step that fails
    /// is in `report.failed`. The write is over the tag of the object that was read, so
    /// an operator who cancelled in between keeps the cancel.
    pub(super) async fn settle_drain(&mut self, report: &mut PassReport) {
        if !report.failed.is_empty() {
            return;
        }
        let (format, tag) = match read_format_tagged(self.hot.as_ref()).await {
            Ok(Some(found)) => found,
            Ok(None) => return self.forget_drain(),
            Err(error) => {
                report.failed.push(error);
                return;
            }
        };
        if format.phase != FormatPhase::Drain {
            return self.forget_drain();
        }
        let since = *self.draining.get_or_insert_with(Instant::now);
        if since.elapsed() < self.config.drain_settle {
            return;
        }
        let drained = Format {
            phase: FormatPhase::Drained,
            epoch: format.epoch,
        };
        let body = format_body(&drained);
        if let Err(error) = self.hot.put_if_match(FORMAT_KEY, body, &tag).await {
            report.failed.push(error);
        }
    }
}
