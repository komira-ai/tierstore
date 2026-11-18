//! A reader process (`fsbench reader`): the files of its share of the list, opened, read and
//! closed over and over, or read whole.
//!
//! The protocol is one JSON line at a time. The reader reads the list and writes
//! [`Answer::Ready`]; then for each [`Go`] line on its standard input it does that phase and
//! writes its answer; it ends when its standard input closes. Its share of the files is every
//! `of`-th file from the `index`-th, so the readers of a run split the list between them and
//! every file is read by one.
//!
//! A cycle is `stat`, `open`, one `read` of the first `read_bytes` bytes, and `close`, each timed
//! on its own. A failed call ends the cycle: the cycle counts as an error, and the first error
//! is kept with its path. A whole-file read opens the file, drops its pages from the page cache
//! first when the phase is cold (`posix_fadvise(DONTNEED)`, which a FUSE file honours), and reads
//! it to its end 128 KiB at a time.

use std::io::{BufRead, Read as _, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::clock::Clock;
use crate::hist::Histogram;
use crate::list::{Entry, FileList};

/// The size of the buffer of a whole-file read.
const WHOLE_READ: usize = 128 * 1024;

/// What a reader is told to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum Go {
    /// Cycles over the share `index` of `of` of the list, until `ns` nanoseconds have passed or
    /// `max_cycles` were made (0: no limit).
    Cycles {
        /// This reader's share.
        index: u32,
        /// The shares.
        of: u32,
        /// How long, in nanoseconds.
        ns: u64,
        /// The most cycles, 0 for no limit.
        max_cycles: u64,
        /// The bytes a cycle reads.
        read_bytes: u32,
    },
    /// Whole-file reads of the share `index` of `of` of the largest files of the list, up to
    /// `budget` bytes of them.
    Whole {
        /// This reader's share.
        index: u32,
        /// The shares.
        of: u32,
        /// The bytes of the largest files to read.
        budget: u64,
        /// Whether to drop each file's cached pages before it is read (a cold read).
        cold: bool,
    },
}

/// What a reader answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "kebab-case")]
pub enum Answer {
    /// The list is read; the reader waits for a [`Go`].
    Ready,
    /// The end of a [`Go::Cycles`] (boxed: its histograms make it the large one).
    Cycles(Box<CycleCounts>),
    /// The end of a [`Go::Whole`].
    Whole(WholeCounts),
}

/// What the cycles of one reader did.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CycleCounts {
    /// The cycles that ended without an error.
    pub count: u64,
    /// The cycles a call of failed.
    pub errors: u64,
    /// The first error, with its call and path.
    pub first_error: Option<String>,
    /// The latency of each call, in nanoseconds.
    pub stat: Histogram,
    /// The latency of `open`.
    pub open: Histogram,
    /// The latency of the first read.
    pub read: Histogram,
    /// The latency of `close`.
    pub close: Histogram,
}

/// What the whole-file reads of one reader did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WholeCounts {
    /// The files read to their end.
    pub files: u64,
    /// The bytes read.
    pub bytes: u64,
    /// The files whose open or read failed.
    pub errors: u64,
}

/// The files of share `index` of `of` of `files`.
fn share(files: &[Entry], index: u32, of: u32) -> Vec<&Entry> {
    files
        .iter()
        .skip(index as usize)
        .step_by(of.max(1) as usize)
        .collect()
}

/// Writes `answer` as one line, and flushes it.
fn answer(output: &mut impl Write, answer: &Answer) -> Result<(), Error> {
    // An answer always encodes: it holds numbers, strings and histograms alone.
    let line = serde_json::to_string(answer).unwrap_or_default();
    writeln!(output, "{line}")
        .and_then(|()| output.flush())
        .map_err(Error::of("reader", "write the answer"))
}

/// Serves a reader: [`Answer::Ready`], then a phase per [`Go`] line of `input`, until it ends.
///
/// # Errors
/// A line that is not a [`Go`], or the answer cannot be written.
pub fn serve(
    list: &FileList,
    tree: &Path,
    input: impl BufRead,
    mut output: impl Write,
) -> Result<(), Error> {
    answer(&mut output, &Answer::Ready)?;
    for line in input.lines() {
        let line = line.map_err(Error::of("reader", "read a go"))?;
        let go: Go = serde_json::from_str(&line)
            .map_err(Error::of("reader", &format!("not a go: {line}")))?;
        let done = match go {
            Go::Cycles {
                index,
                of,
                ns,
                max_cycles,
                read_bytes,
            } => Answer::Cycles(Box::new(cycles(
                &share(&list.files, index, of),
                tree,
                ns,
                max_cycles,
                read_bytes,
            ))),
            Go::Whole {
                index,
                of,
                budget,
                cold,
            } => Answer::Whole(whole(&share(&list.largest(budget), index, of), tree, cold)),
        };
        answer(&mut output, &done)?;
    }
    Ok(())
}

/// Counts a cycle that failed in `call` on `path`, and keeps the first such error.
fn fail(counts: &mut CycleCounts, call: &str, path: &Path, e: std::io::Error) {
    counts.errors += 1;
    if counts.first_error.is_none() {
        counts.first_error = Some(format!("{call} {}: {e}", path.display()));
    }
}

/// The cycles over `files`.
fn cycles(files: &[&Entry], tree: &Path, ns: u64, max_cycles: u64, read_bytes: u32) -> CycleCounts {
    let clock = Clock::start();
    let mut counts = CycleCounts::default();
    let mut buf = vec![0u8; read_bytes as usize];
    let mut next = files.iter().cycle();
    while (max_cycles == 0 || counts.count + counts.errors < max_cycles) && clock.ns() < ns {
        let Some(entry) = next.next() else {
            break;
        };
        let path = tree.join(&entry.path);
        let t0 = clock.ns();
        if let Err(e) = std::fs::metadata(&path) {
            fail(&mut counts, "stat", &path, e);
            continue;
        }
        let t1 = clock.ns();
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) => {
                fail(&mut counts, "open", &path, e);
                continue;
            }
        };
        let t2 = clock.ns();
        if let Err(e) = file.read(&mut buf) {
            fail(&mut counts, "read", &path, e);
            continue;
        }
        let t3 = clock.ns();
        drop(file);
        let t4 = clock.ns();
        counts.stat.add(t1 - t0);
        counts.open.add(t2 - t1);
        counts.read.add(t3 - t2);
        counts.close.add(t4 - t3);
        counts.count += 1;
    }
    for hist in [
        &mut counts.stat,
        &mut counts.open,
        &mut counts.read,
        &mut counts.close,
    ] {
        *hist = std::mem::take(hist).sealed();
    }
    counts
}

/// Reads one file to its end; returns the bytes read.
fn read_whole(path: &Path, cold: bool, buf: &mut [u8]) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    if cold {
        rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
    }
    let mut bytes = 0;
    loop {
        let n = file.read(buf)?;
        if n == 0 {
            return Ok(bytes);
        }
        bytes += n as u64;
    }
}

/// The whole-file reads of `files`.
fn whole(files: &[&Entry], tree: &Path, cold: bool) -> WholeCounts {
    let mut counts = WholeCounts::default();
    let mut buf = vec![0u8; WHOLE_READ];
    for entry in files {
        match read_whole(&tree.join(&entry.path), cold, &mut buf) {
            Ok(bytes) => {
                counts.files += 1;
                counts.bytes += bytes;
            }
            Err(_) => counts.errors += 1,
        }
    }
    counts
}

#[cfg(test)]
#[path = "reader_tests.rs"]
mod tests;
