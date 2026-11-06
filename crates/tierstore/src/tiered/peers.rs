//! The other processes of this proxy id that may be alive, as a process names them in its
//! registration (`peers`).
//!
//! A process that starts under a proxy id replaces the registration, and a process of the same id
//! that is still alive (a hand-off, where the old one drains) writes it back over, each over the
//! other. Neither can tell from the object alone whether the other is dead or merely not the last
//! writer, so each keeps the boots it has reason to think alive and writes them in every body:
//!
//! - *heard*: a body of another boot found where this process's own should have been, after this
//!   process wrote its registration at open. Only a live process could have written it, so the boot
//!   is named from then, and again each time it is heard;
//! - *told*: a boot named by the body found at open, or in the `peers` of a body heard. It may be
//!   dead, so it is named once, and not renewed by anyone's naming it: a boot two processes name
//!   for each other would never go.
//!
//! A boot that has not been heard for `keep` acknowledged beats is dropped. `keep` is the
//! departure time in beats, so a process that beats at all is heard again long before.

use std::collections::BTreeMap;

use crate::registration::Registration;

/// The boots named, each with the acknowledged beats since it was last heard (or told).
#[derive(Clone, Debug)]
pub(super) struct Peers {
    since: BTreeMap<String, u64>,
    keep: u64,
}

impl Peers {
    /// None named, to drop each after `keep` beats (at least one).
    pub(super) fn new(keep: u64) -> Self {
        Self {
            since: BTreeMap::new(),
            keep: keep.max(1),
        }
    }

    /// The boots named, in order.
    pub(super) fn names(&self) -> Vec<String> {
        self.since.keys().cloned().collect()
    }

    /// Takes the registration found at open: its boot and its peers are told, not heard.
    pub(super) fn found_at_open(&mut self, mine: &str, found: &Registration) {
        self.tell(
            mine,
            std::iter::once(&found.boot).chain(found.peers.iter().flatten()),
        );
    }

    /// Takes a registration found in place of this process's own: its boot is heard, its peers told.
    pub(super) fn heard(&mut self, mine: &str, found: &Registration) {
        if found.boot != mine {
            self.since.insert(found.boot.clone(), 0);
        }
        self.tell(mine, found.peers.iter().flatten());
    }

    fn tell<'a>(&mut self, mine: &str, boots: impl Iterator<Item = &'a String>) {
        for boot in boots.filter(|boot| boot.as_str() != mine) {
            self.since.entry(boot.clone()).or_insert(0);
        }
    }

    /// One acknowledged beat has gone: every boot gets older, and one not heard for `keep` beats
    /// is dropped.
    pub(super) fn beat(&mut self) {
        let keep = self.keep;
        self.since.retain(|_, beats| {
            *beats += 1;
            *beats < keep
        });
    }
}
