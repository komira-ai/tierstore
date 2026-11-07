//! The three steps of `tierstore format-epoch` that move a store onto the markers of
//! `docs/design/put-without-lock.md` and back (8.1 and 8.2 there):
//!
//! - `--activate-marks` writes `{"epoch":3,"marks_bucket":"<name>","phase":"active-marks"}` over
//!   `drained`, once everything the markers need is in place;
//! - `--drain` over `active-marks` is the way back: it writes `drain` with the marks bucket still
//!   named, waits for every proxy to be at rest and for the leader to finish the markers, and
//!   writes `drained`;
//! - `--restore-entries` writes `{"epoch":2,"phase":"active"}` over `drained`, once no proxy can
//!   write a marker and the marks bucket is empty.
//!
//! None of them writes anything but the format object (and the rollback deletes the `WANT` markers
//! that ask for a copy-back, which owe nothing). Each checks all it needs before it writes, as
//! the verbs of the module above do, and without `--apply` it only reports.

use std::io::Write;

use super::super::VerbError;
use super::super::registrations::{
    Registration, check_departed, check_protocol, not_at_rest, not_drained,
};
use super::{
    ACTIVE_EPOCH, Found, Work, changed, count, current, phase_name, registrations_of, still_waiting,
};
use crate::Backend;
use crate::Error;
use crate::format::{Format, FormatPhase, MARKS_EPOCH, format_body};
use crate::journal::{ENTRIES_PREFIX, FORMAT_KEY, JOURNAL_PREFIX};
use crate::marker::{MarkerState, decode_marker};
use crate::registration::PROTO_MARKS;

/// The marks bucket of the hot store, as the verb reaches it.
pub(super) struct Marks<'a> {
    /// The bucket itself.
    pub(super) store: &'a dyn Backend,
    /// The name the format record gives it (`hot_store.marks_bucket`).
    pub(super) name: &'a str,
}

/// How many markers the verb lists at once.
const PAGE: usize = 1000;

/// What to say when the hot tier has no marks bucket in this release.
const NO_MARKS: &str = "this hot tier has no marks bucket in this release (a `dir:<path>` hot \
                        tier cannot run the markers); the markers need an `s3:<bucket>` hot tier";

fn refused(why: impl Into<String>) -> VerbError {
    VerbError::Refused(why.into())
}

/// The marks bucket the flags name.
fn marks_of<'a>(work: &'a Work<'_>) -> Result<&'a Marks<'a>, VerbError> {
    work.marks.as_ref().ok_or_else(|| refused(NO_MARKS))
}

/// Refuses unless the marks bucket can be listed, and holds no object. A bucket that cannot be
/// listed does not exist or is not granted; one that holds an object is not the empty bucket the
/// activation and the end of the rollback need.
async fn require_empty(marks: &Marks<'_>, then: &str) -> Result<(), VerbError> {
    let name = marks.name;
    let page = marks.store.list_page("", None, 1).await.map_err(|error| {
        refused(format!(
            "the marks bucket {name:?} cannot be listed ({error}): create it on the hot store and \
             grant the credentials of the proxies list, read, write and delete on it"
        ))
    })?;
    if let Some(first) = page.first() {
        return Err(refused(format!(
            "refusing to {then}: the marks bucket {name:?} holds an object, {:?}; the markers are \
             run to completion by `--drain` over `active-marks` before `drained` is written, so \
             what is left was not put there by a rollback (a `WANT` marker, which owes nothing, \
             may be deleted by hand; look at the others first)",
            first.key
        )));
    }
    Ok(())
}

/// Refuses while an entry or a journal object of the earlier protocols exists: under the markers
/// none is written, and the release that reads them would act on a leftover.
async fn require_no_entries(work: &Work<'_>, then: &str) -> Result<(), VerbError> {
    let entries = count(work.hot, ENTRIES_PREFIX).await?;
    if entries > 0 {
        return Err(refused(format!(
            "refusing to {then}: {entries} entries under _entries/ are held; the entry protocol \
             finishes every one at the drain, so look at them with `tierstore entries ls`"
        )));
    }
    let journals = count(work.hot, JOURNAL_PREFIX).await?;
    if journals > 0 {
        return Err(refused(format!(
            "refusing to {then}: the store has {journals} journal objects of a release before the \
             entries; drain it with the bridge release first"
        )));
    }
    Ok(())
}

/// The text of the rotation the activation takes for done when `--old-release-fenced` is passed.
const FENCE: &str = "an old release that starts after the activation does not read markers on a hot \
                     miss, so it serves the cold copy of a key that was deleted; rotate the \
                     credential it uses on the hot and the cold store, giving the new one to the \
                     processes of this release first, so that a restarted old process cannot \
                     reach the store, and then pass --old-release-fenced";

/// `--activate-marks`.
pub(super) async fn activate(work: &Work<'_>, out: &mut dyn Write) -> Result<(), VerbError> {
    let marks = marks_of(work)?;
    let found = current(work.hot).await?;
    let want = Format {
        phase: FormatPhase::ActiveMarks,
        epoch: Some(MARKS_EPOCH),
        marks_bucket: Some(marks.name.to_owned()),
    };
    let tag = match found.as_ref() {
        Some((format, _)) if *format == want => {
            writeln!(
                out,
                "the format is already active-marks at epoch {MARKS_EPOCH}, in the marks bucket {:?}",
                marks.name
            )?;
            return Ok(());
        }
        Some((format, tag)) if format.phase == FormatPhase::Drained => tag.as_str(),
        _ => return Err(not_drained_format(&found)),
    };
    writeln!(out, "format: drained")?;
    let registrations = registrations_of(work).await?;
    check_departed(&registrations, &work.cli.departed, "activate marks")?;
    check_protocol(&registrations, PROTO_MARKS)?;
    let unseen = not_drained(&registrations);
    if !unseen.is_empty() {
        return Err(refused(format!(
            "refusing to activate marks: proxies that have not reported the phase drained, so \
             that may still skip the marker on a hot miss: {}; run it again after their next \
             beat",
            unseen.join(", ")
        )));
    }
    // Every live registration acts on `drained`, so what `not_at_rest` still names is a write that
    // was admitted before its proxy saw the phase, and may make its entry after the format
    // says `active-marks`, with no marker (`docs/design/put-without-lock.md`, 8.2).
    let writing = not_at_rest(&registrations);
    if !writing.is_empty() {
        return Err(refused(format!(
            "refusing to activate marks: proxies that may still be finishing a write begun \
             before the pause: {}; run it again after their next beat",
            writing.join("; ")
        )));
    }
    writeln!(
        out,
        "registrations: {} live, all protocol {PROTO_MARKS} or later, acting on drained and with \
         no write in flight",
        live(&registrations)
    )?;
    require_no_entries(work, "activate marks").await?;
    writeln!(out, "no entry and no journal object is held")?;
    require_empty(marks, "activate marks").await?;
    writeln!(
        out,
        "marks bucket: {:?} exists and is empty (it is not the hot bucket)",
        marks.name
    )?;
    if !work.cli.old_release_fenced {
        return Err(refused(format!(
            "refusing to activate marks: the old release is not fenced: {FENCE}"
        )));
    }
    writeln!(
        out,
        "the old release is fenced (asserted by --old-release-fenced)"
    )?;
    let record = String::from_utf8_lossy(&format_body(&want)).into_owned();
    if !work.cli.apply {
        writeln!(
            out,
            "dry run: would write the format {record}; nothing was written; add --apply to write it"
        )?;
        return Ok(());
    }
    changed(
        work.hot
            .put_if_match(FORMAT_KEY, format_body(&want), tag)
            .await
            .map(drop),
    )?;
    writeln!(
        out,
        "wrote the format: active-marks, epoch {MARKS_EPOCH}, marks bucket {:?}",
        marks.name
    )?;
    writeln!(
        out,
        "the proxies take it up within their format_refresh (5 s by default) and write on markers \
         from then; until each does it answers writes 503. To go back: `--drain --apply`, then \
         `--restore-entries --apply`"
    )?;
    Ok(())
}

/// The number of live registrations.
fn live(registrations: &[Registration]) -> usize {
    registrations
        .iter()
        .filter(|found| matches!(found.class, super::super::registrations::Class::Live { .. }))
        .count()
}

fn not_drained_format(found: &Found) -> VerbError {
    let named = match found
        .as_ref()
        .and_then(|(format, _)| format.marks_bucket.as_deref())
    {
        Some(bucket) => format!(" (it names the marks bucket {bucket:?})"),
        None => String::new(),
    };
    refused(format!(
        "refusing to activate marks: the format is {}{named}, not drained; `--drain --apply` \
         first (the one planned pause of writes), roll the release that knows markers, and run \
         this again",
        phase_name(found)
    ))
}

/// `--restore-entries`.
pub(super) async fn restore(work: &Work<'_>, out: &mut dyn Write) -> Result<(), VerbError> {
    let marks = marks_of(work)?;
    let found = current(work.hot).await?;
    let active = Format {
        phase: FormatPhase::Active,
        epoch: Some(ACTIVE_EPOCH),
        marks_bucket: None,
    };
    let tag = match found.as_ref() {
        Some((format, _)) if *format == active => {
            writeln!(out, "the format is already active at epoch {ACTIVE_EPOCH}")?;
            return Ok(());
        }
        Some((format, tag)) if format.phase == FormatPhase::Drained => tag.as_str(),
        _ => {
            return Err(refused(format!(
                "refusing to restore entries: the format is {}, not drained; to leave \
                 active-marks run `--drain --apply` and let it write drained",
                phase_name(&found)
            )));
        }
    };
    writeln!(out, "format: drained")?;
    let registrations = registrations_of(work).await?;
    check_departed(&registrations, &work.cli.departed, "restore entries")?;
    let busy = not_at_rest(&registrations);
    if !busy.is_empty() {
        return Err(refused(format!(
            "refusing to restore entries: proxies that may still write a marker: {}",
            busy.join("; ")
        )));
    }
    writeln!(
        out,
        "registrations: {} live, none acting on active-marks and none with a write in flight",
        live(&registrations)
    )?;
    require_no_entries(work, "restore entries").await?;
    require_empty(marks, "restore entries").await?;
    writeln!(out, "marks bucket: {:?} is empty", marks.name)?;
    if !work.cli.apply {
        writeln!(
            out,
            "dry run: would write the format {}; nothing was written; add --apply to write it",
            String::from_utf8_lossy(&format_body(&active))
        )?;
        return Ok(());
    }
    changed(
        work.hot
            .put_if_match(FORMAT_KEY, format_body(&active), tag)
            .await
            .map(drop),
    )?;
    writeln!(out, "wrote the format: active, epoch {ACTIVE_EPOCH}")?;
    writeln!(
        out,
        "not done by this verb: cold must be up, since the entry protocol copies to it; this \
         release still runs the entry protocol, so its proxies go on writing on entries, and the \
         release before the markers can run again"
    )?;
    Ok(())
}

/// The marks bucket the record names must be the one the flags name, or the verb would look at
/// another bucket than the leader works.
fn same_bucket(format: &Format, marks: &Marks<'_>) -> Result<(), VerbError> {
    match format.marks_bucket.as_deref() {
        Some(recorded) if recorded == marks.name => Ok(()),
        recorded => Err(refused(format!(
            "the format record names the marks bucket {recorded:?}, and the flags name {:?}; pass \
             the bucket of hot_store.marks_bucket (--marks-bucket)",
            marks.name
        ))),
    }
}

/// `--drain` over `active-marks`, whose record is `format` at `tag`: writes `drain` with the
/// bucket still named, and goes on as [`finish`].
pub(super) async fn begin_rollback(
    work: &Work<'_>,
    format: &Format,
    tag: &str,
    out: &mut dyn Write,
) -> Result<(), VerbError> {
    let marks = marks_of(work)?;
    same_bucket(format, marks)?;
    let registrations = registrations_of(work).await?;
    check_departed(&registrations, &work.cli.departed, "drain")?;
    let drain = Format {
        phase: FormatPhase::Drain,
        epoch: format.epoch,
        marks_bucket: format.marks_bucket.clone(),
    };
    if !work.cli.apply {
        writeln!(
            out,
            "dry run: would write the format drain over active-marks, keeping the marks bucket \
             {:?} in it, and wait until the proxies are at rest and the markers are done; nothing \
             was written; add --apply to do it",
            marks.name
        )?;
        return Ok(());
    }
    changed(
        work.hot
            .put_if_match(FORMAT_KEY, format_body(&drain), tag)
            .await
            .map(drop),
    )?;
    writeln!(out, "wrote the format: drain, over active-marks")?;
    finish(work, marks, out).await
}

/// `--drain` over a record that says `drain` and names the marks bucket: the rollback was begun,
/// and is resumed.
pub(super) async fn resume_rollback(
    work: &Work<'_>,
    format: &Format,
    out: &mut dyn Write,
) -> Result<(), VerbError> {
    let marks = marks_of(work)?;
    same_bucket(format, marks)?;
    writeln!(out, "the format already says drain, over active-marks")?;
    if !work.cli.apply {
        writeln!(
            out,
            "dry run: would wait until the proxies are at rest and the markers are done, and \
             write drained; nothing was written; add --apply to do it"
        )?;
        return Ok(());
    }
    finish(work, marks, out).await
}

/// Waits, with no time limit, until every live proxy that reads markers reports a phase other than
/// `active-marks` with no write in flight, then until the marks bucket is empty (it deletes the
/// `WANT` markers, which owe nothing, and the leader finishes the others), and then writes
/// `drained`. The order is the point: markers are looked at only after every proxy has said that
/// it makes no more, so none appears after the look that finds none.
async fn finish(work: &Work<'_>, marks: &Marks<'_>, out: &mut dyn Write) -> Result<(), VerbError> {
    writeln!(
        out,
        "waiting until every proxy reports a phase other than active-marks with no write in \
         flight, and the leader has finished the markers of the marks bucket {:?}; there is no \
         time limit, interrupt to stop and --cancel to resume writes",
        marks.name
    )?;
    let mut dropped = 0;
    loop {
        let found = current(work.hot).await?;
        let tag = match found.as_ref() {
            Some((format, tag)) if format.rolls_back_marks() => tag.as_str(),
            Some((format, _)) if format.phase == FormatPhase::Drained => {
                writeln!(out, "drained: the format was written by another run")?;
                return Ok(());
            }
            _ => {
                return Err(refused(format!(
                    "not waiting: the format is {}, so the rollback was changed by someone else",
                    phase_name(&found)
                )));
            }
        };
        let registrations = registrations_of(work).await?;
        check_departed(&registrations, &work.cli.departed, "drain")?;
        let mut waiting = not_at_rest(&registrations);
        if waiting.is_empty() {
            let left = settle(marks).await?;
            dropped += left.dropped;
            if left.total() == 0 {
                let drained = Format {
                    phase: FormatPhase::Drained,
                    epoch: None,
                    marks_bucket: None,
                };
                changed(
                    work.hot
                        .put_if_match(FORMAT_KEY, format_body(&drained), tag)
                        .await
                        .map(drop),
                )?;
                writeln!(
                    out,
                    "drained: no proxy writes a marker, the marks bucket is empty{}. Roll the \
                     release before the markers back if it is to run, and then run \
                     `--restore-entries --apply`",
                    dropped_note(dropped)
                )?;
                return Ok(());
            }
            waiting.push(left.describe());
        }
        still_waiting(work, out, &waiting.join("; ")).await?;
    }
}

/// What a look at the marks bucket found.
#[derive(Debug, Default, PartialEq, Eq)]
struct Left {
    dirty: usize,
    lock: usize,
    clearing: usize,
    /// A state this release does not know, or a body that is not a marker, or a `WANT` that
    /// changed under the delete.
    other: usize,
    /// `WANT` markers deleted.
    dropped: usize,
}

impl Left {
    fn total(&self) -> usize {
        self.dirty + self.lock + self.clearing + self.other
    }

    fn describe(&self) -> String {
        format!(
            "{} markers are not finished (DIRTY {}, LOCK {}, CLEARING {}, other {})",
            self.total(),
            self.dirty,
            self.lock,
            self.clearing,
            self.other
        )
    }
}

/// What the end of the rollback says of the `WANT` markers it deleted.
fn dropped_note(dropped: usize) -> String {
    match dropped {
        0 => String::new(),
        dropped => format!(" ({dropped} WANT markers were deleted)"),
    }
}

/// Reads the markers of the marks bucket and deletes the `WANT` ones (a request for a copy-back
/// owes nothing, and the leader leaves it), by the tag it read. Counts the others.
async fn settle(marks: &Marks<'_>) -> Result<Left, VerbError> {
    let mut left = Left::default();
    let mut after: Option<String> = None;
    loop {
        let page = marks.store.list_page("", after.as_deref(), PAGE).await?;
        for object in &page {
            let (body, tag) = match marks.store.get_tagged(&object.key, None).await {
                Ok(found) => found,
                Err(Error::NotFound(_)) => continue,
                Err(error) => return Err(error.into()),
            };
            match decode_marker(&body).map(|marker| marker.st) {
                Ok(MarkerState::Want) => match marks.store.delete_if_match(&object.key, &tag).await
                {
                    Ok(()) | Err(Error::NotFound(_)) => left.dropped += 1,
                    Err(Error::PreconditionFailed(_)) => left.other += 1,
                    Err(error) => return Err(error.into()),
                },
                Ok(MarkerState::Dirty) => left.dirty += 1,
                Ok(MarkerState::Lock) => left.lock += 1,
                Ok(MarkerState::Clearing) => left.clearing += 1,
                Ok(MarkerState::Other(_)) | Err(_) => left.other += 1,
            }
        }
        after = page.last().map(|last| last.key.clone());
        if page.len() < PAGE {
            return Ok(left);
        }
    }
}

#[cfg(test)]
#[path = "marks_rollback_tests.rs"]
mod rollback_tests;
#[cfg(test)]
#[path = "marks_tests.rs"]
mod tests;
