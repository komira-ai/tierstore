//! The explorer on small transition systems whose answers are known.

use tierstore_model::explore::{Limits, Model, Succ, explore};

/// A counter that counts up to `top`, may reset to zero, and breaks a rule at `bad`.
struct Counter {
    top: u8,
    bad: Option<u8>,
    stuck_at: Option<u8>,
    reset: bool,
    converge_at: u8,
}

impl Model for Counter {
    type S = u8;
    type A = &'static str;

    fn init(&self) -> Vec<u8> {
        vec![0]
    }

    fn successors(&self, s: &u8, out: &mut Vec<Succ<u8, &'static str>>) {
        if *s < self.top && self.stuck_at != Some(*s) {
            out.push(Succ {
                act: "up",
                to: Ok(*s + 1),
                bg: true,
            });
        }
        if self.reset && *s > 0 {
            out.push(Succ {
                act: "reset",
                to: Ok(0),
                bg: true,
            });
        }
        if *s == 200 {
            out.push(Succ {
                act: "boom",
                to: Err("A9 a step that is itself the violation".into()),
                bg: false,
            });
        }
    }

    fn check(&self, s: &u8) -> Option<String> {
        (self.bad == Some(*s)).then(|| format!("A7 reached {s}"))
    }

    fn terminal(&self, s: &u8) -> Option<String> {
        (*s == self.top && self.bad == Some(255)).then(|| "A11 ended".to_string())
    }

    fn quiescent(&self, _: &u8) -> bool {
        true
    }

    fn converged(&self, s: &u8) -> bool {
        *s == self.converge_at
    }

    fn show_act(&self, a: &&'static str) -> String {
        (*a).to_string()
    }

    fn show_state(&self, s: &u8) -> String {
        format!("n={s}")
    }
}

fn counter() -> Counter {
    Counter {
        top: 5,
        bad: None,
        stuck_at: None,
        reset: false,
        converge_at: 5,
    }
}

fn limits(liveness: bool) -> Limits {
    Limits::new(1000, liveness)
}

#[test]
fn a_system_without_a_violation_is_explored_completely() {
    let o = explore(&counter(), limits(true));
    assert!(o.trace.is_none());
    assert_eq!((o.states, o.transitions, o.truncated), (6, 5, false));
    assert_eq!(o.live_starts, 6);
}

#[test]
fn the_counterexample_is_a_shortest_path_to_the_violation() {
    let m = Counter {
        bad: Some(3),
        reset: true,
        ..counter()
    };
    let t = explore(&m, limits(false)).trace.expect("a violation");
    assert_eq!(t.rule(), "A7");
    let acts: Vec<&str> = t.steps.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(acts, ["up", "up", "up"]);
    assert_eq!(t.steps[2].1, "n=3");
}

#[test]
fn a_step_that_is_itself_a_violation_ends_the_trace_with_that_step() {
    let m = Counter {
        top: 250,
        ..counter()
    };
    let t = explore(&m, limits(false)).trace.expect("a violation");
    assert_eq!(t.rule(), "A9");
    assert_eq!(t.steps.len(), 201);
    assert_eq!(t.steps.last().map(|s| s.0.as_str()), Some("boom"));
}

#[test]
fn a_terminal_state_is_judged() {
    let m = Counter {
        bad: Some(255),
        ..counter()
    };
    let t = explore(&m, limits(false)).trace.expect("a violation");
    assert_eq!(t.rule(), "A11");
}

#[test]
fn an_initial_state_that_breaks_a_rule_is_a_counterexample_of_no_steps() {
    let m = Counter {
        bad: Some(0),
        ..counter()
    };
    let t = explore(&m, limits(false)).trace.expect("a violation");
    assert!(t.steps.is_empty());
}

#[test]
fn the_state_bound_stops_the_search_and_says_so() {
    let m = Counter {
        top: 100,
        ..counter()
    };
    let o = explore(&m, Limits::new(10, true));
    assert!(o.truncated);
    assert!(o.trace.is_none());
    assert_eq!(
        o.live_starts, 0,
        "liveness is not judged on a part of the graph"
    );
}

#[test]
fn liveness_fails_when_the_background_steps_cannot_reach_convergence() {
    let m = Counter {
        stuck_at: Some(3),
        ..counter()
    };
    let t = explore(&m, limits(true)).trace.expect("a violation");
    assert_eq!(t.rule(), "A4");
}

#[test]
fn liveness_holds_when_convergence_stays_reachable_through_a_cycle() {
    let m = Counter {
        reset: true,
        ..counter()
    };
    let o = explore(&m, limits(true));
    assert!(o.trace.is_none(), "{:?}", o.trace);
}

#[test]
fn judging_one_rule_prunes_the_paths_that_break_another() {
    let m = Counter {
        bad: Some(3),
        ..counter()
    };
    let mut only_other = Limits::new(1000, false);
    only_other.only = Some("A9");
    let o = explore(&m, only_other);
    assert!(
        o.trace.is_none(),
        "an A7 state is not judged when only A9 is"
    );
    assert_eq!(o.states, 6, "and the search goes on past it");
    let mut only_this = Limits::new(1000, false);
    only_this.only = Some("A7");
    assert_eq!(
        explore(&m, only_this).trace.map(|t| t.message),
        Some("A7 reached 3".to_string())
    );
    let boom = Counter {
        top: 250,
        ..counter()
    };
    let mut skip = Limits::new(1000, false);
    skip.only = Some("A7");
    assert!(
        explore(&boom, skip).trace.is_none(),
        "a violating step of another rule is pruned"
    );
}
