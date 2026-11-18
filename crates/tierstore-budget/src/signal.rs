//! A signal's window and baseline (4.3): the samples of the current window, and the baseline,
//! the windowed minimum (over `baseline_ms`) of the median of each closed window. Pure: the time
//! is passed in.

use std::collections::VecDeque;

/// What one closed window says about a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No sample in the window: no evidence either way.
    Silent,
    /// The window's median is at most twice the baseline, or there is no baseline yet.
    Clean,
    /// The window's median is above twice the baseline.
    Above,
}

/// One signal of one scope (a drive's first byte of one size class, a drive's queue wait, a
/// node's or the client's network time per MiB).
#[derive(Debug, Clone, Default)]
pub struct Series {
    samples: Vec<u32>,
    /// The median of each closed window with a sample, with the time it closed.
    history: VecDeque<(u64, u32)>,
}

impl Series {
    /// Adds a sample to the current window.
    pub fn push(&mut self, value: u32) {
        self.samples.push(value);
    }

    /// The baseline at `now`: the minimum of the window medians of the last `span_ms`.
    #[must_use]
    pub fn baseline(&self, now: u64, span_ms: u64) -> Option<u32> {
        self.history
            .iter()
            .filter(|(at, _)| at.saturating_add(span_ms) >= now)
            .map(|(_, m)| *m)
            .min()
    }

    /// Closes the window at `now`: judges its median against twice `max(baseline, floor)`, then
    /// adds the median to the history and drops medians older than `span_ms`.
    pub fn close(&mut self, now: u64, span_ms: u64, floor: u32) -> Verdict {
        let Some(median) = median(&mut self.samples) else {
            return Verdict::Silent;
        };
        self.samples.clear();
        let verdict = match self.baseline(now, span_ms) {
            Some(b) if median > b.max(floor).saturating_mul(2) => Verdict::Above,
            _ => Verdict::Clean,
        };
        self.history.push_back((now, median));
        while self
            .history
            .front()
            .is_some_and(|(at, _)| at.saturating_add(span_ms) < now)
        {
            self.history.pop_front();
        }
        verdict
    }
}

/// The median (the lower middle of an even count), or `None` with no sample.
fn median(samples: &mut [u32]) -> Option<u32> {
    samples.sort_unstable();
    samples
        .len()
        .checked_sub(1)
        .and_then(|last| samples.get(last / 2))
        .copied()
}

/// The window a scope's controller decides on: `max(window_parts parts, window_ms)`, and
/// whether a charge waited on this scope's permits during it (the backlog an increase needs).
#[derive(Debug, Clone, Copy, Default)]
pub struct Window {
    /// When it opened.
    pub start: u64,
    /// Parts (or samples) seen in it.
    pub parts: u32,
    /// A charge found this scope's permits fully used and waited.
    pub backlog: bool,
}

impl Window {
    /// A window opening at `now`.
    #[must_use]
    pub fn at(now: u64) -> Self {
        Self {
            start: now,
            ..Self::default()
        }
    }

    /// Whether the window has seen `parts` parts and lasted `ms`.
    #[must_use]
    pub fn due(&self, now: u64, parts: u32, ms: u64) -> bool {
        self.parts >= parts && now.saturating_sub(self.start) >= ms
    }
}

/// The worst of several verdicts: `Above` over `Clean` over `Silent`.
#[must_use]
pub fn worst(a: Verdict, b: Verdict) -> Verdict {
    match (a, b) {
        (Verdict::Above, _) | (_, Verdict::Above) => Verdict::Above,
        (Verdict::Clean, _) | (_, Verdict::Clean) => Verdict::Clean,
        _ => Verdict::Silent,
    }
}
