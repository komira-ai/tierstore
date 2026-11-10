//! The table of tree numbers: a file in the runtime directory of fixed-size records, record `n`
//! (from 1) being the digest of the tree numbered `n`. A header says which layout of inode numbers
//! the numbers are of (`layout 1`: `tree_number << 24 | index`, `ino.rs`), and, in the word after
//! the layout, the epoch of the mount the numbers belong to.
//!
//! A number is written before the first reply that names an inode of the tree, and is read by
//! number, so a start reads neither the whole file nor needs the trees it names. The table lives
//! as long as the mount: a new mount starts it empty (a reboot takes the mount with it).
//!
//! The epoch is 0 for a FUSE mount, whose table is made afresh by every start (a new mount has a
//! new connection and holds no number). An NFS mount outlives the server, so its start reopens the
//! table, and keeps the numbers, when the file holds the epoch the start is of (a hash of the
//! machine's boot, `nfs/boot.rs`); a file of another epoch, or of no epoch, is replaced. The word
//! was reserved and zero before: a release that reads it as reserved reads this file unchanged.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::Mutex;

use crate::tree_manifest::TreeDigest;

use super::ino::MOST_TREES;
use super::locked;

/// The first bytes of the file.
const MAGIC: &[u8; 8] = b"tsfstree";

/// The layout of the inode numbers (header word).
pub(super) const LAYOUT: u32 = 1;

/// The bytes of the header: the magic, the layout and a reserved word.
const HEADER: u64 = 16;

/// The bytes of a record: a digest.
const RECORD: u64 = 32;

/// The name of the file in the runtime directory.
pub(super) const FILE: &str = "trees";

/// Why the table cannot be used.
#[derive(Debug, thiserror::Error)]
pub(in crate::server) enum TableError {
    /// The file could not be made, written or read.
    #[error("the table of tree numbers: {0}")]
    Io(#[from] io::Error),
    /// Every tree number is taken.
    #[error("the table of tree numbers is full")]
    Full,
}

/// The table and what it holds, by digest.
#[derive(Debug)]
pub(super) struct TreeTable {
    file: File,
    /// The epoch in the header.
    epoch: u32,
    /// The number of each digest the table has, and the last number given.
    state: Mutex<(HashMap<TreeDigest, u64>, u64)>,
}

impl TreeTable {
    /// Makes the table `<dir>/trees` afresh, with its header and no records.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn create(dir: &Path) -> Result<Self, TableError> {
        Self::create_of(dir, 0)
    }

    /// Makes the table afresh, of the mount `epoch`.
    fn create_of(dir: &Path, epoch: u32) -> Result<Self, TableError> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join(FILE))?;
        let mut header = [0u8; HEADER as usize];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&LAYOUT.to_le_bytes());
        header[12..16].copy_from_slice(&epoch.to_le_bytes());
        file.write_all_at(&header, 0)?;
        Ok(Self {
            file,
            epoch,
            state: Mutex::new((HashMap::new(), 0)),
        })
    }

    /// The table `<dir>/trees` as the mount of `epoch` left it, with every number it gave, if the
    /// file is a table of this layout and of this epoch (an epoch of 0 is never reopened);
    /// otherwise a table made afresh. The second is whether the numbers were kept.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn open_or_create(dir: &Path, epoch: u32) -> Result<(Self, bool), TableError> {
        if epoch != 0
            && let Some(table) = Self::load(dir)?.filter(|table| table.epoch == epoch)
        {
            return Ok((table, true));
        }
        Ok((Self::create_of(dir, epoch)?, false))
    }

    /// The table in `<dir>/trees`, whatever its epoch: none when there is no such file, or it is
    /// not a table of this layout. A partly written last record is not a number.
    ///
    /// # Errors
    /// What the system said, other than that the file is missing.
    pub(super) fn load(dir: &Path) -> Result<Option<Self>, TableError> {
        let file = match File::options().read(true).write(true).open(dir.join(FILE)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        (&file).read_to_end(&mut bytes)?;
        let layout = bytes
            .get(8..12)
            .and_then(|word| <[u8; 4]>::try_from(word).ok());
        let epoch = bytes
            .get(12..16)
            .and_then(|word| <[u8; 4]>::try_from(word).ok());
        let (Some(layout), Some(epoch)) = (layout, epoch) else {
            return Ok(None);
        };
        if &bytes[..8] != MAGIC || u32::from_le_bytes(layout) != LAYOUT {
            return Ok(None);
        }
        let records = bytes[HEADER as usize..].chunks_exact(RECORD as usize);
        let last = records.len() as u64;
        // A record is 32 bytes, which always read as a digest.
        let numbers = (1..)
            .zip(records)
            .filter_map(|(number, record)| {
                TreeDigest::from_hex(&hex::encode(record)).map(|digest| (digest, number))
            })
            .collect();
        Ok(Some(Self {
            file,
            epoch: u32::from_le_bytes(epoch),
            state: Mutex::new((numbers, last)),
        }))
    }

    /// The epoch of the mount the table belongs to; 0 is none.
    #[cfg(test)]
    pub(super) fn epoch(&self) -> u32 {
        self.epoch
    }

    /// The number of the tree `digest`, given it if it has none: the record is written first.
    ///
    /// # Errors
    /// [`TableError::Full`] with every number taken; what the system said if the record is not
    /// written (the number is then not given).
    pub(super) fn number_of(&self, digest: &TreeDigest) -> Result<u64, TableError> {
        let mut state = locked(&self.state);
        if let Some(number) = state.0.get(digest) {
            return Ok(*number);
        }
        let number = state.1 + 1;
        if number > MOST_TREES {
            return Err(TableError::Full);
        }
        self.file
            .write_all_at(&digest_bytes(digest), HEADER + (number - 1) * RECORD)?;
        state.0.insert(*digest, number);
        state.1 = number;
        Ok(number)
    }

    /// The number of the tree `digest`, if it has one.
    pub(super) fn find(&self, digest: &TreeDigest) -> Option<u64> {
        locked(&self.state).0.get(digest).copied()
    }

    /// The digest of the tree numbered `number`: none for a number never given.
    ///
    /// # Errors
    /// What the system said if the record cannot be read.
    pub(super) fn digest_of(&self, number: u64) -> Result<Option<TreeDigest>, TableError> {
        let last = locked(&self.state).1;
        if number == 0 || number > last {
            return Ok(None);
        }
        let mut record = [0u8; RECORD as usize];
        self.file
            .read_exact_at(&mut record, HEADER + (number - 1) * RECORD)?;
        Ok(TreeDigest::from_hex(&hex::encode(record)))
    }
}

impl TreeTable {
    /// Takes every number, as if the table were full.
    #[cfg(test)]
    pub(super) fn exhaust(&self) {
        locked(&self.state).1 = MOST_TREES;
    }
}

/// The 32 bytes of `digest`.
fn digest_bytes(digest: &TreeDigest) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    // A digest prints as sixty-four hex digits.
    let _ = hex::decode_to_slice(digest.to_string(), &mut bytes);
    bytes
}

#[cfg(test)]
#[path = "table_tests.rs"]
mod tests;
