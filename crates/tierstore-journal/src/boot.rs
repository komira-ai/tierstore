//! The boot id (section 8): which kernel boot wrote a snapshot or a journal.
//!
//! A process crash keeps the kernel and its page cache, so what the journal says was committed
//! is on the device or in the page cache. A power loss or a kernel crash may not have kept what
//! a device acknowledged (some devices reorder flushes), so after one every restored cell is
//! re-hashed. The boot id tells the two apart: the snapshot and the journal carry the id of the
//! boot that wrote them, and a restart compares it with its own.

use std::fmt;
use std::path::{Path, PathBuf};

/// The kernel's boot id: the 128-bit UUID of `/proc/sys/kernel/random/boot_id`, new at every
/// boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BootId(pub [u8; 16]);

/// Why the boot id could not be had. Neither is fatal to the cache: a caller that gets one runs
/// with no boot id, which makes every restored cell `recheck` (section 8).
#[derive(Debug, thiserror::Error)]
pub enum BootIdError {
    /// The file could not be read.
    #[error("read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The error.
        #[source]
        source: std::io::Error,
    },
    /// The text is not a UUID of 32 hex digits in the 8-4-4-4-12 form.
    #[error("boot id {text:?} is not a UUID")]
    Malformed {
        /// The text read, without its trailing newline.
        text: String,
    },
}

impl BootId {
    /// Where Linux publishes the boot id.
    pub const LINUX_PATH: &'static str = "/proc/sys/kernel/random/boot_id";

    /// The id of the text the kernel writes: a UUID in the 8-4-4-4-12 form, lowercase or
    /// uppercase hex, with or without a trailing newline.
    pub fn parse(text: &str) -> Result<Self, BootIdError> {
        let trimmed = text.strip_suffix('\n').unwrap_or(text);
        let malformed = || BootIdError::Malformed {
            text: trimmed.to_owned(),
        };
        let groups: Vec<&str> = trimmed.split('-').collect();
        let lengths: Vec<usize> = groups.iter().map(|g| g.len()).collect();
        if lengths != [8, 4, 4, 4, 12] {
            return Err(malformed());
        }
        let digits: Vec<u8> = groups.concat().into_bytes();
        let mut id = [0u8; 16];
        for (byte, pair) in id.iter_mut().zip(digits.chunks_exact(2)) {
            let hi = hex_digit(pair[0]).ok_or_else(malformed)?;
            let lo = hex_digit(pair[1]).ok_or_else(malformed)?;
            *byte = (hi << 4) | lo;
        }
        Ok(Self(id))
    }

    /// The id in the file at `path` ([`BootId::LINUX_PATH`] in production; a test passes a file
    /// of its scratch directory).
    pub fn read(path: &Path) -> Result<Self, BootIdError> {
        let text = std::fs::read_to_string(path).map_err(|source| BootIdError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text)
    }
}

impl fmt::Display for BootId {
    /// The 8-4-4-4-12 lowercase form the kernel writes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Whether what was written in boot `written` may be trusted without a re-hash in boot `now`:
/// only when both are known and equal. No boot id on either side is another boot (section 8:
/// "different boot, or no boot_id").
#[must_use]
pub fn same_boot(written: Option<BootId>, now: Option<BootId>) -> bool {
    matches!((written, now), (Some(w), Some(n)) if w == n)
}
