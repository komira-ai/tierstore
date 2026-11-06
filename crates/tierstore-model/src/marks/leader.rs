//! The leader: the copier (with the reaper and the copy-back), the evictor and the stray sweep,
//! and the leader's crash and takeover.

use super::client::Pc;
use super::config::{Flaw, NC, Op};
use super::state::{Act, Marks, Mk, St, State, W_COP, W_EVICT, W_GHOST_LD, W_SWEEP, Why};
use crate::explore::Succ;

/// The copier's program counter. `a` is the version of the marker it read.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cop {
    Idle,
    /// Hot HEAD for a `DIRTY` marker; `g` is its `gone`.
    Head {
        a: u8,
        g: bool,
    },
    Cold {
        a: u8,
        h: u8,
        g: bool,
    },
    /// Fix: GET the marker again after the cold HEAD.
    Chk {
        a: u8,
        c: u8,
        g: bool,
    },
    Get {
        a: u8,
        c: u8,
        g: bool,
    },
    Put {
        a: u8,
        c: u8,
        h: u8,
        g: bool,
    },
    Clear {
        a: u8,
    },
    Del,
    GDel {
        a: u8,
    },
    Hold {
        a: u8,
    },
    /// Fix: the hold after a copy over the cold object of a deleted key.
    Hold2 {
        a: u8,
    },
    /// Fix: hot HEAD after the hold.
    HeadH {
        a: u8,
    },
    HeadC {
        a: u8,
    },
    Orph {
        a: u8,
    },
    /// Reap the lock of a departed owner to `k`: 0 `DIRTY(gone=false)`, 1 `DIRTY(gone=true)`, 2
    /// `CLEARING`, 3 `DIRTY` keeping the lock's `gone`.
    Reap {
        a: u8,
        k: u8,
    },
    Fin,
    CbLock {
        a: u8,
    },
    CbHead {
        n: u8,
    },
    CbRead {
        n: u8,
    },
    CbPut {
        n: u8,
        c: u8,
    },
    CbRel {
        n: u8,
        k: u8,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Ev {
    Idle,
    Lock,
    Head { n: u8 },
    Cold { n: u8, h: u8 },
    Del { n: u8 },
    Rel { n: u8, k: u8 },
    RelDel,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sw {
    Idle,
    Create,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Ld {
    pub era: u8,
    pub cop: Cop,
    pub ev: Ev,
    pub sw: Sw,
    /// Evictions the evictor may still start.
    pub left: u8,
}

impl Ld {
    pub fn new(era: u8, left: u8) -> Ld {
        Ld {
            era,
            cop: Cop::Idle,
            ev: Ev::Idle,
            sw: Sw::Idle,
            left,
        }
    }

    pub(super) fn versions(&mut self, f: &mut dyn FnMut(&mut u8)) {
        match &mut self.cop {
            Cop::Head { a, .. }
            | Cop::Cold { a, .. }
            | Cop::Chk { a, .. }
            | Cop::Get { a, .. }
            | Cop::Put { a, .. }
            | Cop::Clear { a }
            | Cop::GDel { a }
            | Cop::Hold { a }
            | Cop::HeadH { a }
            | Cop::Hold2 { a }
            | Cop::HeadC { a }
            | Cop::Orph { a }
            | Cop::Reap { a, .. }
            | Cop::CbLock { a } => f(a),
            Cop::CbHead { n } | Cop::CbRead { n } | Cop::CbPut { n, .. } | Cop::CbRel { n, .. } => {
                f(n)
            }
            Cop::Idle | Cop::Del | Cop::Fin => {}
        }
        match &mut self.ev {
            Ev::Head { n } | Ev::Cold { n, .. } | Ev::Del { n } | Ev::Rel { n, .. } => f(n),
            Ev::Idle | Ev::Lock | Ev::RelDel => {}
        }
    }

    fn owner(&self) -> u8 {
        8 + self.era
    }
}

impl Marks {
    /// A request is between its first and its last read of a parallel pair or triple (a hot-miss
    /// read, wave 1 of a create-only put): the proxy discards one slower than `read_window`.
    fn pair_in_flight(&self, s: &State) -> bool {
        s.cl.iter().take(NC).any(|c| {
            c.alive
                && match c.pc {
                    Pc::G1 { m, c: cc } => m != 0 || cc != 0,
                    // The cold page was read, the marker of the cold-only key is read next.
                    Pc::L3 { .. } => true,
                    Pc::P1 { h, c: cc, m } => {
                        matches!(self.op_of(c), Op::PutCo(_)) && (h != 0 || cc != 0 || m != 0)
                    }
                    _ => false,
                }
        })
    }

    /// What the copier does once cold holds what hot holds: clear at once, or, for a key whose
    /// marker says `gone` (cold held a deleted object), hold first (fix).
    fn after_copy(&self, a: u8, g: bool) -> Cop {
        if g && self.cfg.recreate_hold {
            Cop::Hold2 { a }
        } else {
            Cop::Clear { a }
        }
    }

    /// A put has a marker or not but its object write has not been answered: it may still land.
    pub(super) fn put_object_pending(&self, s: &State) -> bool {
        s.cl.iter()
            .take(NC)
            .any(|c| c.alive && matches!(c.pc, Pc::P2 { o: 0, .. }))
    }

    fn put_marker_landed_object_pending(&self, s: &State) -> bool {
        s.cl.iter()
            .take(NC)
            .any(|c| c.alive && matches!(c.pc, Pc::P2 { o: 0, m: 1 }))
    }

    pub(super) fn leader_crash_steps(&self, s: &State, out: &mut Vec<Succ<State, Act>>) {
        if self.cfg.crash_leader {
            self.emit(
                s,
                out,
                Marks::act(
                    super::state::W_ENV,
                    "the leader dies; a successor takes over",
                ),
                false,
                |ns| {
                    ns.crashes += 1;
                    ns.era += 1;
                    let fresh = Ld::new(ns.era, ns.ld.left);
                    let old = std::mem::replace(&mut ns.ld, fresh);
                    if self.cfg.ghost.is_some() && ns.gl.is_none() {
                        ns.gl = Some(old);
                    }
                    Ok(())
                },
            );
        }
    }

    pub(super) fn ghost_leader_steps(&self, s: &State, out: &mut Vec<Succ<State, Act>>) {
        let Some(g) = s.gl.as_ref() else { return };
        let mut tmp_state = s.clone();
        tmp_state.ld = g.clone();
        tmp_state.gl = None;
        let mut tmp = Vec::new();
        self.copier_steps(&tmp_state, &mut tmp);
        self.evictor_steps(&tmp_state, &mut tmp, true);
        for x in tmp {
            let act = Act {
                who: W_GHOST_LD,
                what: x.act.what,
            };
            match x.to {
                Ok(mut ns) => {
                    let changed = ns.hot != s.hot || ns.cold != s.cold || ns.mk != s.mk;
                    if !changed {
                        continue;
                    }
                    let kind = super::client::ghost_kind_of(s, &ns);
                    if !kind.is_some_and(|k| self.ghost_allows(k)) {
                        continue;
                    }
                    ns.ld = s.ld.clone();
                    ns.gl = None;
                    out.push(Succ {
                        act,
                        to: Ok(ns),
                        bg: false,
                    });
                }
                Err(m) => {
                    if super::client::err_kinds_of(&m)
                        .iter()
                        .any(|&k| self.ghost_allows(k))
                    {
                        out.push(Succ {
                            act,
                            to: Err(m),
                            bg: false,
                        });
                    }
                }
            }
        }
    }

    pub(super) fn leader_steps(&self, s: &State, out: &mut Vec<Succ<State, Act>>) {
        if self.cfg.copier {
            self.copier_steps(s, out);
        }
        if self.cfg.evictions > 0 || s.ld.ev != Ev::Idle {
            self.evictor_steps(s, out, false);
        }
        if self.cfg.sweep {
            self.sweep_steps(s, out);
        }
        if self.cfg.stall_leader
            && s.ld.cop != Cop::Idle
            && matches!(
                s.ld.cop,
                Cop::Head { .. } | Cop::Cold { .. } | Cop::Get { .. } | Cop::Put { .. }
            )
        {
            self.emit(
                s,
                out,
                Marks::act(
                    super::state::W_ENV,
                    "the leader stalls (alive, but no more steps)",
                ),
                false,
                |ns| {
                    ns.stalled = true;
                    Ok(())
                },
            );
        }
    }

    fn mk(&self, st: St, gone: bool, dirty: bool, why: Why, owner: u8, ver: u8) -> Mk {
        Mk {
            st,
            gone,
            dirty,
            why,
            owner,
            ver,
        }
    }

    fn copier_steps(&self, s: &State, out: &mut Vec<Succ<State, Act>>) {
        let w = W_COP;
        let me = s.ld.owner();
        let ver_is = |a: u8| matches!(s.mk, Some(m) if m.ver == a);
        match s.ld.cop {
            Cop::Idle => {
                let Some(m) = s.mk else { return };
                let next = match m.st {
                    St::Dirty => Some(Cop::Head { a: m.ver, g: m.gone }),
                    St::Want => Some(Cop::CbLock { a: m.ver }),
                    St::Lock if self.owner_dead(s, m.owner) && self.cfg.flaw != Flaw::NoReap => {
                        let k = match m.why {
                            Why::Overwrite | Why::ColdLand => {
                                if self.cfg.reap_keeps_gone { 3 } else { 0 }
                            }
                            Why::Delete => 1,
                            Why::Evict | Why::CopyBack => 2,
                        };
                        Some(Cop::Reap { a: m.ver, k })
                    }
                    St::Clearing if self.owner_dead(s, m.owner) && self.cfg.flaw != Flaw::NoReap => Some(Cop::Fin),
                    _ => None,
                };
                if let Some(n) = next {
                    self.emit(s, out, Marks::act(w, "scan: GET the marker and pick it up"), true, |ns| {
                        ns.ld.cop = n;
                        Ok(())
                    });
                }
            }
            Cop::Head { a, g } => self.emit(s, out, Marks::act(w, "hot HEAD"), true, |ns| {
                ns.ld.cop = if ns.hot != 0 {
                    Cop::Cold { a, h: ns.hot, g }
                } else if g {
                    Cop::GDel { a }
                } else if self.cfg.flaw != Flaw::NoOrphanGuard && self.put_object_pending(ns) {
                    // orphan_after has not passed: a put whose object is still on its way may
                    // land; the marker is seen again on a later pass.
                    Cop::Idle
                } else {
                    Cop::Orph { a }
                };
                Ok(())
            }),
            Cop::Cold { a, h, g } => self.emit(s, out, Marks::act(w, "cold HEAD"), true, |ns| {
                ns.ld.cop = if ns.cold == h {
                    self.after_copy(a, g)
                } else if self.cfg.copier_rechecks_marker {
                    Cop::Chk { a, c: ns.cold, g }
                } else {
                    Cop::Get { a, c: ns.cold, g }
                };
                Ok(())
            }),
            Cop::Chk { a, c, g } => self.emit(s, out, Marks::act(w, "GET the marker again: unchanged since the scan?"), true, |ns| {
                ns.ld.cop = if ver_is(a) { Cop::Get { a, c, g } } else { Cop::Idle };
                Ok(())
            }),
            Cop::Get { a, c, g } => self.emit(s, out, Marks::act(w, "GET hot (stream)"), true, |ns| {
                ns.ld.cop = if ns.hot == 0 { Cop::Idle } else { Cop::Put { a, c, h: ns.hot, g } };
                Ok(())
            }),
            Cop::Put { a, c, h, g } => self.emit(s, out, Marks::act(w, "write cold (create-only, or If-Match the cold tag)"), true, |ns| {
                if ns.cold == c || self.cfg.flaw == Flaw::CopierPlainColdPut {
                    ns.cold = h;
                    if self.cfg.evict_age_from_copy && self.cfg.evictions > 0 {
                        ns.young |= self.co_inflight(ns);
                    }
                    ns.ld.cop = self.after_copy(a, g);
                } else {
                    ns.ld.cop = Cop::Idle;
                }
                Ok(())
            }),
            Cop::Clear { a } => self.emit(s, out, Marks::act(w, "CAS the marker to CLEARING on the version read before the copy"), true, |ns| {
                if self.cfg.flaw == Flaw::ClearWithoutCas {
                    ns.ld.cop = Cop::Del;
                } else if let Some(m) = ns.mk.filter(|m| m.ver == a) {
                    let ver = ns.fresh();
                    ns.mk_put(Mk { st: St::Clearing, gone: m.gone, dirty: m.dirty, why: m.why, owner: me, ver })?;
                    ns.ld.cop = Cop::Del;
                } else {
                    ns.ld.cop = Cop::Idle;
                }
                Ok(())
            }),
            Cop::Del => self.emit(s, out, Marks::act(w, "delete the marker (plain DELETE)"), true, |ns| {
                match ns.mk {
                    Some(m) if m.st == St::Clearing && m.owner == me => ns.mk = None,
                    Some(m) => {
                        return Err(format!(
                            "A9 the copier deleted a marker that is {:?} owned by {}, not its own CLEARING",
                            m.st, m.owner
                        ));
                    }
                    None => {}
                }
                ns.ld.cop = Cop::Idle;
                Ok(())
            }),
            Cop::GDel { a } => self.emit(s, out, Marks::act(w, "cold DELETE (the key is gone)"), true, |ns| {
                ns.cold = 0;
                ns.ld.cop = Cop::Hold { a };
                Ok(())
            }),
            Cop::Hold { a } => {
                // The tombstone hold: it passes only when no read pair that began before the
                // cold delete is still open (the proxy discards a pair slower than read_window).
                // It also outlasts every put in flight when the cold delete was sent: a put whose
                // marker landed but whose object has not must have finished (or died) by now.
                let reads_done = self.cfg.flaw == Flaw::NoHold || !self.pair_in_flight(s);
                if reads_done && !self.put_marker_landed_object_pending(s) {
                    self.emit(s, out, Marks::act(w, "after tombstone_hold: GET the marker again"), true, |ns| {
                        ns.ld.cop = if !ver_is(a) {
                            Cop::Idle
                        } else if self.cfg.hold_rechecks_hot {
                            Cop::HeadH { a }
                        } else {
                            Cop::HeadC { a }
                        };
                        Ok(())
                    });
                }
            }
            Cop::Hold2 { a } => {
                if self.cfg.flaw == Flaw::NoHold || !self.pair_in_flight(s) {
                    self.emit(s, out, Marks::act(w, "after tombstone_hold: GET the marker again (cold was overwritten)"), true, |ns| {
                        ns.ld.cop = if ver_is(a) { Cop::Clear { a } } else { Cop::Idle };
                        Ok(())
                    });
                }
            }
            Cop::HeadH { a } => self.emit(s, out, Marks::act(w, "after the hold: hot HEAD again"), true, |ns| {
                ns.ld.cop = if ns.hot != 0 { Cop::Cold { a, h: ns.hot, g: true } } else { Cop::HeadC { a } };
                Ok(())
            }),
            Cop::HeadC { a } => self.emit(s, out, Marks::act(w, "cold HEAD after the hold"), true, |ns| {
                ns.ld.cop = if ns.cold == 0 { Cop::Clear { a } } else { Cop::GDel { a } };
                Ok(())
            }),
            Cop::Orph { a } => self.emit(s, out, Marks::act(w, "orphan_after passed: CAS the marker to CLEARING"), true, |ns| {
                if let Some(m) = ns.mk.filter(|m| m.ver == a) {
                    let ver = ns.fresh();
                    ns.mk_put(Mk { st: St::Clearing, gone: m.gone, dirty: m.dirty, why: m.why, owner: me, ver })?;
                    ns.ld.cop = Cop::Del;
                } else {
                    ns.ld.cop = Cop::Idle;
                }
                Ok(())
            }),
            Cop::Reap { a, k } => self.emit(s, out, Marks::act(w, "reap: CAS the dead owner's lock"), true, |ns| {
                if let Some(m) = ns.mk.filter(|m| m.ver == a) {
                    let ver = ns.fresh();
                    let (st, gone) = match k {
                        0 => (St::Dirty, false),
                        1 => (St::Dirty, true),
                        2 => (St::Clearing, m.gone),
                        _ => (St::Dirty, m.gone),
                    };
                    ns.mk_put(Mk { st, gone, dirty: st == St::Dirty, why: m.why, owner: me, ver })?;
                    if st == St::Clearing {
                        ns.ld.cop = Cop::Del;
                        return Ok(());
                    }
                }
                ns.ld.cop = Cop::Idle;
                Ok(())
            }),
            Cop::Fin => self.emit(s, out, Marks::act(w, "finish a dead owner's CLEARING: delete the marker"), true, |ns| {
                if matches!(ns.mk, Some(m) if m.st == St::Clearing) {
                    ns.mk = None;
                }
                ns.ld.cop = Cop::Idle;
                Ok(())
            }),
            Cop::CbLock { a } => self.emit(s, out, Marks::act(w, "copy-back: CAS WANT to LOCK(COPYBACK)"), true, |ns| {
                if self.cfg.flaw == Flaw::CopyBackNoLock {
                    ns.ld.cop = Cop::CbHead { n: 0 };
                } else if ns.mk.is_some_and(|m| m.ver == a) {
                    let ver = ns.fresh();
                    ns.mk_put(self.mk(St::Lock, false, false, Why::CopyBack, me, ver))?;
                    ns.ld.cop = Cop::CbHead { n: ver };
                } else {
                    ns.ld.cop = Cop::Idle;
                }
                Ok(())
            }),
            Cop::CbHead { n } => self.emit(s, out, Marks::act(w, "copy-back: hot HEAD"), true, |ns| {
                ns.ld.cop = if ns.hot != 0 { Cop::CbRel { n, k: 2 } } else { Cop::CbRead { n } };
                Ok(())
            }),
            Cop::CbRead { n } => self.emit(s, out, Marks::act(w, "copy-back: read cold"), true, |ns| {
                ns.ld.cop = if ns.cold == 0 { Cop::CbRel { n, k: 2 } } else { Cop::CbPut { n, c: ns.cold } };
                Ok(())
            }),
            Cop::CbPut { n, c } => self.emit(s, out, Marks::act(w, "copy-back: put hot (create-only)"), true, |ns| {
                if ns.hot == 0 || self.cfg.flaw == Flaw::CopyBackPlainPut {
                    self.hot_land(ns, c);
                    ns.ld.cop = Cop::CbRel { n, k: 2 };
                } else {
                    ns.ld.cop = Cop::CbRel { n, k: 0 };
                }
                Ok(())
            }),
            Cop::CbRel { n, k } => self.emit(s, out, Marks::act(w, "copy-back: release the lock (CAS)"), true, |ns| {
                if ns.mk.is_some_and(|m| m.ver == n) {
                    let ver = ns.fresh();
                    if k == 2 {
                        ns.mk_put(self.mk(St::Clearing, false, false, Why::CopyBack, me, ver))?;
                        ns.ld.cop = Cop::Del;
                    } else {
                        ns.mk_put(self.mk(St::Dirty, false, true, Why::CopyBack, me, ver))?;
                        ns.ld.cop = Cop::Idle;
                    }
                } else {
                    ns.ld.cop = Cop::Idle;
                }
                Ok(())
            }),
        }
    }

    fn evictor_steps(&self, s: &State, out: &mut Vec<Succ<State, Act>>, _ghost: bool) {
        let w = W_EVICT;
        let me = s.ld.owner();
        match s.ld.ev {
            Ev::Idle => {
                if s.ld.left > 0 && s.hot != 0 {
                    self.emit(s, out, Marks::act(w, "usage is over high_water: start evicting this key"), false, |ns| {
                        ns.ld.left -= 1;
                        ns.ld.ev = if self.cfg.flaw == Flaw::EvictNoLock { Ev::Head { n: 0 } } else { Ev::Lock };
                        Ok(())
                    });
                }
            }
            Ev::Lock => self.emit(s, out, Marks::act(w, "create the lock (create-only)"), true, |ns| {
                if ns.mk.is_none() {
                    let ver = ns.fresh();
                    ns.mk_put(self.mk(St::Lock, false, false, Why::Evict, me, ver))?;
                    ns.ld.ev = Ev::Head { n: ver };
                } else {
                    ns.ld.ev = Ev::Idle;
                }
                Ok(())
            }),
            Ev::Head { n } => self.emit(s, out, Marks::act(w, "hot HEAD"), true, |ns| {
                ns.ld.ev = Ev::Cold { n, h: ns.hot };
                Ok(())
            }),
            Ev::Cold { n, h } => self.emit(s, out, Marks::act(w, "cold HEAD and compare"), true, |ns| {
                ns.ld.ev = if h == 0 {
                    Ev::Rel { n, k: 2 }
                } else if ns.cold == h {
                    Ev::Del { n }
                } else {
                    Ev::Rel { n, k: 0 }
                };
                Ok(())
            }),
            // evict_min_age: the object (and its cold copy) are old enough that no request that
            // started before them is still running.
            Ev::Del { .. }
                if self.cfg.flaw != Flaw::NoEvictMinAge
                    && (s.young != 0 || self.put_object_pending(s)) => {}
            Ev::Del { n } => self.emit(s, out, Marks::act(w, "hot DELETE (plain) under the lock"), true, |ns| {
                if self.cfg.a8 && !matches!(ns.mk, Some(m) if m.ver == n && m.owner == me && m.st == St::Lock) {
                    return Err("A8 hot DELETE by the evictor without a live lock on the key".into());
                }
                ns.hot = 0;
                ns.ld.ev = if n == 0 { Ev::Idle } else { Ev::Rel { n, k: 2 } };
                Ok(())
            }),
            Ev::Rel { n, k } => self.emit(s, out, Marks::act(w, "release the lock (CAS)"), true, |ns| {
                if ns.mk.is_some_and(|m| m.ver == n) {
                    let ver = ns.fresh();
                    if k == 2 {
                        ns.mk_put(self.mk(St::Clearing, false, false, Why::Evict, me, ver))?;
                        ns.ld.ev = Ev::RelDel;
                    } else {
                        ns.mk_put(self.mk(St::Dirty, false, true, Why::Evict, me, ver))?;
                        ns.ld.ev = Ev::Idle;
                    }
                } else {
                    ns.ld.ev = Ev::Idle;
                }
                Ok(())
            }),
            Ev::RelDel => self.emit(s, out, Marks::act(w, "delete the marker (plain DELETE)"), true, |ns| {
                match ns.mk {
                    Some(m) if m.st == St::Clearing && m.owner == me => ns.mk = None,
                    Some(m) => {
                        return Err(format!(
                            "A9 the evictor deleted a marker that is {:?} owned by {}, not its own CLEARING",
                            m.st, m.owner
                        ));
                    }
                    None => {}
                }
                ns.ld.ev = Ev::Idle;
                Ok(())
            }),
        }
    }

    fn sweep_steps(&self, s: &State, out: &mut Vec<Succ<State, Act>>) {
        let w = W_SWEEP;
        match s.ld.sw {
            Sw::Idle => {
                // stray_grace: the sweep looks only at objects no operation is writing.
                if self.clients_quiet(s) && s.mk.is_none() && s.hot != 0 && s.cold != s.hot {
                    self.emit(s, out, Marks::act(w, "merge-join finds a hot object with no marker and a cold that is not a copy"), true, |ns| {
                        ns.ld.sw = Sw::Create;
                        Ok(())
                    });
                }
            }
            Sw::Create => self.emit(
                s,
                out,
                Marks::act(w, "create DIRTY(gone=false) (create-only)"),
                true,
                |ns| {
                    if ns.mk.is_none() {
                        let ver = ns.fresh();
                        ns.mk_put(self.mk(
                            St::Dirty,
                            false,
                            true,
                            Why::Overwrite,
                            s.ld.owner(),
                            ver,
                        ))?;
                    }
                    ns.ld.sw = Sw::Idle;
                    Ok(())
                },
            ),
        }
    }
}
