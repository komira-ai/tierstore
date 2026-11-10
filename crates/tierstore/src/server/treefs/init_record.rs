//! The record of the kernel's `INIT` handshake: what the connection was set up with, kept in the
//! runtime directory so that a process that takes the connection over (after a crash, a restart
//! of the unit, or a hand-over) can serve it without a handshake, since the kernel sends `INIT`
//! once per mount (`docs/design/virtual-build-fs.md` sections 7.1 and 7.2).
//!
//! The file `<runtime dir>/init` is text, one `key value` pair to a line, written by a rename once
//! at the mount and again at each takeover (with the next `generation`):
//!
//! ```text
//! tierstore-fs-init 1
//! generation 1
//! mount_id 0123456789abcdef0123456789abcdef
//! kernel_abi 7.40
//! capabilities 0x...
//! flags 0x...
//! max_readahead 131072
//! max_write 1048576
//! time_gran_ns 1
//! max_stack_depth 1
//! ```
//!
//! The first line names the format and its version. The format changes only by adding (AGENTS.md
//! B8): a reader takes the keys it knows, ignores the others and ignores the version, so a record
//! of a later release is read by this one and this one's by an earlier one; a key this release
//! needs and cannot find is the one fault, and the process then does not take the connection over
//! (see `run`). The fixtures are `fs_init_record.txt` and `fs_init_record_newer.txt` in
//! `crates/tierstore/tests/stored_formats/fixtures/`.

use std::io;
use std::path::Path;
use std::time::Duration;

use fuser::{InitFlags, InitOutcome, Version};

/// The name of the file in the runtime directory.
pub(super) const FILE: &str = "init";

/// The first word of the file's first line.
const MAGIC: &str = "tierstore-fs-init";

/// The version this release writes.
const VERSION: u32 = 1;

/// Why the record cannot be used.
#[derive(Debug, thiserror::Error)]
pub(in crate::server) enum RecordError {
    /// The file could not be read or written.
    #[error("the INIT record {path}: {source}")]
    Io {
        /// The file.
        path: String,
        /// What the system said.
        source: io::Error,
    },
    /// The file is not a record of this format.
    #[error("the INIT record does not begin with {MAGIC}")]
    NotARecord,
    /// A key this release needs is not in the file.
    #[error("the INIT record has no {0}")]
    Missing(&'static str),
    /// A key's value is not what the key holds.
    #[error("the INIT record's {0} is not a number of its kind")]
    Malformed(&'static str),
}

/// What is recorded: the outcome of the handshake, and how many processes have served the
/// connection before this one (1 for the one that mounted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Record {
    /// What the handshake set up.
    pub outcome: InitOutcome,
    /// The count of processes that have served the connection, this one included. It is in the
    /// file handles this process hands out, so that a release or a read of a handle an earlier
    /// process gave out is told from one of this process's.
    pub generation: u32,
    /// The id the pins of the connection are made under (`Settings::mount_id`): the process that
    /// takes over goes on with the pins of the one before.
    pub mount_id: String,
}

impl Record {
    /// The record of a connection a process has just set up.
    pub(super) fn first(outcome: InitOutcome, mount_id: String) -> Self {
        Self {
            outcome,
            generation: 1,
            mount_id,
        }
    }

    /// The record the next process to take the connection over writes: the same outcome, the next
    /// generation (wrapping at the width the handles give it).
    pub(super) fn next(&self) -> Self {
        Self {
            outcome: self.outcome,
            generation: super::handle::next_generation(self.generation),
            mount_id: self.mount_id.clone(),
        }
    }

    /// The text of the file.
    pub(super) fn text(&self) -> String {
        let outcome = &self.outcome;
        format!(
            "{MAGIC} {VERSION}\n\
             generation {}\n\
             mount_id {}\n\
             kernel_abi {}.{}\n\
             capabilities {:#x}\n\
             flags {:#x}\n\
             max_readahead {}\n\
             max_write {}\n\
             time_gran_ns {}\n\
             max_stack_depth {}\n",
            self.generation,
            self.mount_id,
            outcome.kernel_abi.0,
            outcome.kernel_abi.1,
            outcome.capabilities.bits(),
            outcome.flags.bits(),
            outcome.max_readahead,
            outcome.max_write,
            outcome.time_gran.as_nanos(),
            outcome.max_stack_depth,
        )
    }

    /// The record the text of a file holds.
    ///
    /// # Errors
    /// [`RecordError::NotARecord`] for text that does not begin with the format's name;
    /// [`RecordError::Missing`] and [`RecordError::Malformed`] for a key this release needs.
    pub(super) fn parse(text: &str) -> Result<Self, RecordError> {
        let mut lines = text.lines();
        let head = lines.next().unwrap_or_default();
        if head.split(' ').next() != Some(MAGIC) {
            return Err(RecordError::NotARecord);
        }
        let pairs: Vec<(&str, &str)> = lines.filter_map(|line| line.split_once(' ')).collect();
        let find = |key: &'static str| -> Result<&str, RecordError> {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| *value)
                .ok_or(RecordError::Missing(key))
        };
        let number = |key: &'static str| -> Result<u64, RecordError> {
            find(key)?.parse().map_err(|_| RecordError::Malformed(key))
        };
        let small = |key: &'static str| -> Result<u32, RecordError> {
            u32::try_from(number(key)?).map_err(|_| RecordError::Malformed(key))
        };
        let bits = |key: &'static str| -> Result<InitFlags, RecordError> {
            let value = find(key)?;
            let digits = value
                .strip_prefix("0x")
                .ok_or(RecordError::Malformed(key))?;
            let bits = u64::from_str_radix(digits, 16).map_err(|_| RecordError::Malformed(key))?;
            Ok(InitFlags::from_bits_retain(bits))
        };
        let abi = find("kernel_abi")?
            .split_once('.')
            .and_then(|(major, minor)| Some(Version(major.parse().ok()?, minor.parse().ok()?)))
            .ok_or(RecordError::Malformed("kernel_abi"))?;
        Ok(Self {
            outcome: InitOutcome {
                kernel_abi: abi,
                capabilities: bits("capabilities")?,
                flags: bits("flags")?,
                max_readahead: small("max_readahead")?,
                max_write: small("max_write")?,
                time_gran: Duration::from_nanos(number("time_gran_ns")?),
                max_stack_depth: small("max_stack_depth")?,
            },
            generation: small("generation")?,
            mount_id: find("mount_id")?.to_owned(),
        })
    }
}

/// Writes `record` to the file in `dir`, by a rename, so that a reader sees a whole file.
///
/// # Errors
/// [`RecordError::Io`].
pub(super) fn write(dir: &Path, record: &Record) -> Result<(), RecordError> {
    let path = dir.join(FILE);
    let mut new = path.clone().into_os_string();
    new.push(".new");
    std::fs::write(&new, record.text())
        .and_then(|()| std::fs::rename(&new, &path))
        .map_err(|source| RecordError::Io {
            path: path.display().to_string(),
            source,
        })
}

/// Reads the record in `dir`.
///
/// # Errors
/// [`RecordError::Io`] for a file that is not there or cannot be read, and those of
/// [`Record::parse`].
pub(super) fn read(dir: &Path) -> Result<Record, RecordError> {
    let path = dir.join(FILE);
    let text = std::fs::read_to_string(&path).map_err(|source| RecordError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Record::parse(&text)
}

#[cfg(test)]
#[path = "init_record_tests.rs"]
mod tests;
