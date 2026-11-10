//! The passthrough half of the core's opens (`docs/design/virtual-build-fs.md` section 6.4): where
//! `INIT` offered passthrough, every non-empty file is opened by the kernel from the cache file of
//! its blob, under one backing id per blob for as long as any open of it lives.

use std::sync::Arc;

use crate::tree_manifest::{BlobSource, TreeDigest, TreeEntry};

use super::super::backing::{BackingTable, Registrar, Slot};
use super::{Core, Errno, Handle, Loaded, Opened};

impl Core {
    /// Makes the opens of non-empty files passthrough opens from now on. The frontend calls this
    /// once, when `INIT` has offered passthrough and it has asked for it.
    pub(in crate::server::treefs) fn enable_passthrough(&self) {
        let table = BackingTable::new(self.backing_file.clone(), self.settings.max_backing_ids);
        // Once only: a second call would have no table to add.
        let _ = self.backing.set(table);
    }

    /// Whether non-empty files are opened by passthrough.
    #[cfg(test)]
    pub(in crate::server::treefs) fn passthrough(&self) -> bool {
        self.backing.get().is_some()
    }

    /// The backing table, if passthrough was chosen.
    #[cfg(test)]
    pub(in crate::server::treefs) fn backing_table(&self) -> Option<&Arc<BackingTable>> {
        self.backing.get()
    }

    /// An open of the blob in passthrough mode. If the blob has a live id, the open is counted on
    /// it; if not, the cache file is had (filled first if it must be), `register` gives it to the
    /// kernel, and the id it made is the blob's until the last release.
    pub(super) fn open_backed(
        &self,
        table: &Arc<BackingTable>,
        (loaded, number, entry): (&Loaded, u64, &TreeEntry),
        blob: (&TreeDigest, u64, Option<&BlobSource>),
        register: &mut Registrar<'_>,
    ) -> Result<Opened, Errno> {
        let digest = blob.0;
        let backing = match table.open(digest) {
            Slot::Live(backing) => {
                self.pin_once(loaded, digest);
                backing
            }
            // On an error the ticket is dropped, and the opens that wait for this blob go on.
            Slot::Register(ticket) => {
                let file = self.blob(loaded, number, entry, blob)?;
                let registered = register(&file).map_err(|why| {
                    tracing::error!("tierstore fs: cannot register {digest}: {why}");
                    Errno::Io
                })?;
                ticket.registered(registered)
            }
        };
        let fh = self.new_handle(Handle::Backed(*digest));
        Ok(Opened::Backed { fh, backing })
    }
}

#[cfg(test)]
#[path = "passthrough_tests.rs"]
mod tests;
