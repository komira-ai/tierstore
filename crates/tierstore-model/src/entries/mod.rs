//! Today's protocol, as `docs/design/tierstore-design.md` sections 5 and 6.2 describe it
//! (`entry_put.rs`, `entry_delete.rs`, the entry reconciler): a per-key entry
//! `_entries/<sha256(K)>` is the lock; a put is write-once; a delete waits for a copy; a GET
//! never reads the entry. It runs through the same explorer and the same linearizability
//! tracker as the marker model, with the same properties that apply to it, as a check that the
//! model and its properties accept a protocol that is known to work.

use crate::explore::{Model, Succ};

mod steps;
use crate::lin::{Eff, Effs, Lin};

const NC: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EOp {
    Put,
    Del,
    Cb,
    Evict,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ESt {
    Held,
    Pending,
    Sealed,
}

/// The entry `(op, state, owner)`; `ver` is its ETag.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ent {
    pub op: EOp,
    pub st: ESt,
    pub owner: u8,
    pub ver: u8,
}

/// A client operation of the baseline: puts are write-once (create-only).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BOp {
    Put,
    Delete,
    Get,
}

/// The wrong variants of the baseline each property of it has a negative for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BFlaw {
    None,
    /// A delete that does not wait for the copy it amended.
    DeleteNoWait,
    /// A put that lands without checking the tiers under its entry.
    PutNoCheck,
    /// A put that acknowledges before it lands (the CAS before the landing).
    AckBeforeLanding,
    /// The leader's copy that does not take the HELD entry first.
    CopyNoHeld,
}

#[derive(Clone, Debug)]
pub struct BCfg {
    pub scripts: [Vec<BOp>; NC],
    pub leader: bool,
    pub evictions: u8,
    pub copy_back: bool,
    /// Start with the key evicted: cold holds version 2, hot nothing.
    pub cold_only: bool,
    pub crash_clients: bool,
    pub crash_leader: bool,
    pub max_crashes: u8,
    pub flaw: BFlaw,
    pub max_states: usize,
}

impl BCfg {
    pub fn new(p0: &[BOp], p1: &[BOp]) -> BCfg {
        BCfg {
            scripts: [p0.to_vec(), p1.to_vec()],
            leader: true,
            evictions: 0,
            copy_back: false,
            cold_only: false,
            crash_clients: false,
            crash_leader: false,
            max_crashes: 0,
            flaw: BFlaw::None,
            max_states: 20_000_000,
        }
    }

    fn gidx(&self, c: usize, j: usize) -> u8 {
        (self.scripts[..c].iter().map(Vec::len).sum::<usize>() + j) as u8
    }

    fn value(&self, g: u8) -> u8 {
        let before = self
            .scripts
            .iter()
            .flatten()
            .take(usize::from(g))
            .filter(|op| **op != BOp::Get)
            .count();
        3 + before as u8
    }

    fn effs(&self) -> Effs {
        let mut e: Effs = [None; 8];
        for c in 0..NC {
            for (j, op) in self.scripts[c].iter().enumerate() {
                let g = self.gidx(c, j);
                e[usize::from(g)] = match op {
                    BOp::Put => Some(Eff::PutCo(self.value(g))),
                    BOp::Delete => Some(Eff::Del),
                    BOp::Get => None,
                };
            }
        }
        e
    }
}

/// The client's program counter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BPc {
    Idle,
    PHead,
    PEnt,
    PRead,
    /// Lockless HEADs of a refused put (hot, cold): 0 unread, 1 absent, else present.
    PLock {
        h: u8,
        c: u8,
    },
    /// The check under the entry (hot, cold).
    PChk {
        h: u8,
        c: u8,
    },
    PLand,
    PAck,
    /// Free the entry, then answer `r` (1 is 412, 2 is 503).
    PFree {
        r: u8,
    },
    DEnt,
    DRead,
    /// Amend or claim the entry read as version `a`; `k`: 1 amend a pending put, 2 amend the
    /// leader's copy (wait), 3 claim a handed-over delete (wait).
    DAmend {
        a: u8,
        k: u8,
    },
    DWait,
    DCold {
        k: u8,
    },
    DHot {
        k: u8,
    },
    DFin {
        k: u8,
    },
    DPoll,
    GHot,
    GCold,
    GWant,
}

impl BPc {
    fn versions(&mut self, f: &mut dyn FnMut(&mut u8)) {
        if let BPc::DAmend { a, .. } = self {
            f(a);
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct BCl {
    pub next: u8,
    pub pc: BPc,
    pub alive: bool,
    pub poss: u8,
    /// The version of the entry this client owns.
    pub mine: u8,
}

/// The leader's program counter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LPc {
    Idle,
    /// CAS `(PUT, PENDING)` to `(PUT, HELD, leader)`.
    Held {
        a: u8,
    },
    Head,
    Cold {
        h: u8,
    },
    Get,
    Copy {
        v: u8,
    },
    /// CAS the HELD entry (version `n`) to SEALED.
    SealPut {
        n: u8,
    },
    /// Reap a departed owner's HELD entry to PENDING.
    Reap {
        a: u8,
    },
    SealDel {
        a: u8,
    },
    DelCold,
    DelHot,
    SealCb {
        a: u8,
    },
    CbRead,
    CbPut {
        c: u8,
    },
    /// Delete the entry the leader sealed.
    DelEnt,
    EvH,
    EvC {
        h: u8,
    },
    EvEnt,
    EvDel,
}

impl LPc {
    fn versions(&mut self, f: &mut dyn FnMut(&mut u8)) {
        match self {
            LPc::Held { a } | LPc::Reap { a } | LPc::SealDel { a } | LPc::SealCb { a } => f(a),
            LPc::SealPut { n } => f(n),
            _ => {}
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct BState {
    pub hot: u8,
    pub cold: u8,
    pub ent: Option<Ent>,
    pub cl: [BCl; NC],
    pub era: u8,
    pub pc: LPc,
    pub left: u8,
    pub lin: Lin,
    /// Deletes invoked and not acknowledged: while one is open the entry protocol lets reads
    /// see the key come and go (design doc section 5, Delete: before the acknowledgement GET
    /// and HEAD may still return the bytes).
    pub del_open: u8,
    pub crashes: u8,
    pub nn: u8,
}

pub struct Entries {
    pub cfg: BCfg,
    effs: Effs,
}

/// The ways the baseline can end a client operation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum R {
    Ok,
    R412,
    Fail503,
    Unk503,
    Read(u8),
}

type Out = Vec<Succ<BState, (u8, &'static str)>>;

impl BState {
    fn truth(&self) -> u8 {
        if self.hot != 0 { self.hot } else { self.cold }
    }

    fn fresh(&mut self) -> u8 {
        self.nn += 1;
        self.nn
    }

    fn canon(&mut self) {
        let mut map: Vec<(u8, u8)> = Vec::new();
        let mut visit = |v: &mut u8| {
            if *v == 0 {
                return;
            }
            let n = match map.iter().find(|p| p.0 == *v) {
                Some(p) => p.1,
                None => {
                    let n = map.len() as u8 + 1;
                    map.push((*v, n));
                    n
                }
            };
            *v = n;
        };
        if let Some(e) = self.ent.as_mut() {
            visit(&mut e.ver);
        }
        for c in self.cl.iter_mut() {
            c.pc.versions(&mut visit);
            visit(&mut c.mine);
        }
        self.pc.versions(&mut visit);
        self.nn = map.len() as u8;
    }
}

impl Entries {
    pub fn new(cfg: BCfg) -> Entries {
        let effs = cfg.effs();
        Entries { cfg, effs }
    }

    fn emit(
        &self,
        s: &BState,
        out: &mut Out,
        act: (u8, &'static str),
        bg: bool,
        f: impl FnOnce(&mut BState) -> Result<(), String>,
    ) {
        let mut ns = s.clone();
        let to = f(&mut ns).and_then(|()| self.post(&mut ns)).map(|()| ns);
        out.push(Succ { act, to, bg });
    }

    fn post(&self, s: &mut BState) -> Result<(), String> {
        let t = s.truth();
        if !s.lin.observe(&self.effs, t) && s.del_open == 0 {
            let conditional = (0..8u8).any(|g| {
                matches!(self.effs[usize::from(g)], Some(Eff::PutCo(v)) if v == t)
                    && !s.lin.applied_everywhere(g)
            });
            return Err(format!(
                "{} truth of the key is version {t} (hot {}, cold {}) but the acknowledged operations do not allow it",
                if conditional { "A5" } else { "A1" },
                s.hot,
                s.cold
            ));
        }
        for c in s.cl.iter_mut() {
            if c.alive && c.pc != BPc::Idle {
                c.poss |= 1 << t;
            }
        }
        s.canon();
        Ok(())
    }

    fn owner_dead(&self, s: &BState, owner: u8) -> bool {
        if usize::from(owner) < NC {
            !s.cl[usize::from(owner)].alive
        } else {
            owner - 8 != s.era
        }
    }

    fn me(&self, s: &BState) -> u8 {
        8 + s.era
    }

    fn ack(&self, s: &mut BState, c: usize, r: R) -> Result<(), String> {
        let t = s.truth();
        s.cl[c].poss |= 1 << t;
        let poss = s.cl[c].poss;
        let g = self.cfg.gidx(c, usize::from(s.cl[c].next));
        match r {
            R::Ok => {
                s.del_open &= !(1 << g);
                if !s.lin.ack(&self.effs, g) {
                    return Err(format!(
                        "A1 operation {g} was acknowledged but no order lets it take effect"
                    ));
                }
                if !s.lin.clone().observe(&self.effs, t) {
                    let rule = if self.cfg.value(g) == t
                        && matches!(self.effs[usize::from(g)], Some(Eff::PutCo(_)))
                    {
                        "A5"
                    } else {
                        "A1"
                    };
                    return Err(format!(
                        "{rule} operation {g} was acknowledged 200 although the key held another version when it took effect"
                    ));
                }
            }
            R::R412 => {
                if poss >> 1 == 0 {
                    return Err(format!(
                        "A5 operation {g} answered 412 but the key never existed while it ran"
                    ));
                }
            }
            R::Fail503 => {
                if !s.lin.kill(g) {
                    return Err(format!(
                        "A24 operation {g} answered a failure that says it did nothing, but it took effect"
                    ));
                }
            }
            R::Unk503 => {}
            R::Read(v) => {
                if poss >> v & 1 == 0 {
                    return Err(format!(
                        "A2 a read returned version {v} but the key only held {poss:#b} while it ran"
                    ));
                }
            }
        }
        Ok(())
    }

    fn end(&self, s: &mut BState, c: usize) {
        let cl = &mut s.cl[c];
        cl.next += 1;
        cl.pc = BPc::Idle;
        cl.poss = 0;
        cl.mine = 0;
    }

    fn ack_end(&self, s: &mut BState, c: usize, r: R) -> Result<(), String> {
        self.ack(s, c, r)?;
        self.end(s, c);
        Ok(())
    }

    fn val(&self, s: &BState, c: usize) -> u8 {
        self.cfg.value(self.cfg.gidx(c, usize::from(s.cl[c].next)))
    }

    fn op(&self, s: &BState, c: usize) -> BOp {
        self.cfg.scripts[c][usize::from(s.cl[c].next)]
    }

    fn finished(&self, s: &BState, c: usize) -> bool {
        !s.cl[c].alive
            || (s.cl[c].pc == BPc::Idle && usize::from(s.cl[c].next) >= self.cfg.scripts[c].len())
    }
}

impl Model for Entries {
    type S = BState;
    type A = (u8, &'static str);

    fn init(&self) -> Vec<BState> {
        let mk = || BCl {
            next: 0,
            pc: BPc::Idle,
            alive: true,
            poss: 0,
            mine: 0,
        };
        let cold = if self.cfg.cold_only { 2 } else { 0 };
        vec![BState {
            hot: 0,
            cold,
            ent: None,
            cl: [mk(), mk()],
            era: 0,
            pc: LPc::Idle,
            left: self.cfg.evictions,
            lin: Lin::new(cold),
            del_open: 0,
            crashes: 0,
            nn: 0,
        }]
    }

    fn successors(&self, s: &BState, out: &mut Out) {
        for c in 0..NC {
            if s.cl[c].alive {
                self.client_steps(s, out, c);
            }
        }
        if self.cfg.leader {
            self.leader_steps(s, out);
        }
        if s.crashes < self.cfg.max_crashes {
            if self.cfg.crash_clients {
                for c in 0..NC {
                    if s.cl[c].alive && s.cl[c].pc != BPc::Idle {
                        self.emit(s, out, (c as u8, "CRASH"), false, |ns| {
                            ns.cl[c].alive = false;
                            ns.crashes += 1;
                            Ok(())
                        });
                    }
                }
            }
            if self.cfg.crash_leader {
                self.emit(
                    s,
                    out,
                    (13, "the leader dies; a successor takes over"),
                    false,
                    |ns| {
                        ns.crashes += 1;
                        ns.era += 1;
                        ns.pc = LPc::Idle;
                        Ok(())
                    },
                );
            }
        }
    }

    fn check(&self, s: &BState) -> Option<String> {
        // The entry protocol's no-entry rule: with no entry and nothing in flight, hot and cold agree.
        let quiet = s.cl.iter().all(|c| !c.alive || c.pc == BPc::Idle) && s.pc == LPc::Idle;
        if s.ent.is_none() && quiet && s.hot != 0 && s.cold != s.hot && s.cold != 0 {
            return Some(format!(
                "A7 no entry and nothing in flight, but hot holds {} and cold {}",
                s.hot, s.cold
            ));
        }
        None
    }

    fn terminal(&self, s: &BState) -> Option<String> {
        for c in 0..NC {
            if !self.finished(s, c) {
                return Some(format!(
                    "A11 client {c} can never finish: stuck at {:?} with the entry {:?}",
                    s.cl[c].pc, s.ent
                ));
            }
        }
        None
    }

    fn quiescent(&self, s: &BState) -> bool {
        (0..NC).all(|c| self.finished(s, c))
    }

    fn converged(&self, s: &BState) -> bool {
        s.ent.is_none() && (s.hot == 0 || s.cold == s.hot)
    }

    fn show_act(&self, a: &(u8, &'static str)) -> String {
        let who = match a.0 {
            0 | 1 => format!("proxy{}", a.0),
            10 => "leader".to_string(),
            _ => "env".to_string(),
        };
        format!("{who}: {}", a.1)
    }

    fn show_state(&self, s: &BState) -> String {
        let mut v = format!(
            "hot={} cold={} entry={:?}",
            s.hot,
            s.cold,
            s.ent.map(|e| (e.op, e.st, e.owner))
        );
        for (i, c) in s.cl.iter().enumerate() {
            if c.alive && c.pc != BPc::Idle {
                v.push_str(&format!(" | proxy{i}:{:?}", c.pc));
            }
        }
        if s.pc != LPc::Idle {
            v.push_str(&format!(" | leader:{:?}", s.pc));
        }
        v
    }
}

#[cfg(test)]
mod tests;
