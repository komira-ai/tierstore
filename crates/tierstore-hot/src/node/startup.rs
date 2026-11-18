//! Startup (10.3 "Hot restart", "A crash leaves only `.up` files"; 10.2 recovery).
//!
//! 1. **Sweep.** Every large upload in progress (`big/.<khash>.<nonce>.up`), every temporary
//!    seal state (`seals/*.tmp`) and repair file (`*.repair`) is removed: none of them was
//!    acknowledged. Cache segments are removed too: their names carry the boot epoch, and the
//!    index nodes drop every hint of an earlier epoch (10.5).
//! 2. **Segments.** A segment whose seal is committed is a sealed block: registered as such if
//!    the block table says it is sealed at its file's length, else converged (a crash between
//!    the commit and the trailer). Every other segment, written by this node or mirrored for
//!    another, is open: it is sealed by majority before anything of it is indexed (the
//!    take-over rule), so a record beyond the sealed length is never indexed. A segment that
//!    cannot be sealed now (this node is in a minority) is retried by the tick, and the node
//!    stays `indexing` until it is.
//! 3. **Large files** are registered for scrub and reclaim.
//! 4. **Index recovery** runs once every segment is sealed: the index's own replay from the mark
//!    or rebuild (`tierstore-index`), on the same files through [`crate::devfs::DevFs`].

use std::sync::Arc;

use tierstore_format::names::HotName;
use tierstore_index::backend::Backend;
use tierstore_index::blocks::BlockState;
use tierstore_index::recover::{RecoverError, Recovery, RecoveryReport};
use tierstore_index::tomblog::TombLog;
use tierstore_store_api::hot::{HotError, SealReason};
use tierstore_store_api::sched::DriveClass;

use super::{FileInfo, FileKind, HotNode, io_err, other_err, sealer};
use crate::devfs::DevFs;

/// What a start did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupReport {
    /// Files the sweep removed.
    pub swept: Vec<String>,
    /// Open segments sealed at startup.
    pub sealed: u64,
    /// Open segments that could not be sealed yet (the tick retries them).
    pub unsealed: u64,
    /// The index recovery, once every segment is sealed.
    pub recovery: Option<RecoveryReport>,
}

fn sweepable(dir: &str, name: &str) -> bool {
    let path = format!("{dir}/{name}");
    matches!(HotName::parse(&path), Some(HotName::LargeUpload { .. }))
        || name.ends_with(".tmp")
        || name.ends_with(".repair")
        || dir == "cache"
}

impl<B: Backend + 'static> HotNode<B> {
    /// The startup sweep, the seal of every open segment, and the index recovery (with the
    /// tombstone log `tomb` when the node keeps one). The caller marks the index ready once its
    /// tombstone pull is done too.
    pub async fn start(&self, tomb: Option<&TombLog>) -> Result<StartupReport, HotError> {
        let n = &self.n;
        let mut report = StartupReport::default();
        for dir in ["big", "seals", "seg", "blk", "cache"] {
            for name in self.list(dir).await? {
                if sweepable(dir, &name) {
                    let path = format!("{dir}/{name}");
                    n.drop_file(&path).await?;
                    report.swept.push(path);
                }
            }
        }
        let now = n.clock.now_ms();
        for name in self.list("seg").await? {
            let Some(HotName::Segment(id)) = HotName::parse(&format!("seg/{name}")) else {
                continue;
            };
            n.ids.observe(id);
            let path = HotName::Segment(id).path();
            let state = n.vote_state(id).await?;
            let len = self.file_len(&path).await?;
            let row = n.index.block(id).map_err(other_err("block table"))?;
            match (state.committed, row) {
                (Some(p), Some(info))
                    if info.state != BlockState::Open && info.length == len =>
                {
                    n.lock().files.insert(
                        path,
                        FileInfo {
                            kind: FileKind::Segment(id),
                            bytes: len,
                            data_len: p.length,
                            dirty: info.state == BlockState::Sealed,
                            last_read_ms: now,
                            last_scrub_ms: now,
                        },
                    );
                }
                (Some(p), _) => n.converge(id, p, &[]).await?,
                (None, _) => {
                    n.lock().unsealed.insert(id);
                }
            }
        }
        let unsealed: Vec<_> = n.lock().unsealed.iter().copied().collect();
        for id in unsealed {
            if sealer::seal_segment(n, id, SealReason::TakeOver).await.is_ok() {
                report.sealed += 1;
            }
        }
        for hh in self.list("big").await? {
            if hh.len() != 2 {
                continue;
            }
            for name in self.list(&format!("big/{hh}")).await? {
                let path = format!("big/{hh}/{name}");
                if let Some(HotName::Large(file)) = HotName::parse(&path) {
                    let bytes = self.file_len(&path).await?;
                    n.lock().files.insert(
                        path,
                        FileInfo {
                            kind: FileKind::Large(file),
                            bytes,
                            data_len: file.size,
                            dirty: true,
                            last_read_ms: now,
                            last_scrub_ms: now,
                        },
                    );
                }
            }
        }
        report.unsealed = n.lock().unsealed.len() as u64;
        if report.unsealed == 0 {
            report.recovery = Some(self.recover(tomb)?);
        }
        Ok(report)
    }

    /// The index recovery of 10.2 on this node's hot files (no cold drives: the cold store adds
    /// its own).
    pub fn recover(&self, tomb: Option<&TombLog>) -> Result<RecoveryReport, HotError> {
        let fs = DevFs::new(Arc::clone(&self.n.dev));
        Recovery {
            hot: &fs,
            cold: Vec::new(),
            tomb,
            geometry: self.n.cfg.geometry,
            window: super::converge::WINDOW,
        }
        .run(&*self.n.index)
        .map_err(|e: RecoverError| other_err("index recovery")(e))
    }

    /// Whether every segment found at startup is sealed.
    pub fn started(&self) -> bool {
        self.n.lock().unsealed.is_empty()
    }

    async fn list(&self, dir: &str) -> Result<Vec<String>, HotError> {
        let (dev, d) = (Arc::clone(&self.n.dev), dir.to_owned());
        self.n
            .sched
            .run(DriveClass::Write, move || dev.list(&d))
            .await?
            .map_err(io_err("list"))
    }

    async fn file_len(&self, path: &str) -> Result<u64, HotError> {
        let (dev, p) = (Arc::clone(&self.n.dev), path.to_owned());
        self.n
            .sched
            .run(DriveClass::Write, move || dev.len(&p))
            .await?
            .map_err(io_err("length"))
    }
}
