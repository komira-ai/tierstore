//! The verdict of a scenario against what its claims require, in each of its five outcomes.

use tierstore_model::scenarios::{Expect, Scenario, all, run, verdict};

fn scenario(id: &str) -> Scenario {
    match all().into_iter().find(|s| s.id == id) {
        Some(s) => s,
        None => panic!("no scenario called {id}"),
    }
}

fn judge(sc: &Scenario) -> (bool, String) {
    verdict(sc, &run(sc))
}

#[test]
fn a_scenario_that_holds_where_it_must_is_a_pass() {
    assert_eq!(judge(&scenario("if-match-race")), (true, "PASS".into()));
}

#[test]
fn a_scenario_that_breaks_the_required_rule_is_a_required_failure() {
    let sc = scenario("negative/no-marker-on-put");
    assert_eq!(judge(&sc), (true, "FAIL as required (A7)".into()));
}

#[test]
fn a_scenario_that_holds_where_a_counterexample_was_required_is_rejected() {
    let mut sc = scenario("if-match-race");
    sc.expect = Expect::Fail("A5");
    assert_eq!(
        judge(&sc),
        (false, "PASS where a A5 counterexample was required".into())
    );
}

#[test]
fn a_counterexample_where_the_scenario_must_hold_is_rejected_with_its_message() {
    let mut sc = scenario("negative/no-marker-on-put");
    sc.expect = Expect::Pass;
    let (good, text) = judge(&sc);
    assert!(!good);
    assert!(text.starts_with("FAIL: A7 no marker"), "{text}");
}

#[test]
fn a_counterexample_of_another_rule_is_rejected_naming_both() {
    let mut sc = scenario("negative/no-marker-on-put");
    sc.expect = Expect::Fail("A1");
    let (good, text) = judge(&sc);
    assert!(!good);
    assert!(
        text.starts_with("FAIL with A7 where A1 was required: A7 no marker"),
        "{text}"
    );
}
