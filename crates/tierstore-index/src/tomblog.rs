//! The tombstone log files `tomb/<origin>/<first_seq>.log` (10.2): this node's own log and the log
//! of every other origin, as stored here.
//!
//! A file holds consecutive framed entries ([`LogRecord`]) starting at the sequence its name
//! carries; the files of an origin continue each other. [`TombLog`] appends this node's entries,
//! stores the entries pulled or pushed from other origins, serves a pull after a cursor, and
//! removes the leading files whose entries are all droppable.
//!
//! Opening reads every file once. A torn or damaged tail of the last file of an origin (what a
//! crash in the middle of an append leaves) is cut at the last valid entry; damage anywhere else
//! is an error, so is a gap between files.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tierstore_format::FormatError;
use tierstore_format::ids::NodeId;
use tierstore_format::names::HotName;
use tierstore_format::tomb::{LogEntry, LogRecord, scan_log};

use crate::fs::Fs;

/// Why the tomb log could not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// A file operation failed.
    #[error("tomb log: {op} {path}: {source}")]
    Io {
        /// The operation: `list`, `len`, `read`, `append`, `truncate`, `sync` or `remove`.
        op: &'static str,
        /// The path the operation was on.
        path: String,
        /// The error.
        source: io::Error,
    },
    /// The bytes of a file, or an entry to write, are not a valid log entry.
    #[error("tomb log: {path}: {source}")]
    Format {
        /// The file.
        path: String,
        /// What is wrong with the bytes.
        source: FormatError,
    },
    /// An entry does not continue the log of its origin.
    #[error("tomb log of origin {origin}: the last sequence is {last}, got {got}")]
    Gap {
        /// The origin of the log.
        origin: NodeId,
        /// The log's last sequence.
        last: u64,
        /// The sequence that does not follow it.
        got: u64,
    },
    /// A file holds an entry of another origin than its directory's.
    #[error("tomb log: {path} of origin {origin} holds an entry of origin {found}")]
    WrongOrigin {
        /// The file.
        path: String,
        /// The origin of the file's directory.
        origin: NodeId,
        /// The origin the entry carries.
        found: NodeId,
    },
    /// An entry of this node's own origin was offered to [`TombLog::store`].
    #[error("tomb log: an entry of this node's own origin {origin} cannot be stored from outside")]
    OwnOrigin {
        /// This node.
        origin: NodeId,
    },
    /// The entries of a file are not numbered from the sequence its name carries.
    #[error("tomb log: {path}: entry at byte {offset} is out of sequence")]
    Corrupt {
        /// The file.
        path: String,
        /// The byte offset of the entry.
        offset: u64,
    },
}

/// A file operation's result with the operation and path as context.
fn io<T>(op: &'static str, path: &str, r: io::Result<T>) -> Result<T, LogError> {
    r.map_err(|source| LogError::Io {
        op,
        path: path.to_owned(),
        source,
    })
}

/// A format result with the file as context.
fn fmt<T>(path: &str, r: Result<T, FormatError>) -> Result<T, LogError> {
    r.map_err(|source| LogError::Format {
        path: path.to_owned(),
        source,
    })
}

fn path_of(origin: NodeId, first_seq: u64) -> String {
    HotName::TombLog { origin, first_seq }.path()
}

/// One file of an origin: its first sequence and its valid length.
#[derive(Debug, Clone)]
struct Segment {
    first: u64,
    len: u64,
}

/// The files of one origin in sequence order and the origin's last sequence.
#[derive(Debug, Clone, Default)]
struct Origin {
    last: u64,
    files: Vec<Segment>,
}

#[derive(Debug, Default)]
struct State {
    origins: BTreeMap<NodeId, Origin>,
}

/// The tomb log files of a node: its own log and the logs of the other origins.
pub struct TombLog {
    fs: Arc<dyn Fs>,
    node: NodeId,
    max_file_bytes: u64,
    state: Mutex<State>,
}

impl TombLog {
    /// Opens the log files under `tomb/`: reads each origin's files in order, cuts a torn tail of
    /// an origin's last file, and learns each origin's last sequence. A new file is started when
    /// the current one would pass `max_file_bytes` (a file holds at least one entry).
    pub fn open(fs: Arc<dyn Fs>, node: NodeId, max_file_bytes: u64) -> Result<Self, LogError> {
        let mut origins = BTreeMap::new();
        for dir in io("list", "tomb", fs.list("tomb"))? {
            // Only a canonical decimal origin names a directory of the log.
            let Some(HotName::TombLog { origin, .. }) =
                HotName::parse(&format!("tomb/{dir}/1.log"))
            else {
                continue;
            };
            let dir = format!("tomb/{dir}");
            let mut firsts: Vec<u64> = io("list", &dir, fs.list(&dir))?
                .iter()
                .filter_map(|name| match HotName::parse(&format!("{dir}/{name}")) {
                    Some(HotName::TombLog { first_seq, .. }) => Some(first_seq),
                    _ => None,
                })
                .collect();
            firsts.sort_unstable();
            if !firsts.is_empty() {
                origins.insert(origin, load_origin(fs.as_ref(), origin, &firsts)?);
            }
        }
        Ok(Self {
            fs,
            node,
            max_file_bytes,
            state: Mutex::new(State { origins }),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends `entry` to this node's own log under the next sequence (1 for the first) and
    /// returns the record; the file is synced before this returns.
    pub fn append(&self, entry: LogEntry) -> Result<LogRecord, LogError> {
        let mut st = self.state();
        let last = st.origins.get(&self.node).map_or(0, |o| o.last);
        let record = LogRecord {
            origin: self.node,
            seq: last + 1,
            entry,
        };
        self.apply(&mut st, std::slice::from_ref(&record), false)?;
        Ok(record)
    }

    /// Stores entries of other origins, pulled or pushed. Each must continue its origin's last
    /// sequence; one at or below it is skipped; a gap is [`LogError::Gap`] and an entry of this
    /// node's own origin [`LogError::OwnOrigin`]. On an error nothing of the call is kept. Returns
    /// how many entries were stored; the files are synced before this returns.
    pub fn store(&self, records: &[LogRecord]) -> Result<u64, LogError> {
        self.apply(&mut self.state(), records, true)
    }

    /// The last sequence of `origin`'s log here (0 when there is none).
    #[must_use]
    pub fn last(&self, origin: NodeId) -> u64 {
        self.state().origins.get(&origin).map_or(0, |o| o.last)
    }

    /// The origins that have a log file here, in order.
    #[must_use]
    pub fn origins(&self) -> Vec<NodeId> {
        self.state().origins.keys().copied().collect()
    }

    /// The entries of `origin` after `after`, at most `limit`, and whether more follow: what a
    /// pull serves.
    pub fn read(
        &self,
        origin: NodeId,
        after: u64,
        limit: usize,
    ) -> Result<(Vec<LogRecord>, bool), LogError> {
        let st = self.state();
        let Some(o) = st.origins.get(&origin) else {
            return Ok((Vec::new(), false));
        };
        let want = limit.saturating_add(1);
        let mut out: Vec<LogRecord> = Vec::new();
        for (i, seg) in o.files.iter().enumerate() {
            let end = o.files.get(i + 1).map_or(o.last, |next| next.first - 1);
            if end <= after {
                continue;
            }
            if out.len() >= want {
                break;
            }
            let records = self.read_file(origin, seg)?;
            out.extend(
                records
                    .into_iter()
                    .filter(|r| r.seq > after)
                    .take(want - out.len()),
            );
        }
        let more = out.len() > limit;
        out.truncate(limit);
        Ok((out, more))
    }

    /// Every entry of `origin` held here, in order.
    pub fn read_all(&self, origin: NodeId) -> Result<Vec<LogRecord>, LogError> {
        Ok(self.read(origin, 0, usize::MAX)?.0)
    }

    /// Removes the leading files of `origin` whose every entry is `droppable`, stopping at the
    /// first file that holds one that is not, and never the last file (it carries the position of
    /// the log). Returns the files removed.
    pub fn compact(
        &self,
        origin: NodeId,
        droppable: &dyn Fn(&LogRecord) -> bool,
    ) -> Result<u64, LogError> {
        let mut st = self.state();
        let Some(o) = st.origins.get_mut(&origin) else {
            return Ok(0);
        };
        let mut removed = 0;
        while o.files.len() > 1 {
            let seg = &o.files[0];
            if !self.read_file(origin, seg)?.iter().all(droppable) {
                break;
            }
            let path = path_of(origin, seg.first);
            io("remove", &path, self.fs.remove(&path))?;
            o.files.remove(0);
            removed += 1;
        }
        Ok(removed)
    }

    /// The valid records of one file, read in one call.
    fn read_file(&self, origin: NodeId, seg: &Segment) -> Result<Vec<LogRecord>, LogError> {
        let path = path_of(origin, seg.first);
        let buf = io("read", &path, self.fs.read_at(&path, 0, seg.len))?;
        fmt(&path, scan_log(&buf, origin))
    }

    /// Writes `records` and syncs the files touched; on an error cuts the files back to what
    /// they held and keeps the state as it was.
    fn apply(
        &self,
        st: &mut State,
        records: &[LogRecord],
        from_outside: bool,
    ) -> Result<u64, LogError> {
        let mut work = st.origins.clone();
        let mut before = BTreeMap::new();
        let result = self
            .write(&mut work, &mut before, records, from_outside)
            .and_then(|stored| {
                for path in before.keys() {
                    io("sync", path, self.fs.sync(path))?;
                }
                Ok(stored)
            });
        match result {
            Ok(stored) => {
                st.origins = work;
                Ok(stored)
            }
            Err(e) => {
                for (path, len) in &before {
                    // Best effort: if the cut fails too, the file is longer than the log knows and the
                    // log must be reopened (open cuts what does not decode as the log's next entry).
                    let _ = self.fs.truncate(path, *len);
                }
                Err(e)
            }
        }
    }

    /// Appends each record that continues its origin to the origin's current file, starting a new
    /// file when the current one would pass the limit; `before` gets each file's length before
    /// the first write to it.
    fn write(
        &self,
        origins: &mut BTreeMap<NodeId, Origin>,
        before: &mut BTreeMap<String, u64>,
        records: &[LogRecord],
        from_outside: bool,
    ) -> Result<u64, LogError> {
        let mut stored = 0;
        for record in records {
            if from_outside && record.origin == self.node {
                return Err(LogError::OwnOrigin { origin: self.node });
            }
            let o = origins.entry(record.origin).or_default();
            if record.seq <= o.last {
                continue;
            }
            if record.seq != o.last + 1 {
                return Err(LogError::Gap {
                    origin: record.origin,
                    last: o.last,
                    got: record.seq,
                });
            }
            let bytes = fmt(&path_of(record.origin, record.seq), record.encode())?;
            let size = bytes.len() as u64;
            let fits = o
                .files
                .last()
                .is_some_and(|s| s.len == 0 || s.len + size <= self.max_file_bytes);
            if !fits {
                o.files.push(Segment {
                    first: record.seq,
                    len: 0,
                });
            }
            let seg = o.files.len() - 1;
            let path = path_of(record.origin, o.files[seg].first);
            before.entry(path.clone()).or_insert(o.files[seg].len);
            io("append", &path, self.fs.append(&path, &bytes))?;
            o.files[seg].len += size;
            o.last = record.seq;
            stored += 1;
        }
        Ok(stored)
    }
}

/// Reads the files of `origin` (their first sequences, ascending) and checks they continue each
/// other; the last file's torn tail is cut.
fn load_origin(fs: &dyn Fs, origin: NodeId, firsts: &[u64]) -> Result<Origin, LogError> {
    let mut o = Origin::default();
    for (i, &first) in firsts.iter().enumerate() {
        let path = path_of(origin, first);
        if first == 0 {
            return Err(LogError::Corrupt { path, offset: 0 });
        }
        if o.files.is_empty() {
            // The head of the log may have been compacted away.
            o.last = first - 1;
        } else if first != o.last + 1 {
            return Err(LogError::Gap {
                origin,
                last: o.last,
                got: first,
            });
        }
        let size = io("len", &path, fs.len(&path))?;
        let buf = io("read", &path, fs.read_at(&path, 0, size))?;
        let (count, valid) = scan_file(fs, origin, &path, first, &buf, i + 1 == firsts.len())?;
        o.last = first - 1 + count;
        o.files.push(Segment { first, len: valid });
    }
    Ok(o)
}

/// The number of entries and the valid length of one file's bytes. A bad entry is a cut of the
/// file at its start when `last`, an error otherwise.
fn scan_file(
    fs: &dyn Fs,
    origin: NodeId,
    path: &str,
    first: u64,
    buf: &[u8],
    last: bool,
) -> Result<(u64, u64), LogError> {
    let mut offset = 0;
    let mut next = first;
    while offset < buf.len() {
        match LogRecord::decode(&buf[offset..]) {
            Ok((record, used)) => {
                if record.origin != origin {
                    return Err(LogError::WrongOrigin {
                        path: path.to_owned(),
                        origin,
                        found: record.origin,
                    });
                }
                if record.seq != next {
                    return Err(LogError::Corrupt {
                        path: path.to_owned(),
                        offset: offset as u64,
                    });
                }
                next += 1;
                offset += used as usize;
            }
            Err(_) if last => {
                // What follows the first bad entry of the last file is unreadable.
                io("truncate", path, fs.truncate(path, offset as u64))?;
                io("sync", path, fs.sync(path))?;
                break;
            }
            Err(source) => return fmt(path, Err(source)),
        }
    }
    Ok((next - first, offset as u64))
}
