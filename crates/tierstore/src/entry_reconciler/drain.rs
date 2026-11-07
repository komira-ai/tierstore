//! The leader's part of the pause over the entry protocol (`tierstore format-epoch --drain` over
//! `active` at epoch 2, [`Format::drains_entries`](crate::format::Format::drains_entries);
//! `docs/design/put-without-lock.md`, 8.2, step 1).
//!
//! Under that format a step works the entries exactly as under `active`, and then asks whether
//! the pause is at rest, in this order:
//!
//! 1. Every registration of `_tiering/proxies/` is of a process that has departed (its registration
//!    is gone, or has stood unchanged for [`EntryReconcilerConfig::departure`], as for the owner of a
//!    held entry), or reports a `phase` of `drain` or `drained` and `writes_in_flight` of 0. A proxy
//!    that reports that phase starts no write, and one that reports a count of zero has none that
//!    began before it read the phase (see `tiered/pause.rs`). A registration that says neither (the
//!    registration of a release that does not write the two fields, or of a process that has not
//!    beaten yet) holds the pause until it has departed.
//! 2. `_entries/`, listed after the registrations were read, is empty. The order matters: a write
//!    that ended before its proxy reported a count of zero has made its entry before the report, so
//!    the listing that follows the report sees it, and no write begins after it.
//! 3. Then `{"phase":"drained"}` is written over the format object by the content tag it had when
//!    the step began, so that a `--cancel` that landed meanwhile (or any other change) wins: the
//!    write fails and nothing is done. It is the record the release before the entries writes, and
//!    the record `--drain` over `active-marks` writes, so the verbs that follow read one `drained`.
//!
//! Nothing here asks the stores anything but the objects of tierstore.

use std::time::Duration;

use super::{EntryReconciler, StepReport};
use crate::Error;
use crate::format::{Format, FormatPhase, format_body};
use crate::journal::{FORMAT_KEY, PROXIES_PREFIX};
use crate::registration::Registration;

impl EntryReconciler {
    /// Writes `drained` if the pause is at rest, as the module says. `tag` is the content tag of
    /// the format object at the start of the step. A call that finds the pause not at rest, or the
    /// format object changed, does nothing; a call whose store calls fail notes them in `report`,
    /// and so does a step that has failed already.
    pub(super) async fn settle_drain(&mut self, tag: &str, report: &mut StepReport) {
        if !report.failed.is_empty() {
            return;
        }
        match self.registrations_at_rest().await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                report.failed.push(error);
                return;
            }
        }
        match self.entries.list(self.config.base.page_size).await {
            Ok(open) if open.is_empty() => {}
            Ok(_) => return,
            Err(error) => {
                report.failed.push(error);
                return;
            }
        }
        let drained = Format {
            phase: FormatPhase::Drained,
            epoch: None,
            marks_bucket: None,
        };
        match self
            .hot
            .put_if_match(FORMAT_KEY, format_body(&drained), tag)
            .await
        {
            Ok(_) => report.drained = true,
            // The object is not as the step read it: the pause was cancelled or replaced, and that
            // is for the next step to read.
            Err(Error::AlreadyExists(_) | Error::NotFound(_) | Error::PreconditionFailed(_)) => {}
            Err(error) => report.failed.push(error),
        }
    }

    /// Whether every registration is of a departed process or reports a pause and no write in
    /// flight, judged on a listing made now.
    async fn registrations_at_rest(&mut self) -> Result<bool, Error> {
        let listed = self.hot.list(PROXIES_PREFIX).await?;
        let now = self.config.base.mono.now();
        self.registrations.observe(now, &listed);
        let wait: Duration = self.config.departure;
        for object in &listed {
            let id = object
                .key
                .strip_prefix(PROXIES_PREFIX)
                .unwrap_or(&object.key);
            if self.registrations.departed(id, now, wait) {
                continue;
            }
            let body = match self.hot.get(&object.key, None).await {
                Ok(body) => body,
                // Deleted since the listing: its process is gone.
                Err(Error::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            let quiet = Registration::decode(&body).is_some_and(|found| {
                matches!(found.phase, Some(FormatPhase::Drain | FormatPhase::Drained))
                    && found.writes_in_flight == Some(0)
            });
            if !quiet {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
