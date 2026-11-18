//! The engine's own baselines of the signals its timeouts are relative to (4.6): first-byte time
//! (p50 for the hedge trigger, p99 for the pre-first-byte timeout) and network time per MiB (the
//! stall timeout after the first byte), per size class. `Budget` keeps the controller's baselines
//! and does not expose them, so the engine keeps the few it needs: the last [`SAMPLES`] samples
//! per class, and the quantile of those. With no sample yet a timeout is its floor.

use std::collections::VecDeque;

/// Samples kept per signal and size class.
pub const SAMPLES: usize = 128;

/// The size classes of [`crate::plan::size_class`].
const CLASSES: usize = 3;

#[derive(Debug, Default, Clone)]
struct Ring {
    samples: VecDeque<u64>,
}

impl Ring {
    fn push(&mut self, v: u64) {
        if self.samples.len() == SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(v);
    }

    /// The `q`-th per-mille sample (nearest rank), or `None` with no sample.
    fn quantile(&self, per_mille: usize) -> Option<u64> {
        let mut v: Vec<u64> = self.samples.iter().copied().collect();
        v.sort_unstable();
        let i = (v.len() * per_mille).div_ceil(1000).saturating_sub(1);
        v.get(i).copied()
    }
}

/// The baselines of every size class.
#[derive(Debug, Default, Clone)]
pub struct Baselines {
    first_byte: [Ring; CLASSES],
    ms_per_mib: [Ring; CLASSES],
}

fn class(size_class: u8) -> usize {
    usize::from(size_class).min(CLASSES - 1)
}

impl Baselines {
    /// Records the first-byte time of a part of `size_class`.
    pub fn first_byte(&mut self, size_class: u8, ms: u64) {
        self.first_byte[class(size_class)].push(ms);
    }

    /// Records the network time per MiB of a finished part of `size_class`.
    pub fn network(&mut self, size_class: u8, ms_per_mib: u64) {
        self.ms_per_mib[class(size_class)].push(ms_per_mib);
    }

    /// The first-byte p50 of `size_class`.
    #[must_use]
    pub fn first_byte_p50(&self, size_class: u8) -> Option<u64> {
        self.first_byte[class(size_class)].quantile(500)
    }

    /// The first-byte p99 of `size_class`.
    #[must_use]
    pub fn first_byte_p99(&self, size_class: u8) -> Option<u64> {
        self.first_byte[class(size_class)].quantile(990)
    }

    /// The median network time per MiB of `size_class`.
    #[must_use]
    pub fn ms_per_mib(&self, size_class: u8) -> Option<u64> {
        self.ms_per_mib[class(size_class)].quantile(500)
    }
}

/// `max(floor, factor x baseline)`, the shape of every timeout of 4.6.
#[must_use]
pub fn timeout(floor: u64, factor: u64, baseline: Option<u64>) -> u64 {
    floor.max(baseline.unwrap_or(0).saturating_mul(factor))
}
