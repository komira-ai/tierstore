//! The health table alone, at instants the tests choose: which endpoint each pick gives, how long
//! an endpoint is set aside, when a probe is due, and what an answer clears.

use super::*;

/// How long a probe holds its endpoint aside in these tests.
const HOLD: Duration = Duration::from_secs(62);

const NS: Duration = Duration::from_nanos(1);

fn picks(health: &Health, now: Instant, count: usize) -> Vec<usize> {
    (0..count).map(|_| health.pick(now).index).collect()
}

/// Asserts that the picks go round the endpoints in turn, and, once one is set aside, round the
/// others alone, each as often, with no probe before its time is up.
#[test]
fn picks_go_round_the_endpoints_in_turn_and_skip_one_set_aside() {
    let health = Health::new(3, 7, HOLD);
    let t0 = Instant::now();
    assert_eq!(picks(&health, t0, 6), [0, 1, 2, 0, 1, 2]);
    assert!(matches!(health.failed(1, t0), Verdict::SetAside(_)));
    let after = picks(&health, t0 + NS, 6);
    assert!(!after.contains(&1), "{after:?}");
    assert_eq!(after.iter().filter(|&&i| i == 0).count(), 3, "{after:?}");
    assert_eq!(health.pick(t0 + NS).probes, Vec::<usize>::new());
    assert_eq!(health.aside(), [false, true, false]);
}

/// Asserts that the time aside is 2 s after the first failure, doubles with each failure after
/// it (a failed probe counts as one), and stops at 30 s; that each time is shortened by at most a
/// quarter of it; and that the cut differs between stores, so their clients do not all come back
/// at once.
#[test]
fn the_time_aside_doubles_from_two_seconds_to_thirty_shortened_by_at_most_a_quarter() {
    let full = [2, 4, 8, 16, 30, 30, 30, 30].map(Duration::from_secs);
    let mut firsts = std::collections::BTreeSet::new();
    for seed in 0..64 {
        let health = Health::new(2, seed, HOLD);
        let t0 = Instant::now();
        let Verdict::SetAside(first) = health.failed(0, t0) else {
            panic!("seed {seed}: the first failure did not set the endpoint aside");
        };
        firsts.insert(first);
        let mut times = vec![first];
        times.extend((1..full.len()).map(|_| health.probe_failed(0, t0).unwrap()));
        for (n, (time, full)) in times.iter().zip(full).enumerate() {
            assert!(
                *time <= full && *time >= full - full / 4,
                "seed {seed}, failure {}: {time:?} is not within a quarter below {full:?}",
                n + 1
            );
        }
    }
    assert!(
        firsts.len() > 32,
        "only {} distinct first times",
        firsts.len()
    );
}

/// Asserts that the last endpoint in turn is never set aside, with two endpoints and with one,
/// however often it fails, and that the picks then all go to it.
#[test]
fn the_last_endpoint_in_turn_is_never_set_aside() {
    let t0 = Instant::now();
    let two = Health::new(2, 1, HOLD);
    assert!(matches!(two.failed(0, t0), Verdict::SetAside(_)));
    for _ in 0..3 {
        assert_eq!(two.failed(1, t0), Verdict::LastInTurn);
    }
    assert_eq!(two.aside(), [true, false]);
    assert_eq!(picks(&two, t0, 4), [1, 1, 1, 1]);

    let one = Health::new(1, 1, HOLD);
    for _ in 0..3 {
        assert_eq!(one.failed(0, t0), Verdict::LastInTurn);
    }
    assert_eq!(one.aside(), [false]);
    assert_eq!(picks(&one, t0, 2), [0, 0]);
    assert!(!Verdict::LastInTurn.aside());
    assert!(Verdict::AlreadyAside.aside());
}

/// Asserts that a failure of an endpoint already set aside (a request that was on its way to it)
/// changes nothing: its probe is due when the first failure said, to the nanosecond, not later.
#[test]
fn a_failure_of_an_endpoint_already_aside_changes_nothing() {
    let health = Health::new(2, 3, HOLD);
    let t0 = Instant::now();
    let Verdict::SetAside(aside) = health.failed(0, t0) else {
        panic!("not set aside");
    };
    assert_eq!(health.failed(0, t0 + aside / 2), Verdict::AlreadyAside);
    assert_eq!(health.pick(t0 + aside - NS).probes, Vec::<usize>::new());
    assert_eq!(health.pick(t0 + aside).probes, [0]);
}

/// Asserts that a probe is started once when the time aside is up, that the endpoint stays aside
/// while it is out, and that another is started when the probe's hold is up with no word of it
/// (a probe whose task ended unheard of does not keep the endpoint aside for good).
#[test]
fn a_probe_is_started_once_when_the_time_is_up_and_again_after_its_hold() {
    let health = Health::new(3, 5, HOLD);
    let t0 = Instant::now();
    let Verdict::SetAside(aside) = health.failed(2, t0) else {
        panic!("not set aside");
    };
    let due = t0 + aside;
    let first = health.pick(due);
    assert_eq!(first.probes, [2]);
    assert_ne!(first.index, 2);
    assert_eq!(health.pick(due).probes, Vec::<usize>::new());
    assert!(!picks(&health, due + HOLD - NS, 4).contains(&2));
    assert_eq!(health.aside(), [false, false, true]);
    assert_eq!(health.pick(due + HOLD).probes, [2]);
}

/// Asserts that an answer puts an endpoint back in turn and clears its failures, so its next
/// failure sets it aside for the first time again (at most 2 s), and that an answer of an
/// endpoint in turn says it was not aside.
#[test]
fn an_answer_puts_the_endpoint_back_and_clears_its_failures() {
    let health = Health::new(2, 9, HOLD);
    let t0 = Instant::now();
    assert!(matches!(health.failed(0, t0), Verdict::SetAside(_)));
    for _ in 0..3 {
        assert!(health.probe_failed(0, t0).is_some());
    }
    assert!(health.answered(0));
    assert_eq!(health.aside(), [false, false]);
    assert!(!health.answered(0));
    let Verdict::SetAside(again) = health.failed(0, t0) else {
        panic!("not set aside");
    };
    assert!(again <= FIRST_ASIDE, "{again:?}");
    assert_eq!(picks(&health, t0, 3), [1, 1, 1]);
}

/// Asserts that a probe with no answer changes nothing for an endpoint that another request's
/// answer put back in turn while the probe was out, even when that endpoint is by then the last
/// one in turn: it is not set aside, so one endpoint stays in turn.
#[test]
fn a_failed_probe_of_an_endpoint_back_in_turn_changes_nothing() {
    let health = Health::new(2, 11, HOLD);
    let t0 = Instant::now();
    let Verdict::SetAside(aside) = health.failed(0, t0) else {
        panic!("not set aside");
    };
    assert_eq!(health.pick(t0 + aside).probes, [0]);
    assert!(health.answered(0));
    assert!(matches!(health.failed(1, t0 + aside), Verdict::SetAside(_)));
    assert_eq!(health.probe_failed(0, t0 + aside), None);
    assert_eq!(health.aside(), [false, true]);
    assert_eq!(picks(&health, t0 + aside, 2), [0, 0]);
}
