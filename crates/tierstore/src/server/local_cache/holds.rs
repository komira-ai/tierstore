//! Holds: the blobs a caller asked the cache to fetch ahead of a job, kept against eviction until
//! the caller lets go.
//!
//! A hold is a name the caller chooses (`hold`), the trees it is for, and a time at which it ends
//! by itself. The blobs of its trees are handed to the prefetch queue one at a time
//! ([`LocalCache::hold_blob`]); the hold remembers each, and when the blob is in the cache, filled
//! by a worker, by a read that promoted it, or there already, the hold pins it, up to
//! `pin_limit`, by the pin machinery of `pins`: the owner is the mount `hold` and the tree
//! `<hold>/<tree>`, so that a blob of two trees is pinned for each, and the pins of one hold end
//! without touching another's. A hold's pins are in memory only: they are not in the pins file and
//! `pins_live` leaves them, because the holds are lost at a restart and a pin whose owner is gone
//! would never end.
//!
//! The first `pin` of a mount for a tree of the hold hands the tree over ([`Inner::attach_holds`]):
//! the hold's pins on it move to the mount's owner, and a blob of the tree that settles later is
//! pinned for the mounts that have pinned for the tree, never for the hold. A tree that a mount
//! had pinned for before the hold asked for a blob of it is handed over at that first blob.
//!
//! A hold ends at [`LocalCache::release_hold`], or when `hold_max` (or the time the caller gave,
//! if shorter) has passed, noticed at the next tick of the cache. Ending it unpins what it owns
//! and drops the blobs it queued that are still waiting for a worker and that no other hold
//! waits for; a fill that has started is left to finish, since its bytes are paid for and a read
//! may have joined it. At most `hold_max_count` holds are open at once, each for at most
//! `hydrate_max_trees` trees however many calls added them.
//!
//! A hold belongs to the key that opened it (the access key id): a call of another key for the id
//! finds no hold. The expansion of its trees is one task per hold: the trees the calls added wait
//! in the hold's list, the task takes them one at a time, and the blobs and bytes it queues count
//! against `hydrate_max_blobs` and `hydrate_max_bytes` for the hold as a whole. A hold carries a
//! generation, and every step of the task, the next tree, each manifest, each blob, finds the hold
//! by id and generation, so that a task whose hold has ended stops, and never writes into a hold
//! that took the name later.
//!
//! Locks are taken in the order table of fills, queue, holds, index; the holds are never locked
//! while the caller holds the index.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use thiserror::Error;

use super::expand::Bound;
use super::layout::is_digest;
use super::metrics::Walked;
use super::pin_set::{HOLD_MOUNT, Owner};
use super::prefetch::Prefetched;
use super::source::Blob;
use super::{Inner, LocalCache, locked};
use crate::tree_manifest::TreeDigest;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Why a hold operation was not done.
#[derive(Debug, Error, PartialEq, Eq)]
pub(in crate::server) enum HoldError {
    /// This cache does not hold the lock of its directory: it fills and pins nothing.
    #[error("this cache does not hold the lock of its directory")]
    ReadOnly,
    /// `hold_max_count` holds are open already.
    #[error("{0} holds are open already")]
    TooMany(usize),
    /// No hold of this name is open (it was released, or it expired).
    #[error("no hold {0:?} is open")]
    NoSuchHold(String),
    /// The hold is not for this tree.
    #[error("the hold {hold:?} is not for the tree {tree:?}")]
    NoSuchTree { hold: String, tree: String },
    /// The hold would be for more trees than `hydrate_max_trees`.
    #[error("{given} trees in the hold, at most {max}")]
    TooManyTrees { given: usize, max: usize },
    /// A tree or a blob is named by something that is not the lower-case hex of a SHA-256.
    #[error("{0:?} is not the lower-case hex of a SHA-256")]
    NotADigest(String),
}

/// What a hold came to when it ended: the blobs it asked for, by what became of each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct Released {
    /// The trees the hold was for.
    pub(in crate::server) trees: usize,
    /// The trees of the hold whose expansion ended with every blob handed to the queue.
    pub(in crate::server) expanded: usize,
    /// The trees that were not expanded: a manifest was refused or could not be fetched, or the
    /// key of the call may not read it.
    pub(in crate::server) refused: usize,
    /// The trees whose walk stopped at a bound of the call, or because the hold ended.
    pub(in crate::server) cut: usize,
    /// The blobs a fill kept after the hold asked for them.
    pub(in crate::server) filled: usize,
    /// The blobs the cache held when the hold asked for them, or by the time their turn came.
    pub(in crate::server) present: usize,
    /// The blobs turned away by the queue, dropped when their turn came (they waited too long, or
    /// did not fit), or dropped by this release because nothing else wanted them.
    pub(in crate::server) dropped: usize,
    /// The blobs whose fill ended without them.
    pub(in crate::server) failed: usize,
    /// The blobs still queued for another hold, or being filled: left alone.
    pub(in crate::server) pending: usize,
}

/// What became of a blob a hold asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    /// Queued or being filled.
    Pending,
    /// The cache held it.
    Present,
    /// A fill kept it.
    Filled,
    /// A fill ended without it.
    Failed,
    /// It was not filled: turned away, dropped from the queue, or dropped by the release.
    Dropped,
}

/// What opening a hold, or adding to it, came to.
pub(super) struct Opened {
    pub(super) generation: u64,
    /// The trees the call added that the hold was not for.
    pub(super) added: Vec<String>,
}

/// A blob a hold asked for.
struct Wanted {
    /// The trees of the hold it is in: it is pinned for each.
    trees: BTreeSet<String>,
    state: State,
}

/// What the expansion of a hold has queued, which the bounds of `hydrate_max_blobs` and
/// `hydrate_max_bytes` are judged against: one count for the hold, however many calls made it.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Spent {
    pub(super) blobs: usize,
    pub(super) bytes: u64,
    /// The bound the expansion reached, once it has: the trees after it are cut unread.
    pub(super) reached: Option<Bound>,
}

/// One hold.
struct Hold {
    /// The access key id of the key that opened it: no other key may add to it or end it.
    owner: String,
    /// Tells this hold from a later one of the same id, so that the expansion of a hold that
    /// ended does not hand blobs to the one that took its name.
    generation: u64,
    /// When it ends by itself, in milliseconds of the cache's clock.
    expires: u64,
    /// The trees added by calls that no walk has taken yet.
    unwalked: VecDeque<TreeDigest>,
    /// Whether a task is expanding `unwalked`: at most one task per hold.
    walking: bool,
    spent: Spent,
    trees: BTreeSet<String>,
    /// The trees that were attached to a mount while the hold was open (or before it asked for a
    /// blob of them): it pins nothing of them, the attachments do.
    handed: BTreeSet<String>,
    blobs: HashMap<String, Wanted>,
    /// The trees whose expansion has ended, by how it ended: expanded, refused, cut.
    walked: [usize; 3],
}

/// The open holds.
#[derive(Default)]
pub(super) struct Holds {
    by_id: HashMap<String, Hold>,
    /// The generation the next hold opened gets.
    generations: u64,
    /// For each blob that a hold still waits for, the holds.
    waiting: HashMap<String, BTreeSet<String>>,
    /// For each tree that a mount has pinned for (see [`Inner::attach_holds`]), the mounts: the
    /// attachments a blob of a handed tree is pinned for.
    attached: BTreeMap<String, BTreeSet<String>>,
}

/// The trees of `listed` that `had` does not have, each once.
fn fresh_trees(listed: &[String], had: Option<&BTreeSet<String>>) -> BTreeSet<String> {
    let new = |tree: &&String| !had.is_some_and(|had| had.contains(*tree));
    listed.iter().filter(new).cloned().collect()
}

/// The tree of the pin owner of `tree` of the hold `id`.
fn owner_tree(id: &str, tree: &str) -> String {
    format!("{id}/{tree}")
}

impl Holds {
    /// Notes that the hold `id` waits for `hex`, in `tree`: pending now, whatever it was.
    fn want(&mut self, at: (&str, u64), tree: &str, hex: &str) -> Result<(), HoldError> {
        let id = at.0;
        let hold = self
            .by_id
            .get_mut(id)
            .filter(|hold| hold.generation == at.1)
            .ok_or_else(|| HoldError::NoSuchHold(id.to_owned()))?;
        if !hold.trees.contains(tree) {
            return Err(HoldError::NoSuchTree {
                hold: id.to_owned(),
                tree: tree.to_owned(),
            });
        }
        let wanted = hold.blobs.entry(hex.to_owned()).or_insert_with(|| Wanted {
            trees: BTreeSet::new(),
            state: State::Pending,
        });
        let _ = wanted.trees.insert(tree.to_owned());
        wanted.state = State::Pending;
        // A tree that is attached already is the attachments' to pin, from the first blob.
        if self.attached.contains_key(tree) {
            let _ = hold.handed.insert(tree.to_owned());
        }
        let _ = self
            .waiting
            .entry(hex.to_owned())
            .or_default()
            .insert(id.to_owned());
        Ok(())
    }

    /// Marks `hex` as `state` for every hold that waited for it; the owners to pin it for if it is
    /// in the cache: the hold's, for a tree it still owns the pins of, and else the attachments'.
    fn settle(&mut self, hex: &str, state: State) -> Vec<Owner> {
        let ids = self.waiting.remove(hex).unwrap_or_default();
        let mut owners = Vec::new();
        for id in ids {
            let found = self.by_id.get_mut(&id).and_then(|hold| {
                let Hold { blobs, handed, .. } = hold;
                blobs.get_mut(hex).map(|wanted| (wanted, &*handed))
            });
            found.into_iter().for_each(|(wanted, handed)| {
                wanted.state = state;
                for tree in &wanted.trees {
                    if handed.contains(tree) {
                        let mounts = self.attached.get(tree).into_iter().flatten();
                        owners.extend(mounts.map(|mount| Owner {
                            mount: mount.clone(),
                            tree: tree.clone(),
                        }));
                    } else {
                        owners.push(Owner {
                            mount: HOLD_MOUNT.to_owned(),
                            tree: owner_tree(&id, tree),
                        });
                    }
                }
            });
        }
        owners
    }

    /// Forgets every attachment `live` does not keep: it is given the mount and the tree.
    fn unattach(&mut self, live: impl Fn(&str, &str) -> bool) {
        self.attached.retain(|tree, mounts| {
            mounts.retain(|mount| live(mount, tree));
            !mounts.is_empty()
        });
    }

    /// Marks `hex` as `state` for the hold `id` alone, which waits for it no more.
    fn settle_one(&mut self, id: &str, hex: &str, state: State) {
        // The hold may have ended since it was asked: then there is nothing to mark.
        let hold = self
            .by_id
            .get_mut(id)
            .and_then(|hold| hold.blobs.get_mut(hex));
        hold.into_iter().for_each(|wanted| wanted.state = state);
        let _ = self.stop_waiting(id, hex);
    }

    /// The hold `id` waits for `hex` no more; says whether another hold still does.
    fn stop_waiting(&mut self, id: &str, hex: &str) -> bool {
        let others = self.waiting.get_mut(hex).is_some_and(|ids| {
            let _ = ids.remove(id);
            !ids.is_empty()
        });
        if !others {
            let _ = self.waiting.remove(hex);
        }
        others
    }
}

impl Inner {
    /// Opens the hold `id` for the key `owner`, or adds `trees` to it if that key opened it
    /// already; see [`LocalCache::hydrate`]. Says the generation of the hold and the trees
    /// that are new to it.
    pub(super) fn open_hold(
        &self,
        id: &str,
        owner: &str,
        expires_ms: Option<u64>,
        trees: &[String],
    ) -> Result<Opened, HoldError> {
        if !self.read_write() {
            return Err(HoldError::ReadOnly);
        }
        if let Some(bad) = trees.iter().find(|tree| !is_digest(tree)) {
            return Err(HoldError::NotADigest(bad.clone()));
        }
        let settings = &self.config.prefetch;
        let longest = u64::try_from(settings.hold_max.as_millis()).unwrap_or(u64::MAX);
        let lasts = expires_ms.map_or(longest, |ms| ms.min(longest));
        let expires = self.clock.now().saturating_add(lasts);
        let mut holds = locked(&self.prefetch.holds);
        let open = holds.by_id.len();
        // A hold of another key is not there for this one: the answer is the one for a hold
        // that has ended, which tells nothing of the other key's ids.
        let (known, fresh, given) = match holds.by_id.get(id) {
            Some(hold) if hold.owner == owner => {
                let fresh = fresh_trees(trees, Some(&hold.trees));
                (Some(hold.generation), fresh, hold.trees.len())
            }
            Some(_) => return Err(HoldError::NoSuchHold(id.to_owned())),
            None if open >= settings.hold_max_count => return Err(HoldError::TooMany(open)),
            None => (None, fresh_trees(trees, None), 0),
        };
        let given = given + fresh.len();
        if given > settings.max_trees {
            let max = settings.max_trees;
            return Err(HoldError::TooManyTrees { given, max });
        }
        let generation = known.unwrap_or(holds.generations + 1);
        holds.generations = holds.generations.max(generation);
        let hold = holds.by_id.entry(id.to_owned()).or_insert_with(|| Hold {
            owner: owner.to_owned(),
            generation,
            expires,
            unwalked: VecDeque::new(),
            walking: false,
            spent: Spent::default(),
            trees: BTreeSet::new(),
            handed: BTreeSet::new(),
            blobs: HashMap::new(),
            walked: [0; 3],
        });
        hold.trees.extend(fresh.iter().cloned());
        self.counters
            .holds_open
            .store(holds.by_id.len() as u64, Ordering::Relaxed);
        Ok(Opened {
            generation,
            added: fresh.into_iter().collect(),
        })
    }

    /// Adds `trees` to the list of the hold `id` of `generation` that no walk has taken, and says
    /// whether the caller starts the task that walks them: it does when none is running.
    pub(super) fn queue_walk(&self, at: (&str, u64), trees: &[TreeDigest]) -> bool {
        let mut holds = locked(&self.prefetch.holds);
        let Some(hold) = holds
            .by_id
            .get_mut(at.0)
            .filter(|hold| hold.generation == at.1)
        else {
            return false;
        };
        hold.unwalked.extend(trees.iter().copied());
        let start = !hold.walking && !hold.unwalked.is_empty();
        hold.walking |= start;
        start
    }

    /// The next tree of the hold `id` of `generation` for the task that walks them, with what the
    /// hold's walks have queued so far; none, and the task ends, when the hold has ended or has no
    /// tree left.
    pub(super) fn next_tree(&self, at: (&str, u64)) -> Option<(TreeDigest, Spent)> {
        let mut holds = locked(&self.prefetch.holds);
        let hold = holds
            .by_id
            .get_mut(at.0)
            .filter(|hold| hold.generation == at.1)?;
        let next = hold.unwalked.pop_front().map(|tree| (tree, hold.spent));
        hold.walking = next.is_some();
        next
    }

    /// Whether the hold `id` of `generation` is open still.
    pub(super) fn hold_live(&self, at: (&str, u64)) -> bool {
        let holds = locked(&self.prefetch.holds);
        holds
            .by_id
            .get(at.0)
            .is_some_and(|hold| hold.generation == at.1)
    }

    /// See [`LocalCache::hold_blob`].
    pub(super) fn hold_blob(
        self: &Arc<Self>,
        at: (&str, u64),
        tree: &str,
        blob: &Blob<'_>,
    ) -> Result<Prefetched, HoldError> {
        let id = at.0;
        if !is_digest(blob.sha256) {
            return Err(HoldError::NotADigest(blob.sha256.to_owned()));
        }
        // The hold waits for the blob before the queue has it, so that a fill that ends at once
        // finds the hold.
        locked(&self.prefetch.holds).want(at, tree, blob.sha256)?;
        let outcome = self.queue_blob(blob);
        match outcome {
            Prefetched::Queued | Prefetched::Joined => {}
            Prefetched::Present => self.hold_settle(blob.sha256, State::Present),
            Prefetched::TooLarge | Prefetched::Busy | Prefetched::Off | Prefetched::ReadOnly => {
                locked(&self.prefetch.holds).settle_one(id, blob.sha256, State::Dropped);
            }
        }
        Ok(outcome)
    }

    /// The blob `hex` is settled as `state`: for the holds that waited for it, and, if it is in the
    /// cache, pinned for each of their trees.
    pub(super) fn hold_settle(&self, hex: &str, state: State) {
        let mut holds = locked(&self.prefetch.holds);
        let owners = holds.settle(hex, state);
        if matches!(state, State::Present | State::Filled) {
            let blobs = [hex.to_owned()];
            for owner in owners {
                // Past `pin_limit` the pin is refused and the blob counted as used: recency only.
                let _ = self.pin(&owner.mount, &owner.tree, &blobs);
            }
        }
    }

    /// A mount has pinned for `tree`: the tree is attached. The pins of every hold that lists it
    /// and has not handed it over yet move to the attachment, and from then on the blobs of the
    /// tree that a fill settles for the hold are pinned for the attachments. Called after each
    /// successful pin of a mount (never for the mount of the holds).
    pub(super) fn attach_holds(&self, mount: &str, tree: &str) {
        let mut holds = locked(&self.prefetch.holds);
        let _ = holds
            .attached
            .entry(tree.to_owned())
            .or_default()
            .insert(mount.to_owned());
        let to = Owner {
            mount: mount.to_owned(),
            tree: tree.to_owned(),
        };
        let mut index = locked(&self.index);
        let mut handovers = 0;
        for (id, hold) in &mut holds.by_id {
            if hold.trees.contains(tree) && hold.handed.insert(tree.to_owned()) {
                let from = Owner {
                    mount: HOLD_MOUNT.to_owned(),
                    tree: owner_tree(id, tree),
                };
                let _ = index.pins.transfer(&from, &to);
                handovers += 1;
            }
        }
        self.counters
            .hold_handovers
            .fetch_add(handovers, Ordering::Relaxed);
    }

    /// Some attachments are gone (a mount unpinned a tree, or a sweep took the pins of the mounts
    /// and trees `live` does not keep): a hold that handed a tree over pins nothing of it for them.
    pub(super) fn detach_holds(&self, live: impl Fn(&str, &str) -> bool) {
        locked(&self.prefetch.holds).unattach(live);
    }

    /// See [`LocalCache::release_hold`]; also what the end of a hold's time does (`owner` none).
    /// A hold of another key than `owner` is not there.
    fn end_hold(&self, id: &str, owner: Option<&str>) -> Result<Released, HoldError> {
        let mut flights = locked(&self.flights);
        let mut queue = locked(&self.prefetch.queue);
        let mut holds = locked(&self.prefetch.holds);
        let theirs = holds
            .by_id
            .get(id)
            .is_some_and(|hold| owner.is_some_and(|key| key != hold.owner));
        if theirs {
            return Err(HoldError::NoSuchHold(id.to_owned()));
        }
        let hold = holds
            .by_id
            .remove(id)
            .ok_or_else(|| HoldError::NoSuchHold(id.to_owned()))?;
        let [expanded, refused, cut] = hold.walked;
        let mut released = Released {
            trees: hold.trees.len(),
            expanded,
            refused,
            cut,
            ..Released::default()
        };
        let mut gone = Vec::new();
        for (hex, wanted) in &hold.blobs {
            match wanted.state {
                State::Present => released.present += 1,
                State::Filled => released.filled += 1,
                State::Failed => released.failed += 1,
                State::Dropped => released.dropped += 1,
                State::Pending => {
                    let wanted_by_others = holds.stop_waiting(id, hex);
                    if !wanted_by_others && queue.take(hex).is_some() {
                        gone.push(flights.remove(hex));
                        let _ = locked(&self.invalidated).remove(hex);
                        released.dropped += 1;
                    } else {
                        released.pending += 1;
                    }
                }
            }
        }
        {
            let mut index = locked(&self.index);
            for tree in &hold.trees {
                let owner = Owner {
                    mount: HOLD_MOUNT.to_owned(),
                    tree: owner_tree(id, tree),
                };
                let _ = index.pins.unpin(&owner);
            }
        }
        self.counters
            .holds_open
            .store(holds.by_id.len() as u64, Ordering::Relaxed);
        drop(holds);
        drop(queue);
        drop(flights);
        drop(gone);
        Ok(released)
    }

    /// Notes that the expansion of a tree of the hold `id` of `generation` ended as `how`, with
    /// `spent` queued by the hold's walks in all. A hold that has ended since has nothing to note.
    pub(super) fn hold_walked(&self, at: (&str, u64), how: Walked, spent: Spent) {
        let slot = match how {
            Walked::Expanded => 0,
            Walked::Refused | Walked::AccessDenied => 1,
            Walked::Cut => 2,
        };
        let mut holds = locked(&self.prefetch.holds);
        let found = holds
            .by_id
            .get_mut(at.0)
            .filter(|hold| hold.generation == at.1);
        if let Some(hold) = found {
            hold.walked[slot] += 1;
            hold.spent = spent;
        }
    }

    /// Ends the holds whose time has passed. Called at every tick of the cache.
    pub(super) fn expire_holds(&self) {
        let now = self.clock.now();
        let due: Vec<String> = locked(&self.prefetch.holds)
            .by_id
            .iter()
            .filter(|(_, hold)| hold.expires <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            // A hold released since the list was made is gone already.
            let _ = self.end_hold(&id, None);
        }
    }
}

impl LocalCache {
    /// Opens the hold `id` for `trees` (lower-case hex digests), as the key `""`, with no
    /// expansion; the tests of the holds use it. See [`Inner::open_hold`].
    ///
    /// # Errors
    /// As [`Inner::open_hold`].
    #[cfg(test)]
    pub(in crate::server) fn open_hold(
        &self,
        id: &str,
        expires_ms: Option<u64>,
        trees: &[String],
    ) -> Result<usize, HoldError> {
        let _ = self.inner.open_hold(id, "", expires_ms, trees)?;
        let holds = locked(&self.inner.prefetch.holds);
        Ok(holds.by_id.get(id).map_or(0, |hold| hold.trees.len()))
    }

    /// Hands `blob`, a blob of `tree` of the hold `id`, to the prefetch queue (see
    /// [`prefetch`](Self::prefetch)) and remembers that the hold wants it: when it is in the cache
    /// the hold pins it. Says what the queue did with it.
    ///
    /// # Errors
    /// [`HoldError::NoSuchHold`] if the hold is not open (it was released or it expired), with
    /// nothing queued; [`HoldError::NoSuchTree`] if it is not for `tree`;
    /// [`HoldError::NotADigest`] for a blob that is not named by a digest.
    #[cfg(test)]
    pub(in crate::server) fn hold_blob(
        &self,
        id: &str,
        tree: &str,
        blob: &Blob<'_>,
    ) -> Result<Prefetched, HoldError> {
        let generation = locked(&self.inner.prefetch.holds)
            .by_id
            .get(id)
            .map_or(0, |hold| hold.generation);
        self.inner.hold_blob((id, generation), tree, blob)
    }

    /// Ends the hold `id` of any key: see [`release_hold_of`](Self::release_hold_of).
    #[cfg(test)]
    pub(in crate::server) fn release_hold(&self, id: &str) -> Result<Released, HoldError> {
        self.inner.end_hold(id, None)
    }

    /// Ends the hold `id` that the key `owner` opened: unpins what it pinned, drops the blobs it
    /// queued that wait for a worker and that no other hold waits for, and says what became of the
    /// blobs it asked for. The expansion of its trees stops at its next step.
    ///
    /// # Errors
    /// [`HoldError::NoSuchHold`] if the hold is not open, or another key opened it.
    pub(in crate::server) fn release_hold_of(
        &self,
        id: &str,
        owner: &str,
    ) -> Result<Released, HoldError> {
        self.inner.end_hold(id, Some(owner))
    }
}
