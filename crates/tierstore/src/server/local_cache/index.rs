//! What the cache holds, in memory: each object's size and tag, and the order of last use.
//!
//! The order is a counter, not a time: an object that is used takes the next number, and the
//! oldest use is the lowest. The files' times are only the seed of the order at start and a
//! record kept for the next one (see [`Index::take_touched`]).

use std::collections::{BTreeMap, HashMap};

use crate::Meta;

use super::layout::Origin;
use super::pin_set::PinSet;

/// One object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub size: u64,
    pub tag: String,
    pub modified: u64,
    /// When the entry was filled or last found to be the upstream's version, on the cache's
    /// clock in milliseconds; none for an entry that was found on the disk at start, whose age
    /// is not known (see `revalidate`).
    checked: Option<u64>,
    /// The place in the order of use.
    used: u64,
    /// Where the bytes came from: an entry of origin `span` is not an object of the upstream.
    pub(super) origin: Origin,
}

impl Entry {
    /// What a HEAD says of the object.
    pub(super) fn meta(&self) -> Meta {
        Meta {
            size: self.size,
            tag: self.tag.clone(),
            modified: self.modified,
        }
    }
}

/// The objects of the cache, by name, and in the order of their last use.
#[derive(Debug, Default)]
pub(super) struct Index {
    entries: HashMap<String, Entry>,
    order: BTreeMap<u64, String>,
    next: u64,
    bytes: u64,
    /// The objects used since the last [`Index::take_touched`], with the time of the last use.
    touched: HashMap<String, u64>,
    /// The trees that pin objects (see `pins`).
    pub(super) pins: PinSet,
}

impl Index {
    /// The bytes of all the objects.
    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The number of objects.
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The object `hex`, without counting it as used.
    pub(super) fn get(&self, hex: &str) -> Option<&Entry> {
        self.entries.get(hex)
    }

    /// Adds the object `hex`, as the most recently used; replaces one of that name. `checked` is
    /// when the upstream was last seen to hold this version of it (see [`Entry::checked`]).
    pub(super) fn insert(&mut self, hex: &str, meta: (u64, String, u64), checked: Option<u64>) {
        self.insert_from(hex, meta, checked, Origin::Object);
    }

    /// [`insert`](Self::insert) of an object of the given origin.
    pub(super) fn insert_from(
        &mut self,
        hex: &str,
        (size, tag, modified): (u64, String, u64),
        checked: Option<u64>,
        origin: Origin,
    ) {
        let _ = self.detach(hex);
        let used = self.next;
        self.next += 1;
        let entry = Entry {
            size,
            tag,
            modified,
            checked,
            used,
            origin,
        };
        self.order.insert(used, hex.to_owned());
        self.entries.insert(hex.to_owned(), entry);
        self.bytes += size;
        // An object written over is still the one the trees pin.
        self.pins.resize(hex, size);
    }

    /// Takes the right to check the object `hex` against the upstream, if a check is due at
    /// `now`: it was filled or last checked more than `after` milliseconds ago, or never, or at a
    /// time after `now` (the clock was set back). With `after` 0 a check is always due, so that
    /// `revalidate_after = "0s"` checks every hit, also one in the millisecond of the fill or of
    /// the last check. The entry's time is set to `now` at once, so
    /// that the hits that come while the check is out are served and send none of their own: a
    /// cached object costs one check an interval. Gives what the entry says of the object, and
    /// the time it had before, which [`Index::unclaim`] puts back if the check does not end.
    pub(super) fn claim(&mut self, hex: &str, now: u64, after: u64) -> Option<(Meta, Option<u64>)> {
        let entry = self.entries.get_mut(hex)?;
        let due = entry
            .checked
            .is_none_or(|then| after == 0 || then > now || now - then > after);
        if !due {
            return None;
        }
        let before = entry.checked.replace(now);
        Some((entry.meta(), before))
    }

    /// Puts back the time `before` of the entry `hex` that [`Index::claim`] set to `claimed`,
    /// unless the entry is not that claim any more (it was filled again, or checked since).
    pub(super) fn unclaim(&mut self, hex: &str, claimed: u64, before: Option<u64>) {
        if let Some(entry) = self.entries.get_mut(hex)
            && entry.checked == Some(claimed)
        {
            entry.checked = before;
        }
    }

    /// Takes the object out, and its pins; gives it if it was there.
    pub(super) fn remove(&mut self, hex: &str) -> Option<Entry> {
        self.pins.forget(hex);
        self.detach(hex)
    }

    /// Takes the object out of the entries and the order, and leaves its pins.
    fn detach(&mut self, hex: &str) -> Option<Entry> {
        let entry = self.entries.remove(hex)?;
        self.order.remove(&entry.used);
        self.touched.remove(hex);
        self.bytes -= entry.size;
        Some(entry)
    }

    /// Counts the object as used at `now`, and gives its size, if it is there.
    pub(super) fn used(&mut self, hex: &str, now: u64) -> Option<u64> {
        let entry = self.entries.get_mut(hex)?;
        self.order.remove(&entry.used);
        entry.used = self.next;
        self.next += 1;
        self.order.insert(entry.used, hex.to_owned());
        self.touched.insert(hex.to_owned(), now);
        Some(entry.size)
    }

    /// The names of the objects, in no order (the caller sorts, outside the lock the index is
    /// behind).
    pub(super) fn names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// The names and sizes of the objects, the least recently used first.
    pub(super) fn oldest_first(&self) -> Vec<(String, u64)> {
        let size = |hex: &String| self.entries.get(hex).map_or(0, |entry| entry.size);
        self.order
            .values()
            .map(|hex| (hex.clone(), size(hex)))
            .collect()
    }

    /// The objects used since the last call, with the time of the last use of each.
    pub(super) fn take_touched(&mut self) -> Vec<(String, u64)> {
        let mut touched: Vec<(String, u64)> = self.touched.drain().collect();
        touched.sort();
        touched
    }
}
