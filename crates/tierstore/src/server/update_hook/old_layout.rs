//! The refusal of a node that still runs the two-process layout: the S3 process and, beside it,
//! `tierstore fs` as the second unit `tierstore-fs@a` or `tierstore-fs@b`.
//!
//! This release has no step for that unit. It only looks at it (its units' state, its start
//! markers `run-fs-a` and `run-fs-b`, and the record keys `fs_live` and `fs_next`) and, when it
//! finds the layout, refuses with [`Reason::ModeUnsupported`] before anything is saved, started
//! or stopped. The way out is the release that switches the node to the `fs` interface, in the
//! one process of the node's file.

use super::ports::{Family, Instance, SystemdPorts};
use super::record::Record;
use super::report::{Done, Reason};

/// The refusal of a node that still runs the two-process layout, for what was `found` of it.
pub(super) fn refusal(found: &[String]) -> Done {
    Done::fail(
        Reason::ModeUnsupported,
        format!(
            "this node still runs the two-process layout ({}): switch this node to the fs \
             interface with release M first",
            found.join("; ")
        ),
    )
}

/// The units of the second template that run, as the refusal says them.
pub(super) fn running_units(ports: &SystemdPorts<'_>) -> Result<Vec<String>, Done> {
    let mut running = Vec::new();
    for instance in Instance::ALL {
        let state = ports.fs_units.state(instance)?;
        if state.running() {
            let state = format!("{state:?}").to_lowercase();
            running.push(format!("{} is {state}", Family::Fs.unit(instance)));
        }
    }
    Ok(running)
}

/// What shows that the node still runs the two-process layout: a running unit of the second
/// template, a marker of it, or a record that names its instance. Nothing is started or stopped.
fn found(ports: &SystemdPorts<'_>, record: Option<&Record>) -> Result<Vec<String>, Done> {
    let mut found = running_units(ports)?;
    for instance in Instance::ALL {
        if ports.fs_markers.present(instance)? {
            found.push(format!(
                "the marker {} is present",
                Family::Fs.marker_file(instance)
            ));
        }
    }
    if let Some(live) = record.and_then(|record| record.fs_live) {
        found.push(format!("the record names fs_live {live}"));
    }
    Ok(found)
}

/// `Err` when the node still runs the two-process layout, by what its fs units, its fs markers and
/// `record` (the record at rest) say.
pub(super) fn refuse(ports: &SystemdPorts<'_>, record: Option<&Record>) -> Result<(), Done> {
    let found = found(ports, record)?;
    if found.is_empty() {
        Ok(())
    } else {
        Err(refusal(&found))
    }
}

/// `Err` when `record` is of an update in flight that moved the fs process (`fs_next`) or gave it
/// up (`fs_switch`): only an earlier release wrote such a record, and its way back goes through
/// the fs process, which this release does not touch.
pub(super) fn refuse_record(record: &Record) -> Result<(), Done> {
    if !record.updating {
        return Ok(());
    }
    let mut found = Vec::new();
    if let Some(next) = record.fs_next {
        found.push(format!("the update in flight names fs_next {next}"));
    }
    if record.fs_switch {
        found.push("the update in flight says fs_switch".to_owned());
    }
    if found.is_empty() {
        Ok(())
    } else {
        Err(refusal(&found))
    }
}
