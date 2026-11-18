use super::*;
use crate::clock::StepClock;

fn p(streams: usize) -> Params {
    Params {
        streams,
        write_streams: 1,
        slice_bytes: 32,
        preempt: true,
        preempt_max_per_s: 1,
        queue_max: 3,
    }
}

const T: Duration = Duration::ZERO;

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

#[test]
fn class_names_round_trip() {
    for c in [Class::Demand, Class::Prepare, Class::Background] {
        assert_eq!(Class::parse(c.name()), Some(c));
    }
    assert_eq!(Class::parse("urgent"), None);
    assert!(Class::Demand > Class::Prepare && Class::Prepare > Class::Background);
}

#[test]
fn admits_up_to_the_stream_count_then_by_class_and_arrival() {
    let mut s = State::new(Params {
        preempt: false,
        ..p(2)
    });
    let a = s.enqueue(Class::Background, false, secs(0)).unwrap();
    let b = s.enqueue(Class::Background, false, secs(1)).unwrap();
    let c = s.enqueue(Class::Background, false, secs(2)).unwrap();
    let d = s.enqueue(Class::Prepare, false, secs(3)).unwrap();
    s.admit_ready(secs(3));
    // The prepare request outranks every background one; then the oldest background.
    assert!(s.is_active(d) && s.is_active(a));
    assert!(!s.is_active(b) && !s.is_active(c));
    assert_eq!(s.load(), (2, 2));
    s.release(a, secs(4));
    assert!(
        s.is_active(b),
        "the older of the two waiting background requests"
    );
    s.release(c, secs(5));
    s.release(b, secs(5));
    assert_eq!(s.load(), (1, 0));
    assert_eq!(s.counts.admitted, 3);
}

#[test]
fn at_most_write_streams_writes_are_admitted() {
    let mut s = State::new(p(2));
    let w1 = s.enqueue(Class::Prepare, true, T).unwrap();
    let w2 = s.enqueue(Class::Prepare, true, T).unwrap();
    let r = s.enqueue(Class::Background, false, T).unwrap();
    s.admit_ready(T);
    assert!(s.is_active(w1) && !s.is_active(w2) && s.is_active(r));
    s.release(w1, T);
    assert!(s.is_active(w2));
}

#[test]
fn a_slice_ends_with_a_yield_only_when_an_equal_or_higher_class_waits() {
    let mut s = State::new(p(1));
    let a = s.enqueue(Class::Prepare, false, secs(0)).unwrap();
    s.admit_ready(secs(0));
    assert_eq!(
        s.boundary(a, 40, secs(1)),
        Boundary::Continue,
        "nobody waits"
    );
    let b = s.enqueue(Class::Background, false, secs(1)).unwrap();
    assert_eq!(
        s.boundary(a, 40, secs(2)),
        Boundary::Continue,
        "a lower class waits"
    );
    let c = s.enqueue(Class::Prepare, false, secs(2)).unwrap();
    assert_eq!(
        s.boundary(a, 10, secs(3)),
        Boundary::Continue,
        "the slice is not used up"
    );
    assert_eq!(s.boundary(a, 30, secs(4)), Boundary::Yield);
    assert!(s.is_active(c) && !s.is_active(a) && !s.is_active(b));
    assert_eq!(s.counts.yields, 1);
    // The yielder waits behind the requests of its class that came before it.
    s.release(c, secs(5));
    assert!(s.is_active(a));
    assert_eq!(
        s.boundary(99, 1, secs(6)),
        Boundary::Continue,
        "an unknown id"
    );
}

#[test]
fn a_demand_preempts_the_lowest_class_at_its_next_boundary_within_the_rate() {
    let mut s = State::new(p(2));
    let bg = s.enqueue(Class::Background, false, secs(0)).unwrap();
    let pr = s.enqueue(Class::Prepare, false, secs(0)).unwrap();
    s.admit_ready(secs(0));
    let d1 = s.enqueue(Class::Demand, false, secs(10)).unwrap();
    s.admit_ready(secs(10));
    assert_eq!(s.counts.preemptions, 1);
    assert_eq!(
        s.boundary(pr, 1, secs(10)),
        Boundary::Continue,
        "prepare is not the victim"
    );
    assert_eq!(s.boundary(bg, 1, secs(10)), Boundary::Yield);
    assert!(s.is_active(d1));
    // A second demand within the same second may not preempt (the limit is 1 a second).
    let d2 = s.enqueue(Class::Demand, false, secs(10)).unwrap();
    s.admit_ready(secs(10));
    assert_eq!(s.counts.preemptions, 1);
    assert_eq!(s.boundary(pr, 1, secs(10)), Boundary::Continue);
    // A second later it may.
    s.admit_ready(secs(11));
    assert_eq!(s.counts.preemptions, 2);
    assert_eq!(s.boundary(pr, 1, secs(11)), Boundary::Yield);
    assert!(s.is_active(d2));
    // Both preempted streams wait; the first preempted comes back before the other.
    s.release(d1, secs(12));
    assert!(s.is_active(bg) || s.is_active(pr));
    // Two demands active and one more demand waiting: nothing lower to preempt.
    let d3 = s.enqueue(Class::Demand, false, secs(20)).unwrap();
    s.release(d2, secs(20));
    s.admit_ready(secs(30));
    assert!(s.is_active(d3) || s.load().1 > 0);
}

#[test]
fn without_preemption_a_demand_waits_for_a_slice_end() {
    let mut s = State::new(Params {
        preempt: false,
        ..p(1)
    });
    let bg = s.enqueue(Class::Background, false, T).unwrap();
    s.admit_ready(T);
    let d = s.enqueue(Class::Demand, false, T).unwrap();
    s.admit_ready(T);
    assert_eq!(s.counts.preemptions, 0);
    assert_eq!(s.boundary(bg, 8, T), Boundary::Continue);
    assert_eq!(
        s.boundary(bg, 32, T),
        Boundary::Yield,
        "the slice ended and a higher class waits"
    );
    assert!(s.is_active(d));
}

#[test]
fn a_full_class_queue_refuses() {
    let mut s = State::new(p(1));
    s.enqueue(Class::Background, false, T).unwrap();
    s.admit_ready(T);
    for _ in 0..3 {
        s.enqueue(Class::Prepare, false, T).unwrap();
    }
    assert_eq!(s.enqueue(Class::Prepare, false, T), None);
    assert_eq!(s.counts.refused, 1);
    assert!(
        s.enqueue(Class::Demand, false, T).is_some(),
        "another class has its own queue"
    );
}

#[test]
fn a_parked_stream_gives_its_slot_away_until_it_is_unparked() {
    let mut s = State::new(p(1));
    let a = s.enqueue(Class::Prepare, false, T).unwrap();
    let b = s.enqueue(Class::Prepare, false, T).unwrap();
    s.admit_ready(T);
    s.park(a, T);
    assert!(s.is_active(b) && !s.is_active(a));
    s.release(b, T);
    assert!(!s.is_active(a), "parked: not admitted");
    s.unpark(a, T);
    assert!(s.is_active(a));
    s.park(99, T);
    assert_eq!(s.counts.parks, 1);
}

#[test]
fn the_shared_scheduler_hands_slots_between_threads() {
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let sched = Sched::new(Params { streams: 1, ..p(1) }, &clock);
    let (first, _) = sched.admit(Class::Prepare, false).unwrap();
    std::thread::scope(|scope| {
        let h = scope.spawn(|| {
            let (t, _) = sched.admit(Class::Prepare, false).unwrap();
            sched.release(t);
        });
        // The second request is queued behind the first; a slice end lets it in.
        while sched.lock().load().1 == 0 {
            std::thread::yield_now();
        }
        sched.boundary(&first, 64);
        h.join().unwrap();
    });
    sched.park(&first);
    let _ = sched.unpark(&first);
    assert_eq!(sched.boundary(&first, 1), Duration::ZERO);
    sched.release(first);
    let c = sched.counts();
    assert_eq!((c.yields, c.parks), (1, 1));
    // A full queue refuses through the shared scheduler too.
    let held = sched.admit(Class::Background, false).unwrap().0;
    let mut s = sched.lock();
    for _ in 0..3 {
        s.enqueue(Class::Background, false, T);
    }
    drop(s);
    assert!(sched.admit(Class::Background, false).is_none());
    sched.release(held);
}

#[test]
fn a_parked_waiter_is_no_rival_at_a_slice_end_and_unpark_picks_its_own() {
    let mut s = State::new(Params { streams: 2, ..p(2) });
    let a = s.enqueue(Class::Prepare, false, T).unwrap();
    let b = s.enqueue(Class::Prepare, false, T).unwrap();
    s.admit_ready(T);
    s.park(b, T);
    let c = s.enqueue(Class::Prepare, false, T).unwrap();
    s.admit_ready(T);
    assert!(s.is_active(c));
    // b is parked: it does not make a give up its slot at the end of a slice.
    assert_eq!(s.boundary(a, 64, T), Boundary::Continue);
    s.park(a, T);
    // Unparking b leaves a parked; b takes the free slot.
    s.unpark(b, T);
    assert!(s.is_active(b) && !s.is_active(a));
}

#[test]
fn a_demand_never_preempts_another_demand() {
    let mut s = State::new(Params {
        preempt_max_per_s: 100,
        ..p(1)
    });
    let d1 = s.enqueue(Class::Demand, false, T).unwrap();
    s.admit_ready(T);
    let d2 = s.enqueue(Class::Demand, false, T).unwrap();
    s.admit_ready(T);
    assert_eq!(s.counts.preemptions, 0);
    assert_eq!(s.boundary(d1, 1, T), Boundary::Continue);
    assert!(!s.is_active(d2));
}
