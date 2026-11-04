//! The operator messages of the server: a `tracing` subscriber that writes each event to stderr
//! as one line, and the policy that says what is logged at which level.
//!
//! The server says what it does with `tracing::info!`, `warn!` and `error!`, and this subscriber,
//! which the binary installs before anything else runs, turns each event into the line an operator
//! reads in the journal. Only events of the
//! `tierstore` crate are written; events of other crates (the S3 client and its runtime, the
//! object-store backends) are dropped, so the operator sees the same lines as before. A line is the event's message and
//! then its fields as ` name=value`, with no time, no level and no target: the journal stamps
//! the time, and the message texts, which already begin with `tierstore: `, are what the pages
//! quote. There is no flag and no environment variable for it.
//!
//! The levels:
//!
//! - `error`: the process cannot do what it was started for, and says why before it stops or
//!   gives that part up.
//! - `warn`: something went wrong or was left out and the process goes on: a connection closed
//!   for a stall or a failed handshake, an accept that failed, a scrub that dropped an object or
//!   a tick that failed, a lock that could not be tried, an object that does not hold its digest.
//! - `info`: what the process did on purpose: the start line, a mode change, a sweep's result,
//!   the drain's counts, a test build's notice.
//! - `debug` and `trace`: for the developer. Events below `info` are dropped here, so a library
//!   may say more at `debug` without the operator seeing it.
//!
//! The output of a command that answers the operator who ran it (`--version`,
//! `--capabilities`, `--print-config`) is not logging and stays on stdout.

use std::fmt::{Debug, Write as _};
use std::io::{self, Write};
use std::sync::{Mutex, PoisonError};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::SetGlobalDefaultError;
use tracing::{Event, Level, Metadata, Subscriber};

/// Installs the stderr subscriber for the process, showing `info` and above. A process that
/// already has a global subscriber keeps it.
pub(crate) fn install() {
    let _ = try_install();
}

/// Installs the stderr subscriber as the global default; an error says one is already set.
pub(super) fn try_install() -> Result<(), SetGlobalDefaultError> {
    tracing::subscriber::set_global_default(LineLog::new(io::stderr(), Level::INFO))
}

/// Says each line of `text` as a `warn` event: the notes a drain ends with, each of which is
/// empty when there is nothing to say.
pub(crate) fn notes(text: &str) {
    for line in text.lines() {
        tracing::warn!("{line}");
    }
}

/// Writes `text` to standard output: the answer of a command, or the start line a supervisor
/// reads, which is not logging. Like `print!`, it panics when standard output cannot be written.
#[expect(
    clippy::disallowed_macros,
    reason = "an answer on standard output, which is not logging"
)]
pub(crate) fn answer(text: &str) {
    print!("{text}");
}

/// A subscriber that writes one line to `W` for each event at `max` or more severe.
pub(crate) struct LineLog<W> {
    out: Mutex<W>,
    max: Level,
}

impl<W: Write> LineLog<W> {
    /// A subscriber over `out` that shows events at `max` and more severe ones.
    pub(crate) fn new(out: W, max: Level) -> Self {
        Self {
            out: Mutex::new(out),
            max,
        }
    }
}

/// The message of an event and its other fields, as they are written.
#[derive(Default)]
struct Line {
    message: String,
    fields: String,
}

impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        // Writing to a `String` does not fail.
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

impl<W: Write + Send + 'static> Subscriber for LineLog<W> {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        // A more verbose level compares greater. Only this crate's events are written.
        let target = metadata.target();
        let ours = target == "tierstore" || target.starts_with("tierstore::");
        ours && *metadata.level() <= self.max
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        // Spans are not written; one id stands for all of them.
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut line = Line::default();
        event.record(&mut line);
        let text = format!("{}{}\n", line.message, line.fields);
        // A stderr that cannot be written to has nowhere to say so.
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = out.write_all(text.as_bytes());
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}
