//! What a process that stops serving the mount hands the put directory's successor: the file
//! `put-handover` in `fs.runtime_dir`, written by a rename when the process has stopped reading
//! requests (for a hand-over, and before a restart that leaves the mount to the service manager),
//! read by the process that takes the connection over and removed by it once its put directory
//! serves, and removed by a process that serves again after a hand-over that did not complete.
//!
//! ```text
//! tierstore-fs-put-handover 1
//! dir <lookups or -> <made or seen> <path>   a directory the kernel holds
//! open <handle> <id>                        an open the kernel holds, of the staged file <id>
//! ```
//!
//! The kernel holds directories by the inode numbers this process gave out, which are hashes of
//! their paths, so the successor must have the paths; and opens by numbers that only this process
//! knows the staged file of. A start without the file (after a crash) finds the directories that
//! hold names from the store, and takes an open over at its first request.
//!
//! A reader takes the lines it knows and leaves out any other, and the words after those it knows,
//! so a later release may add both; it reads a file of any version by those lines (a version is
//! the format's count of changes, all additive). A line that is not what it says is left out. The fixtures are
//! `fs_put_handover.txt` and `fs_put_handover_newer.txt` in
//! `crates/tierstore/tests/stored_formats/fixtures/`.

use std::io;
use std::path::Path;

/// The name of the file in the runtime directory.
pub(in crate::server) const FILE: &str = "put-handover";

/// The first line.
const HEADER: &str = "tierstore-fs-put-handover 1";

/// A directory the kernel holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct HeldDir {
    /// The lookups it holds; none when they are not known.
    pub lookups: Option<u64>,
    /// Whether `mkdir` made it.
    pub made: bool,
    /// Its path below `put/`.
    pub path: String,
}

/// What a successor is handed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct Handed {
    pub dirs: Vec<HeldDir>,
    /// Each open, by its number, with the number of its staged file.
    pub opens: Vec<(u64, u64)>,
}

/// The text of `handed`.
pub(in crate::server) fn encode(handed: &Handed) -> String {
    let mut text = format!("{HEADER}\n");
    for dir in &handed.dirs {
        let lookups = dir
            .lookups
            .map_or_else(|| "-".to_owned(), |lookups| lookups.to_string());
        let made = if dir.made { "made" } else { "seen" };
        text.push_str(&format!("dir {lookups} {made} {}\n", dir.path));
    }
    for (handle, id) in &handed.opens {
        text.push_str(&format!("open {handle} {id}\n"));
    }
    text
}

/// What the text `text` hands over; none when it is not a hand-over file.
pub(in crate::server) fn decode(text: &str) -> Option<Handed> {
    let mut lines = text.lines();
    let _version = lines.next()?.strip_prefix("tierstore-fs-put-handover ")?;
    let mut handed = Handed::default();
    for line in lines {
        let mut words = line.split(' ');
        match words.next() {
            Some("dir") => {
                let lookups = match words.next() {
                    Some("-") => None,
                    Some(number) => match number.parse() {
                        Ok(number) => Some(number),
                        Err(_) => continue,
                    },
                    None => continue,
                };
                let made = match words.next() {
                    Some("made") => true,
                    Some("seen") => false,
                    _ => continue,
                };
                let Some(path) = words.next().filter(|path| !path.is_empty()) else {
                    continue;
                };
                handed.dirs.push(HeldDir {
                    lookups,
                    made,
                    path: path.to_owned(),
                });
            }
            Some("open") => {
                let numbers = (words.next(), words.next());
                if let (Some(Ok(handle)), Some(Ok(id))) =
                    (numbers.0.map(str::parse), numbers.1.map(str::parse))
                {
                    handed.opens.push((handle, id));
                }
            }
            _ => {}
        }
    }
    Some(handed)
}

/// Writes `handed` to the file in `runtime_dir`, by a rename of a file written beside it.
///
/// # Errors
/// What the system said.
pub(in crate::server) fn write(runtime_dir: &Path, handed: &Handed) -> io::Result<()> {
    let path = runtime_dir.join(FILE);
    let beside = runtime_dir.join(format!("{FILE}.new"));
    // The runtime directory lives as long as the boot, and so does the mount the file is of: no
    // sync is needed for the successor to read it.
    std::fs::write(&beside, encode(handed))?;
    std::fs::rename(&beside, path)
}

/// What the file in `runtime_dir` hands over: none when there is none, or it is not a hand-over
/// file (said). The file stays until [`remove`], so that a start that fails after reading it leaves
/// it for the next.
///
/// # Errors
/// What the system said when the file is there and cannot be read.
pub(in crate::server) fn read(runtime_dir: &Path) -> io::Result<Option<Handed>> {
    let path = runtime_dir.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let handed = decode(&text);
    if handed.is_none() {
        tracing::warn!(
            "tierstore fs: {} is not a hand-over of the put directory; left out",
            path.display()
        );
    }
    Ok(handed)
}

/// Removes the file in `runtime_dir`, if it is there: for the process that took it over, once it
/// serves, and for a process that serves again after a hand-over that did not complete.
///
/// # Errors
/// What the system said.
pub(in crate::server) fn remove(runtime_dir: &Path) -> io::Result<()> {
    match std::fs::remove_file(runtime_dir.join(FILE)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Writes `handed` as [`write`] does, and says a failure: the successor then finds what the
/// store holds.
pub(in crate::server) fn write_or_say(runtime_dir: &Path, handed: &Handed) {
    if let Err(error) = write(runtime_dir, handed) {
        tracing::warn!("tierstore fs: the put directory's hand-over is not written: {error}");
    }
}

/// Removes the file as [`remove`] does, and says a failure.
pub(in crate::server) fn remove_or_say(runtime_dir: &Path) {
    if let Err(error) = remove(runtime_dir) {
        tracing::warn!("tierstore fs: the put directory's hand-over is not removed: {error}");
    }
}

#[cfg(test)]
#[path = "carry_tests.rs"]
mod tests;
