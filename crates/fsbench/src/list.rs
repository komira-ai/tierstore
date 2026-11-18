//! The fixed input of a benchmark: the regular files of a tree, by path, with their sizes.
//!
//! `fsbench list` walks a tree once and writes the list; every run of every variant then reads
//! the same list, so the files opened are the same whatever serves them, and the list's SHA-256
//! is recorded with each run. The format is a header line, `# fsbench list 1`, and one line per
//! file, sorted by path: its size in bytes, one tab, and its path (`4096`, tab, `include/a.h`).
//! A path is relative to the tree, in `/`-joined
//! components, none of them empty, `.` or `..`. Symbolic links and other special files are left
//! out. A name with a tab or a newline in it, or that is not UTF-8, cannot be listed.

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::Error;

/// The first line of a list.
pub const HEADER: &str = "# fsbench list 1";

/// One file of a list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path below the tree's root.
    pub path: String,
    /// The size in bytes.
    pub size: u64,
}

/// A list, as read from its file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileList {
    /// The files, sorted by path.
    pub files: Vec<Entry>,
    /// The SHA-256 of the list's text, lowercase hex.
    pub sha256: String,
}

impl FileList {
    /// The sum of the sizes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// The largest files, largest first (equal sizes by path), until their sizes add up to
    /// `budget` or more (the last file taken may pass it), or the list ends; empty files are never taken. The whole-file reads of a
    /// run are of these.
    #[must_use]
    pub fn largest(&self, budget: u64) -> Vec<Entry> {
        let mut by_size: Vec<&Entry> = self.files.iter().filter(|f| f.size > 0).collect();
        by_size.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
        let mut taken = Vec::new();
        let mut total = 0u64;
        for entry in by_size {
            if total >= budget {
                break;
            }
            total += entry.size;
            taken.push(entry.clone());
        }
        taken
    }
}

/// The regular files below `root`, sorted by path.
///
/// # Errors
/// A directory that cannot be read, or a name that cannot be listed.
pub fn walk(root: &Path) -> Result<Vec<Entry>, Error> {
    let mut files = Vec::new();
    let mut dirs = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = dirs.pop() {
        let entries = std::fs::read_dir(&dir).map_err(Error::at("read the directory", &dir))?;
        for entry in entries {
            let entry = entry.map_err(Error::at("read the directory", &dir))?;
            let at = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                return Err(Error::Usage(format!(
                    "{}: the name is not UTF-8",
                    at.display()
                )));
            };
            if name.contains(['\t', '\n']) {
                return Err(Error::Usage(format!(
                    "{}: a name with a tab or a newline cannot be listed",
                    at.display()
                )));
            }
            let path = format!("{prefix}{name}");
            let meta = std::fs::symlink_metadata(&at).map_err(Error::at("stat", &at))?;
            if meta.is_dir() {
                dirs.push((at, format!("{path}/")));
            } else if meta.is_file() {
                files.push(Entry {
                    path,
                    size: meta.len(),
                });
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// The text of the list of `files` (sorted by path).
#[must_use]
pub fn render(files: &[Entry]) -> String {
    let mut text = format!("{HEADER}\n");
    for file in files {
        text.push_str(&format!("{}\t{}\n", file.size, file.path));
    }
    text
}

/// The SHA-256 of `text`, lowercase hex.
#[must_use]
pub fn sha256(text: &[u8]) -> String {
    hex::encode(Sha256::digest(text))
}

/// Walks `root` and writes its list to `out`; returns the list.
///
/// # Errors
/// As [`walk`], or `out` cannot be written.
pub fn write(root: &Path, out: &Path) -> Result<FileList, Error> {
    let files = walk(root)?;
    let text = render(&files);
    std::fs::write(out, &text).map_err(Error::at("write", out))?;
    Ok(FileList {
        files,
        sha256: sha256(text.as_bytes()),
    })
}

/// Reads the list at `path`.
///
/// # Errors
/// The file cannot be read, or a line is not of the format (the error names it).
pub fn read(path: &Path) -> Result<FileList, Error> {
    let bytes = std::fs::read(path).map_err(Error::at("read", path))?;
    let bad = |line: usize, why: &str| Error::Parse {
        path: PathBuf::from(path),
        line,
        why: why.to_owned(),
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| bad(1, "the list is not UTF-8"))?;
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(bad(1, &format!("the first line is not `{HEADER}`")));
    }
    let mut files: Vec<Entry> = Vec::new();
    for (n, line) in lines.enumerate() {
        let line_no = n + 2;
        let Some((size, file)) = line.split_once('\t') else {
            return Err(bad(line_no, "no tab between the size and the path"));
        };
        let size = size
            .parse::<u64>()
            .map_err(|_| bad(line_no, "the size is not a number of bytes"))?;
        if file.split('/').any(|part| matches!(part, "" | "." | "..")) {
            return Err(bad(line_no, "the path has an empty, `.` or `..` component"));
        }
        if files.last().is_some_and(|last| last.path.as_str() >= file) {
            return Err(bad(
                line_no,
                "the paths are not sorted, or one is listed twice",
            ));
        }
        files.push(Entry {
            path: file.to_owned(),
            size,
        });
    }
    Ok(FileList {
        files,
        sha256: sha256(&bytes),
    })
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
