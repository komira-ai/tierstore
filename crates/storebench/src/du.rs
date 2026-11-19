//! `du`: what one silo drive holds, by directory, counted the way the drive's file system does.
//!
//! The walk starts at `--root` (a drive's mount point), lists every entry down to `--depth`
//! levels (`root/x` is level 1, `root/x/y` level 2) and, for each, adds up everything below it:
//! the files, the directories, their *apparent* bytes (`st_size` of regular files) and their
//! *allocated* bytes (`st_blocks * 512` of everything, directories included). The allocated
//! bytes are what the drive spends; the gap between them and the apparent bytes (and between
//! them and the objects the drive is meant to hold) is what C-6 explains.
//!
//! The walk does not follow symbolic links (a link is one file with the blocks of the link) and
//! does not cross into another file system (a directory with another `st_dev` is skipped whole).
//! A directory that cannot be read, or an entry that cannot be inspected, is counted in `errors`
//! and skipped. The tree is streamed: memory is a tally per directory level and one record per
//! entry down to the depth, never one per file.

use std::cell::Cell;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::table::{bytes, count};

/// The arguments of `storebench du`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The directory to walk (a drive's mount point).
    #[arg(long)]
    pub root: PathBuf,
    /// How many levels below the root get a line of their own.
    #[arg(long, default_value_t = 2)]
    pub depth: usize,
    /// Where the report (JSON) is written.
    #[arg(long)]
    pub out: PathBuf,
}

/// What lies under one path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The path relative to the root (`.` for the root itself).
    pub path: String,
    /// Files below it (every entry that is not a directory; a symbolic link is one).
    pub files: u64,
    /// Directories below it (not itself).
    pub dirs: u64,
    /// `st_size` of the regular files below it.
    pub apparent: u64,
    /// `st_blocks * 512` of everything below it, itself included.
    pub allocated: u64,
    /// Directories and entries below it that could not be read.
    pub errors: u64,
}

/// The size of the file system the root is on, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filesystem {
    /// Its size.
    pub size: u64,
    /// What is in use.
    pub used: u64,
    /// What a process that is not the superuser can still write.
    pub free: u64,
}

/// The result of a walk.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuReport {
    /// The root as given.
    pub root: String,
    /// The depth walked.
    pub depth: usize,
    /// The whole root.
    pub total: Entry,
    /// The file system the root is on.
    pub filesystem: Filesystem,
    /// Every entry at level 1 to `depth`, in path order.
    pub entries: Vec<Entry>,
}

/// What a subtree holds, added up.
#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    files: u64,
    dirs: u64,
    apparent: u64,
    allocated: u64,
    errors: u64,
}

impl Tally {
    /// Adds `other` to this one.
    fn add(&mut self, other: &Tally) {
        self.files += other.files;
        self.dirs += other.dirs;
        self.apparent += other.apparent;
        self.allocated += other.allocated;
        self.errors += other.errors;
    }

    /// The entry of this tally at `path`.
    fn entry(&self, path: String) -> Entry {
        Entry {
            path,
            files: self.files,
            dirs: self.dirs,
            apparent: self.apparent,
            allocated: self.allocated,
            errors: self.errors,
        }
    }
}

/// `result`'s value, or `None` after counting its error in `errors`.
pub fn note<T>(result: std::io::Result<T>, errors: &Cell<u64>) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(_) => {
            errors.set(errors.get() + 1);
            None
        }
    }
}

/// The walker's settings and what it has found.
struct Walk {
    depth: usize,
    device: u64,
    entries: Vec<Entry>,
}

impl Walk {
    /// The tally of everything below the directory `dir` (not `dir`'s own blocks), found at
    /// `level` below the root; entries down to the depth are recorded under their path.
    fn visit(&mut self, dir: &Path, relative: &str, level: usize) -> Tally {
        let mut total = Tally::default();
        let errors = Cell::new(0);
        let listing = note(std::fs::read_dir(dir), &errors);
        let children = listing
            .into_iter()
            .flatten()
            .filter_map(|item| note(item.and_then(|e| e.metadata().map(|m| (e, m))), &errors));
        for (entry, meta) in children {
            let recorded = level < self.depth;
            let path = if recorded {
                let name = entry.file_name().to_string_lossy().into_owned();
                if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                }
            } else {
                String::new()
            };
            let mut child = Tally::default();
            let mut inside = 0;
            if meta.is_dir() {
                if meta.dev() != self.device {
                    continue;
                }
                child = self.visit(&entry.path(), &path, level + 1);
                inside = 1;
            } else {
                child.files = 1;
                child.apparent = if meta.is_file() { meta.size() } else { 0 };
            }
            child.allocated += meta.blocks() * 512;
            if recorded {
                self.entries.push(child.entry(path));
            }
            total.add(&child);
            total.dirs += inside;
        }
        total.errors += errors.get();
        total
    }
}

/// The size, use and free space of the file system `path` is on.
///
/// # Errors
/// [`Error::Io`] when `statvfs` fails.
pub fn filesystem(path: &Path) -> Result<Filesystem, Error> {
    let s = rustix::fs::statvfs(path).map_err(|e| Error::io("statvfs", path, e.into()))?;
    let unit = s.f_frsize;
    Ok(Filesystem {
        size: s.f_blocks * unit,
        used: s.f_blocks.saturating_sub(s.f_bfree) * unit,
        free: s.f_bavail * unit,
    })
}

/// Walks `root` to `depth` levels (see the module's text).
///
/// # Errors
/// [`Error::Io`] when the root cannot be inspected or its file system cannot be measured;
/// [`Error::Usage`] when it is not a directory.
pub fn walk(root: &Path, depth: usize) -> Result<DuReport, Error> {
    walk_on(root, depth, |device| device)
}

/// [`walk`] with the device that directories must be on given as a function of the root's own:
/// a directory on any other device is skipped.
///
/// # Errors
/// As [`walk`].
pub fn walk_on(
    root: &Path,
    depth: usize,
    device: impl FnOnce(u64) -> u64,
) -> Result<DuReport, Error> {
    let meta = std::fs::symlink_metadata(root).map_err(|e| Error::io("walk", root, e))?;
    if !meta.is_dir() {
        return Err(Error::Usage(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let filesystem = filesystem(root)?;
    let mut walk = Walk {
        depth,
        device: device(meta.dev()),
        entries: Vec::new(),
    };
    let mut total = walk.visit(root, "", 0);
    total.allocated += meta.blocks() * 512;
    walk.entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DuReport {
        root: root.display().to_string(),
        depth,
        total: total.entry(".".to_owned()),
        filesystem,
        entries: walk.entries,
    })
}

/// Writes `value` to `path` as indented JSON.
///
/// # Errors
/// [`Error::Io`] when the file cannot be written.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let text = serde_json::to_vec_pretty(value).map_err(|e| Error::io("encode", path, e.into()))?;
    std::fs::write(path, text).map_err(|e| Error::io("write", path, e))
}

/// Reads the JSON file `path` as a `T`.
///
/// # Errors
/// [`Error::Io`] when it cannot be read; [`Error::Parse`] (with the line the parser stopped at)
/// when it is not a `T`.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let text = std::fs::read(path).map_err(|e| Error::io("read", path, e))?;
    serde_json::from_slice(&text).map_err(|e| Error::parse(path, e.line(), e.to_string()))
}

/// The one line `run` prints about a report.
#[must_use]
pub fn summary_line(report: &DuReport, out: &Path) -> String {
    format!(
        "{}: {} files, {} dirs, {} apparent, {} allocated, {} errors, {} entries in {}",
        report.root,
        count(report.total.files),
        count(report.total.dirs),
        bytes(report.total.apparent),
        bytes(report.total.allocated),
        report.total.errors,
        report.entries.len(),
        out.display()
    )
}

/// Runs `storebench du`: walks the root, writes the report to `--out` and prints one line.
///
/// # Errors
/// As [`walk`], and [`Error::Io`] when the report or the line cannot be written.
pub fn run(args: &Args, stdout: &mut dyn Write) -> Result<(), Error> {
    let report = walk(&args.root, args.depth)?;
    write_json(&args.out, &report)?;
    writeln!(stdout, "{}", summary_line(&report, &args.out))
        .map_err(|e| Error::io("write", "standard output", e))
}

#[cfg(test)]
#[path = "du_tests.rs"]
mod tests;
