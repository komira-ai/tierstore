//! A linearizability tracker for one key: the set of (value, ops-already-linearized)
//! configurations that the acknowledged history and the observed truth still allow.
//!
//! Values are 0 (absent) and 1..=7. An operation is an index 0..8 with an effect. The tracker is
//! told when an operation is invoked, when it is acknowledged, and what the truth of the key is
//! at each state; it rejects a history no order of the operations can explain. An invoked
//! operation that is never acknowledged (a crash, an unknown 503) stays free to take effect at
//! any later point, as in any linearizability check.

/// What a mutating operation does to the value of the key when it takes effect.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Eff {
    /// Plain put: the value becomes `v`.
    Put(u8),
    /// Create-only put: the value becomes `v` if it is absent (otherwise the put is a 412).
    PutCo(u8),
    /// `If-Match` put: the value becomes `v` if it is the tag (otherwise 404 or 412).
    PutIm(u8, u8),
    /// Delete.
    Del,
}

/// The effect of each operation index; `None` for reads and for operations without an effect.
pub type Effs = [Option<Eff>; 8];

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Lin {
    cfgs: Vec<u16>,
    invoked: u8,
    dead: u8,
}

fn val(c: u16) -> u8 {
    (c & 7) as u8
}

fn mask(c: u16) -> u8 {
    (c >> 3) as u8
}

fn apply(e: Eff, c: u16) -> Option<u16> {
    let v = val(c);
    let nv = match e {
        Eff::Put(x) => x,
        Eff::PutCo(x) if v == 0 => x,
        Eff::PutIm(x, t) if t != 0 && v == t => x,
        Eff::Del => 0,
        Eff::PutCo(_) | Eff::PutIm(..) => return None,
    };
    Some((c & !7) | u16::from(nv))
}

impl Lin {
    pub fn new(initial: u8) -> Lin {
        Lin {
            cfgs: vec![u16::from(initial)],
            invoked: 0,
            dead: 0,
        }
    }

    pub fn invoke(&mut self, op: u8) {
        self.invoked |= 1 << op;
    }

    /// Every configuration reachable by linearizing any invoked, live, pending operations.
    fn closure(&self, effs: &Effs) -> Vec<u16> {
        let mut seen = [0u64; 32];
        let mut out = Vec::new();
        let mut stack = Vec::new();
        for &c in &self.cfgs {
            if seen[usize::from(c >> 6)] >> (c & 63) & 1 == 0 {
                seen[usize::from(c >> 6)] |= 1 << (c & 63);
                stack.push(c);
            }
        }
        while let Some(c) = stack.pop() {
            out.push(c);
            for op in 0..8u8 {
                let eligible = self.invoked >> op & 1 == 1
                    && self.dead >> op & 1 == 0
                    && mask(c) >> op & 1 == 0;
                if !eligible {
                    continue;
                }
                let Some(e) = effs[usize::from(op)] else {
                    continue;
                };
                if let Some(n) = apply(e, c) {
                    let n = n | (1 << (3 + op));
                    if seen[usize::from(n >> 6)] >> (n & 63) & 1 == 0 {
                        seen[usize::from(n >> 6)] |= 1 << (n & 63);
                        stack.push(n);
                    }
                }
            }
        }
        out
    }

    /// Replace the configurations; an empty set is a failure and leaves them as they were.
    fn set(&mut self, mut v: Vec<u16>) -> bool {
        v.sort_unstable();
        v.dedup();
        if v.is_empty() {
            return false;
        }
        self.cfgs = v;
        true
    }

    /// The truth of the key was observed to be `t`. False: no order explains it.
    pub fn observe(&mut self, effs: &Effs, t: u8) -> bool {
        if self.cfgs.iter().any(|&c| val(c) == t) {
            self.cfgs.retain(|&c| val(c) == t);
            return true;
        }
        let v: Vec<u16> = self
            .closure(effs)
            .into_iter()
            .filter(|&c| val(c) == t)
            .collect();
        self.set(v)
    }

    /// Operation `op`, an effectful one, was acknowledged as having taken effect.
    pub fn ack(&mut self, effs: &Effs, op: u8) -> bool {
        let v: Vec<u16> = self
            .closure(effs)
            .into_iter()
            .filter(|&c| mask(c) >> op & 1 == 1)
            .collect();
        self.set(v)
    }

    /// `op` was answered with a failure that guarantees it did nothing: the histories that
    /// applied it are impossible. False if no history is left.
    pub fn kill(&mut self, op: u8) -> bool {
        self.dead |= 1 << op;
        let v: Vec<u16> = self
            .cfgs
            .iter()
            .copied()
            .filter(|&c| mask(c) >> op & 1 == 0)
            .collect();
        self.set(v)
    }

    /// Every history still allowed has already applied `op`.
    pub fn applied_everywhere(&self, op: u8) -> bool {
        self.cfgs.iter().all(|&c| mask(c) >> op & 1 == 1)
    }

    /// The values the key may hold now or after any of the pending operations took effect.
    pub fn may_hold(&self, effs: &Effs) -> u8 {
        self.closure(effs)
            .iter()
            .fold(0u8, |m, &c| m | (1 << val(c)))
    }
}
