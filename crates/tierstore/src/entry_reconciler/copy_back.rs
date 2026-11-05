//! The copy-back of the cold tier's object into the hot tier, for an entry `(CB, PENDING)` that a
//! read of a cold object asked for. The request carries no bytes: the reconciler copies the object
//! the key holds when it takes the entry, so that a copy-back cannot bring back what a delete
//! removed, as long as the reconciler that sealed the entry still holds the lease. The hot write
//! stops at the first chunk it asks for after the margin of the lease is gone; a write already
//! sent to a store that applies it later, or one of a frozen process, is not fenced.

use std::collections::BTreeSet;

use super::EntryReconciler;
use super::work::Verdict;
use crate::Error;
use crate::entry::{EntryOp, EntryState, Seen};

impl EntryReconciler {
    /// Notes the copy-backs in `items`: when each was first seen (a new entry of the key, by its
    /// nonce, starts again), and which this step drops without a copy because they have waited
    /// [`EntryReconcilerConfig::copy_back_ttl`](super::EntryReconcilerConfig) or more, or are not
    /// among the oldest [`EntryReconcilerConfig::copy_back_max`](super::EntryReconcilerConfig).
    pub(super) fn note_copy_backs(&mut self, items: &[(String, Seen)], now: u64) {
        let read: BTreeSet<&String> = items.iter().map(|(_, seen)| &seen.entry.key).collect();
        let open: BTreeSet<&String> = items
            .iter()
            .filter(|(_, seen)| is_copy_back(seen))
            .map(|(_, seen)| &seen.entry.key)
            .collect();
        for key in read.difference(&open) {
            self.copy_backs.remove(*key);
        }
        for (_, seen) in items.iter().filter(|(_, seen)| is_copy_back(seen)) {
            let entry = &seen.entry;
            let known = self
                .copy_backs
                .get(&entry.key)
                .is_some_and(|(nonce, _)| *nonce == entry.nonce);
            if !known {
                self.copy_backs
                    .insert(entry.key.clone(), (entry.nonce.clone(), now));
            }
        }
        let ttl = crate::deadline::millis(self.config.copy_back_ttl);
        let mut by_age: Vec<(u64, &String)> = self
            .copy_backs
            .iter()
            .map(|(key, (_, first))| (*first, key))
            .collect();
        by_age.sort();
        self.copy_back_drops = by_age
            .iter()
            .enumerate()
            .filter(|(rank, (first, _))| {
                *rank >= self.config.copy_back_max || now.saturating_sub(*first) >= ttl
            })
            .map(|(_, (_, key))| (*key).clone())
            .collect();
    }

    /// The step of `(CB, PENDING)`: seal it, copy under the seal unless the entry is to be
    /// dropped, and remove it. A seal that fails means a delete amended the entry: no copy
    /// starts, and the entry is left as the delete made it.
    pub(super) async fn copy_back(&self, seen: &Seen) -> Result<Verdict, Error> {
        let Some(sealed) = self.seal(seen).await? else {
            return Ok(Verdict::Changed);
        };
        let key = &sealed.entry.key;
        if !self.copy_back_drops.contains(key) {
            self.land_copy_back(key).await?;
        }
        self.release(&sealed).await
    }

    /// Streams the cold object of `key` into the hot tier, create-only, if the hot tier has none:
    /// nothing if the hot tier has one, the cold tier has none, or the hot tier is full. The
    /// object is read once, and no bytes are held whole; the write stops, and lands nothing, once less
    /// than the margin of the lease is left. A failure leaves the entry sealed, which
    /// the next step removes.
    async fn land_copy_back(&self, key: &str) -> Result<(), Error> {
        match self.hot.head(key).await {
            Ok(_) => return Ok(()),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let opened = match self.cold.open_read(key, None).await {
            Ok(opened) => opened,
            Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        self.require_lease()?;
        let body = self.while_leased(opened.body);
        match self.hot.put_stream(key, Some(opened.meta.size), body).await {
            Ok(()) | Err(Error::AlreadyExists(_) | Error::Full(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Whether `seen` is a copy-back that has not been taken.
fn is_copy_back(seen: &Seen) -> bool {
    (seen.entry.op, seen.entry.st) == (EntryOp::Cb, EntryState::Pending)
}
