//! A22: the old release beside the markers.

use std::collections::HashSet;
use tierstore_model::explore::{Model, Succ};
use tierstore_model::marks::{Cfg, Init, Marks, Op, Phase};

/// Every state reachable from the initial one, with no property judged.
fn reachable(cfg: Cfg) -> Vec<<Marks as Model>::S> {
    let m = Marks::new(cfg);
    let mut seen = HashSet::new();
    let mut todo = m.init();
    let mut all = Vec::new();
    while let Some(s) = todo.pop() {
        if !seen.insert(s.clone()) {
            continue;
        }
        let mut out: Vec<Succ<_, _>> = Vec::new();
        m.successors(&s, &mut out);
        for x in out {
            todo.extend(x.to.ok());
        }
        all.push(s);
    }
    all
}

#[test]
fn an_old_release_proxy_under_active_marks_answers_503_and_writes_nothing() {
    let mut cfg = Cfg::new(Init::Both);
    cfg.scripts[2] = vec![Op::OldPut, Op::OldDelete];
    cfg.phase = Phase::ActiveMarks;
    cfg.copier = false;
    cfg.sweep = false;
    let states = reachable(cfg);
    assert_eq!(states.len(), 3, "the old proxy ran both operations");
    for s in &states {
        assert_eq!((s.hot, s.cold, s.mk), (2, 2, None), "{s:?}");
    }
    let last = states
        .iter()
        .find(|s| s.cl[2].next == 2)
        .expect("both operations ended");
    assert_eq!(
        last.ans[0], 8,
        "the put was a 503 that says nothing was done"
    );
    assert_eq!(last.ans[1], 8, "the delete likewise");
}

#[test]
fn an_old_release_proxy_beside_markers_writes_hot_and_cold_with_no_marker() {
    let mut cfg = Cfg::new(Init::Both);
    cfg.scripts[2] = vec![Op::OldDelete];
    cfg.phase = Phase::ActiveEpoch2;
    cfg.copier = false;
    cfg.sweep = false;
    let states = reachable(cfg);
    assert!(
        states
            .iter()
            .any(|s| s.hot == 0 && s.cold == 0 && s.mk.is_none())
    );
}
