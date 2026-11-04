//! What the entry reconciler does to one entry: the step of the table of the design for the entry's
//! operation and state. Every step is idempotent, so that a step that was cut off, or that a new
//! leader finds half done, is done again from the start.

use super::EntryReconciler;
use crate::backend::ByteRange;
use crate::entry::{EntryOp, EntryState, Outcome, Seen, Step};
use crate::{Error, Meta};

/// What a look at one entry came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Nothing to do: the entry is in a state no step of this release acts on.
    Skipped,
    /// A live owner holds the entry: it waits for the owner, or for the owner to depart.
    Pinned,
    /// An entry changed, or was removed, or a store object was deleted or copied.
    Changed,
    /// As `Changed`, and the hot object and the cold object of the key differ in their bytes: the
    /// hot object is kept.
    Mismatched,
}

/// How the cold tier came to hold the hot object's bytes, or not.
enum Replicated {
    /// The cold tier holds them now, or there was nothing to copy.
    Same,
    /// The cold tier holds another object under the key.
    Differs,
}

/// A missing object is as good as a deleted one.
fn ignore_missing(answer: Result<(), Error>) -> Result<(), Error> {
    match answer {
        Err(Error::NotFound(_)) => Ok(()),
        other => other,
    }
}

impl EntryReconciler {
    /// Does the step the table gives for `seen`, at `now` on the monotonic clock.
    pub(super) async fn process(&self, seen: &Seen, now: u64) -> Result<Verdict, Error> {
        match (seen.entry.op, seen.entry.st) {
            (EntryOp::Put, EntryState::Pending) => self.copy_pending(seen).await,
            (EntryOp::Del, EntryState::Pending) => match self.seal(seen).await? {
                Some(sealed) => self.finish_delete(&sealed).await,
                None => Ok(Verdict::Changed),
            },
            (EntryOp::Cb, EntryState::Pending) => match self.seal(seen).await? {
                Some(sealed) => self.release(&sealed).await,
                None => Ok(Verdict::Changed),
            },
            (EntryOp::Del, EntryState::Sealed) => self.finish_delete(seen).await,
            (EntryOp::Put | EntryOp::Cb | EntryOp::Evict, EntryState::Sealed) => {
                self.release(seen).await
            }
            (EntryOp::Put | EntryOp::Del, EntryState::Held) => self.held(seen, now).await,
            _ => Ok(Verdict::Skipped),
        }
    }

    /// An entry held by someone: its own put, left by a copy that was cut off, is copied again;
    /// that of an owner that has departed is reaped; that of a live owner is waited for.
    async fn held(&self, seen: &Seen, now: u64) -> Result<Verdict, Error> {
        let entry = &seen.entry;
        let Some(owner) = entry.owner.as_deref() else {
            return Ok(Verdict::Skipped);
        };
        if owner == self.config.base.holder {
            return match entry.op {
                EntryOp::Put => self.copy_held(seen).await,
                _ => Ok(Verdict::Skipped),
            };
        }
        if !self
            .registrations
            .departed(owner, now, self.config.departure)
        {
            return Ok(Verdict::Pinned);
        }
        if let Outcome::Done(_) = self.entries.advance(seen, Step::Reap).await?
            && entry.op == EntryOp::Put
        {
            self.abort_uploads(&entry.key).await?;
        }
        Ok(Verdict::Changed)
    }

    /// Takes the hold on a landed put, and copies it. A hold that fails (a delete amended the
    /// entry) means no copy starts.
    async fn copy_pending(&self, seen: &Seen) -> Result<Verdict, Error> {
        match self.entries.advance(seen, Step::HoldForCopy).await? {
            Outcome::Done(held) => self.copy_held(&held).await,
            Outcome::Lost | Outcome::Refused => Ok(Verdict::Changed),
        }
    }

    /// Copies the key of an entry this reconciler holds, then seals the entry and removes it. A
    /// seal that fails means a delete amended the entry while the copy ran: the entry is left as
    /// that delete made it, and the delete is waiting for its release.
    async fn copy_held(&self, held: &Seen) -> Result<Verdict, Error> {
        let verdict = match self.replicate(&held.entry.key).await? {
            Replicated::Same => Verdict::Changed,
            Replicated::Differs => Verdict::Mismatched,
        };
        if let Outcome::Done(sealed) = self.entries.advance(held, Step::Seal).await? {
            self.release(&sealed).await?;
        }
        Ok(verdict)
    }

    /// Seals the entry, or gives `None` when it changed under the call.
    async fn seal(&self, seen: &Seen) -> Result<Option<Seen>, Error> {
        match self.entries.advance(seen, Step::Seal).await? {
            Outcome::Done(sealed) => Ok(Some(sealed)),
            Outcome::Lost | Outcome::Refused => Ok(None),
        }
    }

    /// Removes a sealed entry.
    async fn release(&self, sealed: &Seen) -> Result<Verdict, Error> {
        self.entries.release(sealed).await?;
        Ok(Verdict::Changed)
    }

    /// Deletes the key from the cold tier and then from the hot tier, and removes the sealed entry
    /// of the delete, which lets the deletes that wait for it answer.
    async fn finish_delete(&self, sealed: &Seen) -> Result<Verdict, Error> {
        self.require_lease()?;
        let key = &sealed.entry.key;
        ignore_missing(self.cold.delete(key).await)?;
        self.require_lease()?;
        ignore_missing(self.hot.delete(key).await)?;
        self.release(sealed).await
    }

    /// Refuses a destructive step or a publish to the cold tier when less than the margin of the
    /// lease is left.
    fn require_lease(&self) -> Result<(), Error> {
        if self.lease.margin_ok() {
            return Ok(());
        }
        Err(Error::Unavailable(
            "less than the margin of the lease is left".to_owned(),
        ))
    }

    /// Makes the cold tier hold the hot object of `key`: nothing if the hot tier has none, a copy
    /// (create-only) if the cold tier has none, and a comparison of the bytes if it has one.
    async fn replicate(&self, key: &str) -> Result<Replicated, Error> {
        let hot = match self.hot.head(key).await {
            Ok(hot) => hot,
            Err(Error::NotFound(_)) => return Ok(Replicated::Same),
            Err(error) => return Err(error),
        };
        match self.cold.head(key).await {
            Ok(cold) => self.compare(key, &hot, &cold).await,
            Err(Error::NotFound(_)) => self.publish(key, &hot).await,
            Err(error) => Err(error),
        }
    }

    /// Streams the hot object to the cold tier, create-only. An object that appeared there since
    /// the look counts as copied.
    async fn publish(&self, key: &str, hot: &Meta) -> Result<Replicated, Error> {
        self.require_lease()?;
        let body = match self.hot.get_stream(key, None).await {
            Ok(body) => body,
            Err(Error::NotFound(_)) => return Ok(Replicated::Same),
            Err(error) => return Err(error),
        };
        self.require_lease()?;
        match self.cold.put_stream(key, Some(hot.size), body).await {
            Ok(()) | Err(Error::AlreadyExists(_)) => Ok(Replicated::Same),
            Err(error) => Err(error),
        }
    }

    /// Whether the cold object has the bytes of the hot one, read in ranges of at most
    /// [`EntryReconcilerConfig::compare_range`](super::EntryReconcilerConfig) bytes, so that
    /// neither is held whole; the read ends at the first difference.
    async fn compare(&self, key: &str, hot: &Meta, cold: &Meta) -> Result<Replicated, Error> {
        if hot.size != cold.size {
            return Ok(Replicated::Differs);
        }
        let step = self.config.compare_range.max(1);
        let mut offset = 0;
        while offset < hot.size {
            let range = ByteRange::new(offset, step.min(hot.size - offset));
            let mine = self.hot.get(key, Some(range)).await?;
            let theirs = self.cold.get(key, Some(range)).await?;
            if mine != theirs {
                return Ok(Replicated::Differs);
            }
            offset += range.size();
        }
        Ok(Replicated::Same)
    }

    /// Aborts the multipart uploads that the dead owner of a put left open for `key`, on both
    /// tiers. It is safe because the reconciler holds the key now. A store that does not do
    /// multipart uploads has none.
    async fn abort_uploads(&self, key: &str) -> Result<(), Error> {
        let page_size = self.config.base.page_size.max(1);
        for tier in [&self.hot, &self.cold] {
            let mut after: Option<(String, String)> = None;
            loop {
                let marker = after
                    .as_ref()
                    .map(|(k, id)| (k.as_str(), Some(id.as_str())));
                let page = match tier.list_uploads(key, marker, page_size).await {
                    Ok(page) => page,
                    Err(Error::Unsupported(_)) => break,
                    Err(error) => return Err(error),
                };
                let last = page.len() < page_size;
                for upload in page.iter().filter(|upload| upload.key == key) {
                    ignore_missing(tier.abort_upload(key, &upload.id).await)?;
                }
                if last {
                    break;
                }
                after = page
                    .last()
                    .map(|upload| (upload.key.clone(), upload.id.clone()));
            }
        }
        Ok(())
    }
}
