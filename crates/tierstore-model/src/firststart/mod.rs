//! The first start of a hot tier that has no `_tiering/format` object, as section 13.1 of
//! `docs/design/tierstore-design.md` decides it: a hot tier with no format object is new, and a
//! node writes the format object itself with a create-only put, after its first beat. Nothing is
//! listed first. It runs through the same explorer as the marker model, and is independent of it.
//!
//! # What is modelled
//!
//! One hot tier. The format object is one of three things ([`Fmt`]): not there, the record a start
//! writes (every node of one configuration writes the same record, the one its tier kind fixes,
//! so what the record says is not a state), or a record someone else has since written (the
//! operator's `drain`; a start must never replace it). The model is the same for an `s3:` tier
//! (`active-marks`) and a `dir:` tier (`active`): the target is a function of the configuration.
//!
//! Actors: up to two nodes of this release (`Pc` is the program counter of one start: register,
//! read the format, the create-only put, its answer) and at most one node of an earlier release
//! (`OldS`). Every request is one atomic step, so the read and the put that follows it have every
//! interleaving of the other actors between them. A node of this release may crash between any two
//! steps and restart (`crashes`); the operator may change the record once it exists (`operator`).
//! The answer of a put is one of three: it took effect and was read; it took effect and the
//! answer is ambiguous; it did not take effect and the answer is ambiguous (a 409 that is
//! answered while another writer's put is in flight, so it is enabled only while another node has
//! not yet put, which is what that answer means). A node that has an ambiguous answer does not
//! retry: it reads the object at every beat and follows it.
//!
//! The node of an earlier release writes journals and never reads the format object. It is not
//! supported beside this rule (a stated residual), and its scenarios show what happens.
//!
//! # Properties
//!
//! - `FS1`: once the format object exists, no journal exists: an acknowledged write of an earlier
//!   release is never left that this release does not read. It holds only where no earlier release
//!   runs or has left a journal: those scenarios are the residual the design states.
//! - `FS2`: where the scenario says the tier is new (`must_activate`), every node that is not down
//!   ends writable, with the format object written.
//! - `FS4`: a start never replaces a format object that someone else has written.

use crate::explore::{Model, Succ};

/// The nodes of this release in a scenario (at most).
pub const NEW: usize = 2;

/// The format object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Fmt {
    /// Not there.
    None,
    /// The record a start writes.
    Written,
    /// A record someone else wrote over it (a pause of the operator).
    Changed,
}

/// How a node writes the format object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Create {
    /// One request that fails where the object exists (`If-None-Match: *`; on `dir:` the hard
    /// link of `DirBackend::publish`).
    OnlyIfAbsent,
    /// A check that finds it absent, then an unconditional write: the create that is not atomic.
    CheckThenWrite,
}

/// How a start answers an ambiguous put.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Flaw {
    None,
    /// The wrong variant: after an ambiguous answer the node reads the object once and never again.
    NoFollow,
}

/// A scenario of the first start.
#[derive(Clone, Debug)]
pub struct FsCfg {
    /// Nodes of this release that start (1 or 2).
    pub nodes: usize,
    /// Crashes (each followed by a restart) of nodes of this release, in total.
    pub crashes: u8,
    pub create: Create,
    pub flaw: Flaw,
    /// The put may be answered ambiguously.
    pub ambiguous: bool,
    /// An ambiguous answer without effect may come even when no other put is in flight (every
    /// create is answered 409 and none lands): the stated residual.
    pub lost_always: bool,
    /// The operator may change the record once it exists.
    pub operator: bool,
    /// A node of an earlier release may run: it writes journals and never reads the format object.
    pub old: bool,
    /// Times the old node may start from the scenario's start.
    pub old_starts: u8,
    /// The format object exists at the start (a restart after the activation).
    pub init_format: Fmt,
    /// A journal of an earlier release is there at the start.
    pub init_journal: bool,
    /// The tier is new and no earlier release starts: every node must end writable.
    pub must_activate: bool,
    pub max_states: usize,
}

impl FsCfg {
    /// A new tier, `nodes` nodes of this release, no earlier release, no crash.
    pub fn new(nodes: usize) -> FsCfg {
        FsCfg {
            nodes,
            crashes: 0,
            create: Create::OnlyIfAbsent,
            flaw: Flaw::None,
            ambiguous: false,
            lost_always: false,
            operator: false,
            old: false,
            old_starts: 0,
            init_format: Fmt::None,
            init_journal: false,
            must_activate: true,
            max_states: 5_000_000,
        }
    }
}

/// The program counter of one node of this release.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Pc {
    /// Not part of the scenario.
    Off,
    Start,
    /// Registered and beating; the format is not read yet.
    Registered,
    /// The read found no object: the create-only put is next.
    Put,
    /// The check of the non-atomic create found the object absent.
    Blind,
    /// The put was sent and took effect; its answer is not read.
    Answer,
    /// The answer was ambiguous (or the object was not there): writes answer 503 and the node
    /// reads the object at every beat.
    Refused,
    Writable,
    Down,
}

impl Pc {
    fn writable(self) -> bool {
        matches!(self, Pc::Writable)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct OldS {
    alive: bool,
    wrote: bool,
    starts: u8,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct S {
    fmt: Fmt,
    /// A `_journal/` object exists (only an earlier release makes one).
    jr: bool,
    /// A start replaced a record someone else wrote.
    clobbered: bool,
    crashes: u8,
    nodes: [Pc; NEW],
    old: OldS,
}

/// One step.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Act {
    Register(usize),
    ReadFormat(usize),
    Create(usize),
    CreateAmbiguousApplied(usize),
    CreateAmbiguousLost(usize),
    Check(usize),
    Overwrite(usize),
    Answer(usize),
    Beat(usize),
    Crash(usize),
    Restart(usize),
    Operator,
    OldStart,
    OldWrite,
    OldDie,
}

pub struct FirstStart {
    cfg: FsCfg,
}

impl FirstStart {
    pub fn new(cfg: FsCfg) -> FirstStart {
        FirstStart { cfg }
    }

    fn node(&self, s: &S, i: usize, pc: Pc, act: Act, edit: impl FnOnce(&mut S)) -> Succ<S, Act> {
        let mut n = s.clone();
        n.nodes[i] = pc;
        edit(&mut n);
        Succ {
            act,
            to: Ok(n),
            bg: false,
        }
    }

    /// The effect of a put that takes effect: the record is written, replacing whatever is there.
    fn written(s: &mut S) {
        s.clobbered |= s.fmt == Fmt::Changed;
        s.fmt = Fmt::Written;
    }

    fn node_steps(&self, s: &S, i: usize, out: &mut Vec<Succ<S, Act>>) {
        let go = |pc: Pc, act: Act| self.node(s, i, pc, act, |_| {});
        match s.nodes[i] {
            Pc::Off | Pc::Down => {}
            Pc::Start => out.push(go(Pc::Registered, Act::Register(i))),
            Pc::Registered => {
                let next = if s.fmt == Fmt::None {
                    Pc::Put
                } else {
                    Pc::Writable
                };
                out.push(go(next, Act::ReadFormat(i)));
            }
            Pc::Put => match self.cfg.create {
                Create::OnlyIfAbsent if s.fmt != Fmt::None => {
                    // The put fails where the object exists: the node follows it.
                    out.push(go(Pc::Writable, Act::Create(i)))
                }
                Create::OnlyIfAbsent => {
                    out.push(self.node(s, i, Pc::Answer, Act::Create(i), Self::written));
                    if self.cfg.ambiguous {
                        out.push(self.node(
                            s,
                            i,
                            Pc::Refused,
                            Act::CreateAmbiguousApplied(i),
                            Self::written,
                        ));
                        let other_puts = (0..NEW).any(|j| j != i && s.nodes[j] == Pc::Put);
                        if other_puts || self.cfg.lost_always {
                            out.push(go(Pc::Refused, Act::CreateAmbiguousLost(i)));
                        }
                    }
                }
                Create::CheckThenWrite if s.fmt != Fmt::None => {
                    out.push(go(Pc::Writable, Act::Check(i)))
                }
                Create::CheckThenWrite => out.push(go(Pc::Blind, Act::Check(i))),
            },
            Pc::Blind => out.push(self.node(s, i, Pc::Answer, Act::Overwrite(i), Self::written)),
            Pc::Answer => out.push(go(Pc::Writable, Act::Answer(i))),
            Pc::Refused => {
                let follows = self.cfg.flaw != Flaw::NoFollow;
                if s.fmt != Fmt::None && follows {
                    out.push(go(Pc::Writable, Act::Beat(i)));
                }
            }
            Pc::Writable => {}
        }
        if s.crashes < self.cfg.crashes && !matches!(s.nodes[i], Pc::Off | Pc::Down) {
            out.push(self.node(s, i, Pc::Down, Act::Crash(i), |n| n.crashes += 1));
        }
        if s.nodes[i] == Pc::Down {
            out.push(go(Pc::Start, Act::Restart(i)));
        }
    }

    fn old_steps(&self, s: &S, out: &mut Vec<Succ<S, Act>>) {
        let mut push = |act: Act, edit: &dyn Fn(&mut S)| {
            let mut n = s.clone();
            edit(&mut n);
            out.push(Succ {
                act,
                to: Ok(n),
                bg: false,
            });
        };
        if self.cfg.operator && s.fmt == Fmt::Written {
            push(Act::Operator, &|n| n.fmt = Fmt::Changed);
        }
        if !self.cfg.old {
            return;
        }
        let old = &s.old;
        if !old.alive && old.starts > 0 {
            push(Act::OldStart, &|n| {
                n.old.alive = true;
                n.old.starts -= 1;
            });
        }
        if old.alive && !old.wrote {
            push(Act::OldWrite, &|n| {
                n.jr = true;
                n.old.wrote = true;
            });
        }
        if old.alive {
            push(Act::OldDie, &|n| n.old.alive = false);
        }
    }
}

impl Model for FirstStart {
    type S = S;
    type A = Act;

    fn init(&self) -> Vec<S> {
        let c = &self.cfg;
        let mut nodes = [Pc::Off; NEW];
        for node in nodes.iter_mut().take(c.nodes) {
            *node = Pc::Start;
        }
        vec![S {
            fmt: c.init_format,
            jr: c.init_journal,
            clobbered: false,
            crashes: 0,
            nodes,
            old: OldS {
                alive: false,
                wrote: false,
                starts: c.old_starts,
            },
        }]
    }

    fn successors(&self, s: &S, out: &mut Vec<Succ<S, Act>>) {
        for i in 0..NEW {
            self.node_steps(s, i, out);
        }
        self.old_steps(s, out);
    }

    fn check(&self, s: &S) -> Option<String> {
        if s.clobbered {
            return Some(
                "FS4 a start replaced a format object that someone else had written".into(),
            );
        }
        (s.fmt != Fmt::None && s.jr).then(|| {
            "FS1 the format object exists beside a journal of an earlier release that this release does not read"
                .into()
        })
    }

    fn terminal(&self, s: &S) -> Option<String> {
        let stuck = s.nodes.iter().any(|pc| *pc != Pc::Off && !pc.writable());
        (self.cfg.must_activate && stuck).then(|| {
            "FS2 the tier is new and nothing else starts, but the format object is missing or a node is not writable"
                .into()
        })
    }

    fn quiescent(&self, _: &S) -> bool {
        false
    }

    fn converged(&self, _: &S) -> bool {
        true
    }

    fn show_act(&self, a: &Act) -> String {
        format!("{a:?}")
    }

    fn show_state(&self, s: &S) -> String {
        format!("{s:?}")
    }
}
