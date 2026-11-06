//! The proxies: every client operation of the document as a sequence of atomic steps.

use super::config::{Body, CORRUPT, Flaw, GhostKind, NC, Op};
use super::state::{Act, Cl, Marks, Mk, Resp, St, State, Why};
use crate::explore::Succ;

mod steps;

/// Where a proxy is in its operation. Section numbers are those of the design document.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Pc {
    Idle,
    /// 5.1 wave 1. `h`, `c`: 0 unread, 1 absent, else version + 1. `m`: 0 unread, 1 no marker,
    /// 2 marker, 3 marker saying gone. A plain put reads only hot (`c` and `m` start as 1).
    P1 {
        h: u8,
        c: u8,
        m: u8,
    },
    /// 5.1 wave 2: the object and the marker. `o`: 0 pending, 1 landed, 2 refused. `m`: the same.
    P2 {
        o: u8,
        m: u8,
    },
    /// 5.1 step 2: a create-only put about to answer 412 re-checks once: GET the marker (409
    /// for `LOCK(DELETE)`, 412 unless it says `gone`), then, for `gone`, HEAD hot (present: the
    /// key was recreated, 412; absent: deleted, 409).
    Rc,
    RcH,
    /// 5.1 table row 2: GET the marker after the object landed and the marker create said 412.
    Pm0,
    PmCas {
        a: u8,
    },
    PmHead {
        a: u8,
    },
    PmWait,
    PmWaitHead,
    /// 5.2: the hot DELETE timed out; the lock is kept until the call can no longer land.
    TWait,
    PmCreate,
    /// 5.2 and the lock steps of 5.3, 5.5, 5.6.
    LkCreate,
    LkGet,
    LkCas {
        a: u8,
        g: bool,
    },
    LkWait,
    /// 5.3 steps 3 and 4.
    OHead,
    ORepl {
        e: u8,
    },
    OCreate,
    /// 5.7 reads (also wave 1 of `If-Match`, 5.4b).
    G0,
    G1 {
        m: u8,
        c: u8,
    },
    Gw,
    /// A read or list found `gone`: HEAD hot once more (fix).
    Gc,
    /// 5.4b step 3: resolve under the lock.
    IrH,
    IrC,
    /// 5.6. `DHead` is the fix's HEAD of hot under the lock and `DGone` its CAS of `gone`.
    DHead,
    DGone {
        present: bool,
    },
    DDel,
    /// 5.5.
    ClC,
    ClW {
        c: u8,
    },
    ClH,
    ClD,
    /// 5.8.
    L1 {
        h: u8,
        m: u8,
    },
    L2 {
        h: u8,
        m: u8,
    },
    L3 {
        c: u8,
    },
    L4,
    /// The release of a lock after the acknowledgement. `k`: 0 `DIRTY(gone=false)`, 1
    /// `DIRTY(gone=true)`, 2 `CLEARING` then delete, 3 `DIRTY` keeping the lock's `gone`.
    Rel {
        k: u8,
    },
    RelDel,
    /// The previous release's entry-protocol operations (they ignore markers).
    OldP1 {
        h: u8,
        c: u8,
    },
    OldP2,
    OldG0,
    OldG1,
    OldD0,
    OldD1,
}

impl Pc {
    pub(super) fn versions(&mut self, f: &mut dyn FnMut(&mut u8)) {
        match self {
            Pc::PmCas { a } | Pc::PmHead { a } | Pc::LkCas { a, .. } => f(a),
            _ => {}
        }
    }

    /// The object of a put landed but its marker is not confirmed: the window of 7.1 row 3.
    fn landed_unmarked(&self) -> bool {
        matches!(self, Pc::P2 { o: 1, .. })
            || matches!(
                self,
                Pc::Pm0
                    | Pc::PmCas { .. }
                    | Pc::PmHead { .. }
                    | Pc::PmWait
                    | Pc::PmWaitHead
                    | Pc::PmCreate
            )
    }
}

/// How a write whose answer may be lost ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Out {
    Ok,
    Fail,
    UnkApplied,
    UnkNot,
}

/// A slot for a stalled proxy's pending call: the first of the two that holds none. A ghost is
/// always alive, so a slot that is not alive is free; at most two proxies stall in one run.
fn free_ghost_slot(s: &State) -> usize {
    (NC..NC + 2).find(|&k| !s.cl[k].alive).unwrap_or(NC)
}

pub(super) fn ghost_kind_of(a: &State, b: &State) -> Option<GhostKind> {
    if a.hot != b.hot {
        Some(if a.hot != 0 && b.hot == 0 {
            GhostKind::HotDelete
        } else if a.hot == 0 {
            GhostKind::HotCreate
        } else {
            GhostKind::HotReplace
        })
    } else if a.cold != b.cold {
        Some(GhostKind::ColdWrite)
    } else if a.mk != b.mk {
        Some(if b.mk.is_none() {
            GhostKind::MarkerDelete
        } else {
            GhostKind::MarkerWrite
        })
    } else {
        None
    }
}

/// The ghost kinds whose landing an assertion message reports.
pub(super) fn err_kinds_of(msg: &str) -> &'static [GhostKind] {
    if msg.starts_with("A8") {
        &[GhostKind::HotDelete, GhostKind::HotReplace]
    } else if msg.starts_with("A9") {
        &[GhostKind::MarkerDelete]
    } else {
        &[]
    }
}

impl Marks {
    pub(super) fn ghost_allows(&self, k: GhostKind) -> bool {
        match self.cfg.ghost {
            Some(GhostKind::Any) => true,
            Some(g) => g == k,
            None => false,
        }
    }

    pub(super) fn client_steps(&self, s: &State, out: &mut Vec<Succ<State, Act>>, c: usize) {
        if !s.cl[c].ghost {
            self.pc_steps(s, out, c);
            return;
        }
        // A ghost: a stalled proxy's pending call. It takes one step, and only a step that
        // changes a store is a landing (a read of a ghost is not observable).
        if self.cfg.timeouts {
            // The store drops a queued call (its request lifetime ends) without applying it.
            self.emit(
                s,
                out,
                Marks::act(c as u8, "the queued call is dropped by the store"),
                false,
                |ns| {
                    ns.cl[c].alive = false;
                    ns.cl[c].ghost = false;
                    ns.cl[c].pc = Pc::Idle;
                    Ok(())
                },
            );
        }
        let mut tmp = Vec::new();
        self.pc_steps(s, &mut tmp, c);
        for x in tmp {
            match x.to {
                Ok(mut ns) => {
                    let Some(k) = ghost_kind_of(s, &ns) else {
                        continue;
                    };
                    if !self.ghost_allows(k) {
                        continue;
                    }
                    ns.cl[c].alive = false;
                    ns.cl[c].ghost = false;
                    ns.cl[c].pc = Pc::Idle;
                    out.push(Succ {
                        act: x.act,
                        to: Ok(ns),
                        bg: false,
                    });
                }
                Err(m) => {
                    if err_kinds_of(&m).iter().any(|&k| self.ghost_allows(k)) {
                        out.push(Succ {
                            act: x.act,
                            to: Err(m),
                            bg: false,
                        });
                    }
                }
            }
        }
    }

    /// 5.6 step 2: the holder's hot DELETE times out. The proxy answers 503 (unknown outcome);
    /// the call stays queued at the store and may land later (a ghost of this proxy). With the
    /// correction the proxy keeps its lock until the call can no longer land.
    pub(super) fn timeout_step(&self, s: &State, out: &mut Vec<Succ<State, Act>>, c: usize) {
        let k = free_ghost_slot(s);
        self.emit(
            s,
            out,
            Marks::act(
                c as u8,
                "the hot DELETE times out: 503, the call stays queued",
            ),
            false,
            |ns| {
                let mut g = ns.cl[c].clone();
                g.ghost = true;
                g.alive = true;
                ns.cl[k] = g;
                self.ack(ns, c, Resp::Unk503)?;
                ns.cl[c].pc =
                    if self.cfg.timeout_keeps_lock && self.cfg.flaw != Flaw::ReleaseOnTimeout {
                        Pc::TWait
                    } else {
                        Pc::Rel { k: 1 }
                    };
                Ok(())
            },
        );
    }

    pub(super) fn crash_steps(&self, s: &State, out: &mut Vec<Succ<State, Act>>) {
        if !self.crashes_left(s) {
            return;
        }
        if self.cfg.crash_clients {
            for c in 0..NC {
                if !s.cl[c].alive || s.cl[c].pc == Pc::Idle {
                    continue;
                }
                self.emit(s, out, Marks::act(c as u8, "CRASH"), false, |ns| {
                    if ns.cl[c].pc.landed_unmarked() {
                        ns.tainted = true;
                    }
                    ns.cl[c].alive = false;
                    ns.crashes += 1;
                    if self.cfg.ghost.is_some() {
                        let k = free_ghost_slot(ns);
                        let mut g = ns.cl[c].clone();
                        g.ghost = true;
                        g.alive = true;
                        ns.cl[k] = g;
                    }
                    Ok(())
                });
            }
        }
        self.leader_crash_steps(s, out);
    }

    fn why(op: Op) -> Why {
        match op {
            Op::Delete => Why::Delete,
            Op::PutCold => Why::ColdLand,
            _ => Why::Overwrite,
        }
    }

    /// The version id this operation writes (a consumed stream writes bytes nobody sent).
    fn value(&self, cl: &Cl) -> u8 {
        if cl.bad {
            CORRUPT
        } else {
            self.cfg
                .value(self.cfg.gidx(usize::from(cl.id), usize::from(cl.next)))
        }
    }

    fn ack_end(&self, ns: &mut State, c: usize, r: Resp) -> Result<(), String> {
        self.ack(ns, c, r)?;
        self.end_op(ns, c);
        Ok(())
    }

    /// The release finished (or the lock was lost): deliver the pending answer, end the operation.
    fn finish(&self, ns: &mut State, c: usize) -> Result<(), String> {
        match ns.cl[c].fin {
            1 => self.ack(ns, c, Resp::R404)?,
            2 => self.ack(ns, c, Resp::R412)?,
            3 => self.ack(ns, c, Resp::Stream503)?,
            4 => self.ack(ns, c, Resp::R409)?,
            _ => {}
        }
        self.end_op(ns, c);
        Ok(())
    }

    fn holds(&self, ns: &State, c: usize) -> bool {
        let cl = &ns.cl[c];
        cl.ln != 0 && matches!(ns.mk, Some(m) if m.st == St::Lock && m.ver == cl.ln)
    }

    /// A8: a hot DELETE or a plain replace is made by the live holder of the key's lock.
    fn need_lock(&self, ns: &State, c: usize, what: &str) -> Result<(), String> {
        if self.cfg.a8 && !self.holds(ns, c) {
            return Err(format!("A8 {what} without a live lock on the key"));
        }
        Ok(())
    }

    /// The lock is ours: go on with the operation.
    fn locked(&self, ns: &mut State, c: usize) {
        let op = self.op_of(&ns.cl[c]);
        ns.cl[c].pc = match op {
            Op::Put(_) => Pc::OHead,
            Op::PutIm(t) => {
                if self.cfg.flaw == Flaw::IfMatchNoRecheck {
                    Pc::ORepl { e: t }
                } else {
                    Pc::IrH
                }
            }
            Op::Delete => {
                if self.cfg.delete_decides_gone {
                    Pc::DHead
                } else {
                    Pc::DDel
                }
            }
            _ => Pc::ClC,
        };
    }

    fn outcomes(&self, applicable: bool) -> Vec<Out> {
        let mut v = vec![if applicable { Out::Ok } else { Out::Fail }];
        if self.cfg.ambiguity {
            if applicable {
                v.push(Out::UnkApplied);
            }
            v.push(Out::UnkNot);
        }
        v
    }

    /// A lock create (`cas` None) or lock CAS on version `a` whose previous `gone` was `g`.
    fn lock_attempt(
        &self,
        s: &State,
        out: &mut Vec<Succ<State, Act>>,
        c: usize,
        cas: Option<(u8, bool)>,
    ) {
        let cl = &s.cl[c];
        let op = self.op_of(cl);
        let why = Marks::why(op);
        if why == Why::Delete
            && self.cfg.flaw == Flaw::DeleteWaitsForCopier
            && s.ld.cop != super::leader::Cop::Idle
        {
            return;
        }
        let applicable = match cas {
            None => s.mk.is_none(),
            Some((a, _)) => matches!(s.mk, Some(m) if m.ver == a),
        };
        let label = if cas.is_none() {
            "create the lock (create-only)"
        } else {
            "take the lock over by CAS"
        };
        for o in self.outcomes(applicable) {
            self.emit(s, out, Marks::act(c as u8, label), false, |ns| {
                let ver = ns.fresh();
                let prev = cas.is_some_and(|x| x.1);
                let gone = match why {
                    Why::Delete => {
                        (self.cfg.flaw != Flaw::DeleteGoneLate && !self.cfg.delete_decides_gone)
                            || prev
                    }
                    Why::ColdLand => false,
                    _ => prev,
                };
                let m = Mk {
                    st: St::Lock,
                    gone,
                    dirty: true,
                    why,
                    owner: ns.cl[c].id,
                    ver,
                };
                if matches!(o, Out::Ok | Out::UnkApplied) {
                    ns.mk_put(m)?;
                }
                match o {
                    Out::Ok => self.resume(ns, c, true, ver, gone),
                    Out::Fail => self.resume(ns, c, false, 0, false),
                    _ if self.cfg.flaw == Flaw::AssumeLockOnUnknown => {
                        self.resume(ns, c, true, ver, gone)
                    }
                    _ => {
                        ns.cl[c].rb = ver;
                        Ok(())
                    }
                }
            });
        }
    }

    /// The result of a lock write is known (answered or read back).
    fn resume(
        &self,
        ns: &mut State,
        c: usize,
        ok: bool,
        ver: u8,
        gone: bool,
    ) -> Result<(), String> {
        if ok {
            ns.cl[c].ln = ver;
            ns.cl[c].lg = gone;
            self.locked(ns, c);
        } else {
            ns.cl[c].pc = Pc::LkGet;
        }
        Ok(())
    }

    /// 5.2: poll a marker that is a lock or `CLEARING`; give up with 503 when its owner is dead.
    fn wait_steps(
        &self,
        s: &State,
        out: &mut Vec<Succ<State, Act>>,
        c: usize,
        free: Pc,
        landed: bool,
    ) {
        match s.mk {
            Some(m) if matches!(m.st, St::Lock | St::Clearing) => {
                if self.owner_dead(s, m.owner) && self.cfg.flaw != Flaw::WaitForever {
                    self.emit(
                        s,
                        out,
                        Marks::act(c as u8, "lock_wait expires: 503"),
                        false,
                        |ns| {
                            let r = if landed { Resp::Unk503 } else { Resp::Wait503 };
                            // 7.1 row 3: the object landed and the answer is "may not have landed".
                            ns.tainted |= landed;
                            self.ack_end(ns, c, r)
                        },
                    );
                }
            }
            _ => {
                self.emit(
                    s,
                    out,
                    Marks::act(c as u8, "poll: the marker left the lock"),
                    false,
                    |ns| {
                        ns.cl[c].pc = free;
                        Ok(())
                    },
                );
            }
        }
    }

    fn p1_next(&self, ns: &mut State, c: usize, (h, cc, m): (u8, u8, u8)) -> Result<(), String> {
        let op = self.op_of(&ns.cl[c]);
        let no_recheck = self.cfg.flaw == Flaw::CreateOnlyNoRecheck;
        if h > 1 {
            // Hot holds the key.
            if matches!(op, Op::PutCo(_)) {
                if !no_recheck {
                    ns.cl[c].pc = Pc::Rc;
                    return Ok(());
                }
                // The document before the re-check: wait for the marker read of the wave.
                if m == 0 {
                    return Ok(());
                }
                let r = if m == 4 { Resp::R409 } else { Resp::R412 };
                return self.ack_end(ns, c, r);
            }
            ns.cl[c].pc = if self.cfg.flaw == Flaw::OverwriteNoLock {
                Pc::OHead
            } else {
                Pc::LkCreate
            };
        } else if h == 1 && cc != 0 && m != 0 {
            let exists = cc > 1 && m != 3 && self.cfg.flaw != Flaw::CreateOnlyIgnoresCold;
            if matches!(op, Op::PutCo(_)) && exists {
                if !no_recheck {
                    ns.cl[c].pc = Pc::Rc;
                    return Ok(());
                }
                let r = if m == 4 { Resp::R409 } else { Resp::R412 };
                return self.ack_end(ns, c, r);
            }
            let marker = if self.cfg.flaw == Flaw::NoMarkerOnPut {
                1
            } else {
                0
            };
            ns.cl[c].pc = Pc::P2 { o: 0, m: marker };
            if marker == 1 {
                // Without a marker write the put has nothing left to wait for after its object.
            }
        }
        Ok(())
    }

    fn p2_next(&self, ns: &mut State, c: usize, (o, m): (u8, u8)) -> Result<(), String> {
        if o == 0 || m == 0 {
            return Ok(());
        }
        let op = self.op_of(&ns.cl[c]);
        match (o, m) {
            (1, 1) if self.cfg.puts_unbounded && self.cfg.put_reasserts_marker => {
                ns.cl[c].pc = Pc::Pm0;
                Ok(())
            }
            (1, 1) => self.ack_end(ns, c, Resp::Ok),
            (1, _) => {
                ns.cl[c].pc = Pc::Pm0;
                Ok(())
            }
            _ => match op {
                Op::PutCo(_) => {
                    // The store refused the object: the key was there. Re-check before the 412.
                    ns.cl[c].pc = Pc::Rc;
                    Ok(())
                }
                Op::Put(Body::Replay) => {
                    ns.cl[c].pc = Pc::LkCreate;
                    Ok(())
                }
                _ => {
                    if self.cfg.flaw == Flaw::StreamRaceContinues {
                        ns.cl[c].bad = true;
                        ns.cl[c].pc = Pc::LkCreate;
                        Ok(())
                    } else {
                        self.ack_end(ns, c, Resp::Stream503)
                    }
                }
            },
        }
    }

    fn finish_read(
        &self,
        ns: &mut State,
        c: usize,
        r: u8,
        from_cold_unmarked: bool,
    ) -> Result<(), String> {
        match self.op_of(&ns.cl[c]) {
            Op::PutIm(t) => {
                if r == 0 {
                    self.ack_end(ns, c, Resp::R404)
                } else if r != t {
                    self.ack_end(ns, c, Resp::R412)
                } else {
                    ns.cl[c].pc = Pc::LkCreate;
                    Ok(())
                }
            }
            _ => {
                self.ack(ns, c, Resp::Read(r))?;
                if from_cold_unmarked {
                    ns.cl[c].pc = Pc::Gw;
                } else {
                    self.end_op(ns, c);
                }
                Ok(())
            }
        }
    }

    fn oldp1_next(&self, ns: &mut State, c: usize, (h, cc): (u8, u8)) -> Result<(), String> {
        if h > 1 || cc > 1 {
            return self.ack_end(ns, c, Resp::R412);
        }
        if h == 1 && cc == 1 {
            ns.cl[c].pc = Pc::OldP2;
        }
        Ok(())
    }

    fn l1_next(&self, ns: &mut State, c: usize, (h, m): (u8, u8)) -> Result<(), String> {
        if h != 0 && m != 0 {
            ns.cl[c].pc = Pc::L2 { h, m };
        }
        Ok(())
    }

    fn g1_next(&self, ns: &mut State, c: usize, (m, cc): (u8, u8)) -> Result<(), String> {
        if m == 0 || cc == 0 {
            return Ok(());
        }
        if m == 3 && self.cfg.gone_rechecks_hot {
            ns.cl[c].pc = Pc::Gc;
            return Ok(());
        }
        let r = if m == 3 { 0 } else { cc - 1 };
        self.finish_read(ns, c, r, r != 0 && m == 1)
    }

    /// The replace (or create) under the lock landed: acknowledge, then release.
    fn after_replace(&self, ns: &mut State, c: usize) -> Result<(), String> {
        self.ack(ns, c, Resp::Ok)?;
        ns.cl[c].pc = if self.cfg.flaw == Flaw::OverwriteNoLock {
            Pc::Pm0
        } else {
            Pc::Rel { k: 0 }
        };
        Ok(())
    }

    /// The operation changes nothing: release the lock keeping its `gone`, then answer
    /// (`fin`: 1 is 404, 2 is 412, 3 is the 503 of a stream).
    fn fail_release(&self, ns: &mut State, c: usize, fin: u8) -> Result<(), String> {
        ns.cl[c].fin = fin;
        ns.cl[c].pc = Pc::Rel { k: 3 };
        Ok(())
    }
}
