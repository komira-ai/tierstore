//! The step function of a proxy: one arm per program counter.

use super::Pc;
use crate::explore::Succ;
use crate::marks::config::{Body, Flaw, Op, Phase};
use crate::marks::state::{Act, Marks, Mk, Resp, St, State, Why};

impl Marks {
    pub(super) fn pc_steps(&self, s: &State, out: &mut Vec<Succ<State, Act>>, c: usize) {
        let cl = &s.cl[c];
        let w = c as u8;
        let id = usize::from(cl.id);
        if cl.rb != 0 {
            self.emit(
                s,
                out,
                Marks::act(w, "read the marker back for the version written"),
                false,
                |ns| {
                    let rb = ns.cl[c].rb;
                    let (ok, g) = match ns.mk {
                        Some(m) => (m.ver == rb, m.gone),
                        None => (false, false),
                    };
                    ns.cl[c].rb = 0;
                    self.resume(ns, c, ok, rb, g)
                },
            );
            return;
        }
        match cl.pc {
            Pc::Idle => {
                if usize::from(cl.next) < self.cfg.scripts[id].len() {
                    self.emit(s, out, Marks::act(w, "the request arrives"), false, |ns| {
                        let op = self.op_of(&ns.cl[c]);
                        let g = self.cfg.gidx(id, usize::from(ns.cl[c].next));
                        if self.effs[usize::from(g)].is_some() {
                            ns.lin.invoke(g);
                        }
                        ns.cl[c].poss = 1 << ns.truth();
                        let old_closed = self.cfg.phase == Phase::ActiveMarks;
                        ns.cl[c].pc = match op {
                            Op::Put(_) => Pc::P1 { h: 0, c: 1, m: 1 },
                            Op::PutCo(_) => Pc::P1 { h: 0, c: 0, m: 0 },
                            Op::PutIm(_) | Op::Get => Pc::G0,
                            Op::PutCold => {
                                if self.cfg.flaw == Flaw::ColdLandNoLock {
                                    Pc::ClC
                                } else {
                                    Pc::LkCreate
                                }
                            }
                            Op::Delete => Pc::LkCreate,
                            Op::List => Pc::L1 { h: 0, m: 0 },
                            Op::OldPut | Op::OldDelete if old_closed => {
                                return self.ack_end(ns, c, Resp::Fail503);
                            }
                            Op::OldPut => Pc::OldP1 { h: 0, c: 0 },
                            Op::OldDelete => Pc::OldD0,
                            Op::OldGet => Pc::OldG0,
                        };
                        Ok(())
                    });
                }
            }
            Pc::P1 { h, c: cc, m } => {
                if h == 0 {
                    self.emit(s, out, Marks::act(w, "wave 1: hot HEAD"), false, |ns| {
                        let hv = ns.hot + 1;
                        ns.cl[c].pc = Pc::P1 { h: hv, c: cc, m };
                        self.p1_next(ns, c)
                    });
                }
                if cc == 0 {
                    self.emit(s, out, Marks::act(w, "wave 1: cold HEAD"), false, |ns| {
                        let v = ns.cold + 1;
                        ns.cl[c].pc = Pc::P1 { h, c: v, m };
                        self.p1_next(ns, c)
                    });
                }
                if m == 0 {
                    self.emit(s, out, Marks::act(w, "wave 1: GET the marker"), false, |ns| {
                        let v = match ns.mk {
                            None => 1,
                            Some(k)
                                if self.cfg.flaw == Flaw::CreateOnlyNoRecheck
                                    && k.st == St::Lock
                                    && k.why == Why::Delete =>
                            {
                                4
                            }
                            Some(k) if k.gone => 3,
                            Some(_) => 2,
                        };
                        ns.cl[c].pc = Pc::P1 { h, c: cc, m: v };
                        self.p1_next(ns, c)
                    });
                }
            }
            Pc::Rc => self.emit(s, out, Marks::act(w, "re-check before the 412: GET the marker"), false, |ns| {
                match ns.mk {
                    Some(k) if k.st == St::Lock && k.why == Why::Delete => self.ack_end(ns, c, Resp::R409),
                    Some(k) if k.gone => {
                        ns.cl[c].pc = Pc::RcH;
                        Ok(())
                    }
                    _ => self.ack_end(ns, c, Resp::R412),
                }
            }),
            Pc::RcH => self.emit(s, out, Marks::act(w, "re-check: the marker says gone, HEAD hot"), false, |ns| {
                let r = if ns.hot != 0 { Resp::R412 } else { Resp::R409 };
                self.ack_end(ns, c, r)
            }),
            Pc::P2 { o, m } => {
                if o == 0 {
                    self.emit(s, out, Marks::act(w, "wave 2: create the object (create-only)"), false, |ns| {
                        let v = self.value(&ns.cl[c]);
                        let r = if ns.hot == 0 {
                            self.hot_land(ns, v);
                            1
                        } else {
                            2
                        };
                        ns.cl[c].pc = Pc::P2 { o: r, m };
                        self.p2_next(ns, c)
                    });
                }
                if m == 0 {
                    self.emit(s, out, Marks::act(w, "wave 2: create the marker (create-only)"), false, |ns| {
                        let ver = ns.fresh();
                        let r = if ns.mk.is_none() {
                            ns.mk = Some(Mk { st: St::Dirty, gone: false, dirty: true, why: Why::Overwrite, owner: ns.cl[c].id, ver });
                            1
                        } else if self.cfg.flaw == Flaw::GoneClearedFirst && matches!(ns.mk, Some(k) if k.st == St::Dirty) {
                            // Bump the marker to gone=false before the object has landed.
                            let id = ns.cl[c].id;
                            ns.mk_put(Mk { st: St::Dirty, gone: false, dirty: true, why: Why::Overwrite, owner: id, ver })?;
                            1
                        } else {
                            2
                        };
                        ns.cl[c].pc = Pc::P2 { o, m: r };
                        self.p2_next(ns, c)
                    });
                }
            }
            Pc::Pm0 => self.emit(s, out, Marks::act(w, "GET the marker"), false, |ns| {
                ns.cl[c].pc = match ns.mk {
                    None => Pc::PmCreate,
                    Some(m) if matches!(m.st, St::Dirty | St::Want) => {
                        if self.cfg.head_before_bump { Pc::PmHead { a: m.ver } } else { Pc::PmCas { a: m.ver } }
                    }
                    Some(_) => Pc::PmWait,
                };
                Ok(())
            }),
            Pc::PmHead { a } => self.emit(s, out, Marks::act(w, "HEAD hot before bumping the marker"), false, |ns| {
                if ns.hot == 0 {
                    self.ack_end(ns, c, Resp::Ok)
                } else {
                    ns.cl[c].pc = Pc::PmCas { a };
                    Ok(())
                }
            }),
            Pc::PmCas { a } => self.emit(s, out, Marks::act(w, "CAS the marker to DIRTY(gone=false)"), false, |ns| {
                if let Some(old) = ns.mk.filter(|m| m.ver == a) {
                    let ver = ns.fresh();
                    let id = ns.cl[c].id;
                    let gone = self.cfg.bump_keeps_gone && old.gone;
                    ns.mk_put(Mk { st: St::Dirty, gone, dirty: true, why: Why::Overwrite, owner: id, ver })?;
                    self.ack_end(ns, c, Resp::Ok)
                } else {
                    ns.cl[c].pc = Pc::Pm0;
                    Ok(())
                }
            }),
            Pc::PmCreate => self.emit(s, out, Marks::act(w, "create the marker (create-only)"), false, |ns| {
                if ns.mk.is_none() {
                    let ver = ns.fresh();
                    let id = ns.cl[c].id;
                    ns.mk = Some(Mk { st: St::Dirty, gone: false, dirty: true, why: Why::Overwrite, owner: id, ver });
                    self.ack_end(ns, c, Resp::Ok)
                } else {
                    ns.cl[c].pc = Pc::Pm0;
                    Ok(())
                }
            }),
            Pc::PmWait => self.wait_steps(s, out, c, Pc::PmWaitHead, true),
            Pc::PmWaitHead => self.emit(s, out, Marks::act(w, "HEAD hot after the wait"), false, |ns| {
                if ns.hot == 0 {
                    self.ack_end(ns, c, Resp::Ok)
                } else {
                    ns.cl[c].pc = Pc::Pm0;
                    Ok(())
                }
            }),
            Pc::LkCreate => self.lock_attempt(s, out, c, None),
            Pc::LkGet => self.emit(s, out, Marks::act(w, "GET the marker (the lock create said 412)"), false, |ns| {
                match ns.mk {
                    None => ns.cl[c].pc = Pc::LkCreate,
                    Some(m) if matches!(m.st, St::Dirty | St::Want) => {
                        if m.gone && self.cfg.coldland_503_on_gone && self.op_of(&ns.cl[c]) == Op::PutCold {
                            return self.ack_end(ns, c, Resp::Fail503);
                        }
                        ns.cl[c].pc = Pc::LkCas { a: m.ver, g: m.gone };
                    }
                    Some(m) => {
                        if self.cfg.flaw == Flaw::ImmediateLock503 {
                            if !self.owner_dead(ns, m.owner) {
                                return Err("A6 a writer answered 503 at once because a live holder had the lock".into());
                            }
                            return self.ack_end(ns, c, Resp::Wait503);
                        }
                        ns.cl[c].pc = Pc::LkWait;
                    }
                }
                Ok(())
            }),
            Pc::LkCas { a, g } => self.lock_attempt(s, out, c, Some((a, g))),
            Pc::LkWait => self.wait_steps(s, out, c, Pc::LkCreate, false),
            Pc::OHead => self.emit(s, out, Marks::act(w, "HEAD hot under the lock"), false, |ns| {
                let e = ns.hot;
                ns.cl[c].pc = if e == 0 { Pc::OCreate } else { Pc::ORepl { e } };
                Ok(())
            }),
            Pc::ORepl { e } => self.emit(s, out, Marks::act(w, "replace hot (If-Match the tag read under the lock)"), false, |ns| {
                let v = self.value(&ns.cl[c]);
                let plain = matches!(self.cfg.flaw, Flaw::PlainReplace | Flaw::IfMatchNoRecheck);
                if self.cfg.flaw == Flaw::PlainReplace || self.cfg.flaw == Flaw::OverwriteNoLock {
                    self.need_lock(ns, c, "replace of hot by a call that carries no tag read under a lock")?;
                }
                if ns.hot == e || plain {
                    ns.hot = v;
                    self.after_replace(ns, c)
                } else if ns.hot == 0 {
                    ns.cl[c].pc = Pc::OCreate;
                    Ok(())
                } else if matches!(self.op_of(&ns.cl[c]), Op::Put(Body::Replay)) {
                    ns.cl[c].pc = Pc::OHead;
                    Ok(())
                } else {
                    self.fail_release(ns, c, 3)
                }
            }),
            Pc::OCreate => self.emit(s, out, Marks::act(w, "create hot (create-only, hot was absent)"), false, |ns| {
                let v = self.value(&ns.cl[c]);
                if ns.hot == 0 {
                    self.hot_land(ns, v);
                    self.after_replace(ns, c)
                } else if matches!(self.op_of(&ns.cl[c]), Op::Put(Body::Replay)) {
                    ns.cl[c].pc = Pc::OHead;
                    Ok(())
                } else {
                    self.fail_release(ns, c, 3)
                }
            }),
            Pc::G0 => self.emit(s, out, Marks::act(w, "GET hot"), false, |ns| {
                if ns.hot != 0 {
                    let v = ns.hot;
                    self.finish_read(ns, c, v, false)
                } else {
                    let m = if self.cfg.flaw == Flaw::ReadIgnoresMarker { 1 } else { 0 };
                    ns.cl[c].pc = Pc::G1 { m, c: 0 };
                    Ok(())
                }
            }),
            Pc::G1 { .. }
                if self.cfg.flaw == Flaw::ReadWaitsOnLock
                    && matches!(s.mk, Some(k) if matches!(k.st, St::Lock | St::Clearing)) => {}
            Pc::G1 { m, c: cc } => {
                if m == 0 {
                    self.emit(s, out, Marks::act(w, "hot missed: GET the marker (in parallel with cold)"), false, |ns| {
                        let v = match ns.mk {
                            None => 1,
                            Some(k) if k.gone => 3,
                            Some(_) => 2,
                        };
                        ns.cl[c].pc = Pc::G1 { m: v, c: cc };
                        self.g1_next(ns, c)
                    });
                }
                if cc == 0 {
                    self.emit(s, out, Marks::act(w, "hot missed: read cold (in parallel with the marker)"), false, |ns| {
                        let v = ns.cold + 1;
                        ns.cl[c].pc = Pc::G1 { m, c: v };
                        self.g1_next(ns, c)
                    });
                }
            }
            Pc::Gw => self.emit(s, out, Marks::act(w, "ask for a copy-back: create the WANT marker (best effort)"), false, |ns| {
                if ns.mk.is_none() {
                    let ver = ns.fresh();
                    let id = ns.cl[c].id;
                    ns.mk = Some(Mk { st: St::Want, gone: false, dirty: false, why: Why::CopyBack, owner: id, ver });
                }
                self.end_op(ns, c);
                Ok(())
            }),
            Pc::Gc => self.emit(s, out, Marks::act(w, "the marker said gone: HEAD hot once more"), false, |ns| {
                let v = ns.hot;
                self.finish_read(ns, c, v, false)
            }),
            Pc::IrH => self.emit(s, out, Marks::act(w, "If-Match: resolve under the lock, hot HEAD"), false, |ns| {
                let t = match self.op_of(&ns.cl[c]) {
                    Op::PutIm(t) => t,
                    _ => 0,
                };
                let h = ns.hot;
                if h != 0 {
                    if h == t {
                        ns.cl[c].pc = Pc::ORepl { e: h };
                        Ok(())
                    } else {
                        // The tag held at wave 1 and does not now: the key changed during the put.
                        self.fail_release(ns, c, 4)
                    }
                } else if ns.cl[c].lg {
                    self.fail_release(ns, c, 1)
                } else {
                    ns.cl[c].pc = Pc::IrC;
                    Ok(())
                }
            }),
            Pc::IrC => self.emit(s, out, Marks::act(w, "If-Match: resolve under the lock, cold HEAD"), false, |ns| {
                let t = match self.op_of(&ns.cl[c]) {
                    Op::PutIm(t) => t,
                    _ => 0,
                };
                let cv = ns.cold;
                if cv == 0 {
                    self.fail_release(ns, c, 1)
                } else if cv != t {
                    self.fail_release(ns, c, 4)
                } else {
                    ns.cl[c].pc = Pc::OCreate;
                    Ok(())
                }
            }),
            Pc::DHead => self.emit(s, out, Marks::act(w, "HEAD hot under the lock"), false, |ns| {
                ns.cl[c].pc = Pc::DGone { present: ns.hot != 0 };
                Ok(())
            }),
            Pc::DGone { present } => self.emit(s, out, Marks::act(w, "write gone=true into the lock (CAS)"), false, |ns| {
                let ln = ns.cl[c].ln;
                let id = ns.cl[c].id;
                match ns.mk {
                    Some(m) if m.ver == ln && self.cfg.flaw != Flaw::DeleteGoneLate => {
                        let ver = ns.fresh();
                        ns.mk_put(Mk { gone: true, ver, owner: id, ..m })?;
                        ns.cl[c].ln = ver;
                        ns.cl[c].lg = true;
                    }
                    _ => {}
                }
                if present {
                    ns.cl[c].pc = Pc::DDel;
                } else {
                    self.ack(ns, c, Resp::Ok)?;
                    ns.cl[c].pc = Pc::Rel { k: if self.cfg.flaw == Flaw::DeleteNoMarker { 2 } else { 1 } };
                }
                Ok(())
            }),
            Pc::DDel => {
                self.emit(s, out, Marks::act(w, "hot DELETE (plain) under the lock"), false, |ns| {
                    self.need_lock(ns, c, "hot DELETE")?;
                    ns.hot = 0;
                    self.ack(ns, c, Resp::Ok)?;
                    ns.cl[c].pc = Pc::Rel { k: if self.cfg.flaw == Flaw::DeleteNoMarker { 2 } else { 1 } };
                    Ok(())
                });
                if self.cfg.timeouts && !cl.ghost {
                    self.timeout_step(s, out, c);
                }
            }
            Pc::TWait => {
                // 5.2: the margin that outlasts the store's request lifetime has passed once no
                // call of this proxy can land any more.
                if !s.cl.iter().skip(super::NC).any(|g| g.ghost && g.id == cl.id) {
                    self.emit(s, out, Marks::act(w, "the margin has passed: release the lock"), false, |ns| {
                        ns.cl[c].pc = Pc::Rel { k: 1 };
                        Ok(())
                    });
                }
            }
            Pc::ClC => self.emit(s, out, Marks::act(w, "cold HEAD for the tag"), false, |ns| {
                ns.cl[c].pc = Pc::ClW { c: ns.cold };
                Ok(())
            }),
            Pc::ClW { c: tag } => self.emit(s, out, Marks::act(w, "write cold (create-only, or If-Match the cold tag)"), false, |ns| {
                if ns.cold == tag {
                    ns.cold = self.value(&ns.cl[c]);
                    ns.cl[c].pc = Pc::ClH;
                } else {
                    ns.cl[c].pc = Pc::ClC;
                }
                Ok(())
            }),
            Pc::ClH => self.emit(s, out, Marks::act(w, "hot HEAD under the lock"), false, |ns| {
                if ns.hot != 0 {
                    ns.cl[c].pc = Pc::ClD;
                    Ok(())
                } else {
                    self.ack(ns, c, Resp::Ok)?;
                    ns.cl[c].pc = Pc::Rel { k: 2 };
                    Ok(())
                }
            }),
            Pc::ClD => self.emit(s, out, Marks::act(w, "hot DELETE (plain) under the lock"), false, |ns| {
                self.need_lock(ns, c, "hot DELETE")?;
                ns.hot = 0;
                self.ack(ns, c, Resp::Ok)?;
                ns.cl[c].pc = Pc::Rel { k: 2 };
                Ok(())
            }),
            Pc::L1 { h, m } => {
                if h == 0 {
                    self.emit(s, out, Marks::act(w, "LIST wave 1: hot page"), false, |ns| {
                        ns.cl[c].pc = Pc::L1 { h: ns.hot + 1, m };
                        self.l1_next(ns, c)
                    });
                }
                if m == 0 {
                    self.emit(s, out, Marks::act(w, "LIST wave 1: marker page"), false, |ns| {
                        let v = if ns.mk.is_some() { 2 } else { 1 };
                        ns.cl[c].pc = Pc::L1 { h, m: v };
                        self.l1_next(ns, c)
                    });
                }
            }
            Pc::L2 { h, m } => self.emit(s, out, Marks::act(w, "LIST wave 2: cold page, then merge"), false, |ns| {
                let cv = ns.cold;
                if h > 1 {
                    self.finish_read(ns, c, h - 1, false)
                } else if cv == 0 {
                    self.finish_read(ns, c, 0, false)
                } else if m == 2 {
                    ns.cl[c].pc = Pc::L3 { c: cv };
                    Ok(())
                } else {
                    self.finish_read(ns, c, cv, false)
                }
            }),
            Pc::L3 { c: cv } => self.emit(s, out, Marks::act(w, "LIST: GET the marker of a cold-only key"), false, |ns| {
                match ns.mk {
                    Some(m) if m.gone && self.cfg.gone_rechecks_hot => {
                        ns.cl[c].pc = Pc::Gc;
                        Ok(())
                    }
                    Some(m) if m.gone => self.finish_read(ns, c, 0, false),
                    Some(_) => self.finish_read(ns, c, cv, false),
                    None => {
                        ns.cl[c].pc = Pc::L4;
                        Ok(())
                    }
                }
            }),
            Pc::L4 => self.emit(s, out, Marks::act(w, "LIST: marker vanished, cold HEAD decides"), false, |ns| {
                let v = ns.cold;
                self.finish_read(ns, c, v, false)
            }),
            Pc::Rel { k } => self.emit(s, out, Marks::act(w, "release the lock (CAS)"), false, |ns| {
                let ln = ns.cl[c].ln;
                match ns.mk {
                    Some(m) if m.ver == ln => {
                        let ver = ns.fresh();
                        let id = ns.cl[c].id;
                        let lg = ns.cl[c].lg;
                        let gone = match k {
                            0 => self.cfg.bump_keeps_gone && lg,
                            1 => true,
                            _ => lg,
                        };
                        if k == 2 {
                            ns.mk_put(Mk { st: St::Clearing, gone, dirty: false, why: m.why, owner: id, ver })?;
                            ns.cl[c].ln = ver;
                            ns.cl[c].pc = Pc::RelDel;
                            Ok(())
                        } else {
                            ns.mk_put(Mk { st: St::Dirty, gone, dirty: true, why: m.why, owner: id, ver })?;
                            self.finish(ns, c)
                        }
                    }
                    _ => self.finish(ns, c),
                }
            }),
            Pc::RelDel => self.emit(s, out, Marks::act(w, "delete the marker (plain DELETE)"), false, |ns| {
                let id = ns.cl[c].id;
                match ns.mk {
                    Some(m) if m.st == St::Clearing && m.owner == id => ns.mk = None,
                    Some(m) => {
                        return Err(format!(
                            "A9 a marker was deleted from {:?} owned by {} by actor {}, not from its own CLEARING",
                            m.st, m.owner, id
                        ));
                    }
                    None => {}
                }
                self.finish(ns, c)
            }),
            Pc::OldP1 { h, c: cc } => {
                if h == 0 {
                    self.emit(s, out, Marks::act(w, "old release: hot HEAD"), false, |ns| {
                        ns.cl[c].pc = Pc::OldP1 { h: ns.hot + 1, c: cc };
                        self.oldp1_next(ns, c)
                    });
                }
                if cc == 0 {
                    self.emit(s, out, Marks::act(w, "old release: cold HEAD"), false, |ns| {
                        ns.cl[c].pc = Pc::OldP1 { h, c: ns.cold + 1 };
                        self.oldp1_next(ns, c)
                    });
                }
            }
            Pc::OldP2 => self.emit(s, out, Marks::act(w, "old release: create hot (create-only)"), false, |ns| {
                let v = self.value(&ns.cl[c]);
                if ns.hot == 0 {
                    self.hot_land(ns, v);
                    self.ack_end(ns, c, Resp::Ok)
                } else {
                    self.ack_end(ns, c, Resp::R412)
                }
            }),
            Pc::OldG0 => self.emit(s, out, Marks::act(w, "old release: GET hot"), false, |ns| {
                if ns.hot != 0 {
                    let v = ns.hot;
                    self.finish_read(ns, c, v, false)
                } else {
                    ns.cl[c].pc = Pc::OldG1;
                    Ok(())
                }
            }),
            Pc::OldG1 => self.emit(s, out, Marks::act(w, "old release: GET cold (no marker is read)"), false, |ns| {
                let v = ns.cold;
                self.finish_read(ns, c, v, false)
            }),
            Pc::OldD0 => self.emit(s, out, Marks::act(w, "old release: cold DELETE"), false, |ns| {
                ns.cold = 0;
                ns.cl[c].pc = Pc::OldD1;
                Ok(())
            }),
            Pc::OldD1 => self.emit(s, out, Marks::act(w, "old release: hot DELETE"), false, |ns| {
                ns.hot = 0;
                self.ack_end(ns, c, Resp::Ok)
            }),
        }
    }
}
