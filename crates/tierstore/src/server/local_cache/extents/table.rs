//! The entries by extents, in memory: each one's object, its filled ranges and its place in the
//! order of use. They live in the [`Index`](super::super::index::Index), beside the whole
//! objects and apart from them: a read of a key finds a whole object by `Index::get`, which
//! never answers for an entry by extents.

use std::collections::{BTreeMap, HashMap};

use super::super::index::Index;
use super::map::ExtentMap;

/// What the caller says of the object an entry is of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server::local_cache) struct Object {
    pub key: String,
    pub size: u64,
    pub tag: String,
    pub modified: u64,
}

/// An entry by extents.
#[derive(Debug)]
pub(in crate::server::local_cache) struct Partial {
    pub object: Object,
    pub map: ExtentMap,
    /// The place in the order of use, which is the whole objects' order.
    used: u64,
    /// Told apart from an entry of the same name made after this one was removed.
    pub serial: u64,
    /// The fills that are writing to the file; the entry is not evicted while there are any.
    filling: usize,
}

/// The entries by extents of the cache.
#[derive(Debug, Default)]
pub(in crate::server::local_cache) struct ExtentTable {
    entries: HashMap<String, Partial>,
    order: BTreeMap<u64, String>,
    bytes: u64,
    serial: u64,
}

impl ExtentTable {
    /// The filled bytes of all the entries, each counted once.
    pub(in crate::server::local_cache) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The number of entries.
    pub(in crate::server::local_cache) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The entry `hex`.
    pub(in crate::server::local_cache) fn get(&self, hex: &str) -> Option<&Partial> {
        self.entries.get(hex)
    }
}

impl Index {
    /// Adds an entry found on the disk, as the most recently used.
    pub(in crate::server::local_cache) fn extent_found(
        &mut self,
        hex: &str,
        object: Object,
        map: ExtentMap,
    ) {
        let used = self.next;
        self.next += 1;
        let table = &mut self.partials;
        table.serial += 1;
        table.bytes += map.bytes();
        table.order.insert(used, hex.to_owned());
        let serial = table.serial;
        let filling = 0;
        let entry = Partial {
            object,
            map,
            used,
            serial,
            filling,
        };
        table.entries.insert(hex.to_owned(), entry);
    }

    /// Takes a fill's hold of the entry `hex`, making the entry (empty) if there is none; gives
    /// its serial. None if the object is held whole: there is nothing to fill.
    pub(in crate::server::local_cache) fn extent_hold(
        &mut self,
        hex: &str,
        object: &Object,
    ) -> Option<u64> {
        if self.get(hex).is_some() {
            return None;
        }
        if !self.partials.entries.contains_key(hex) {
            self.extent_found(hex, object.clone(), ExtentMap::default());
        }
        let entry = self.partials.entries.get_mut(hex)?;
        entry.filling += 1;
        Some(entry.serial)
    }

    /// Ends a fill's hold of the entry `hex` of `serial`; says whether the entry is left with
    /// no range and no fill, which is an entry for nothing (the caller removes it).
    pub(in crate::server::local_cache) fn extent_release(
        &mut self,
        hex: &str,
        serial: u64,
    ) -> bool {
        let Some(entry) = self.partials.entries.get_mut(hex) else {
            return false;
        };
        if entry.serial != serial {
            return false;
        }
        entry.filling -= 1;
        entry.filling == 0 && entry.map.is_empty()
    }

    /// Records `[start, end)` of the entry `hex` of `serial` as filled; gives the bytes that
    /// were not, and whether the entry is whole now. None if the entry is gone or is another.
    pub(in crate::server::local_cache) fn extent_add(
        &mut self,
        hex: &str,
        serial: u64,
        (start, end): (u64, u64),
    ) -> Option<(u64, bool)> {
        let used = self.next;
        let entry = self.partials.entries.get_mut(hex)?;
        if entry.serial != serial {
            return None;
        }
        let added = entry.map.insert(start, end);
        let whole = entry.map.is_whole(entry.object.size);
        let bytes = entry.map.bytes();
        let before = std::mem::replace(&mut entry.used, used);
        self.next += 1;
        self.partials.order.remove(&before);
        self.partials.order.insert(used, hex.to_owned());
        self.partials.bytes += added;
        // The pin of an entry that is pinned grows with it, and is counted once.
        self.pins.resize(hex, bytes);
        Some((added, whole))
    }

    /// Counts the entry `hex` as used at `now`, and gives its object, if it is there.
    pub(in crate::server::local_cache) fn extent_used(
        &mut self,
        hex: &str,
        now: u64,
    ) -> Option<Object> {
        let used = self.next;
        let entry = self.partials.entries.get_mut(hex)?;
        let before = std::mem::replace(&mut entry.used, used);
        let object = entry.object.clone();
        self.next += 1;
        self.partials.order.remove(&before);
        self.partials.order.insert(used, hex.to_owned());
        self.touched.insert(hex.to_owned(), now);
        Some(object)
    }

    /// Takes the entry `hex` out, and leaves its pins (they go to the whole object that replaces
    /// it, see `Index::insert_from`).
    pub(in crate::server::local_cache) fn extent_take(&mut self, hex: &str) -> Option<Partial> {
        let entry = self.partials.entries.remove(hex)?;
        self.partials.order.remove(&entry.used);
        self.partials.bytes -= entry.map.bytes();
        self.touched.remove(hex);
        Some(entry)
    }

    /// Takes the entry `hex` out, and its pins: it is gone from the cache.
    pub(in crate::server::local_cache) fn extent_drop(&mut self, hex: &str) -> Option<Partial> {
        let entry = self.extent_take(hex)?;
        if self.get(hex).is_none() {
            self.pins.forget(hex);
        }
        Some(entry)
    }

    /// Takes the entry `hex` out if nothing pins it and no fill writes to it.
    pub(in crate::server::local_cache) fn extent_evict(&mut self, hex: &str) -> Option<Partial> {
        let entry = self.partials.entries.get(hex)?;
        if entry.filling > 0 || self.pins.is_pinned(hex) {
            return None;
        }
        self.extent_drop(hex)
    }

    /// The size of the object `hex` as a pin counts it: the whole object's, or the bytes an
    /// entry by extents has filled.
    pub(in crate::server::local_cache) fn held_size(&self, hex: &str) -> Option<u64> {
        let whole = self.get(hex).map(|entry| entry.size);
        let filled = self.partials.get(hex).map(|entry| entry.map.bytes());
        whole.or(filled.filter(|bytes| *bytes > 0))
    }

    /// Every name the cache holds, whole or by extents, with what it costs the budget and
    /// whether it is by extents, the least recently used first.
    pub(in crate::server::local_cache) fn oldest_all(&self) -> Vec<(String, u64, bool)> {
        let whole = self.order_with_used();
        let table = &self.partials;
        let partial = table.order.iter().map(|(used, hex)| {
            let bytes = table.entries.get(hex).map_or(0, |entry| entry.map.bytes());
            (*used, (hex.clone(), bytes, true))
        });
        let mut all: Vec<(u64, (String, u64, bool))> = whole.chain(partial).collect();
        all.sort_by_key(|(used, _)| *used);
        all.into_iter().map(|(_, victim)| victim).collect()
    }
}
