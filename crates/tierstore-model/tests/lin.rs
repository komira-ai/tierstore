//! The linearizability tracker: which histories of one key it accepts and which it rejects.

use tierstore_model::lin::{Eff, Effs, Lin};

fn effs(list: &[(usize, Eff)]) -> Effs {
    let mut e: Effs = [None; 8];
    for (i, f) in list {
        e[*i] = Some(*f);
    }
    e
}

#[test]
fn an_acknowledged_delete_cannot_be_followed_by_the_value_it_deleted() {
    let e = effs(&[(0, Eff::Del)]);
    let mut l = Lin::new(2);
    l.invoke(0);
    assert!(l.ack(&e, 0));
    assert!(l.observe(&e, 0));
    assert!(!l.observe(&e, 2), "the deleted version came back");
}

#[test]
fn an_operation_in_flight_may_take_effect_at_any_point() {
    let e = effs(&[(0, Eff::Put(3))]);
    let mut l = Lin::new(0);
    l.invoke(0);
    assert!(l.observe(&e, 0), "not yet");
    assert!(l.observe(&e, 3), "now");
    assert!(!l.observe(&e, 0), "a put is not undone by nothing");
}

#[test]
fn an_operation_not_yet_invoked_cannot_have_taken_effect() {
    let e = effs(&[(0, Eff::Put(3))]);
    let mut l = Lin::new(0);
    assert!(!l.observe(&e, 3));
}

#[test]
fn a_create_only_put_takes_effect_only_over_an_absent_key() {
    let e = effs(&[(0, Eff::Put(3)), (1, Eff::PutCo(4))]);
    let mut l = Lin::new(0);
    l.invoke(0);
    l.invoke(1);
    assert!(l.observe(&e, 3));
    assert!(
        !l.observe(&e, 4),
        "the create-only put landed over version 3"
    );
}

#[test]
fn an_if_match_put_takes_effect_only_over_its_tag() {
    let e = effs(&[(0, Eff::PutIm(4, 2))]);
    let mut l = Lin::new(3);
    l.invoke(0);
    assert!(!l.ack(&e, 0), "the key never held the tag");
    let mut l = Lin::new(2);
    l.invoke(0);
    assert!(l.ack(&e, 0));
    assert!(l.observe(&e, 4));
}

#[test]
fn two_if_match_puts_with_one_tag_cannot_both_be_acknowledged() {
    let e = effs(&[(0, Eff::PutIm(3, 2)), (1, Eff::PutIm(4, 2))]);
    let mut l = Lin::new(2);
    l.invoke(0);
    l.invoke(1);
    assert!(l.ack(&e, 0));
    assert!(!l.ack(&e, 1));
}

#[test]
fn a_failure_that_says_nothing_happened_rules_out_the_histories_that_applied_it() {
    let e = effs(&[(0, Eff::Put(3)), (1, Eff::Put(4))]);
    let mut l = Lin::new(0);
    l.invoke(0);
    l.invoke(1);
    assert!(l.observe(&e, 4));
    assert!(
        !l.applied_everywhere(0),
        "a history where op 0 never ran remains"
    );
    assert!(l.kill(0), "op 1 alone explains version 4");
    assert!(l.applied_everywhere(1));
    assert!(!l.kill(1), "no history is left without op 1");
}

#[test]
fn the_values_a_key_may_hold_are_those_reachable_by_the_pending_operations() {
    let e = effs(&[(0, Eff::Put(3)), (1, Eff::Del)]);
    let mut l = Lin::new(2);
    assert_eq!(l.may_hold(&e), 1 << 2);
    l.invoke(0);
    l.invoke(1);
    assert_eq!(l.may_hold(&e), (1 << 0) | (1 << 2) | (1 << 3));
}
