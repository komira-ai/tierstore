//! A rig of the cases' tests: a model of a node as the cases see it from outside, whose clock
//! moves only when a case pauses, with a switch for each way a node can go wrong that a case is
//! the test of.
//!
//! A Prepare places its tree at once, unless the upstream fails (the source
//! waits), the disk is full and the model refuses then, or the node holds `max_placements` (the
//! preparation is unmade; the next Prepare or Mount tries again). A Mount answers the placed
//! source's directory, or "unavailable" with the node's reason. A Release frees the place, unless
//! the model keeps it (komira-ai/tierstore#619 undone). A Prepare under a stall of the packs adds
//! its reads' heap to the resident memory, up to the bound when the model has one
//! (komira-ai/tierstore#613). A restart forgets the sources and keeps the placements, as the
//! node keeps them on disk.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use objstore_s3::s3::fake::Serving;
use tierstore_trees_api::provider::v1::Source;

use crate::metrics::Gauges;
use crate::provider::{Code, Refusal};
use crate::rig::Rig;
use crate::tree::{Tree, TreeFault};

pub(crate) fn unavailable(message: &str) -> Refusal {
    Refusal {
        code: Code::Unavailable,
        message: message.to_owned(),
    }
}

/// The model (module docs); every switch is off in [`Model::healthy`].
#[derive(Debug)]
pub(crate) struct Model {
    pub now: Duration,
    sources: BTreeMap<String, String>,
    placed: BTreeSet<String>,
    lingering: usize,
    pub max_placements: usize,
    serving: Serving,
    pub idle: u64,
    heap: u64,
    stalled: u64,
    alive: bool,
    disk_full: bool,
    pub calls: Vec<String>,
    /// The heap one Prepare's queued reads take under a stall, and the most of it the node holds;
    /// none: no bound (#613 undone).
    pub heap_per_prepare: u64,
    pub heap_bound: Option<u64>,
    /// A Release leaves the place counted (#619 undone).
    pub release_keeps_place: bool,
    /// A stall reaches none of the node's reads.
    pub stall_unreached: bool,
    /// The queues stay busy after a stall.
    pub queues_stuck: bool,
    /// How a Mount is answered while the upstream fails.
    pub failing_answer: Refusal,
    /// How a Prepare and a Mount are answered with the disk full; none: the tree is served.
    pub full_answer: Option<Refusal>,
    /// How a read is answered with the disk full; none: as the tree.
    pub full_read: Option<TreeFault>,
    /// The node dies at a Mount with its disk full.
    pub full_kills: bool,
    /// The disk stays full after the rig frees it.
    pub full_sticks: bool,
    /// A restart drops the placements too.
    pub restart_drops_places: bool,
    /// How every read is answered; none: as the tree.
    pub read_fault: Option<TreeFault>,
    /// How a Mount past `max_placements` is answered; none: as the node answers it.
    pub too_many_answer: Option<Refusal>,
    /// A failing upstream still lets the trees be placed.
    pub failing_serves: bool,
    /// The bytes the resident memory grows by at each read of it.
    pub leak_per_read: u64,
    /// The time each Mount takes, on the model's clock.
    pub mount_takes: Duration,
    /// The upstream goes on as it was when the rig has it serve all again (a failure goes on).
    pub failing_sticks: bool,
    /// The stop of the rig fails, saying this.
    pub stop_fails: Option<String>,
    /// The node has no file system: every Prepare and Mount is unavailable for it.
    pub no_fs: bool,
    /// The status of `/readyz`.
    pub ready: u16,
    /// The objects put into the upstream, which the S3 front serves; `front_wrong` changes the
    /// first byte of each it serves, `front_fails` fails each read.
    objects: BTreeMap<String, Vec<u8>>,
    pub front_wrong: bool,
    pub front_fails: bool,
    /// The next answers of a call, by its name and the run of its source ("" for any), before
    /// the model's own.
    pub next: VecDeque<(&'static str, &'static str, Refusal)>,
    /// The rig fails at this call, by its name.
    pub rig_fails: Option<&'static str>,
}

impl Model {
    pub(crate) fn healthy() -> Self {
        Self {
            now: Duration::ZERO,
            sources: BTreeMap::new(),
            placed: BTreeSet::new(),
            lingering: 0,
            max_placements: 1024,
            serving: Serving::All,
            idle: 200 << 20,
            heap: 0,
            stalled: 0,
            alive: true,
            disk_full: false,
            calls: Vec::new(),
            heap_per_prepare: 1 << 20,
            heap_bound: Some(256 << 20),
            release_keeps_place: false,
            stall_unreached: false,
            queues_stuck: false,
            failing_answer: unavailable("the read of cas/l/... failed: 503 SlowDown"),
            full_answer: Some(unavailable(
                "the tree was not made: No space left on device (os error 28)",
            )),
            full_read: None,
            full_kills: false,
            full_sticks: false,
            restart_drops_places: false,
            read_fault: None,
            too_many_answer: None,
            failing_serves: false,
            leak_per_read: 0,
            mount_takes: Duration::ZERO,
            failing_sticks: false,
            stop_fails: None,
            no_fs: false,
            ready: 200,
            objects: BTreeMap::new(),
            front_wrong: false,
            front_fails: false,
            next: VecDeque::new(),
            rig_fails: None,
        }
    }

    fn count(&self) -> usize {
        self.placed.len() + self.lingering
    }

    /// The model's next answer of `call` of the source of `run`, if a test set one.
    fn scripted(&mut self, call: &'static str, run: &str) -> Result<(), Refusal> {
        let at = (self.next.iter())
            .position(|(name, of, _)| *name == call && (of.is_empty() || *of == run));
        at.and_then(|at| self.next.remove(at))
            .map_or(Ok(()), |(_, _, refusal)| Err(refusal))
    }

    fn rig(&self, call: &'static str) -> Result<(), String> {
        if self.rig_fails == Some(call) {
            return Err(format!("{call} failed"));
        }
        Ok(())
    }

    /// Places `run` if the node can now: none, or why it cannot.
    fn place(&mut self, run: &str) -> Result<(), Refusal> {
        if matches!(self.serving, Serving::Failing { .. }) && !self.failing_serves {
            return Err(self.failing_answer.clone());
        }
        if self.disk_full
            && let Some(refusal) = &self.full_answer
        {
            return Err(refusal.clone());
        }
        if self.placed.contains(run) {
            return Ok(());
        }
        if self.count() >= self.max_placements {
            return Err(self.too_many_answer.clone().unwrap_or_else(|| {
                unavailable(&format!(
                    "the tree was not made: too many placements: {} are alive",
                    self.count()
                ))
            }));
        }
        self.placed.insert(run.to_owned());
        Ok(())
    }

    fn no_fs_or(&self) -> Result<(), Refusal> {
        if self.no_fs {
            return Err(unavailable(
                "no file system serves /run/tierstore/fs: the node's file has no [[interface]] with type = \"fs\"",
            ));
        }
        Ok(())
    }

    fn alive_or(&self) -> Result<(), Refusal> {
        if self.alive {
            Ok(())
        } else {
            Err(Refusal {
                code: Code::NoAnswer,
                message: "no node".to_owned(),
            })
        }
    }
}

impl Rig for Model {
    async fn prepare(&mut self, source: &Source, tree: &str) -> Result<(), Refusal> {
        self.calls.push(format!("Prepare {}", source.run));
        self.alive_or()?;
        self.scripted("prepare", &source.run)?;
        self.no_fs_or()?;
        self.sources.insert(source.run.clone(), tree.to_owned());
        if matches!(self.serving, Serving::Stalled { .. }) && !self.stall_unreached {
            self.stalled += 1;
            let heap = self.heap + self.heap_per_prepare;
            self.heap = self.heap_bound.map_or(heap, |bound| heap.min(bound));
        }
        // A preparation that cannot place the tree now is no answer of the Prepare's.
        let _ = self.place(&source.run);
        Ok(())
    }

    async fn mount(&mut self, source: &Source) -> Result<PathBuf, Refusal> {
        self.calls.push(format!("Mount {}", source.run));
        self.now += self.mount_takes;
        self.alive_or()?;
        self.scripted("mount", &source.run)?;
        self.no_fs_or()?;
        if self.disk_full && self.full_kills {
            self.alive = false;
            return self.alive_or().map(|()| PathBuf::new());
        }
        let known = self.sources.contains_key(&source.run);
        if !known && !self.placed.contains(&source.run) {
            return Err(unavailable("the source is not prepared here"));
        }
        if known {
            self.place(&source.run)?;
        }
        Ok(PathBuf::from("/m").join(&source.run))
    }

    async fn release(&mut self, source: &Source) -> Result<(), Refusal> {
        self.calls.push(format!("Release {}", source.run));
        self.alive_or()?;
        self.scripted("release", &source.run)?;
        self.sources.remove(&source.run);
        if self.placed.remove(&source.run) && self.release_keeps_place {
            self.lingering += 1;
        }
        Ok(())
    }

    async fn read(&mut self, _at: &Path, _tree: &Tree) -> Result<(), TreeFault> {
        if let Some(fault) = &self.read_fault {
            return Err(fault.clone());
        }
        match (&self.full_read, self.disk_full) {
            (Some(fault), true) => Err(fault.clone()),
            _ => Ok(()),
        }
    }

    fn placements(&mut self) -> Result<usize, String> {
        self.rig("placements")?;
        Ok(self.count())
    }

    fn resident(&mut self) -> Result<u64, String> {
        self.rig("resident")?;
        self.idle += self.leak_per_read;
        Ok(self.idle + self.heap)
    }

    async fn gauges(&mut self) -> Result<Gauges, String> {
        self.rig("gauges")?;
        let busy = u64::from(self.queues_stuck);
        Ok(Gauges {
            queued_bytes: Some(self.heap.max(busy)),
            queue_depth: Some(0),
            preparing: Some(0),
        })
    }

    async fn put(&mut self, tree: &Tree) -> Result<(), String> {
        self.rig("put")?;
        self.objects.extend(tree.objects().iter().cloned());
        Ok(())
    }

    async fn ready(&mut self) -> Result<(u16, String), String> {
        self.rig("ready")?;
        Ok((self.ready, "ready read-cache\n".to_owned()))
    }

    async fn front_read(&mut self, key: &str, len: u64) -> Result<Vec<u8>, String> {
        if self.front_fails {
            return Err("403 SignatureDoesNotMatch".to_owned());
        }
        let mut bytes = self.objects.get(key).cloned().unwrap_or_default();
        bytes.truncate(usize::try_from(len).unwrap_or(usize::MAX));
        if self.front_wrong {
            bytes[0] ^= 1;
        }
        Ok(bytes)
    }

    async fn serve_as(&mut self, serving: Serving) -> Result<(), String> {
        self.rig("serve_as")?;
        if serving == Serving::All {
            self.heap = 0;
        }
        if !(self.failing_sticks && serving == Serving::All) {
            self.serving = serving;
        }
        Ok(())
    }

    async fn stalled(&mut self) -> Result<u64, String> {
        self.rig("stalled")?;
        Ok(self.stalled)
    }

    async fn restart(&mut self) -> Result<(), String> {
        self.rig("restart")?;
        self.calls.push("restart".to_owned());
        self.sources.clear();
        self.heap = 0;
        if self.restart_drops_places {
            self.placed.clear();
        }
        Ok(())
    }

    fn alive(&mut self) -> bool {
        self.alive
    }

    fn fill_disk(&mut self) -> Result<u64, String> {
        self.rig("fill_disk")?;
        self.disk_full = true;
        Ok(3 << 20)
    }

    fn free_disk(&mut self) -> Result<(), String> {
        self.rig("free_disk")?;
        self.disk_full = self.full_sticks;
        Ok(())
    }

    fn now(&self) -> Duration {
        self.now
    }

    async fn pause(&mut self, time: Duration) {
        self.now += time;
    }

    async fn stop(self) -> Result<(), String> {
        self.stop_fails.map_or(Ok(()), Err)
    }
}
