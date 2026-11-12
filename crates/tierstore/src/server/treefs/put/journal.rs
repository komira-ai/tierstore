//! The journal of the stage directory: one JSON object to a line, appended for each change, so that
//! a start knows what each staged file is.
//!
//! ```text
//! {"op":"create","id":7,"dir":"cas/b/1/n","name":"pack.part","ms":1700000000000}
//! {"op":"rename","id":7,"name":"pack.sha256-<64 hex>.part"}
//! {"op":"commit","id":7,"name":"pack","sha256":"<64 hex>","size":42,"ms":1700000000500}
//! {"op":"remove","id":7}
//! ```
//!
//! - `create`: the file `<id>.stage` of the stage holds the bytes of the name `name` in the
//!   directory `dir` (a key prefix without its last `/`), made at `ms` (milliseconds since the Unix
//!   epoch).
//! - `rename`: the name changed to `name`, a `.part` name, in the same directory.
//! - `commit`: the bytes were put upstream as `<dir>/<name>` at `ms`; the staged file is gone and
//!   the name is shown, with that digest and size, until the committed TTL.
//! - `remove`: the name is gone (aborted, swept or no longer shown).
//!
//! A reader ignores fields it does not know and skips a line it cannot read (an `op` it does not
//! know, or the torn last line of a process that stopped while it wrote), so a later release adds
//! fields and operations without a version. The lines are written without `fsync`: the journal
//! serves a restart of the process (the hand-over of the mount), and the first sweep ten minutes
//! after a start removes the staged files that have no entry. A start rewrites the journal with
//! the live entries alone (a committed entry as a `create` of its final name and its `commit`), to
//! `journal.new`, synced and renamed over `journal`, and so does a sweep once the journal has more
//! than twice as many lines as live entries, plus 64.
//!
//! The fixtures are `fs_put_journal.jsonl` and `fs_put_journal_newer.jsonl` in
//! `crates/tierstore/tests/stored_formats/fixtures/`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The name of the journal in the stage directory.
pub(super) const FILE: &str = "journal";

/// The name of a journal being rewritten.
pub(super) const NEW_FILE: &str = "journal.new";

/// One line of the journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub(super) enum Record {
    /// A staged file was made.
    Create {
        /// The number of the staged file.
        id: u64,
        /// The directory, a key prefix without its last `/`.
        dir: String,
        /// The `.part` name.
        name: String,
        /// When, in milliseconds since the Unix epoch.
        ms: u64,
    },
    /// A staged file has another `.part` name.
    Rename {
        /// The number of the staged file.
        id: u64,
        /// The new name.
        name: String,
    },
    /// A staged file was put upstream.
    Commit {
        /// The number of the staged file.
        id: u64,
        /// The committed name.
        name: String,
        /// The SHA-256 of the bytes, in lower-case hex.
        sha256: String,
        /// The bytes.
        size: u64,
        /// When, in milliseconds since the Unix epoch.
        ms: u64,
    },
    /// A name is gone.
    Remove {
        /// The number of the staged file.
        id: u64,
    },
}

impl Record {
    /// The number of the staged file the record is of.
    pub(super) fn id(&self) -> u64 {
        match self {
            Self::Create { id, .. }
            | Self::Rename { id, .. }
            | Self::Commit { id, .. }
            | Self::Remove { id } => *id,
        }
    }
}

/// The line of a record, with its newline.
pub(super) fn encode(record: &Record) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

/// The records of the journal bytes `text`, in order, and the number of lines skipped.
pub(super) fn decode(text: &[u8]) -> (Vec<Record>, u64) {
    let mut records = Vec::new();
    let mut skipped = 0;
    for line in text.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        match serde_json::from_slice(line) {
            Ok(record) => records.push(record),
            Err(_) => skipped += 1,
        }
    }
    (records, skipped)
}

/// The journal file a store appends to.
#[derive(Debug)]
pub(super) struct Journal {
    /// The stage directory.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read only by the tests, which break the appends")
    )]
    dir: PathBuf,
    /// `journal`, open to append.
    file: File,
    /// The lines in the file.
    lines: u64,
}

impl Journal {
    /// The bytes of the journal in `dir`, or none if there is no journal.
    ///
    /// # Errors
    /// What the read said, other than that there is no file.
    pub(super) fn read(dir: &Path) -> io::Result<Vec<u8>> {
        match std::fs::read(dir.join(FILE)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            read => read,
        }
    }

    /// Writes `records` as the whole journal of `dir`: to `journal.new`, synced, renamed over
    /// `journal`, and the directory synced. Returns the journal, open to append.
    ///
    /// # Errors
    /// What a write, sync, rename or open said.
    pub(super) fn rewrite(dir: &Path, records: &[Record]) -> io::Result<Self> {
        let new = dir.join(NEW_FILE);
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend(encode(record)?);
        }
        let mut file = File::create(&new)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&new, dir.join(FILE))?;
        File::open(dir)?.sync_all()?;
        let file = OpenOptions::new().append(true).open(dir.join(FILE))?;
        Ok(Self {
            dir: dir.to_owned(),
            file,
            lines: records.len() as u64,
        })
    }

    /// Appends `record`, in one write.
    ///
    /// # Errors
    /// What the write said.
    pub(super) fn append(&mut self, record: &Record) -> io::Result<()> {
        self.file.write_all(&encode(record)?)?;
        self.lines += 1;
        Ok(())
    }

    /// The lines in the file.
    pub(super) fn lines(&self) -> u64 {
        self.lines
    }

    /// Makes every later append fail, as a full disk would: the file is opened again read-only.
    #[cfg(test)]
    pub(super) fn break_appends(&mut self) {
        self.file = File::open(self.dir.join(FILE)).unwrap();
    }
}
