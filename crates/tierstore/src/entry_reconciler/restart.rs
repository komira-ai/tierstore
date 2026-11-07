//! Which owners of held entries have restarted since they made them.
//!
//! A proxy that restarts keeps its proxy id, so its registration is never absent and, once the new
//! process beats, never stands unchanged: the owner of the entries its earlier process left held is
//! not "departed" and would be waited for until
//! [`EntryReconcilerConfig::held_ttl`](super::EntryReconcilerConfig). But the earlier process is
//! gone, and the registration says so: it names the `boot` of the process that wrote it, a new
//! process writes its own at open (before it can make an entry), and every entry a proxy holds
//! names its boot as `owner_boot` (see the module `entry`).
//!
//! Two processes of one proxy id can also be alive together (a hand-off: the new process takes the
//! sockets, the old one drains), and the old one's entries are then not left by anyone. They write
//! the one registration over each other, so it names a boot at a time; but each names the boots of
//! the others it has heard of in `peers` (`tiered/peers.rs`), for `departure` of beats after it
//! last heard them. So the registration names every process that was alive within that time.
//!
//! The step therefore reads, after it has read the entries, the registration of each owner of a
//! held put or delete that names a boot (one read for each owner, never kept to the next step),
//! and [`EntryReconciler::restarted`] says whether the boot of the entry is named there by
//! neither `boot` nor `peers`. The registration is read after the entry, so a boot that is not
//! named is a process that was not alive in the window, and the process that made the entry,
//! if it were, would have named itself or been named. A registration that cannot be read, or says
//! nothing of its peers (an older release's empty body or one without the field), or is gone (the
//! owner has departed, which is judged on its own), says nothing here.

use std::collections::{BTreeMap, BTreeSet};

use futures::StreamExt;
use futures::stream;

use super::EntryReconciler;
use super::work::is_held_by_other;
use crate::Error;
use crate::entry::{Entry, Seen};
use crate::journal::proxy_key;
use crate::registration::Registration;

impl EntryReconciler {
    /// The boots that the registrations of the owners of the held puts and deletes of other
    /// owners in `items` name now (an owner whose entry names no boot is not read, nor is one
    /// whose registration does not name its peers), and the errors of the reads that failed.
    pub(super) async fn read_boots(
        &self,
        items: &[(String, Seen)],
    ) -> (BTreeMap<String, BTreeSet<String>>, Vec<Error>) {
        let holder = self.config.base.holder.as_str();
        let owners: BTreeSet<&str> = items
            .iter()
            .map(|(_, seen)| &seen.entry)
            .filter(|entry| is_held_by_other(entry, holder) && entry.owner_boot.is_some())
            .filter_map(|entry| entry.owner.as_deref())
            .collect();
        let parallel = self.config.base.parallel.max(1);
        let reads = stream::iter(owners).map(|owner| async move {
            let body = self.hot.get(&proxy_key(owner), None).await;
            (owner, body)
        });
        let read: Vec<_> = reads.buffer_unordered(parallel).collect().await;
        let mut boots = BTreeMap::new();
        let mut failed = Vec::new();
        for (owner, body) in read {
            match body {
                Ok(body) => {
                    let named = Registration::decode(&body).and_then(|found| found.named());
                    if let Some(named) = named {
                        boots.insert(owner.to_owned(), named);
                    }
                }
                Err(Error::NotFound(_)) => {}
                Err(error) => failed.push(error),
            }
        }
        (boots, failed)
    }

    /// Whether `entry`, held by `owner`, was made by a process that the registration of `owner`
    /// names no more.
    pub(super) fn restarted(&self, entry: &Entry, owner: &str) -> bool {
        match (&entry.owner_boot, self.boots.get(owner)) {
            (Some(made), Some(named)) => !named.contains(made),
            _ => false,
        }
    }
}
