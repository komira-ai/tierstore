//! The named runs: each is one bounded configuration of the marker model with the verdict the
//! document's claims require of it.

use crate::entries::{BCfg, Entries};
use crate::explore::{Limits, Outcome, explore};
use crate::firststart::{FirstStart, FsCfg};
use crate::marks::{Cfg, GhostKind, Init, Marks, Op};

/// What a run must show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expect {
    /// Every property holds in every reachable state.
    Pass,
    /// The checker must stop with a counterexample whose rule starts with this id.
    Fail(&'static str),
}

/// The protocol a scenario runs.
#[derive(Clone, Debug)]
pub enum Sys {
    /// The marker protocol of the design document.
    Marks(Cfg),
    /// Today's entry protocol (baseline).
    Entries(BCfg),
    /// The first start of a hot tier with no format object (`docs/design/tierstore-design.md`,
    /// 13.1).
    FirstStart(FsCfg),
}

pub struct Scenario {
    pub id: &'static str,
    /// The claims of Appendix A this run is evidence for.
    pub claims: &'static str,
    pub desc: &'static str,
    pub sys: Sys,
    pub expect: Expect,
    /// Larger than the PR test set (run by `cargo run -p tierstore-model -- heavy`).
    pub heavy: bool,
    pub liveness: bool,
}

pub fn run(sc: &Scenario) -> Outcome {
    match &sc.sys {
        Sys::Marks(c) => explore(
            &Marks::new(c.clone()),
            Limits::new(c.max_states, sc.liveness),
        ),
        Sys::Entries(c) => explore(
            &Entries::new(c.clone()),
            Limits::new(c.max_states, sc.liveness),
        ),
        Sys::FirstStart(c) => explore(
            &FirstStart::new(c.clone()),
            Limits::new(c.max_states, sc.liveness),
        ),
    }
}

/// Whether the outcome of `sc` is the one its claims require, and a line saying so.
pub fn verdict(sc: &Scenario, o: &Outcome) -> (bool, String) {
    match (&o.trace, sc.expect) {
        (None, Expect::Pass) if !o.truncated => (true, "PASS".into()),
        (None, Expect::Pass) => (false, "TRUNCATED (state bound reached)".into()),
        (Some(t), Expect::Fail(rule)) if t.rule() == rule => {
            (true, format!("FAIL as required ({rule})"))
        }
        (Some(t), Expect::Pass) => (false, format!("FAIL: {}", t.message)),
        (Some(t), Expect::Fail(rule)) => (
            false,
            format!(
                "FAIL with {} where {rule} was required: {}",
                t.rule(),
                t.message
            ),
        ),
        (None, Expect::Fail(rule)) => (
            false,
            format!("PASS where a {rule} counterexample was required"),
        ),
    }
}

/// Run the scenario called `id` and return its verdict line, or the reason it is wrong.
pub fn verify(id: &str) -> Result<String, String> {
    let sc = all()
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("no scenario called {id}"))?;
    let o = run(&sc);
    let (good, text) = verdict(&sc, &o);
    let line = format!("{id}: {} states, {text}", o.states);
    good.then(|| line.clone()).ok_or(line)
}

fn base(init: Init, p0: &[Op], p1: &[Op]) -> Cfg {
    let mut c = Cfg::new(init);
    c.scripts[0] = p0.to_vec();
    c.scripts[1] = p1.to_vec();
    c
}

/// The corrections to the document's protocol that the runs of this check called for, by name.
/// Each one is needed: [`ablation`] removes them one at a time and finds a scenario that fails.
/// A named correction: it sets one switch of the configuration.
pub type Fix = (&'static str, fn(&mut Cfg));

pub const FIXES: [Fix; 11] = [
    ("bump_keeps_gone (5.1 row 2, 5.3 release)", |c| {
        c.bump_keeps_gone = true
    }),
    ("delete_decides_gone (5.6)", |c| {
        c.delete_decides_gone = true
    }),
    ("reap_keeps_gone (5.13)", |c| c.reap_keeps_gone = true),
    ("hold_rechecks_hot (5.9)", |c| c.hold_rechecks_hot = true),
    ("recreate_hold (5.9)", |c| c.recreate_hold = true),
    ("gone_rechecks_hot (5.7, 5.8)", |c| {
        c.gone_rechecks_hot = true
    }),
    ("evict_age_from_copy (5.10)", |c| {
        c.evict_age_from_copy = true
    }),
    ("copier_rechecks_marker (5.9)", |c| {
        c.copier_rechecks_marker = true
    }),
    ("coldland_503_on_gone (5.5)", |c| {
        c.coldland_503_on_gone = true
    }),
    ("put_reasserts_marker (5.1 step 5)", |c| {
        c.put_reasserts_marker = true
    }),
    ("timeout_keeps_lock (5.2, 5.6)", |c| {
        c.timeout_keeps_lock = true
    }),
];

/// The document's protocol with every correction applied.
pub fn fixed(mut c: Cfg) -> Cfg {
    for (_, f) in FIXES {
        f(&mut c);
    }
    c
}

/// The document's protocol with every correction but the `skip`th.
pub fn fixed_without(mut c: Cfg, skip: usize) -> Cfg {
    for (i, (_, f)) in FIXES.iter().enumerate() {
        if i != skip {
            f(&mut c);
        }
    }
    c
}

fn with(mut c: Cfg, f: impl FnOnce(&mut Cfg)) -> Cfg {
    f(&mut c);
    c
}

fn crashy(mut c: Cfg, ghost: Option<GhostKind>) -> Cfg {
    c.crash_clients = true;
    c.crash_leader = true;
    c.max_crashes = 1;
    c.ghost = ghost;
    c
}

mod baseline;
mod first_start;
mod heavy;
mod marks_core;
mod negatives;

/// Every run of the check.
pub fn all() -> Vec<Scenario> {
    let mut v: Vec<Scenario> = Vec::new();
    marks_core::core(&mut v);
    negatives::negatives(&mut v);
    heavy::heavy(&mut v);
    baseline::baseline(&mut v);
    first_start::first_start(&mut v);
    v
}

/// For each correction, the first scenario of the PR set that fails when only that correction is
/// left out: `None` means no scenario needs it.
pub fn ablation() -> Vec<(&'static str, Option<(&'static str, String)>)> {
    let sets: Vec<(&'static str, Cfg)> = all()
        .into_iter()
        .filter(|s| s.expect == Expect::Pass && !s.heavy && !s.id.starts_with("ghost/"))
        .filter_map(|s| match s.sys {
            Sys::Marks(c) => Some((s.id, c)),
            Sys::Entries(_) | Sys::FirstStart(_) => None,
        })
        .collect();
    FIXES
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            let found = sets.iter().find_map(|(id, cfg)| {
                let c = fixed_without(unfix(cfg.clone()), i);
                let m = Marks::new(c.clone());
                let o = explore(&m, Limits::new(c.max_states, true));
                o.trace.map(|t| (*id, t.message))
            });
            (*name, found)
        })
        .collect()
}

/// A configuration with every correction off.
fn unfix(mut c: Cfg) -> Cfg {
    let off = Cfg::new(c.init);
    c.bump_keeps_gone = off.bump_keeps_gone;
    c.delete_decides_gone = off.delete_decides_gone;
    c.reap_keeps_gone = off.reap_keeps_gone;
    c.hold_rechecks_hot = off.hold_rechecks_hot;
    c.recreate_hold = off.recreate_hold;
    c.gone_rechecks_hot = off.gone_rechecks_hot;
    c.evict_age_from_copy = off.evict_age_from_copy;
    c.copier_rechecks_marker = off.copier_rechecks_marker;
    c.coldland_503_on_gone = off.coldland_503_on_gone;
    c.put_reasserts_marker = off.put_reasserts_marker;
    c.timeout_keeps_lock = off.timeout_keeps_lock;
    c
}

/// Rules the checks of this crate report, in the order of the claims they serve.
pub const RULES: [(&str, &str); 12] = [
    ("A1", "no acknowledged operation is lost or undone (I1)"),
    (
        "A2",
        "no superseded or deleted version is served after an acknowledgement; reads through any proxy (I2, I3)",
    ),
    (
        "A4",
        "from quiescence every run converges: no marker, cold equals truth (I4)",
    ),
    ("A5", "If-None-Match and If-Match behave as S3 (I5)"),
    ("A6", "no lock-caused answer but 503 after lock_wait (I6)"),
    (
        "A7",
        "no marker and nothing in flight: hot and cold agree (I7)",
    ),
    (
        "A8",
        "a hot DELETE or replace is made under a live lock or by If-Match (I8)",
    ),
    (
        "A9",
        "a marker is deleted only from its owner's CLEARING (I9, I10)",
    ),
    ("A11", "reads never wait; a writer's wait ends (I12, I13)"),
    (
        "A12",
        "gone is not cleared while hot is absent and cold holds the deleted object",
    ),
    ("A23", "a delete never waits for the copier"),
    (
        "A24",
        "the bytes of a stream that lost its creation race are never truth",
    ),
];

/// One row of [`matrix`].
#[derive(Debug)]
pub struct Row {
    pub rule: &'static str,
    pub what: &'static str,
    /// Scenarios run and states explored for this rule.
    pub scenarios: usize,
    pub states: usize,
    /// The first scenario and message that broke the rule, if any did.
    pub broken: Option<(&'static str, String)>,
    /// False for the liveness of the protocol as written: pruning the paths that break another
    /// rule leaves dead ends that are not the protocol's, so the rule is not judged.
    pub judged: bool,
}

/// For each rule, run the marker scenarios that expect to pass (the document's protocol as
/// written if `literal`, with the corrections otherwise) judging only that rule, so that a
/// violation of another rule prunes its path and does not hide this one.
pub fn matrix(literal: bool) -> Vec<Row> {
    matrix_of(literal, None)
}

/// [`matrix`] over the scenarios called in `only`, if given (a small matrix for tests).
pub fn matrix_of(literal: bool, only: Option<&[&str]>) -> Vec<Row> {
    let sets: Vec<(&'static str, Cfg)> = all()
        .into_iter()
        .filter(|s| only.is_none_or(|ids| ids.contains(&s.id)))
        .filter(|s| {
            (s.expect == Expect::Pass || (literal && s.id.starts_with("doc/")))
                && !s.heavy
                && !s.id.starts_with("ghost/")
                && !s.id.starts_with("lit/")
        })
        .filter_map(|s| match s.sys {
            Sys::Marks(c) => Some((s.id, if literal { unfix(c) } else { c })),
            Sys::Entries(_) | Sys::FirstStart(_) => None,
        })
        .collect();
    RULES
        .iter()
        .map(|(rule, what)| {
            let mut row = Row {
                rule,
                what,
                scenarios: 0,
                states: 0,
                broken: None,
                judged: !(literal && *rule == "A4"),
            };
            for (id, cfg) in &sets {
                let mut limits = Limits::new(cfg.max_states, cfg.copier);
                limits.only = Some(rule);
                let o = explore(&Marks::new(cfg.clone()), limits);
                row.scenarios += 1;
                row.states += o.states;
                if let (None, Some(t), true) = (&row.broken, o.trace, row.judged) {
                    row.broken = Some((id, t.message));
                }
            }
            row
        })
        .collect()
}

/// The shortest counterexample of `rule` in the marker scenario `id`, with the document's
/// protocol as written (`literal`) or with the corrections; `None` if the rule holds there.
pub fn trace_of(
    id: &str,
    literal: bool,
    rule: &str,
) -> Result<Option<crate::explore::Trace>, String> {
    let sc = all()
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("no scenario called {id}"))?;
    let Sys::Marks(cfg) = sc.sys else {
        return Err(format!("{id} is not a marker scenario"));
    };
    let only = RULES
        .iter()
        .map(|r| r.0)
        .find(|r| *r == rule)
        .ok_or_else(|| format!("no rule called {rule}"))?;
    let cfg = if literal { unfix(cfg) } else { cfg };
    let mut limits = Limits::new(cfg.max_states, cfg.copier);
    limits.only = Some(only);
    Ok(explore(&Marks::new(cfg), limits).trace)
}
