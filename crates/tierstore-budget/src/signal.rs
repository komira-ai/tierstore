//! A signal's window and baseline (4.3): the samples of the current window, and the baseline,
//! the windowed minimum (over `baseline_ms`) of the median of each closed window. Pure: the time
//! is passed in.

use std::collections::VecDeque;

/// The samples a window keeps: past this, the oldest are dropped, so a window that does not
/// close (a device whose only news is `LOCATE` answers) holds bounded memory.
pub const MAX_SAMPLES: usize = 1024;

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
    samples: VecDeque<u32>,
    /// The median of each closed window with a sample, with the time it closed.
    history: VecDeque<(u64, u32)>,
}

impl Series {
    /// Adds a sample to the current window.
    pub fn push(&mut self, value: u32) {
        if self.samples.len() == MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
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
        let Some(median) = median(self.samples.make_contiguous()) else {
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

#[cfg(test)]
mod tests {
    // A signal's window and baseline: the windowed minimum (10 minutes) of window medians, judged
    // against twice `max(baseline, floor)`; and the window's `max(parts, time)` rule.
    //
    // Catches:
    // - a baseline that is not the minimum of the span, or that keeps windows older than it;
    // - a window with no sample judged as evidence;
    // - an even count's median taken as the upper middle (a single outlier would move it);
    // - an idle signal (a baseline near zero) read as a decrease;
    // - a window closed before it has both its parts and its time.

    use super::{MAX_SAMPLES, Series, Verdict, Window, worst};

    const SPAN: u64 = 600_000;

    #[test]
    fn an_empty_window_is_silent_and_leaves_no_history() {
        let mut s = Series::default();
        assert_eq!(s.close(0, SPAN, 1), Verdict::Silent);
        assert_eq!(s.baseline(0, SPAN), None);
    }

    #[test]
    fn the_baseline_is_the_minimum_window_median_of_the_span() {
        let mut s = Series::default();
        for v in [10, 12, 8] {
            s.push(v);
        }
        assert_eq!(s.close(0, SPAN, 1), Verdict::Clean, "no baseline yet");
        assert_eq!(s.baseline(0, SPAN), Some(10));
        for v in [40, 1, 41] {
            s.push(v);
        }
        assert_eq!(
            s.close(1_000, SPAN, 1),
            Verdict::Above,
            "median 40 > 2 x 10"
        );
        assert_eq!(
            s.baseline(1_000, SPAN),
            Some(10),
            "the minimum, not the latest"
        );
        // An even count takes the lower middle.
        for v in [1, 2, 3, 400] {
            s.push(v);
        }
        assert_eq!(s.close(2_000, SPAN, 1), Verdict::Clean);
        assert_eq!(s.baseline(2_000, SPAN), Some(2));
    }

    #[test]
    fn windows_older_than_the_span_leave_the_baseline() {
        let mut s = Series::default();
        s.push(10);
        s.close(0, SPAN, 1);
        s.push(40);
        s.close(1_000, SPAN, 1);
        assert_eq!(
            s.baseline(SPAN, SPAN),
            Some(10),
            "exactly the span old is kept"
        );
        assert_eq!(s.baseline(SPAN + 1, SPAN), Some(40));
        assert_eq!(s.baseline(SPAN + 1_001, SPAN), None);
        // Closing a window long after: no baseline to judge by, and the old medians are dropped.
        s.push(100);
        assert_eq!(s.close(SPAN + 1_001, SPAN, 1), Verdict::Clean);
        assert_eq!(s.baseline(SPAN + 1_001, SPAN), Some(100));
    }

    #[test]
    fn a_floor_keeps_an_idle_signal_from_reading_as_load() {
        let mut s = Series::default();
        s.push(0);
        s.close(0, SPAN, 10);
        s.push(20);
        assert_eq!(
            s.close(1, SPAN, 10),
            Verdict::Clean,
            "20 is not above 2 x max(0, 10)"
        );
        s.push(21);
        assert_eq!(s.close(2, SPAN, 10), Verdict::Above);
    }

    #[test]
    fn the_worst_verdict_wins() {
        use Verdict::{Above, Clean, Silent};
        for (a, b, w) in [
            (Silent, Silent, Silent),
            (Silent, Clean, Clean),
            (Clean, Silent, Clean),
            (Clean, Clean, Clean),
            (Above, Silent, Above),
            (Silent, Above, Above),
            (Clean, Above, Above),
            (Above, Clean, Above),
            (Above, Above, Above),
        ] {
            assert_eq!(worst(a, b), w, "{a:?} {b:?}");
        }
    }

    #[test]
    fn a_window_needs_both_its_parts_and_its_time() {
        let mut w = Window::at(1_000);
        assert!(!w.backlog);
        w.parts = 15;
        assert!(!w.due(3_000, 16, 2_000));
        w.parts = 16;
        assert!(!w.due(2_999, 16, 2_000));
        assert!(w.due(3_000, 16, 2_000));
        // A clock read before the window opened is not a negative age.
        assert!(!w.due(0, 16, 2_000));
    }

    #[test]
    fn a_window_keeps_its_newest_samples_only() {
        let mut s = Series::default();
        for _ in 0..MAX_SAMPLES {
            s.push(1_000);
        }
        for _ in 0..MAX_SAMPLES {
            s.push(5);
        }
        assert_eq!(s.close(0, SPAN, 1), Verdict::Clean);
        assert_eq!(
            s.baseline(0, SPAN),
            Some(5),
            "the older samples were dropped"
        );
    }
}
