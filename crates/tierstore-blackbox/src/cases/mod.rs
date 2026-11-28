//! The cases of the black-box test, each against a node of its own (README.md has what each
//! asserts and which failure of the farm it is the test of):
//!
//! - [`Case::NoFs`]: the node started without its `fs` interface and its mount provider, as a farm
//!   job can run it (an ordinary user, no capability, no FUSE device): it serves through its S3
//!   front, and a Prepare has no answer.
//! - [`Case::Cycles`] (T1): Prepare, Mount, read and Release, over and over, at the node's
//!   defaults: every cycle succeeds, and no placement is left a while after the last Release.
//! - [`Case::Placements`] (T2): a burst of Prepares past `trees.max_placements`: each Mount past it
//!   is refused "unavailable" with its reason, and the places free soon after the Releases.
//! - [`Case::Stall`] (T3): the upstream stalled while Prepares arrive: the node's resident memory
//!   stays under its idle size plus a bound and levels off, and its queues are idle soon after.
//! - [`Case::Restart`]: the node killed between a Prepare and its Mount, and started again: the
//!   agent, asking again, gets its tree.
//! - [`Case::Errors`]: the upstream answering only errors for a while: every start waits
//!   (unavailable, never a refusal), the memory stays bounded, and the trees come after.
//! - [`Case::Disk`]: the disk of the cache full: each answer is a success or a refusal with its
//!   reason, nothing reads wrong, the node lives on, and a tree comes once the disk has room.
//!
//! A case asks as the job agent does: a Mount answered "unavailable" is followed, after a pause,
//! by a Prepare and a Mount again, until the agent's budget for a tree is spent. Every wait of a
//! case is a bound on a condition it looks for, read on the rig's clock.

use std::path::PathBuf;
use std::time::Duration;

use tierstore_trees_api::provider::v1::Source;

use crate::provider::{Code, Refusal, source};
use crate::rig::Rig;
use crate::tree::{Tree, TreeFault};

mod cycles;
mod disk;
mod errors;
mod nofs;
mod placements;
mod restart;
mod stall;

/// The agent every case's sources are of.
pub(crate) const AGENT: &str = "blackbox-agent";

/// A case of the test (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Case {
    NoFs,
    Cycles,
    Placements,
    Stall,
    Restart,
    Errors,
    Disk,
}

impl Case {
    /// Every case, in the order the driver runs them.
    pub(crate) const ALL: [Case; 7] = [
        Case::NoFs,
        Case::Cycles,
        Case::Placements,
        Case::Stall,
        Case::Restart,
        Case::Errors,
        Case::Disk,
    ];

    /// The case's name, as `--case` takes it and the report prints it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Case::NoFs => "start-without-fs",
            Case::Cycles => "t1-cycles",
            Case::Placements => "t2-placements",
            Case::Stall => "t3-stall",
            Case::Restart => "restart",
            Case::Errors => "upstream-errors",
            Case::Disk => "disk-full",
        }
    }

    /// Whether the case's node serves the `fs` interface, the FUSE mount, which takes
    /// `/dev/fuse` and a right to mount; `start-without-fs` alone has none.
    pub(crate) fn mounts(self) -> bool {
        self != Case::NoFs
    }

    /// The case named `name`.
    pub(crate) fn of(name: &str) -> Option<Case> {
        Case::ALL.into_iter().find(|case| case.name() == name)
    }
}

/// The numbers of the cases (the driver's flags; README.md gives their defaults and why).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Params {
    /// How soon the placements must be gone after the last Release, and the places free after
    /// the Releases of the held sources (T1, T2).
    pub settle: Duration,
    /// How soon the queues must be idle after the stall ends (T3).
    pub drain: Duration,
    /// The agent's budget for a tree: how long it asks again (`--mount-provider-wait`).
    pub tree_wait: Duration,
    /// The pause between two looks at a condition, and between two asks of the agent.
    pub look: Duration,
    /// How often the resident memory is read.
    pub sample: Duration,
    /// The most the resident memory may be above its idle size.
    pub memory_bound: u64,
    /// The end of the stall over which the memory must level off, and the most it may grow there.
    pub level_window: Duration,
    pub level_growth: u64,
    /// T1: the cycles, and the shape of the tree.
    pub cycles: usize,
    pub cycle_files: usize,
    pub cycle_file_bytes: usize,
    /// T2: the node's `trees.max_placements`, and the Prepares past it.
    pub max_placements: u32,
    pub burst: usize,
    /// T3: the Prepares, the time they arrive over, how long the upstream stalls, the tree.
    pub prepares: usize,
    pub arrival: Duration,
    pub stall: Duration,
    pub stall_files: usize,
    pub stall_file_bytes: usize,
    /// The case of errors: how long the upstream fails, and the sources that ask meanwhile.
    pub errors_for: Duration,
    pub errors_sources: usize,
    /// The case of a full disk: the cycles while it is full, and the tree.
    pub disk_cycles: usize,
    pub disk_files: usize,
    pub disk_file_bytes: usize,
}

impl Params {
    /// The numbers of the plan of the black-box tests (README.md): T1's 2,100 cycles, T2's burst
    /// past the default 1,024 placements, T3's 3,000 Prepares in 60 s over a stall of 300 s with
    /// resident memory under idle plus 1 GiB and growth under 32 MiB over the stall's last 60 s,
    /// queues idle within 120 s, places free within 60 s, and the agent's budget for a tree of
    /// 120 s (`--mount-provider-wait`'s default).
    pub(crate) fn defaults() -> Self {
        Self {
            settle: Duration::from_secs(60),
            drain: Duration::from_secs(120),
            tree_wait: Duration::from_secs(120),
            look: Duration::from_secs(1),
            sample: Duration::from_secs(5),
            memory_bound: 1 << 30,
            level_window: Duration::from_secs(60),
            level_growth: 32 << 20,
            cycles: 2100,
            cycle_files: 16,
            cycle_file_bytes: 4096,
            max_placements: 1024,
            burst: 64,
            prepares: 3000,
            arrival: Duration::from_secs(60),
            stall: Duration::from_secs(300),
            stall_files: 16_384,
            stall_file_bytes: 64,
            errors_for: Duration::from_secs(60),
            errors_sources: 8,
            disk_cycles: 5,
            disk_files: 64,
            disk_file_bytes: 64 << 10,
        }
    }
}

/// What came of a case: what failed (none: it passed) and what it saw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub failures: Vec<String>,
    pub notes: Vec<String>,
}

impl Outcome {
    fn fail(&mut self, what: impl Into<String>) {
        self.failures.push(what.into());
    }

    fn note(&mut self, what: impl Into<String>) {
        self.notes.push(what.into());
    }

    /// Whether the case passed.
    pub(crate) fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Runs `case` on `rig`: its outcome, with a failure of the rig itself as a failure of the case,
/// and a node that no longer runs at its end as another.
pub(crate) async fn run<R: Rig>(case: Case, rig: &mut R, params: &Params) -> Outcome {
    let mut out = Outcome::default();
    let ran = match case {
        Case::NoFs => nofs::run(rig, params, &mut out).await,
        Case::Cycles => cycles::run(rig, params, &mut out).await,
        Case::Placements => placements::run(rig, params, &mut out).await,
        Case::Stall => stall::run(rig, params, &mut out).await,
        Case::Restart => restart::run(rig, params, &mut out).await,
        Case::Errors => errors::run(rig, params, &mut out).await,
        Case::Disk => disk::run(rig, params, &mut out).await,
    };
    if let Err(why) = ran {
        out.fail(format!("the rig: {why}"));
    }
    if !rig.alive() {
        out.fail("the node is not running at the end of the case");
    }
    out
}

/// The source `n` of the case `case`: run `js.<case>-<n>`, index 0.
fn source_of(case: &str, n: usize) -> Source {
    source(AGENT, &format!("js.{case}-{n}"), 0)
}

/// A step of a cycle that did not succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    Prepare(Refusal),
    Mount(Refusal),
    Read(TreeFault),
    Release(Refusal),
    /// The tree came, but this long past the deadline it was asked by.
    Late(Duration),
}

impl Step {
    /// The provider's answer, when this is a Prepare or a Mount that did not succeed.
    fn refusal(&self) -> Option<&Refusal> {
        match self {
            Step::Prepare(refusal) | Step::Mount(refusal) => Some(refusal),
            Step::Read(_) | Step::Release(_) | Step::Late(_) => None,
        }
    }
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Step::Prepare(refusal) => write!(f, "Prepare: {refusal}"),
            Step::Mount(refusal) => write!(f, "Mount: {refusal}"),
            Step::Read(fault) => write!(f, "the read: {fault}"),
            Step::Release(refusal) => write!(f, "Release: {refusal}"),
            Step::Late(late) => write!(f, "the tree came {late:?} past the deadline"),
        }
    }
}

/// One cycle of a job's input, each step answered at its first ask: Prepare, Mount, a read of the
/// tree, Release.
async fn cycle<R: Rig>(rig: &mut R, source: &Source, tree: &Tree) -> Result<(), Step> {
    let at = held(rig, source, tree).await?;
    rig.read(&at, tree).await.map_err(Step::Read)?;
    rig.release(source).await.map_err(Step::Release)
}

/// Prepare and Mount of `source`, each answered at its first ask: the tree's directory.
async fn held<R: Rig>(rig: &mut R, source: &Source, tree: &Tree) -> Result<PathBuf, Step> {
    rig.prepare(source, tree.key())
        .await
        .map_err(Step::Prepare)?;
    rig.mount(source).await.map_err(Step::Mount)
}

/// The agent's asks for the tree of `source` until the rig's time `until`: Prepare and Mount,
/// and after a Mount answered as one the agent waits on, a pause and both again. The tree's
/// directory, read as the tree, or the step that failed: a refusal that fails the start, a read
/// that is not the tree's, the last answer when the time ran out, or a tree that came, but after
/// `until` (a Mount can wait for its tree). The agent pauses `look` between two asks.
async fn obtain<R: Rig>(
    rig: &mut R,
    source: &Source,
    tree: &Tree,
    until: Duration,
    look: Duration,
) -> Result<PathBuf, Step> {
    loop {
        let asked = held(rig, source, tree).await;
        let failed = match asked {
            Ok(at) => {
                rig.read(&at, tree).await.map_err(Step::Read)?;
                let late = rig.now().saturating_sub(until);
                return if late.is_zero() {
                    Ok(at)
                } else {
                    Err(Step::Late(late))
                };
            }
            Err(failed) => failed,
        };
        let waiting = failed
            .refusal()
            .filter(|refusal| !refusal.fails_the_start());
        let Some(refusal) = waiting.cloned() else {
            return Err(failed);
        };
        if rig.now() >= until {
            return Err(Step::Mount(Refusal {
                code: refusal.code,
                message: format!(
                    "no tree by the agent's deadline; the last answer: {}",
                    refusal.message
                ),
            }));
        }
        rig.pause(look).await;
    }
}

/// Waits until the node keeps no placement, at most `within` from now: how long that took, or the
/// placements left.
async fn drained<R: Rig>(
    rig: &mut R,
    within: Duration,
    look: Duration,
) -> Result<Result<Duration, usize>, String> {
    let since = rig.now();
    loop {
        let left = rig.placements()?;
        let waited = rig.now().saturating_sub(since);
        if left == 0 {
            return Ok(Ok(waited));
        }
        if waited >= within {
            return Ok(Err(left));
        }
        rig.pause(look).await;
    }
}

/// The resident memory of the node, read at times of the rig, against its idle size.
#[derive(Debug)]
struct Memory {
    idle: u64,
    samples: Vec<(Duration, u64)>,
}

impl Memory {
    /// The memory of a node whose idle size is its resident memory now.
    fn idle<R: Rig>(rig: &mut R) -> Result<Self, String> {
        Ok(Self {
            idle: rig.resident()?,
            samples: Vec::new(),
        })
    }

    /// Reads the resident memory now.
    fn sample<R: Rig>(&mut self, rig: &mut R) -> Result<(), String> {
        let resident = rig.resident()?;
        self.samples.push((rig.now(), resident));
        Ok(())
    }

    /// The samples over the idle size plus `bound`, as failures.
    fn over(&self, bound: u64) -> Vec<String> {
        let most = self.idle.saturating_add(bound);
        self.samples
            .iter()
            .filter(|(_, resident)| *resident > most)
            .map(|(at, resident)| {
                format!(
                    "at {at:?} the resident memory was {} MiB, over the idle {} MiB and {} MiB",
                    resident >> 20,
                    self.idle >> 20,
                    bound >> 20
                )
            })
            .collect()
    }

    /// The growth over the samples from `from` on: the largest of them less the first.
    fn growth_from(&self, from: Duration) -> u64 {
        let mut late = self
            .samples
            .iter()
            .filter(|(at, _)| *at >= from)
            .map(|(_, r)| *r);
        let first = late.next().unwrap_or_default();
        late.max().map_or(0, |most| most.saturating_sub(first))
    }

    /// The largest sample.
    fn peak(&self) -> u64 {
        self.samples
            .iter()
            .map(|(_, r)| *r)
            .max()
            .unwrap_or(self.idle)
    }
}

/// Whether `refusal` is the agent's "wait and ask again" with a reason.
fn waits(refusal: &Refusal) -> bool {
    refusal.code == Code::Unavailable && !refusal.message.is_empty()
}

#[cfg(test)]
pub(crate) mod model;
#[cfg(test)]
mod tests;
