//! `tierstore format-epoch`: the operator's verb for the one planned pause of writes, which a
//! change of the stored format needs (see the module [`format`](crate::format)).
//!
//! ```text
//! tierstore format-epoch --check    --hot <tier> [--departed <id>]...
//! tierstore format-epoch --drain    --hot <tier> [--departed <id>]... [--apply]
//! tierstore format-epoch --cancel   --hot <tier> [--apply]
//! tierstore format-epoch --activate --hot <tier> [--departed <id>]... [--apply]
//! tierstore format-epoch --activate-marks  --hot <tier> [--marks-bucket <name>] --old-release-fenced [--apply]
//! tierstore format-epoch --restore-entries --hot <tier> [--marks-bucket <name>] [--apply]
//! ```
//!
//! - `--check` reads the format object, the registrations and the open entries, prints them, and
//!   says whether `--drain` would be accepted. It writes nothing and asks the store nothing but
//!   the objects of tierstore.
//! - `--drain` writes `{"phase":"drain"}` and waits, with no time limit, for `drained`. Over an
//!   `active` format at epoch 2 (the entry protocol of this release) the pause keeps the epoch,
//!   and the reconciler of this release that holds the lease writes `drained` once `_entries/` is
//!   empty and every registration that has not departed reports a pause and no write in flight;
//!   the verb prints what it waits for at every poll. Over a store with journals to apply (no
//!   epoch) the reconciler of the bridge release, the one before the entries, writes it, after it
//!   applies the journals. It refuses while a registration that is not running is not named on
//!   `--departed`: a registration of a release before the heartbeat has no heartbeat, only the
//!   operator can say its process is gone, and a wrong answer lets an old writer go on during the
//!   pause. A drain that was begun is resumed by running it again.
//! - `--cancel` puts back the format that was before the drain (the object is deleted, or, if the
//!   store was at an epoch, `active` of that epoch is written), so that writes resume.
//! - `--activate` writes `{"epoch":2,"phase":"active"}` over `drained`, once every registration
//!   that is running has protocol 2 or later. On a store with no format object (a new store) it
//!   creates the object, if the store has no journal object of the releases before the entries
//!   (`_journal/`): a store that has one is drained by the bridge release first.
//!
//! - `--activate-marks`, `--restore-entries` and `--drain` over `active-marks` move the store onto
//!   the markers of `docs/design/put-without-lock.md` and back; they are the module [`marks`].
//!
//! Without `--apply`, the ones that write check, print what they would write and write nothing;
//! `--drain` still waits if the format already says `drain`, since waiting changes nothing.

mod marks;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser};

use self::marks::Marks;
use super::registrations::{
    self, Registration, check_departed, check_protocol, holding_entry_drain,
};
use super::{Hot, VerbError, block_on, finished, parse_duration, parsed};
use crate::format::{Format, FormatPhase, format_body, read_format_tagged};
use crate::journal::{ENTRIES_PREFIX, FORMAT_KEY, JOURNAL_PREFIX, PROXIES_PREFIX};
use crate::{Backend, Clock, Error, Sleeper};

/// The epoch `--activate` writes: the number of the format of the protocol with one journal entry
/// for each key.
const ACTIVE_EPOCH: u64 = 2;

/// The first epoch above [`ACTIVE_EPOCH`].
const ACTIVE_EPOCH_ABOVE: u64 = ACTIVE_EPOCH + 1;

/// The first protocol number of a registration of a process that writes the format of
/// [`ACTIVE_EPOCH`].
const ACTIVE_PROTO: u32 = 2;

/// What to say when the format object is not the one that was read.
const CHANGED: &str =
    "the format object changed while it was being written; run --check and try again";

/// What to say of a phase that this release does not know.
const UNKNOWN: &str = "the format object has a phase this release does not know; use the tool of the release that wrote it";

/// The flags of `tierstore format-epoch`.
#[derive(Parser, Debug)]
#[command(
    name = "tierstore format-epoch",
    about = "Pause and resume writes for a change of the stored format"
)]
pub(super) struct Cli {
    #[command(flatten)]
    hot: Hot,
    #[command(flatten)]
    action: Action,
    /// Write: without it `--drain`, `--cancel` and `--activate` check and print what they would
    /// write.
    #[arg(long)]
    apply: bool,
    /// The marks bucket of the hot store, `hot_store.marks_bucket`, for an `s3:<bucket>` hot tier:
    /// `<hot bucket>-marks` unless named. The verbs of the markers use it, and
    /// `--activate-marks` records it in the format object.
    #[arg(long, value_name = "NAME")]
    marks_bucket: Option<String>,
    /// Say that the credential the release before the markers uses has been rotated, so that a
    /// process of it that starts after `--activate-marks` cannot reach the store.
    #[arg(long)]
    old_release_fenced: bool,
    /// The id of a registration whose process the operator has checked is not running; repeat the
    /// flag or separate ids by commas.
    #[arg(long, value_name = "ID", value_delimiter = ',')]
    departed: Vec<String>,
    /// The interval between the heartbeats of a proxy: a number and `ms`, `s`, `m` or `h`.
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    beat: Duration,
    /// How many beats old a heartbeat may be for its process to count as running.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..))]
    beats: u32,
    /// How often `--drain` reads the format object while it waits: a number and `ms`, `s`, `m`
    /// or `h`.
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    poll: Duration,
}

/// The one thing the verb does.
#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
struct Action {
    /// Print the state and whether `--drain` would be accepted.
    #[arg(long)]
    check: bool,
    /// Pause writes and wait until the journals are applied.
    #[arg(long)]
    drain: bool,
    /// Resume writes: put back the format that was before `--drain`.
    #[arg(long)]
    cancel: bool,
    /// Write the active epoch over `drained`.
    #[arg(long)]
    activate: bool,
    /// Write `active-marks` over `drained`: writes are made on markers from then.
    #[arg(long)]
    activate_marks: bool,
    /// Write the active epoch 2 over `drained` once the markers are done: the way back.
    #[arg(long)]
    restore_entries: bool,
}

/// What the format object is, with the content tag it was read at.
type Found = Option<(Format, String)>;

/// What the verb works with.
struct Work<'a> {
    cli: &'a Cli,
    hot: &'a dyn Backend,
    clock: &'a dyn Clock,
    sleeper: &'a dyn Sleeper,
    /// The marks bucket of the hot store; none for a hot tier that has no marks bucket.
    marks: Option<Marks<'a>>,
}

/// The verb: `args` are the program name, `format-epoch` and the flags. Returns the exit status:
/// 0, 1 for a refusal or a failed call (the reason on `err`), 2 for a flag that is not valid.
pub(super) fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
) -> ExitCode {
    let cli: Cli = match parsed(super::FORMAT_EPOCH, args, out, err) {
        Ok(cli) => cli,
        Err(status) => return status,
    };
    let opened = cli
        .hot
        .open()
        .and_then(|hot| Ok((hot, cli.hot.open_marks(cli.marks_bucket.as_deref())?)));
    let result = match opened {
        Ok((hot, marks)) => block_on(act(
            Work {
                cli: &cli,
                hot: hot.as_ref(),
                clock,
                sleeper,
                marks: marks.as_ref().map(|(name, store)| Marks {
                    store: store.as_ref(),
                    name,
                }),
            },
            out,
        )),
        Err(why) => Err(why),
    };
    finished(super::FORMAT_EPOCH, result, err)
}

async fn act(work: Work<'_>, out: &mut dyn Write) -> Result<(), VerbError> {
    let action = &work.cli.action;
    if action.check {
        check(&work, out).await
    } else if action.drain {
        drain(&work, out).await
    } else if action.cancel {
        cancel(&work, out).await
    } else if action.activate_marks {
        marks::activate(&work, out).await
    } else if action.restore_entries {
        marks::restore(&work, out).await
    } else {
        activate(&work, out).await
    }
}

/// The name of the phase in `found`, for the operator.
fn phase_name(found: &Found) -> &'static str {
    match found.as_ref().map(|(format, _)| format.phase) {
        None => "absent",
        Some(FormatPhase::Drain) => "drain",
        Some(FormatPhase::Drained) => "drained",
        Some(FormatPhase::Active) => "active",
        Some(FormatPhase::ActiveMarks) => "active-marks",
        Some(FormatPhase::Unknown) => "unknown",
    }
}

async fn current(hot: &dyn Backend) -> Result<Found, VerbError> {
    Ok(read_format_tagged(hot).await?)
}

async fn registrations_of(work: &Work<'_>) -> Result<Vec<Registration>, VerbError> {
    let fresh = work.cli.beat.saturating_mul(work.cli.beats);
    Ok(registrations::read(work.hot, work.clock.now(), fresh).await?)
}

async fn count(hot: &dyn Backend, prefix: &str) -> Result<usize, VerbError> {
    Ok(hot.list(prefix).await?.len())
}

/// What `--drain` checks before it writes: the phase is one this release knows and leaves, and every
/// registration is running or named as departed.
fn drain_gate(
    found: &Found,
    registrations: &[Registration],
    departed: &[String],
) -> Result<(), VerbError> {
    if phase_name(found) == "unknown" {
        return Err(VerbError::Refused(UNKNOWN.to_owned()));
    }
    // No verb writes `active` above the epoch of the entries without the markers: a proxy would
    // run entries under it, and the leader would not finish the pause the drain copies from it.
    if let Some((
        Format {
            phase: FormatPhase::Active,
            epoch: Some(epoch @ (ACTIVE_EPOCH_ABOVE..)),
            ..
        },
        _,
    )) = found
    {
        return Err(VerbError::Refused(format!(
            "refusing to drain: the format says active at epoch {epoch}, and the entry protocol of \
             this release is epoch {ACTIVE_EPOCH}; no verb writes that record, so look at who \
             did before changing it"
        )));
    }
    check_departed(registrations, departed, "drain")
}

/// Says why the wait goes on, and waits one poll.
async fn still_waiting(work: &Work<'_>, out: &mut dyn Write, why: &str) -> Result<(), VerbError> {
    writeln!(out, "still waiting: {why}")?;
    work.sleeper.sleep(work.cli.poll).await;
    Ok(())
}

async fn check(work: &Work<'_>, out: &mut dyn Write) -> Result<(), VerbError> {
    let found = current(work.hot).await?;
    let registrations = registrations_of(work).await?;
    let epoch = found.as_ref().and_then(|(format, _)| format.epoch);
    match epoch {
        Some(epoch) => writeln!(out, "format: {} (epoch {epoch})", phase_name(&found))?,
        None => writeln!(out, "format: {}", phase_name(&found))?,
    }
    writeln!(out, "registrations: {}", registrations.len())?;
    for registration in &registrations {
        writeln!(out, "  {}: {}", registration.id, registration.describe())?;
    }
    writeln!(
        out,
        "open entries: {}",
        count(work.hot, ENTRIES_PREFIX).await?
    )?;
    drain_gate(&found, &registrations, &work.cli.departed)?;
    writeln!(out, "--drain would be accepted")?;
    Ok(())
}

async fn drain(work: &Work<'_>, out: &mut dyn Write) -> Result<(), VerbError> {
    let found = current(work.hot).await?;
    match found.as_ref() {
        Some((format, _)) if format.phase == FormatPhase::Drained => {
            writeln!(
                out,
                "the format is already drained: writes are paused and every journal is applied"
            )?;
            return Ok(());
        }
        Some((format, _)) if format.rolls_back_marks() => {
            return marks::resume_rollback(work, format, out).await;
        }
        Some((format, tag)) if format.phase == FormatPhase::ActiveMarks => {
            return marks::begin_rollback(work, format, tag, out).await;
        }
        Some((format, _)) if format.phase == FormatPhase::Drain => {
            writeln!(out, "the format already says drain")?;
        }
        _ => {
            let registrations = registrations_of(work).await?;
            drain_gate(&found, &registrations, &work.cli.departed)?;
            if !work.cli.apply {
                writeln!(
                    out,
                    "dry run: would write the format {{\"phase\":\"drain\"}}; nothing was written; add --apply to write it"
                )?;
                return Ok(());
            }
            let epoch = found.as_ref().and_then(|(format, _)| format.epoch);
            let body = format_body(&Format {
                phase: FormatPhase::Drain,
                epoch,
                marks_bucket: None,
            });
            let wrote = match &found {
                Some((_, tag)) => work.hot.put_if_match(FORMAT_KEY, body, tag).await.map(drop),
                None => work.hot.put(FORMAT_KEY, body).await,
            };
            changed(wrote)?;
            writeln!(out, "wrote the format: drain")?;
        }
    }
    wait(work, out).await
}

/// A write of the format object that did not land because the object was not as read is a
/// refusal; any other failure is the store's.
fn changed(wrote: Result<(), Error>) -> Result<(), VerbError> {
    match wrote {
        Ok(()) => Ok(()),
        Err(Error::AlreadyExists(_) | Error::NotFound(_) | Error::PreconditionFailed(_)) => {
            Err(VerbError::Refused(CHANGED.to_owned()))
        }
        Err(other) => Err(other.into()),
    }
}

/// Reads the format object until it says `drained`, and says what it waits for.
async fn wait(work: &Work<'_>, out: &mut dyn Write) -> Result<(), VerbError> {
    let over_entries = current(work.hot)
        .await?
        .is_some_and(|(format, _)| format.drains_entries());
    let leader = if over_entries {
        "once `_entries/` is empty and every registration that has not departed reports a pause \
         and no write in flight"
    } else {
        "once every journal is applied and the format has said drain for the settle time"
    };
    writeln!(
        out,
        "waiting for the format to say drained: the reconciler that holds the lease writes it {leader}; \
         there is no time limit, interrupt to stop and --cancel to resume writes"
    )?;
    loop {
        let found = current(work.hot).await?;
        match found.as_ref().map(|(format, _)| format.phase) {
            Some(FormatPhase::Drained) => {
                writeln!(
                    out,
                    "drained: writes are paused and every journal is applied"
                )?;
                return Ok(());
            }
            Some(FormatPhase::Drain) => {}
            _ => {
                return Err(VerbError::Refused(format!(
                    "not waiting: the format is {}, so the pause was lifted by someone else",
                    phase_name(&found)
                )));
            }
        }
        let why = if over_entries {
            holding(work).await?
        } else {
            let registered = count(work.hot, PROXIES_PREFIX).await?;
            format!("the format says drain; {registered} registrations")
        };
        still_waiting(work, out, &why).await?;
    }
}

/// What the leader's `drained` waits for under the pause over the entry protocol, for the
/// operator: the open entries and the registrations that may still write.
async fn holding(work: &Work<'_>) -> Result<String, VerbError> {
    let entries = count(work.hot, ENTRIES_PREFIX).await?;
    let registrations = registrations_of(work).await?;
    let mut why = vec![format!("{entries} open entries")];
    why.extend(holding_entry_drain(&registrations));
    Ok(why.join("; "))
}

async fn cancel(work: &Work<'_>, out: &mut dyn Write) -> Result<(), VerbError> {
    let found = current(work.hot).await?;
    let paused = matches!(
        found.as_ref().map(|(format, _)| format.phase),
        Some(FormatPhase::Drain | FormatPhase::Drained)
    );
    let Some((format, tag)) = found.as_ref().filter(|_| paused) else {
        return Err(VerbError::Refused(format!(
            "nothing to cancel: the format is {}, not drain or drained",
            phase_name(&found)
        )));
    };
    // The pause of a rollback from `active-marks` is put back as `active-marks`: `active` at its
    // epoch 3 would make the proxies write on entries beside the markers.
    let restored = if format.rolls_back_marks() {
        Some(Format {
            phase: FormatPhase::ActiveMarks,
            epoch: format.epoch,
            marks_bucket: format.marks_bucket.clone(),
        })
    } else {
        format.epoch.map(|epoch| Format {
            phase: FormatPhase::Active,
            epoch: Some(epoch),
            marks_bucket: None,
        })
    };
    if !work.cli.apply {
        let what = match &restored {
            Some(_) => "write the active format back",
            None => "delete the format object",
        };
        writeln!(
            out,
            "dry run: would {what}; nothing was written; add --apply to do it"
        )?;
        return Ok(());
    }
    let done = match restored {
        Some(active) => work
            .hot
            .put_if_match(FORMAT_KEY, format_body(&active), tag)
            .await
            .map(drop),
        None => work.hot.delete_if_match(FORMAT_KEY, tag).await,
    };
    changed(done)?;
    writeln!(
        out,
        "cancelled: writes resume as every proxy reads the format again"
    )?;
    Ok(())
}

async fn activate(work: &Work<'_>, out: &mut dyn Write) -> Result<(), VerbError> {
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
        Some((format, tag)) if format.phase == FormatPhase::Drained => Some(tag.as_str()),
        Some(_) => return Err(not_drained(&found)),
        None => {
            // A new store, or one that was never drained: only a store with no journal object has
            // nothing that the entries would leave unapplied.
            let journals = count(work.hot, JOURNAL_PREFIX).await?;
            if journals > 0 {
                return Err(VerbError::Refused(format!(
                    "refusing to activate: there is no format object and the store has {journals} \
                     journal objects of a release before the entries; drain it with the bridge \
                     release first"
                )));
            }
            None
        }
    };
    let registrations = registrations_of(work).await?;
    check_departed(&registrations, &work.cli.departed, "activate")?;
    check_protocol(&registrations, ACTIVE_PROTO)?;
    if !work.cli.apply {
        writeln!(
            out,
            "dry run: would write the format {{\"epoch\":{ACTIVE_EPOCH},\"phase\":\"active\"}}; nothing was written; add --apply to write it"
        )?;
        return Ok(());
    }
    let body = format_body(&active);
    let wrote = match tag {
        Some(tag) => work.hot.put_if_match(FORMAT_KEY, body, tag).await.map(drop),
        None => work.hot.put(FORMAT_KEY, body).await,
    };
    changed(wrote)?;
    writeln!(out, "wrote the format: active, epoch {ACTIVE_EPOCH}")?;
    writeln!(
        out,
        "not done by this verb: rotate the old write credential, so that a restarted process of \
         the old release cannot write; processes of the new release get the new credential first"
    )?;
    Ok(())
}

fn not_drained(found: &Found) -> VerbError {
    VerbError::Refused(format!(
        "refusing to activate: the format is {}, not drained",
        phase_name(found)
    ))
}

#[cfg(test)]
#[path = "format_epoch_tests.rs"]
mod tests;
