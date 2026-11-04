//! `tierstore entries`: the operator's read-only view of the journal entries of keys, the objects
//! `_entries/<sha256 of the key>` in the hot tier (see the module [`entry`](crate::entry)).
//!
//! ```text
//! tierstore entries ls   --hot <tier>
//! tierstore entries show --hot <tier> --key <key>
//! ```
//!
//! `ls` prints one line for each open entry: the name (the SHA-256 of the key in hex), the
//! operation and the state, the owner, the seconds since the entry was last written, and the key.
//! `show` prints one entry, found by the key. Both only read; neither changes an entry.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use bytes::Bytes;
use clap::{Parser, Subcommand};

use super::{Hot, VerbError, block_on, finished, parsed};
use crate::entry::{EntryOp, EntryState, decode_entry, entry_key};
use crate::journal::ENTRIES_PREFIX;
use crate::{Backend, Clock, Error, Meta};

/// The flags of `tierstore entries`.
#[derive(Parser, Debug)]
#[command(
    name = "tierstore entries",
    about = "List and show the journal entries of keys"
)]
struct Cli {
    #[command(subcommand)]
    what: What,
}

/// What to print.
#[derive(Subcommand, Debug)]
enum What {
    /// List every open entry.
    Ls {
        #[command(flatten)]
        hot: Hot,
    },
    /// Show the entry of one key.
    Show {
        #[command(flatten)]
        hot: Hot,
        /// The key of the object the entry is about.
        #[arg(long)]
        key: String,
    },
}

/// The verb: `args` are the program name, `entries` and the rest. Returns the exit status: 0, 1
/// for a failed call or an entry that cannot be read (the reason on `err`), 2 for a flag that is
/// not valid.
pub(super) fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    clock: &dyn Clock,
) -> ExitCode {
    let cli: Cli = match parsed(super::ENTRIES, args, out, err) {
        Ok(cli) => cli,
        Err(status) => return status,
    };
    let (hot, key) = match &cli.what {
        What::Ls { hot } => (hot, None),
        What::Show { hot, key } => (hot, Some(key.as_str())),
    };
    let result = match hot.open() {
        Ok(tier) => block_on(print(tier.as_ref(), clock.now(), key, out)),
        Err(why) => Err(why),
    };
    finished(super::ENTRIES, result, err)
}

async fn print(
    hot: &dyn Backend,
    now: u64,
    key: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), VerbError> {
    match key {
        None => list(hot, now, out).await,
        Some(key) => show(hot, now, key, out).await,
    }
}

fn op_name(op: EntryOp) -> &'static str {
    match op {
        EntryOp::Put => "PUT",
        EntryOp::Del => "DEL",
        EntryOp::Cb => "CB",
        EntryOp::Evict => "EVICT",
        EntryOp::Unknown => "UNKNOWN",
    }
}

fn state_name(state: EntryState) -> &'static str {
    match state {
        EntryState::Held => "HELD",
        EntryState::Pending => "PENDING",
        EntryState::Sealed => "SEALED",
        EntryState::Unknown => "UNKNOWN",
    }
}

async fn list(hot: &dyn Backend, now: u64, out: &mut dyn Write) -> Result<(), VerbError> {
    let mut printed = 0_usize;
    for object in hot.list(ENTRIES_PREFIX).await? {
        let name = object
            .key
            .strip_prefix(ENTRIES_PREFIX)
            .unwrap_or(&object.key);
        let body = match hot.get(&object.key, None).await {
            Ok(body) => body,
            Err(Error::NotFound(_)) => continue,
            Err(error) => return Err(error.into()),
        };
        printed += 1;
        match decode_entry(&body) {
            Ok(entry) => {
                let age = now.saturating_sub(object.modified) / 1000;
                writeln!(
                    out,
                    "{name} {} {} owner={} age={age}s key={:?}",
                    op_name(entry.op),
                    state_name(entry.st),
                    entry.owner.as_deref().unwrap_or("-"),
                    entry.key
                )?;
            }
            Err(error) => writeln!(out, "{name} unreadable: {error}")?,
        }
    }
    writeln!(out, "{printed} open entries")?;
    Ok(())
}

/// The head and the body of the object `name`, or none if it is not there.
async fn read_one(hot: &dyn Backend, name: &str) -> Result<Option<(Meta, Bytes)>, Error> {
    let read = async {
        let meta = hot.head(name).await?;
        let body = hot.get(name, None).await?;
        Ok((meta, body))
    };
    match read.await {
        Ok(found) => Ok(Some(found)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

async fn show(
    hot: &dyn Backend,
    now: u64,
    key: &str,
    out: &mut dyn Write,
) -> Result<(), VerbError> {
    let name = entry_key(key);
    let Some((meta, body)) = read_one(hot, &name).await? else {
        writeln!(out, "no open entry for key {key:?} (object {name})")?;
        return Ok(());
    };
    let entry =
        decode_entry(&body).map_err(|error| VerbError::Refused(format!("{name}: {error}")))?;
    writeln!(out, "object: {name}")?;
    writeln!(out, "key: {:?}", entry.key)?;
    writeln!(out, "op: {}", op_name(entry.op))?;
    writeln!(out, "state: {}", state_name(entry.st))?;
    writeln!(out, "owner: {}", entry.owner.as_deref().unwrap_or("none"))?;
    writeln!(out, "nonce: {}", entry.nonce)?;
    writeln!(out, "created_ms: {}", entry.created_ms)?;
    writeln!(
        out,
        "age: {}s since the last write",
        now.saturating_sub(meta.modified) / 1000
    )?;
    writeln!(out, "tag: {}", meta.tag)?;
    Ok(())
}

#[cfg(test)]
#[path = "entries_tests.rs"]
mod tests;
