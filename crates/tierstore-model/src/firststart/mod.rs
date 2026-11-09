//! The first start of a hot tier that has no `_tiering/format` object, as section 13.1 of
//! `docs/design/tierstore-design.md` proposes it: a node proves the tier is new, then writes
//! the format object create-only, so that no operator verb is needed. It runs through the same
//! explorer as the marker model, and is independent of it.
//!
//! # What is modelled
//!
//! One hot tier. The format object is one bit (it exists or not): every node writes the same
//! record, the target its tier kind fixes (`Target::Marks` for an `s3:` tier with an empty marks
//! bucket, `Target::Entries` for `dir:`), so what the record says is not a state. The things the
//! earlier releases leave are bits too: a `_journal/` object (`jr`), an `_entries/` object
//! (`init_entries` from before the start, `en_new` written by a node of this release under epoch 2),
//! a marker (`bucket_used` from before the start, `mk_new`), and the existence of the marks bucket.
//!
//! Actors: up to two nodes of this release (`Pc` is the program counter of one start: register,
//! read the format, the four lists of the proof, the create-only put, its answer) and at most
//! one node of an earlier release (`OldS`). Every request is one atomic step, so a list and the
//! put that follows it have every interleaving of the other actors between them. A node of this
//! release may crash between any two steps and restart (`crashes`); the old node may start (or
//! restart, `old_starts`), write a journal (an acknowledged write) and die. It never reads the
//! format object: a release before the bridge release does not know it, and what the bridge
//! release does with a phase it does not know is not assumed. Its registration stays whatever
//! happens to the node (an empty registration says nothing of whether the node is alive, and no
//! clock of a node can be compared with the hot tier's to age a `proto` 1 one), and any
//! registration of an earlier release stops the proof.
//!
//! # Properties
//!
//! - `FS1`: once the format object exists, no journal, no `_entries/` object of an earlier
//!   release and no marker of an earlier start exists: an acknowledged write of an earlier
//!   release is never left that this release does not read.
//! - `FS2`: where the scenario says the tier is new and nothing else starts (`must_activate`),
//!   every node that is not down ends writable, with the format object written.
//! - `FS3`: the record is never written at epoch 3 over a marks bucket that does not exist.
//!
//! The wrong variants of [`Flaw`] each drop one test of the proof (rejected by `FS1` or `FS3`),
//! or count a registration of this release as one that blocks (rejected by `FS2`: the nodes
//! block each other and none activates); [`Create::CheckThenWrite`] is the create that is not atomic (the case of a
//! second process on a `dir:` tier, if the file system did not make the link atomic), which must
//! still pass, since every writer writes the same record.

use crate::explore::{Model, Succ};

/// The nodes of this release in a scenario (at most).
pub const NEW: usize = 2;

/// What a node of this release writes: fixed by the kind of the hot tier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Target {
    /// An `s3:` hot tier with its marks bucket: `active-marks`, epoch 3.
    Marks,
    /// A `dir:` hot tier: `active`, epoch 2.
    Entries,
}

/// When the old node may start, relative to the format object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum OldAt {
    Any,
    BeforeFormat,
    AfterFormat,
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

/// A test of the proof a wrong variant leaves out.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Flaw {
    None,
    NoRegistrationCheck,
    NoJournalCheck,
    NoEntriesCheck,
    NoMarksCheck,
    /// The registrations listed include this node's own.
    CountsOwnRegistration,
    /// The registrations listed include those of nodes of this release that start now.
    CountsPeerRegistrations,
}

/// A scenario of the first start.
#[derive(Clone, Debug)]
pub struct FsCfg {
    pub target: Target,
    /// Nodes of this release that start (1 or 2).
    pub nodes: usize,
    /// Crashes (each followed by a restart) of nodes of this release, in total.
    pub crashes: u8,
    pub create: Create,
    pub flaw: Flaw,
    /// A node of an earlier release may run: it registers (an empty body or `proto` 1), writes
    /// journals, and never reads the format object.
    pub old: bool,
    /// The old node is running, registered, at the start of the scenario.
    pub old_up: bool,
    /// The old node has the id of the node of this release (an upgrade in place): the proof
    /// leaves its registration out, as it leaves out the node's own.
    pub old_same_id: bool,
    /// Creates, in total, whose answer is an error that does not say whether the object was
    /// written (a 409 `ConditionalRequestConflict`, a timeout): each takes effect or not, and the
    /// node reads the format object at its next beat.
    pub lost_puts: u8,
    /// Times the old node may start or restart from the scenario's start.
    pub old_starts: u8,
    /// When the old node may start or restart: any time, only once the format object exists,
    /// or only while it does not.
    pub old_at: OldAt,
    /// The format object exists at the start (a restart after the activation).
    pub init_format: bool,
    pub init_journal: bool,
    pub init_entries: bool,
    /// The marks bucket exists, and holds a marker.
    pub bucket_exists: bool,
    pub bucket_used: bool,
    /// The tier is new and no earlier release starts: every node must end writable.
    pub must_activate: bool,
    pub max_states: usize,
}

impl FsCfg {
    /// A new tier of `target`, `nodes` nodes of this release, an empty marks bucket, no earlier
    /// release, no crash.
    pub fn new(target: Target, nodes: usize) -> FsCfg {
        FsCfg {
            target,
            nodes,
            crashes: 0,
            create: Create::OnlyIfAbsent,
            flaw: Flaw::None,
            old: false,
            old_up: false,
            old_same_id: false,
            lost_puts: 0,
            old_starts: 0,
            old_at: OldAt::Any,
            init_format: false,
            init_journal: false,
            init_entries: false,
            bucket_exists: true,
            bucket_used: false,
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
    /// Registered; the format is not read yet.
    Registered,
    /// The proof, one list at a time; the flag is whether a list found something (the first list
    /// has nothing to carry).
    Regs,
    Entries(bool),
    Journal(bool),
    Marks(bool),
    /// The proof is complete (the flag says it failed).
    Put(bool),
    /// The check of the non-atomic create found the object absent.
    Blind,
    /// The put was sent and took effect; its answer is not read.
    Answer,
    /// The proof failed, or the object was not there: writes answer 503.
    Refused,
    Writable,
    /// Writable, and has written once.
    Wrote,
    Down,
}

impl Pc {
    fn writable(self) -> bool {
        matches!(self, Pc::Writable | Pc::Wrote)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct OldS {
    alive: bool,
    /// Its registration exists: it stays after the node dies, whatever its age.
    registered: bool,
    wrote: bool,
    starts: u8,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct S {
    /// `_tiering/format` exists.
    fmt: bool,
    /// A `_journal/` object exists (only an earlier release makes one).
    jr: bool,
    en_new: bool,
    mk_new: bool,
    /// A node wrote the record at epoch 3 over a missing marks bucket.
    no_bucket: bool,
    crashes: u8,
    /// Creates answered with an error that does not say whether they took effect.
    lost: u8,
    nodes: [Pc; NEW],
    old: OldS,
}

/// One step.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Act {
    Register(usize),
    ReadFormat(usize),
    ListRegistrations(usize),
    ListEntries(usize),
    ListJournal(usize),
    ListMarks(usize),
    Refuse(usize),
    Create(usize),
    /// A create answered 409 or timed out that took effect.
    CreateLanded(usize),
    /// A create answered 409 or timed out that did not take effect.
    CreateVoid(usize),
    Check(usize),
    Overwrite(usize),
    Answer(usize),
    Beat(usize),
    ClientWrite(usize),
    Crash(usize),
    Restart(usize),
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

    /// Whether the registration of the old node stops the proof: any registration of an earlier
    /// release does, live, silent or empty, since no clock of the node can be compared with the
    /// hot tier's, unless it has the node's own id (the proof leaves that one out).
    fn old_blocks(&self, s: &S) -> bool {
        self.cfg.old && s.old.registered && !self.cfg.old_same_id
    }

    fn old_may_start(&self, format_exists: bool) -> bool {
        match self.cfg.old_at {
            OldAt::Any => true,
            OldAt::BeforeFormat => !format_exists,
            OldAt::AfterFormat => format_exists,
        }
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

    fn node_steps(&self, s: &S, i: usize, out: &mut Vec<Succ<S, Act>>) {
        let flaw = self.cfg.flaw;
        let go = |pc: Pc, act: Act| self.node(s, i, pc, act, |_| {});
        match s.nodes[i] {
            Pc::Off | Pc::Refused | Pc::Down => {}
            Pc::Start => out.push(go(Pc::Registered, Act::Register(i))),
            Pc::Registered => {
                let next = if s.fmt { Pc::Writable } else { Pc::Regs };
                out.push(go(next, Act::ReadFormat(i)));
            }
            Pc::Regs => {
                let old = self.old_blocks(s) && flaw != Flaw::NoRegistrationCheck;
                let own = flaw == Flaw::CountsOwnRegistration;
                let peers = flaw == Flaw::CountsPeerRegistrations
                    && (0..NEW).any(|j| j != i && !matches!(s.nodes[j], Pc::Off | Pc::Start));
                out.push(go(
                    Pc::Entries(old || own || peers),
                    Act::ListRegistrations(i),
                ));
            }
            Pc::Entries(b) => {
                let seen = (self.cfg.init_entries || s.en_new) && flaw != Flaw::NoEntriesCheck;
                out.push(go(Pc::Journal(b || seen), Act::ListEntries(i)));
            }
            Pc::Journal(b) => {
                let seen = s.jr && flaw != Flaw::NoJournalCheck;
                let next = match self.cfg.target {
                    Target::Marks => Pc::Marks(b || seen),
                    Target::Entries => Pc::Put(b || seen),
                };
                out.push(go(next, Act::ListJournal(i)));
            }
            Pc::Marks(b) => {
                let used = !self.cfg.bucket_exists || self.cfg.bucket_used || s.mk_new;
                let seen = used && flaw != Flaw::NoMarksCheck;
                out.push(go(Pc::Put(b || seen), Act::ListMarks(i)));
            }
            Pc::Put(true) => out.push(go(Pc::Refused, Act::Refuse(i))),
            Pc::Put(false) => match self.cfg.create {
                Create::OnlyIfAbsent => {
                    out.push(self.node(s, i, Pc::Answer, Act::Create(i), |n| {
                        n.fmt = true;
                        n.no_bucket |= self.no_bucket_written();
                    }));
                    if s.lost < self.cfg.lost_puts && !s.fmt {
                        out.push(self.node(s, i, Pc::Refused, Act::CreateLanded(i), |n| {
                            n.fmt = true;
                            n.lost += 1;
                            n.no_bucket |= self.no_bucket_written();
                        }));
                        out.push(self.node(s, i, Pc::Refused, Act::CreateVoid(i), |n| {
                            n.lost += 1;
                        }));
                    }
                }
                Create::CheckThenWrite if s.fmt => out.push(go(Pc::Answer, Act::Check(i))),
                Create::CheckThenWrite => out.push(go(Pc::Blind, Act::Check(i))),
            },
            Pc::Blind => out.push(self.node(s, i, Pc::Answer, Act::Overwrite(i), |n| {
                n.fmt = true;
                n.no_bucket |= self.no_bucket_written();
            })),
            Pc::Answer => out.push(go(Pc::Writable, Act::Answer(i))),
            Pc::Writable => {
                out.push(self.node(
                    s,
                    i,
                    Pc::Wrote,
                    Act::ClientWrite(i),
                    |n| match self.cfg.target {
                        Target::Marks => n.mk_new = true,
                        Target::Entries => n.en_new = true,
                    },
                ))
            }
            Pc::Wrote => {}
        }
        if s.nodes[i] == Pc::Refused && s.fmt {
            out.push(go(Pc::Writable, Act::Beat(i)));
        }
        if s.crashes < self.cfg.crashes && !matches!(s.nodes[i], Pc::Off | Pc::Down) {
            out.push(self.node(s, i, Pc::Down, Act::Crash(i), |n| n.crashes += 1));
        }
        if s.nodes[i] == Pc::Down {
            out.push(go(Pc::Start, Act::Restart(i)));
        }
    }

    /// Whether a write of the record by a node is a write at epoch 3 over a missing bucket.
    fn no_bucket_written(&self) -> bool {
        self.cfg.target == Target::Marks && !self.cfg.bucket_exists
    }

    fn old_steps(&self, s: &S, out: &mut Vec<Succ<S, Act>>) {
        if !self.cfg.old {
            return;
        }
        let old = &s.old;
        let mut push = |act: Act, edit: &dyn Fn(&mut S)| {
            let mut n = s.clone();
            edit(&mut n);
            out.push(Succ {
                act,
                to: Ok(n),
                bg: false,
            });
        };
        if !old.alive && old.starts > 0 && self.old_may_start(s.fmt) {
            push(Act::OldStart, &|n| {
                n.old.alive = true;
                n.old.registered = true;
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
        let up = c.old && c.old_up;
        vec![S {
            fmt: c.init_format,
            jr: c.init_journal,
            en_new: false,
            mk_new: false,
            no_bucket: false,
            crashes: 0,
            lost: 0,
            nodes,
            old: OldS {
                alive: up,
                registered: up,
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
        let left = s.jr || self.cfg.init_entries || self.cfg.bucket_used;
        if s.fmt && left {
            return Some(
                "FS1 the format object exists beside a journal, an entry or a marker of an earlier start that this release does not read"
                    .into(),
            );
        }
        s.no_bucket.then(|| {
            "FS3 the record was written at epoch 3 over a marks bucket that does not exist".into()
        })
    }

    fn terminal(&self, s: &S) -> Option<String> {
        let stuck = s.nodes.iter().any(|pc| *pc != Pc::Off && !pc.writable());
        (self.cfg.must_activate && (!s.fmt || stuck)).then(|| {
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
