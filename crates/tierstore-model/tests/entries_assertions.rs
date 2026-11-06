//! The assertion messages of the baseline's properties, reached by handing the model a state
//! that breaks them.

use tierstore_model::entries::{BCfg, BOp, BState, Entries, R};
use tierstore_model::explore::Model;
use tierstore_model::lin::Lin;

fn start(p0: &[BOp], p1: &[BOp]) -> (Entries, BState) {
    let m = Entries::new(BCfg::new(p0, p1));
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
fn an_acknowledged_put_that_no_order_explains_is_rejected_as_a_lost_operation() {
    let (m, mut s) = start(&[BOp::Put], &[]);
    let msg = failure(m.ack(&mut s, 0, R::Ok));
    assert_eq!(
        msg,
        "A1 operation 0 was acknowledged but no order lets it take effect"
    );
}

#[test]
fn a_put_acknowledged_over_a_key_that_held_another_version_is_rejected_as_a_broken_condition() {
    let (m, mut s) = start(&[BOp::Put], &[]);
    s.lin.invoke(0);
    s.hot = 5;
    let msg = failure(m.ack(&mut s, 0, R::Ok));
    assert_eq!(
        msg,
        "A1 operation 0 was acknowledged 200 although the key held another version when it took effect"
    );
    // Truth is the put's own version but the history says the key was already there.
    let (m, mut other) = start(&[BOp::Put], &[]);
    other.hot = m.cfg.value(0);
    other.lin = Lin::new(9);
    other.lin.invoke(0);
    let msg = failure(m.ack(&mut other, 0, R::Ok));
    assert!(
        msg.starts_with("A5 operation 0 was acknowledged 200"),
        "{msg}"
    );
}

#[test]
fn a_412_for_a_key_that_never_existed_is_rejected() {
    let (m, mut s) = start(&[BOp::Put], &[]);
    let msg = failure(m.ack(&mut s, 0, R::R412));
    assert_eq!(
        msg,
        "A5 operation 0 answered 412 but the key never existed while it ran"
    );
    let (m, mut s) = start(&[BOp::Put], &[]);
    s.hot = 3;
    assert_eq!(m.ack(&mut s, 0, R::R412), Ok(()));
}

#[test]
fn a_failure_answer_for_a_put_that_took_effect_is_rejected() {
    let (m, mut s) = start(&[BOp::Put], &[]);
    s.lin.invoke(0);
    s.hot = m.cfg.value(0);
    s.lin.ack(&m.effs, 0);
    let msg = failure(m.ack(&mut s, 0, R::Fail503));
    assert_eq!(
        msg,
        "A24 operation 0 answered a failure that says it did nothing, but it took effect"
    );
    assert_eq!(m.ack(&mut start(&[BOp::Put], &[]).1, 0, R::Unk503), Ok(()));
}

#[test]
fn a_read_of_a_version_the_key_never_held_is_rejected() {
    let (m, mut s) = start(&[BOp::Get], &[]);
    let msg = failure(m.ack(&mut s, 0, R::Read(4)));
    assert_eq!(
        msg,
        "A2 a read returned version 4 but the key only held 0b1 while it ran"
    );
}

#[test]
fn hot_and_cold_that_differ_with_no_entry_and_nothing_in_flight_are_rejected() {
    let (m, mut s) = start(&[], &[]);
    assert_eq!(m.check(&s), None);
    s.hot = 3;
    s.cold = 4;
    assert_eq!(
        m.check(&s).as_deref(),
        Some("A7 no entry and nothing in flight, but hot holds 3 and cold 4")
    );
}

#[test]
fn a_client_with_work_left_and_no_step_is_reported_stuck() {
    let (m, s) = start(&[BOp::Put], &[]);
    assert_eq!(
        m.terminal(&s).as_deref(),
        Some("A11 client 0 can never finish: stuck at Idle with the entry None")
    );
    let (m, s) = start(&[], &[]);
    assert_eq!(m.terminal(&s), None);
}

#[test]
fn actions_are_named_by_the_actor_that_takes_them() {
    let (m, _) = start(&[], &[]);
    assert_eq!(m.show_act(&(1, "x")), "proxy1: x");
    assert_eq!(m.show_act(&(10, "x")), "leader: x");
    assert_eq!(m.show_act(&(13, "x")), "env: x");
}
