//! The last step of a pass while writes are paused: when the format object says `drain`, the
//! reconciler that holds the lease writes `drained` once every journal it knows is applied to its
//! end (see the module `format`).

use tokio::time::Instant;

use super::{PassReport, Reconciler};
use crate::format::{Format, FormatPhase, format_body, read_format, read_format_tagged};
use crate::journal::FORMAT_KEY;

impl Reconciler {
    /// Forgets since when `drain` was seen: the lease is not held, or the format is not `drain`.
    pub(super) fn forget_drain(&mut self) {
        self.draining = None;
    }

    /// Writes `drained` over `drain` if the format says `drain` and has said so for
    /// [`ReconcilerConfig::drain_settle`](super::ReconcilerConfig::drain_settle) (long enough for
    /// every proxy to read it again, which is all the wait gives: a write that a proxy admitted
    /// before it read `drain` and that is still running is not waited for), and the pass had no
    /// failure: every journal
    /// object listed was read and applied, and each checkpoint stored, since a step that fails
    /// is in `report.failed`. The object is read again with its tag just before the write, and the
    /// write is over that tag, so an operator who cancelled in between keeps the cancel, and a
    /// cancel seen by that second read starts the settle time again.
    pub(super) async fn settle_drain(&mut self, report: &mut PassReport) {
        if !report.failed.is_empty() {
            return;
        }
        match read_format(self.hot.as_ref()).await {
            Ok(Some(format)) if format.phase == FormatPhase::Drain => {}
            Ok(_) => return self.forget_drain(),
            Err(error) => {
                report.failed.push(error);
                return;
            }
        }
        let since = *self.draining.get_or_insert_with(Instant::now);
        if since.elapsed() < self.config.drain_settle {
            return;
        }
        let (format, tag) = match read_format_tagged(self.hot.as_ref()).await {
            Ok(Some(found)) if found.0.phase == FormatPhase::Drain => found,
            Ok(_) => return self.forget_drain(),
            Err(error) => {
                report.failed.push(error);
                return;
            }
        };
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
