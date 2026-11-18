//! A signal's window and baseline: the windowed minimum (10 minutes) of window medians, judged
//! against twice `max(baseline, floor)`; and the window's `max(parts, time)` rule.
//!
//! Catches:
//! - a baseline that is not the minimum of the span, or that keeps windows older than it;
//! - a window with no sample judged as evidence;
//! - an even count's median taken as the upper middle (a single outlier would move it);
//! - an idle signal (a baseline near zero) read as a decrease;
//! - a window closed before it has both its parts and its time.

use tierstore_budget::signal::{Series, Verdict, Window, worst};

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
