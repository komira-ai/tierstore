//! The steps of the entry protocol: the proxies' put, delete and get, and the leader's loop.

use super::*;

impl Entries {
    pub(super) fn client_steps(&self, s: &BState, out: &mut Out, c: usize) {
        let w = c as u8;
        let me = w;
        match s.cl[c].pc {
            BPc::Idle => {
                if usize::from(s.cl[c].next) < self.cfg.scripts[c].len() {
                    self.emit(s, out, (w, "the request arrives"), false, |ns| {
                        let g = self.cfg.gidx(c, usize::from(ns.cl[c].next));
                        if self.effs[usize::from(g)].is_some() {
                            ns.lin.invoke(g);
                        }
                        if self.effs[usize::from(g)] == Some(Eff::Del) {
                            ns.del_open |= 1 << g;
                        }
                        ns.cl[c].poss = 1 << ns.truth();
                        ns.cl[c].pc = match self.op(ns, c) {
                            BOp::Put => BPc::PHead,
                            BOp::Delete => BPc::DEnt,
                            BOp::Get => BPc::GHot,
                        };
                        Ok(())
                    });
                }
            }
            BPc::PHead => self.emit(
                s,
                out,
                (w, "hot HEAD (a found key is 412 with no entry)"),
                false,
                |ns| {
                    if ns.hot != 0 {
                        self.ack_end(ns, c, R::R412)
                    } else {
                        ns.cl[c].pc = BPc::PEnt;
                        Ok(())
                    }
                },
            ),
            BPc::PEnt => self.emit(
                s,
                out,
                (w, "create the entry (PUT, HELD) create-only"),
                false,
                |ns| {
                    if ns.ent.is_none() {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            op: EOp::Put,
                            st: ESt::Held,
                            owner: me,
                            ver,
                        });
                        ns.cl[c].mine = ver;
                        ns.cl[c].pc = if self.cfg.flaw == BFlaw::PutNoCheck {
                            BPc::PLand
                        } else if self.cfg.flaw == BFlaw::AckBeforeLanding {
                            BPc::PAck
                        } else {
                            BPc::PChk { h: 0, c: 0 }
                        };
                    } else {
                        ns.cl[c].pc = BPc::PRead;
                    }
                    Ok(())
                },
            ),
            BPc::PRead => self.emit(
                s,
                out,
                (w, "GET the entry (the create said EXISTS)"),
                false,
                |ns| {
                    match ns.ent {
                        None => ns.cl[c].pc = BPc::PEnt,
                        Some(e) => {
                            let lockless = matches!(
                                (e.op, e.st),
                                (EOp::Put | EOp::Cb, ESt::Pending) | (_, ESt::Sealed)
                            );
                            ns.cl[c].pc = if lockless {
                                BPc::PLock { h: 0, c: 0 }
                            } else {
                                return self.ack_end(ns, c, R::Fail503);
                            };
                        }
                    }
                    Ok(())
                },
            ),
            BPc::PLock { h, c: cc } => {
                if h == 0 {
                    self.emit(s, out, (w, "lockless hot HEAD"), false, |ns| {
                        ns.cl[c].pc = BPc::PLock {
                            h: ns.hot + 1,
                            c: cc,
                        };
                        self.plock_next(ns, c)
                    });
                }
                if cc == 0 {
                    self.emit(s, out, (w, "lockless cold HEAD"), false, |ns| {
                        ns.cl[c].pc = BPc::PLock { h, c: ns.cold + 1 };
                        self.plock_next(ns, c)
                    });
                }
            }
            BPc::PChk { h, c: cc } => {
                if h == 0 {
                    self.emit(
                        s,
                        out,
                        (w, "check under the entry: hot HEAD"),
                        false,
                        |ns| {
                            ns.cl[c].pc = BPc::PChk {
                                h: ns.hot + 1,
                                c: cc,
                            };
                            self.pchk_next(ns, c)
                        },
                    );
                }
                if cc == 0 {
                    self.emit(
                        s,
                        out,
                        (w, "check under the entry: cold HEAD"),
                        false,
                        |ns| {
                            ns.cl[c].pc = BPc::PChk { h, c: ns.cold + 1 };
                            self.pchk_next(ns, c)
                        },
                    );
                }
            }
            BPc::PLand => self.emit(
                s,
                out,
                (w, "land the object in hot with no check"),
                false,
                |ns| {
                    // Only the put without the check lands here, and it lands over nothing.
                    ns.hot = self.val(ns, c);
                    ns.cl[c].pc = BPc::PAck;
                    Ok(())
                },
            ),
            BPc::PAck => self.emit(
                s,
                out,
                (w, "CAS the entry to (PUT, PENDING): the acknowledgement"),
                false,
                |ns| {
                    let mine = ns.cl[c].mine;
                    match ns.ent {
                        Some(e) if e.ver == mine => {
                            let ver = ns.fresh();
                            ns.ent = Some(Ent {
                                st: ESt::Pending,
                                ver,
                                ..e
                            });
                            self.ack_end(ns, c, R::Ok)
                        }
                        _ => self.ack_end(ns, c, R::Unk503),
                    }
                },
            ),
            BPc::PFree { r } => {
                self.emit(s, out, (w, "delete my entry, then answer"), false, |ns| {
                    let mine = ns.cl[c].mine;
                    if matches!(ns.ent, Some(e) if e.ver == mine) {
                        ns.ent = None;
                    }
                    self.ack_end(ns, c, if r == 1 { R::R412 } else { R::Fail503 })
                })
            }
            BPc::DEnt => self.emit(
                s,
                out,
                (w, "create the entry (DEL, HELD) create-only"),
                false,
                |ns| {
                    if ns.ent.is_none() {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            op: EOp::Del,
                            st: ESt::Held,
                            owner: me,
                            ver,
                        });
                        ns.cl[c].mine = ver;
                        ns.cl[c].pc = BPc::DCold { k: 0 };
                    } else {
                        ns.cl[c].pc = BPc::DRead;
                    }
                    Ok(())
                },
            ),
            BPc::DRead => self.emit(
                s,
                out,
                (w, "GET the entry (the create said EXISTS)"),
                false,
                |ns| {
                    ns.cl[c].pc = match ns.ent {
                        None => BPc::DEnt,
                        Some(e) => match (e.op, e.st) {
                            (EOp::Put | EOp::Cb, ESt::Pending) => BPc::DAmend { a: e.ver, k: 1 },
                            (EOp::Put, ESt::Held) if e.owner >= 8 => BPc::DAmend { a: e.ver, k: 2 },
                            (EOp::Del, ESt::Pending) => BPc::DAmend { a: e.ver, k: 3 },
                            _ => BPc::DWait,
                        },
                    };
                    Ok(())
                },
            ),
            BPc::DAmend { a, k } => self.emit(
                s,
                out,
                (w, "CAS the entry to (DEL, HELD, me): amend or claim"),
                false,
                |ns| {
                    if matches!(ns.ent, Some(e) if e.ver == a) {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            op: EOp::Del,
                            st: ESt::Held,
                            owner: me,
                            ver,
                        });
                        ns.cl[c].mine = ver;
                        ns.cl[c].pc = BPc::DCold { k };
                    } else {
                        ns.cl[c].pc = BPc::DRead;
                    }
                    Ok(())
                },
            ),
            BPc::DWait => {
                // Wait for an entry that is another actor's HELD or SEALED to go.
                let busy = matches!(s.ent, Some(e) if matches!(e.st, ESt::Held | ESt::Sealed));
                if !busy {
                    self.emit(
                        s,
                        out,
                        (w, "poll: the entry is gone or no longer held"),
                        false,
                        |ns| {
                            ns.cl[c].pc = BPc::DEnt;
                            Ok(())
                        },
                    );
                }
            }
            BPc::DCold { k } => self.emit(s, out, (w, "cold DELETE"), false, |ns| {
                ns.cold = 0;
                ns.cl[c].pc = BPc::DHot { k };
                Ok(())
            }),
            BPc::DHot { k } => self.emit(s, out, (w, "hot DELETE"), false, |ns| {
                ns.hot = 0;
                ns.cl[c].pc = BPc::DFin { k };
                Ok(())
            }),
            BPc::DFin { k } => self.emit(
                s,
                out,
                (
                    w,
                    "finish: delete the entry, or hand it to the leader as (DEL, PENDING)",
                ),
                false,
                |ns| {
                    let mine = ns.cl[c].mine;
                    if k == 0 {
                        if matches!(ns.ent, Some(e) if e.ver == mine) {
                            ns.ent = None;
                        }
                        return self.ack_end(ns, c, R::Ok);
                    }
                    if let Some(e) = ns.ent.filter(|e| e.ver == mine) {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            st: ESt::Pending,
                            ver,
                            ..e
                        });
                        ns.cl[c].mine = ver;
                    }
                    if k == 1 || self.cfg.flaw == BFlaw::DeleteNoWait {
                        self.ack_end(ns, c, R::Ok)
                    } else {
                        ns.cl[c].pc = BPc::DPoll;
                        Ok(())
                    }
                },
            ),
            BPc::DPoll => {
                // The leader's release: the entry is gone or another operation's.
                // Released: the entry is gone, or is another operation's (the leader's seal of
                // this delete's own entry does not release it).
                if !matches!(s.ent, Some(e) if e.op == EOp::Del) {
                    self.emit(
                        s,
                        out,
                        (w, "poll: the leader released the entry"),
                        false,
                        |ns| self.ack_end(ns, c, R::Ok),
                    );
                }
            }
            BPc::GHot => self.emit(s, out, (w, "GET hot"), false, |ns| {
                if ns.hot != 0 {
                    let v = ns.hot;
                    self.ack_end(ns, c, R::Read(v))
                } else {
                    ns.cl[c].pc = BPc::GCold;
                    Ok(())
                }
            }),
            BPc::GCold => self.emit(s, out, (w, "hot missed: GET cold"), false, |ns| {
                let v = ns.cold;
                self.ack(ns, c, R::Read(v))?;
                if v != 0 && self.cfg.copy_back {
                    ns.cl[c].pc = BPc::GWant;
                } else {
                    self.end(ns, c);
                }
                Ok(())
            }),
            BPc::GWant => self.emit(
                s,
                out,
                (w, "ask for a copy-back: create (CB, PENDING)"),
                false,
                |ns| {
                    if ns.ent.is_none() {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            op: EOp::Cb,
                            st: ESt::Pending,
                            owner: me,
                            ver,
                        });
                    }
                    self.end(ns, c);
                    Ok(())
                },
            ),
        }
    }

    fn plock_next(&self, ns: &mut BState, c: usize) -> Result<(), String> {
        let BPc::PLock { h, c: cc } = ns.cl[c].pc else {
            return Ok(());
        };
        if h > 1 || cc > 1 {
            return self.ack_end(ns, c, R::R412);
        }
        if h == 1 && cc == 1 {
            return self.ack_end(ns, c, R::Fail503);
        }
        Ok(())
    }

    fn pchk_next(&self, ns: &mut BState, c: usize) -> Result<(), String> {
        let BPc::PChk { h, c: cc } = ns.cl[c].pc else {
            return Ok(());
        };
        if h > 1 || cc > 1 {
            ns.cl[c].pc = BPc::PFree { r: 1 };
        } else if h == 1 && cc == 1 {
            ns.cl[c].pc = BPc::PLand;
        }
        Ok(())
    }

    pub(super) fn leader_steps(&self, s: &BState, out: &mut Out) {
        let w = 10u8;
        let me = self.me(s);
        let ver_is = |a: u8| matches!(s.ent, Some(e) if e.ver == a);
        match s.pc {
            LPc::Idle => {
                if let Some(e) = s.ent {
                    let next = match (e.op, e.st) {
                        (EOp::Put, ESt::Pending) => Some(LPc::Held { a: e.ver }),
                        (EOp::Put | EOp::Del, ESt::Held) if self.owner_dead(s, e.owner) => {
                            Some(LPc::Reap { a: e.ver })
                        }
                        (EOp::Del, ESt::Pending) => Some(LPc::SealDel { a: e.ver }),
                        (EOp::Cb, ESt::Pending) => Some(LPc::SealCb { a: e.ver }),
                        (EOp::Put, ESt::Sealed) if self.owner_dead(s, e.owner) => Some(LPc::DelEnt),
                        (EOp::Del, ESt::Sealed) if self.owner_dead(s, e.owner) => {
                            Some(LPc::DelCold)
                        }
                        (EOp::Cb, ESt::Sealed) if self.owner_dead(s, e.owner) => Some(LPc::CbRead),
                        (EOp::Evict, ESt::Sealed) if self.owner_dead(s, e.owner) => {
                            Some(LPc::EvDel)
                        }
                        _ => None,
                    };
                    if let Some(n) = next {
                        self.emit(
                            s,
                            out,
                            (w, "scan: GET the entry and pick it up"),
                            true,
                            |ns| {
                                ns.pc = n;
                                Ok(())
                            },
                        );
                    }
                }
                if s.left > 0 && s.hot != 0 {
                    self.emit(
                        s,
                        out,
                        (w, "usage over high_water: start an eviction"),
                        false,
                        |ns| {
                            ns.left -= 1;
                            ns.pc = LPc::EvH;
                            Ok(())
                        },
                    );
                }
            }
            LPc::Held { a } => self.emit(
                s,
                out,
                (w, "CAS (PUT, PENDING) to (PUT, HELD, leader)"),
                true,
                |ns| {
                    if self.cfg.flaw == BFlaw::CopyNoHeld {
                        ns.pc = LPc::Head;
                    } else if ver_is(a) {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            op: EOp::Put,
                            st: ESt::Held,
                            owner: me,
                            ver,
                        });
                        ns.pc = LPc::Head;
                    } else {
                        ns.pc = LPc::Idle;
                    }
                    Ok(())
                },
            ),
            LPc::Head => self.emit(s, out, (w, "hot HEAD"), true, |ns| {
                ns.pc = if ns.hot != 0 {
                    LPc::Cold { h: ns.hot }
                } else {
                    LPc::SealPut {
                        n: ns.ent.map_or(0, |e| e.ver),
                    }
                };
                Ok(())
            }),
            LPc::Cold { h } => self.emit(s, out, (w, "cold HEAD"), true, |ns| {
                ns.pc = if ns.cold == 0 && h != 0 {
                    LPc::Get
                } else {
                    LPc::SealPut {
                        n: ns.ent.map_or(0, |e| e.ver),
                    }
                };
                Ok(())
            }),
            LPc::Get => self.emit(s, out, (w, "GET hot (stream)"), true, |ns| {
                ns.pc = if ns.hot == 0 {
                    LPc::SealPut {
                        n: ns.ent.map_or(0, |e| e.ver),
                    }
                } else {
                    LPc::Copy { v: ns.hot }
                };
                Ok(())
            }),
            LPc::Copy { v } => self.emit(s, out, (w, "put cold (create-only)"), true, |ns| {
                if ns.cold == 0 {
                    ns.cold = v;
                }
                ns.pc = LPc::SealPut {
                    n: ns.ent.map_or(0, |e| e.ver),
                };
                Ok(())
            }),
            LPc::SealPut { n } => self.emit(
                s,
                out,
                (w, "CAS (PUT, HELD, leader) to SEALED"),
                true,
                |ns| {
                    if let Some(e) = ns.ent.filter(|e| {
                        e.ver == n && (e.owner == me || self.cfg.flaw == BFlaw::CopyNoHeld)
                    }) {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            st: ESt::Sealed,
                            ver,
                            ..e
                        });
                        ns.pc = LPc::DelEnt;
                    } else {
                        ns.pc = LPc::Idle;
                    }
                    Ok(())
                },
            ),
            LPc::Reap { a } => self.emit(
                s,
                out,
                (w, "reap: CAS a departed owner's HELD entry to PENDING"),
                true,
                |ns| {
                    if let Some(e) = ns.ent.filter(|e| e.ver == a) {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            st: ESt::Pending,
                            ver,
                            ..e
                        });
                    }
                    ns.pc = LPc::Idle;
                    Ok(())
                },
            ),
            LPc::SealDel { a } => {
                self.emit(s, out, (w, "CAS (DEL, PENDING) to SEALED"), true, |ns| {
                    if let Some(e) = ns.ent.filter(|e| e.ver == a) {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            st: ESt::Sealed,
                            owner: me,
                            ver,
                            ..e
                        });
                        ns.pc = LPc::DelCold;
                    } else {
                        ns.pc = LPc::Idle;
                    }
                    Ok(())
                })
            }
            LPc::DelCold => self.emit(
                s,
                out,
                (w, "roll the delete forward: cold DELETE"),
                true,
                |ns| {
                    ns.cold = 0;
                    ns.pc = LPc::DelHot;
                    Ok(())
                },
            ),
            LPc::DelHot => self.emit(
                s,
                out,
                (w, "roll the delete forward: hot DELETE"),
                true,
                |ns| {
                    ns.hot = 0;
                    ns.pc = LPc::DelEnt;
                    Ok(())
                },
            ),
            LPc::SealCb { a } => {
                self.emit(s, out, (w, "CAS (CB, PENDING) to SEALED"), true, |ns| {
                    if let Some(e) = ns.ent.filter(|e| e.ver == a) {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            st: ESt::Sealed,
                            owner: me,
                            ver,
                            ..e
                        });
                        ns.pc = LPc::CbRead;
                    } else {
                        ns.pc = LPc::Idle;
                    }
                    Ok(())
                })
            }
            LPc::CbRead => self.emit(s, out, (w, "copy-back: read cold"), true, |ns| {
                ns.pc = if ns.cold == 0 {
                    LPc::DelEnt
                } else {
                    LPc::CbPut { c: ns.cold }
                };
                Ok(())
            }),
            LPc::CbPut { c } => self.emit(
                s,
                out,
                (w, "copy-back: put hot (create-only)"),
                true,
                |ns| {
                    if ns.hot == 0 {
                        ns.hot = c;
                    }
                    ns.pc = LPc::DelEnt;
                    Ok(())
                },
            ),
            LPc::DelEnt => self.emit(s, out, (w, "delete the SEALED entry"), true, |ns| {
                if matches!(ns.ent, Some(e) if e.st == ESt::Sealed) {
                    ns.ent = None;
                }
                ns.pc = LPc::Idle;
                Ok(())
            }),
            LPc::EvH => self.emit(
                s,
                out,
                (w, "eviction: byte compare, hot read"),
                true,
                |ns| {
                    ns.pc = LPc::EvC { h: ns.hot };
                    Ok(())
                },
            ),
            LPc::EvC { h } => self.emit(
                s,
                out,
                (w, "eviction: byte compare, cold read"),
                true,
                |ns| {
                    ns.pc = if h != 0 && ns.cold == h {
                        LPc::EvEnt
                    } else {
                        LPc::Idle
                    };
                    Ok(())
                },
            ),
            LPc::EvEnt => self.emit(
                s,
                out,
                (w, "create (EVICT, SEALED) create-only"),
                true,
                |ns| {
                    if ns.ent.is_none() {
                        let ver = ns.fresh();
                        ns.ent = Some(Ent {
                            op: EOp::Evict,
                            st: ESt::Sealed,
                            owner: me,
                            ver,
                        });
                        ns.pc = LPc::EvDel;
                    } else {
                        ns.pc = LPc::Idle;
                    }
                    Ok(())
                },
            ),
            LPc::EvDel => self.emit(s, out, (w, "hot DELETE (the eviction)"), true, |ns| {
                ns.hot = 0;
                ns.pc = LPc::DelEnt;
                Ok(())
            }),
        }
    }
}
