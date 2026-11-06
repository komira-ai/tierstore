//! A breadth-first explorer with 128-bit state fingerprints, shortest counterexamples and a
//! "can always converge" liveness check over the background-only part of the graph.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::hash::{DefaultHasher, Hash, Hasher};

/// One enabled transition of a state. `to` is `Err` when taking the step is itself a violation
/// (an assertion of the protocol: a DELETE without a lock, say). `bg` marks the steps of the
/// leader's background work, the only ones the liveness check lets run.
pub struct Succ<S, A> {
    pub act: A,
    pub to: Result<S, String>,
    pub bg: bool,
}

/// A transition system with its properties.
pub trait Model {
    type S: Clone + Eq + Hash + Debug;
    type A: Copy + Debug;
    fn init(&self) -> Vec<Self::S>;
    fn successors(&self, s: &Self::S, out: &mut Vec<Succ<Self::S, Self::A>>);
    /// A property of the state itself.
    fn check(&self, s: &Self::S) -> Option<String>;
    /// A property of a state with no enabled step (a deadlock, or the end of the scenario).
    fn terminal(&self, s: &Self::S) -> Option<String>;
    /// No client operation is in flight any more; the liveness check starts here.
    fn quiescent(&self, s: &Self::S) -> bool;
    /// The state liveness must reach.
    fn converged(&self, s: &Self::S) -> bool;
    fn show_act(&self, a: &Self::A) -> String;
    fn show_state(&self, s: &Self::S) -> String;
}

/// Bounds of one exploration.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_states: usize,
    pub liveness: bool,
    /// Judge only the rule with this id (`"A7"`): a violation of any other rule prunes the path
    /// it is found on instead of ending the search.
    pub only: Option<&'static str>,
}

impl Limits {
    pub fn new(max_states: usize, liveness: bool) -> Limits {
        Limits {
            max_states,
            liveness,
            only: None,
        }
    }

    fn judged(&self, msg: &str) -> bool {
        match self.only {
            None => true,
            Some(rule) => msg.split([' ', ':']).next() == Some(rule),
        }
    }
}

/// A counterexample: the rule that broke and the steps that reach it, each with the state after it.
#[derive(Clone, Debug)]
pub struct Trace {
    pub message: String,
    pub steps: Vec<(String, String)>,
}

impl Trace {
    /// The rule id the message starts with (`A8`, `A2`...), up to the first space.
    pub fn rule(&self) -> &str {
        self.message.split([' ', ':']).next().unwrap_or("")
    }
}

/// The result of one exploration.
#[derive(Debug)]
pub struct Outcome {
    pub states: usize,
    pub transitions: usize,
    pub truncated: bool,
    pub live_starts: usize,
    pub trace: Option<Trace>,
}

fn fingerprint<S: Hash>(s: &S) -> u128 {
    let mut a = DefaultHasher::new();
    s.hash(&mut a);
    let mut b = DefaultHasher::new();
    0x9e37_79b9_7f4a_7c15u64.hash(&mut b);
    s.hash(&mut b);
    (u128::from(a.finish()) << 64) | u128::from(b.finish())
}

struct Graph<S, A> {
    states: Vec<S>,
    parent: Vec<(u32, Option<A>)>,
    edges: Vec<(u32, u32, bool)>,
}

impl<S: Clone, A: Copy> Graph<S, A> {
    fn path<M: Model<S = S, A = A>>(&self, m: &M, to: usize) -> Vec<(String, String)> {
        let mut chain = Vec::new();
        let mut at = to;
        while let (p, Some(a)) = &self.parent[at] {
            chain.push((m.show_act(a), m.show_state(&self.states[at])));
            at = *p as usize;
        }
        chain.reverse();
        chain
    }
}

/// Explore every state reachable from `m.init()`, stop at the first violation (the BFS order
/// makes its trace a shortest one), and, if there is none and `limits.liveness`, check that from
/// every quiescent state the background steps can always still reach convergence.
pub fn explore<M: Model>(m: &M, limits: Limits) -> Outcome {
    let mut g: Graph<M::S, M::A> = Graph {
        states: Vec::new(),
        parent: Vec::new(),
        edges: Vec::new(),
    };
    let mut seen: HashMap<u128, u32> = HashMap::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    let mut out = Outcome {
        states: 0,
        transitions: 0,
        truncated: false,
        live_starts: 0,
        trace: None,
    };
    for s in m.init() {
        if let Some(msg) = m.check(&s).filter(|msg| limits.judged(msg)) {
            out.trace = Some(Trace {
                message: msg,
                steps: Vec::new(),
            });
            return out;
        }
        let id = g.states.len() as u32;
        seen.insert(fingerprint(&s), id);
        g.states.push(s);
        g.parent.push((id, None));
        queue.push_back(id);
    }
    let mut buf = Vec::new();
    while let Some(id) = queue.pop_front() {
        let s = g.states[id as usize].clone();
        buf.clear();
        m.successors(&s, &mut buf);
        if buf.is_empty() {
            if let Some(msg) = m.terminal(&s).filter(|msg| limits.judged(msg)) {
                out.trace = Some(Trace {
                    message: msg,
                    steps: g.path(m, id as usize),
                });
                break;
            }
            continue;
        }
        for succ in buf.drain(..) {
            out.transitions += 1;
            let ns = match succ.to {
                Ok(ns) => ns,
                Err(msg) if !limits.judged(&msg) => continue,
                Err(msg) => {
                    let mut steps = g.path(m, id as usize);
                    steps.push((m.show_act(&succ.act), "(the step is the violation)".into()));
                    out.trace = Some(Trace {
                        message: msg,
                        steps,
                    });
                    break;
                }
            };
            let fp = fingerprint(&ns);
            let to = if let Some(&t) = seen.get(&fp) {
                t
            } else {
                let t = g.states.len() as u32;
                seen.insert(fp, t);
                if let Some(msg) = m.check(&ns).filter(|msg| limits.judged(msg)) {
                    g.states.push(ns);
                    g.parent.push((id, Some(succ.act)));
                    out.trace = Some(Trace {
                        message: msg,
                        steps: g.path(m, t as usize),
                    });
                    break;
                }
                g.states.push(ns);
                g.parent.push((id, Some(succ.act)));
                queue.push_back(t);
                t
            };
            if limits.liveness {
                g.edges.push((id, to, succ.bg));
            }
        }
        if out.trace.is_some() {
            break;
        }
        if g.states.len() >= limits.max_states {
            out.truncated = true;
            break;
        }
    }
    out.states = g.states.len();
    if out.trace.is_none() && !out.truncated && limits.liveness && limits.judged("A4") {
        liveness(m, &g, &mut out);
    }
    out
}

fn liveness<M: Model>(m: &M, g: &Graph<M::S, M::A>, out: &mut Outcome) {
    let n = g.states.len();
    // Reverse bg adjacency (CSR) for the backward reach from the converged states.
    let mut indeg = vec![0u32; n + 1];
    let mut outdeg = vec![0u32; n + 1];
    for &(f, t, bg) in &g.edges {
        if bg {
            indeg[t as usize + 1] += 1;
            outdeg[f as usize + 1] += 1;
        }
    }
    for i in 0..n {
        indeg[i + 1] += indeg[i];
        outdeg[i + 1] += outdeg[i];
    }
    let mut rev = vec![0u32; indeg[n] as usize];
    let mut fwd = vec![0u32; outdeg[n] as usize];
    let mut rpos = indeg.clone();
    let mut fpos = outdeg.clone();
    for &(f, t, bg) in &g.edges {
        if bg {
            rev[rpos[t as usize] as usize] = f;
            rpos[t as usize] += 1;
            fwd[fpos[f as usize] as usize] = t;
            fpos[f as usize] += 1;
        }
    }
    let mut can = vec![false; n];
    let mut stack: Vec<u32> = Vec::new();
    for (i, s) in g.states.iter().enumerate() {
        if m.converged(s) {
            can[i] = true;
            stack.push(i as u32);
        }
    }
    while let Some(t) = stack.pop() {
        for k in indeg[t as usize]..indeg[t as usize + 1] {
            let f = rev[k as usize];
            if !can[f as usize] {
                can[f as usize] = true;
                stack.push(f);
            }
        }
    }
    // Forward BFS from every quiescent state along bg edges; the first state that cannot reach
    // convergence is the counterexample.
    let mut origin: HashMap<u32, (u32, u32)> = HashMap::new();
    let mut q: VecDeque<u32> = VecDeque::new();
    for (i, s) in g.states.iter().enumerate() {
        if m.quiescent(s) {
            out.live_starts += 1;
            origin.insert(i as u32, (u32::MAX, i as u32));
            q.push_back(i as u32);
        }
    }
    let mut visited: HashSet<u32> = origin.keys().copied().collect();
    while let Some(s) = q.pop_front() {
        if !can[s as usize] {
            let mut chain = Vec::new();
            let mut at = s;
            let start = origin[&s].1;
            while let Some(&(p, _)) = origin.get(&at) {
                if p == u32::MAX {
                    break;
                }
                chain.push(at);
                at = p;
            }
            chain.reverse();
            let root = at;
            let mut steps = g.path(m, root as usize);
            let _ = start;
            for st in chain {
                steps.push((
                    "(leader step)".to_string(),
                    m.show_state(&g.states[st as usize]),
                ));
            }
            out.trace = Some(Trace {
                message: "A4 liveness: from quiescence the leader cannot reach a state with no marker and cold equal to truth".into(),
                steps,
            });
            return;
        }
        for k in outdeg[s as usize]..outdeg[s as usize + 1] {
            let t = fwd[k as usize];
            if visited.insert(t) {
                origin.insert(t, (s, origin[&s].1));
                q.push_back(t);
            }
        }
    }
}
