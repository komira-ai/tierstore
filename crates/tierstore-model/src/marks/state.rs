//! The state of the marker model, its store primitives and its properties.

use super::client::Pc;
use super::config::{C0, CORRUPT, Cfg, H0, Init, NC, Op};
use super::leader::{Cop, Ev, Ld, Sw};
use crate::explore::{Model, Succ};
use crate::lin::{Eff, Effs, Lin};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum St {
    Dirty,
    Lock,
    Clearing,
    Want,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Why {
    Overwrite,
    Delete,
    Evict,
    CopyBack,
    ColdLand,
}

/// The marker object `_tiering/marks/<K>`; `ver` is its ETag (unique per write).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Mk {
    pub st: St,
    pub gone: bool,
    pub dirty: bool,
    pub why: Why,
    /// A client index (0..8) or `8 + leader generation`.
    pub owner: u8,
    pub ver: u8,
}

/// A client actor: a proxy running its script, or a ghost slot holding a stalled proxy's call.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Cl {
    /// The proxy this actor is (a ghost keeps the id of the proxy it came from).
    pub id: u8,
    /// Index of the script operation in progress or next.
    pub next: u8,
    pub pc: Pc,
    pub alive: bool,
    pub ghost: bool,
    /// The values the key held while this operation ran (for reads and no-effect answers).
    pub poss: u8,
    /// Version of the lock this operation holds (0: none).
    pub ln: u8,
    /// `gone` of the lock this operation holds.
    pub lg: bool,
    /// Version to read back after an answer was lost (0: none).
    pub rb: u8,
    /// The answer to deliver once the release is done (0: none).
    pub fin: u8,
    /// The bytes of a one-shot stream that lost the creation race were consumed.
    pub bad: bool,
}

impl Cl {
    fn new(id: u8) -> Cl {
        Cl {
            id,
            next: 0,
            pc: Pc::Idle,
            alive: id < NC as u8,
            ghost: false,
            poss: 0,
            ln: 0,
            lg: false,
            rb: 0,
            fin: 0,
            bad: false,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct State {
    pub hot: u8,
    pub cold: u8,
    pub mk: Option<Mk>,
    /// Clients in flight when the hot object landed (evict_min_age).
    pub young: u8,
    pub cl: [Cl; NC + 2],
    pub ld: Ld,
    pub gl: Option<Ld>,
    pub era: u8,
    pub lin: Lin,
    pub ans: [u8; 4],
    pub tainted: bool,
    pub crashes: u8,
    pub nn: u8,
    pub stalled: bool,
}

/// An action label: who and what.
#[derive(Clone, Copy, Debug)]
pub struct Act {
    pub who: u8,
    pub what: &'static str,
}

/// Who: the leader's copier, evictor, sweep, the environment, a ghost of the leader.
pub const W_COP: u8 = 10;
pub const W_EVICT: u8 = 11;
pub const W_SWEEP: u8 = 12;
pub const W_ENV: u8 = 13;
pub const W_GHOST_LD: u8 = 14;

/// The answers an operation can end with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resp {
    /// 200 or 204: the operation took effect.
    Ok,
    R412,
    /// 409 `ConditionalRequestConflict`: the retryable answer to a conditional put that met a
    /// concurrent change of the key.
    R409,
    R404,
    /// 503 that guarantees nothing was done.
    Fail503,
    /// 503 after a lock wait ran out (the holder is dead).
    Wait503,
    /// 503 whose outcome is unknown.
    Unk503,
    /// 503 of a stream that lost the creation race.
    Stream503,
    Read(u8),
}

pub struct Marks {
    pub cfg: Cfg,
    pub(super) effs: Effs,
}

impl State {
    pub fn truth(&self) -> u8 {
        if self.hot != 0 {
            self.hot
        } else if matches!(self.mk, Some(m) if m.gone) {
            0
        } else {
            self.cold
        }
    }

    pub(super) fn fresh(&mut self) -> u8 {
        self.nn += 1;
        self.nn
    }

    /// Any write of the marker object. A12: `gone` is never cleared while hot is absent and cold
    /// holds an object, because a reader would then serve the deleted cold object.
    pub(super) fn mk_put(&mut self, new: Mk) -> Result<(), String> {
        if let Some(o) = self.mk
            && o.gone
            && !new.gone
            && self.hot == 0
            && self.cold != 0
        {
            return Err(format!(
                "A12 the marker's gone flag was cleared while hot is absent and cold still holds version {}",
                self.cold
            ));
        }
        self.mk = Some(new);
        Ok(())
    }

    /// Visit every marker version held anywhere in the state, in a fixed order.
    fn versions(&mut self, f: &mut dyn FnMut(&mut u8)) {
        if let Some(m) = self.mk.as_mut() {
            f(&mut m.ver);
        }
        for c in self.cl.iter_mut() {
            c.pc.versions(f);
            f(&mut c.ln);
            f(&mut c.rb);
        }
        self.ld.versions(f);
        if let Some(g) = self.gl.as_mut() {
            g.versions(f);
        }
    }

    /// Rename the versions 1, 2, 3... in order of first appearance, so that states that differ
    /// only in the values of their ETags are one state.
    fn canon(&mut self) {
        let mut map: Vec<(u8, u8)> = Vec::new();
        self.versions(&mut |v| {
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
        });
        self.nn = map.len() as u8;
    }
}

impl Marks {
    pub fn new(cfg: Cfg) -> Marks {
        let effs = cfg.effs();
        Marks { cfg, effs }
    }

    pub(super) fn act(who: u8, what: &'static str) -> Act {
        Act { who, what }
    }

    pub(super) fn emit(
        &self,
        s: &State,
        out: &mut Vec<Succ<State, Act>>,
        act: Act,
        bg: bool,
        f: impl FnOnce(&mut State) -> Result<(), String>,
    ) {
        let mut ns = s.clone();
        let to = f(&mut ns).and_then(|()| self.post(&mut ns)).map(|()| ns);
        out.push(Succ { act, to, bg });
    }

    /// After every step: the truth of the key must be explained by the acknowledged history.
    fn post(&self, s: &mut State) -> Result<(), String> {
        let infl = self.co_inflight(s);
        s.young &= infl;
        let t = s.truth();
        if !s.lin.observe(&self.effs, t) {
            let may = s.lin.may_hold(&self.effs);
            // A value written by a conditional put that the history cannot explain is a broken
            // condition (A5); any other unexplained value is a lost or undone operation (A1).
            let conditional = (0..8u8).any(|g| {
                matches!(self.effs[usize::from(g)], Some(Eff::PutCo(v) | Eff::PutIm(v, _)) if v == t)
                    && !s.lin.applied_everywhere(g)
            });
            let rule = if conditional { "A5" } else { "A1" };
            return Err(if t == CORRUPT {
                "A24 the bytes of a one-shot stream that lost its creation race are the key's truth"
                    .to_string()
            } else {
                format!(
                    "{rule} truth of the key is version {t} (hot {}, cold {}, marker {:?}) but the acknowledged operations allow only versions {:?}",
                    s.hot,
                    s.cold,
                    s.mk.map(|m| (m.st, m.gone)),
                    (0..8).filter(|v| may >> v & 1 == 1).collect::<Vec<u8>>()
                )
            });
        }
        for c in s.cl.iter_mut() {
            if c.alive && c.pc != Pc::Idle {
                c.poss |= 1 << t;
            }
        }
        s.canon();
        Ok(())
    }

    /// The client actors with a create-only put between its start and the landing of its object.
    pub(super) fn co_inflight(&self, s: &State) -> u8 {
        let mut m = 0u8;
        for (i, c) in s.cl.iter().enumerate().take(NC) {
            let co = matches!(
                self.cfg.scripts[usize::from(c.id)].get(usize::from(c.next)),
                Some(Op::PutCo(_))
            );
            if c.alive && co && matches!(c.pc, Pc::P1 { .. } | Pc::P2 { .. }) {
                m |= 1 << i;
            }
        }
        m
    }

    pub(super) fn owner_dead(&self, s: &State, owner: u8) -> bool {
        if usize::from(owner) < NC {
            !s.cl[usize::from(owner)].alive
        } else {
            owner - 8 != s.era
        }
    }

    /// The operation the actor runs.
    pub(super) fn op_of(&self, cl: &Cl) -> Op {
        self.cfg.scripts[usize::from(cl.id)][usize::from(cl.next)]
    }

    /// Deliver an answer to the client and record it against the history.
    pub(super) fn ack(&self, s: &mut State, c: usize, r: Resp) -> Result<(), String> {
        let t = s.truth();
        if s.cl[c].ghost {
            return Ok(());
        }
        s.cl[c].poss |= 1 << t;
        let poss = s.cl[c].poss;
        let cl = &s.cl[c];
        let op = self.op_of(cl);
        let g = self.cfg.gidx(usize::from(cl.id), usize::from(cl.next));
        let code = match r {
            Resp::Ok => {
                if !s.lin.ack(&self.effs, g) {
                    let rule = if matches!(op, Op::PutCo(_) | Op::PutIm(_)) {
                        "A5"
                    } else {
                        "A1"
                    };
                    return Err(format!(
                        "{rule} operation {g} ({op:?}) was acknowledged but no order of the operations lets it take effect"
                    ));
                }
                // The acknowledgement must agree with what the stores show now: a create-only
                // or If-Match put acknowledged 200 whose object is not the key's truth, or whose
                // condition did not hold when it took effect, broke its condition.
                if !s.lin.clone().observe(&self.effs, t) {
                    let own = self.cfg.value(g) == t;
                    let rule = if t == CORRUPT {
                        "A24"
                    } else if own && matches!(op, Op::PutCo(_) | Op::PutIm(_)) {
                        "A5"
                    } else {
                        "A1"
                    };
                    return Err(format!(
                        "{rule} operation {g} ({op:?}) was acknowledged 200 although the key held another version when it took effect (truth now {t})"
                    ));
                }
                1
            }
            Resp::R409 => {
                let ok = match op {
                    Op::PutIm(tag) => poss >> tag & 1 == 1 && poss & !(1 << tag) != 0,
                    _ => {
                        poss & 1 != 0
                            || self.cfg.evictions > 0
                            || self
                                .cfg
                                .scripts
                                .iter()
                                .flatten()
                                .any(|o| matches!(o, Op::Delete | Op::OldDelete))
                    }
                };
                if !ok {
                    return Err(format!(
                        "A5 operation {g} ({op:?}) answered 409 but nothing changed the key while it ran (it held {poss:#b})"
                    ));
                }
                9
            }
            Resp::R412 => {
                if matches!(op, Op::PutCo(_)) && t == 0 {
                    return Err(format!(
                        "A5 operation {g} ({op:?}) answered 412 but the key was absent when the answer was decided (a delete completed first: S3 answers 409)"
                    ));
                }
                let ok = match op {
                    Op::PutIm(tag) => (1..8u8).any(|v| poss >> v & 1 == 1 && v != tag),
                    _ => poss >> 1 != 0,
                };
                if !ok {
                    return Err(format!(
                        "A5 operation {g} ({op:?}) answered 412 but the key never held a version that refuses it (it held {poss:#b})"
                    ));
                }
                2
            }
            Resp::R404 => {
                if poss & 1 == 0 {
                    return Err(format!(
                        "A5 operation {g} ({op:?}) answered 404 but the key was never absent while it ran"
                    ));
                }
                3
            }
            Resp::Fail503 | Resp::Stream503 | Resp::Wait503 => {
                if !s.lin.kill(g) {
                    return Err(format!(
                        "A24 operation {g} ({op:?}) answered a 503 that says it did nothing, but its bytes are the key's truth"
                    ));
                }
                match r {
                    Resp::Stream503 => 6,
                    Resp::Wait503 => 4,
                    _ => 8,
                }
            }
            Resp::Unk503 => 5,
            Resp::Read(v) => {
                if poss >> v & 1 == 0 {
                    return Err(format!(
                        "A2 a {op:?} returned version {v} but while it ran the key only held {:?} (a deleted or older version was served)",
                        (0..8).filter(|x| poss >> x & 1 == 1).collect::<Vec<u8>>()
                    ));
                }
                7
            }
        };
        if usize::from(g) < s.ans.len() {
            s.ans[usize::from(g)] = code;
        }
        Ok(())
    }

    pub(super) fn end_op(&self, s: &mut State, c: usize) {
        let cl = &mut s.cl[c];
        cl.next += 1;
        cl.pc = Pc::Idle;
        cl.fin = 0;
        cl.ln = 0;
        cl.lg = false;
        cl.rb = 0;
        cl.poss = 0;
        cl.bad = false;
    }

    /// A hot object was created: remember which create-only puts were in flight (evict_min_age).
    pub(super) fn hot_land(&self, s: &mut State, v: u8) {
        s.hot = v;
        if self.cfg.evictions > 0 {
            s.young = self.co_inflight(s);
        }
    }

    fn init_state(&self) -> State {
        let mut s = State {
            hot: 0,
            cold: 0,
            mk: None,
            young: 0,
            cl: [Cl::new(0), Cl::new(1), Cl::new(2), Cl::new(3), Cl::new(4)],
            ld: Ld::new(0, self.cfg.evictions),
            gl: None,
            era: 0,
            lin: Lin::new(0),
            ans: [0; 4],
            tainted: false,
            crashes: 0,
            nn: 0,
            stalled: false,
        };
        s.cl[3].alive = false;
        s.cl[4].alive = false;
        let dirty = |gone: bool, ver: u8| Mk {
            st: St::Dirty,
            gone,
            dirty: true,
            why: Why::Overwrite,
            owner: 0,
            ver,
        };
        match self.cfg.init {
            Init::Empty => {}
            Init::HotDirty => {
                s.hot = H0;
                s.mk = Some(dirty(false, 1));
            }
            Init::Lagging => {
                s.hot = H0;
                s.cold = C0;
                s.mk = Some(dirty(false, 1));
            }
            Init::Both => {
                s.hot = H0;
                s.cold = H0;
            }
            Init::ColdOnly => s.cold = H0,
            Init::DeletedCold => {
                s.cold = C0;
                s.mk = Some(dirty(true, 1));
            }
        }
        s.lin = Lin::new(s.truth());
        s.canon();
        s
    }

    pub(super) fn crashes_left(&self, s: &State) -> bool {
        s.crashes < self.cfg.max_crashes
    }

    /// No client is between the start and the end of an operation.
    pub(super) fn clients_quiet(&self, s: &State) -> bool {
        s.cl.iter().all(|c| !c.alive || c.pc == Pc::Idle)
    }

    fn finished(&self, c: &Cl) -> bool {
        !c.alive
            || (c.pc == Pc::Idle
                && usize::from(c.next) >= self.cfg.scripts[usize::from(c.id)].len())
    }
}

impl Model for Marks {
    type S = State;
    type A = Act;

    fn init(&self) -> Vec<State> {
        vec![self.init_state()]
    }

    fn successors(&self, s: &State, out: &mut Vec<Succ<State, Act>>) {
        for c in 0..NC + 2 {
            if s.cl[c].alive {
                self.client_steps(s, out, c);
            }
        }
        self.crash_steps(s, out);
        if !s.stalled {
            self.leader_steps(s, out);
        }
        self.ghost_leader_steps(s, out);
    }

    fn check(&self, s: &State) -> Option<String> {
        // A7: with no marker and nothing in flight, hot and cold agree (the crash window of an
        // object that landed before its marker is excluded: the sweep repairs it).
        let idle = self.clients_quiet(s) && s.ld.sw == Sw::Idle;
        if s.mk.is_none() && idle && !s.tainted && s.hot != 0 && s.cold != s.hot {
            return Some(format!(
                "A7 no marker and nothing in flight, but hot holds version {} and cold {}",
                s.hot, s.cold
            ));
        }
        None
    }

    fn terminal(&self, s: &State) -> Option<String> {
        if self.cfg.stall_leader && s.stalled {
            // A stalled (not dead) leader may leave CLEARING or a lock of its own; a writer
            // waiting on those is allowed, any other blocked client is a wait on the copier.
            let owned =
                matches!(s.mk, Some(m) if m.owner >= 8 && matches!(m.st, St::Lock | St::Clearing));
            for c in s.cl.iter().take(NC) {
                if !self.finished(c) && !owned {
                    return Some(format!(
                        "A23 client {} is blocked at {:?} behind a stalled copier that holds nothing",
                        c.id, c.pc
                    ));
                }
            }
            return None;
        }
        for c in s.cl.iter().take(NC) {
            if !self.finished(c) {
                return Some(format!(
                    "A11 client {} can never finish: stuck at {:?} with the marker {:?}",
                    c.id,
                    c.pc,
                    s.mk.map(|m| (m.st, m.owner))
                ));
            }
        }
        if self.cfg.clean() {
            for (j, a) in s.ans.iter().enumerate() {
                if *a == 4 {
                    return Some(format!(
                        "A6 operation {j} got a 503 caused by a lock where no lock holder died"
                    ));
                }
            }
        }
        None
    }

    fn quiescent(&self, s: &State) -> bool {
        s.cl.iter().take(NC).all(|c| self.finished(c))
            && s.cl.iter().skip(NC).all(|c| !c.alive)
            && s.gl.is_none()
            && !s.stalled
    }

    fn converged(&self, s: &State) -> bool {
        s.mk.is_none() && (s.hot == 0 || s.cold == s.hot)
    }

    fn show_act(&self, a: &Act) -> String {
        let who = match a.who {
            0..=2 => format!("proxy{}", a.who),
            3 | 4 => format!("ghost{}", a.who - 3),
            W_COP => "leader.copier".into(),
            W_EVICT => "leader.evictor".into(),
            W_SWEEP => "leader.sweep".into(),
            W_ENV => "env".into(),
            _ => "ghost-leader".into(),
        };
        format!("{who}: {}", a.what)
    }

    fn show_state(&self, s: &State) -> String {
        let mk = match s.mk {
            None => "none".to_string(),
            Some(m) => {
                let gone = if m.gone { " gone" } else { "" };
                let why = if m.st == St::Lock {
                    format!(" {:?}", m.why)
                } else {
                    String::new()
                };
                format!("{:?}{gone}{why} owner={}", m.st, m.owner)
            }
        };
        let mut v = format!("hot={} cold={} marker=[{mk}]", s.hot, s.cold);
        for c in s.cl.iter() {
            if c.alive && (c.pc != Pc::Idle || c.ghost) {
                v.push_str(&format!(
                    " | {}{}:{:?}",
                    if c.ghost { "ghost of proxy" } else { "proxy" },
                    c.id,
                    c.pc
                ));
            }
        }
        if s.ld.cop != Cop::Idle {
            v.push_str(&format!(" | copier:{:?}", s.ld.cop));
        }
        if s.ld.ev != Ev::Idle {
            v.push_str(&format!(" | evictor:{:?}", s.ld.ev));
        }
        v
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
