//! The assertion messages of the marker model's properties, reached by handing the model a
//! state that breaks them.

use tierstore_model::explore::Model;
use tierstore_model::marks::{Act, Body, Cfg, Init, Marks, Op, Resp, State};

/// The version id of the bytes of a stream that lost its creation race.
const CORRUPT: u8 = 7;

fn start(p0: &[Op], p1: &[Op]) -> (Marks, State) {
    let mut cfg = Cfg::new(Init::Empty);
    cfg.scripts[0] = p0.to_vec();
    cfg.scripts[1] = p1.to_vec();
    let m = Marks::new(cfg);
    let s = m.init().remove(0);
    (m, s)
}

fn failure(r: Result<(), String>) -> String {
    match r {
        Err(m) => m,
        Ok(()) => panic!("the state was accepted"),
    }
}

#[test]
fn bytes_a_stream_lost_that_become_the_truth_of_the_key_are_rejected() {
    let (m, mut s) = start(&[Op::Put(Body::Stream)], &[]);
    s.hot = CORRUPT;
    let msg = failure(m.post(&mut s));
    assert_eq!(
        msg,
        "A24 the bytes of a one-shot stream that lost its creation race are the key's truth"
    );
}

#[test]
fn an_acknowledgement_no_order_of_the_operations_explains_is_rejected() {
    let (m, mut s) = start(&[Op::Put(Body::Replay)], &[]);
    let msg = failure(m.ack(&mut s, 0, Resp::Ok));
    assert_eq!(
        msg,
        "A1 operation 0 (Put(Replay)) was acknowledged but no order of the operations lets it take effect"
    );
    let (m, mut s) = start(&[Op::PutCo(Body::Replay)], &[]);
    let msg = failure(m.ack(&mut s, 0, Resp::Ok));
    assert!(msg.starts_with("A5 operation 0 (PutCo(Replay))"), "{msg}");
}

#[test]
fn a_409_when_nothing_changed_the_key_is_rejected() {
    let (m, mut s) = start(&[Op::PutCo(Body::Replay)], &[]);
    s.hot = 2;
    let msg = failure(m.ack(&mut s, 0, Resp::R409));
    assert_eq!(
        msg,
        "A5 operation 0 (PutCo(Replay)) answered 409 but nothing changed the key while it ran (it held 0b100)"
    );
    // A delete in the scripts is a change that may have been in flight.
    let (m, mut s) = start(&[Op::PutCo(Body::Replay)], &[Op::Delete]);
    s.hot = 2;
    assert_eq!(m.ack(&mut s, 0, Resp::R409), Ok(()));
}

#[test]
fn a_412_for_a_put_when_the_key_was_never_present_is_rejected() {
    let (m, mut s) = start(&[Op::Put(Body::Replay)], &[]);
    let msg = failure(m.ack(&mut s, 0, Resp::R412));
    assert_eq!(
        msg,
        "A5 operation 0 (Put(Replay)) answered 412 but the key never held a version that refuses it (it held 0b1)"
    );
}

#[test]
fn a_404_when_the_key_was_never_absent_is_rejected() {
    let (m, mut s) = start(&[Op::Put(Body::Replay)], &[]);
    s.hot = 2;
    s.cl[0].poss = 0;
    let msg = failure(m.ack(&mut s, 0, Resp::R404));
    assert_eq!(
        msg,
        "A5 operation 0 (Put(Replay)) answered 404 but the key was never absent while it ran"
    );
}

#[test]
fn a_503_that_says_nothing_was_done_when_the_bytes_took_effect_is_rejected() {
    let (m, mut s) = start(&[Op::Put(Body::Replay)], &[]);
    s.lin.invoke(0);
    s.hot = m.cfg.value(0);
    s.lin.ack(&m.effs, 0);
    let msg = failure(m.ack(&mut s, 0, Resp::Fail503));
    assert_eq!(
        msg,
        "A24 operation 0 (Put(Replay)) answered a 503 that says it did nothing, but its bytes are the key's truth"
    );
}

#[test]
fn a_lock_caused_503_in_a_run_without_failures_is_reported() {
    let (m, mut s) = start(&[], &[]);
    s.ans[2] = 4;
    assert_eq!(
        m.terminal(&s).as_deref(),
        Some("A6 operation 2 got a 503 caused by a lock where no lock holder died")
    );
    s.ans[2] = 8;
    assert_eq!(m.terminal(&s), None);
}

#[test]
fn a_client_with_work_left_and_no_step_is_reported_stuck() {
    let (m, s) = start(&[Op::Put(Body::Replay)], &[]);
    assert_eq!(
        m.terminal(&s).as_deref(),
        Some("A11 client 0 can never finish: stuck at Idle with the marker None")
    );
}

#[test]
fn actions_are_named_by_the_actor_that_takes_them() {
    let (m, _) = start(&[], &[]);
    for (who, name) in [
        (0, "proxy0"),
        (3, "ghost0"),
        (10, "leader.copier"),
        (11, "leader.evictor"),
        (12, "leader.sweep"),
        (13, "env"),
        (14, "ghost-leader"),
    ] {
        assert_eq!(m.show_act(&Act { who, what: "x" }), format!("{name}: x"));
    }
}
