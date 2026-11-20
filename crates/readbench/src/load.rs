//! Many clients fetching trees at once from one node: every distinct tree is prepared once, then
//! every client starts together (after a wall-clock start time the nodes of a run agree on), each
//! with its own cache directory.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use crate::Error;
use crate::clock::Clock;
use crate::record::{ClientRun, Mode};
use crate::replay::{Client, Knobs, Prepared, Store, prepare, run_engine, run_shipped};

/// What one node of a run does.
#[derive(Debug, Clone)]
pub struct LoadPlan {
    /// The trees (hex digests of root manifests) the clients take in turn.
    pub trees: Vec<String>,
    /// The clients this node runs.
    pub clients: u32,
    /// The index of this node's first client in the run.
    pub client_base: u32,
    /// The clients of the run on every node (the budget divides the store by this many).
    pub total_clients: u32,
    /// How the clients fetch.
    pub mode: Mode,
    /// The wall-clock time (milliseconds since the epoch) every node starts its clients at.
    pub start_at_wall_ms: Option<u64>,
    /// Whether to keep the clients' cache directories.
    pub keep: bool,
}

/// Returns once the clock's wall time has reached `wall_ms`, looking again after each wait of at
/// most 100 ms; at once when it is past.
pub async fn wait_until(clock: &dyn Clock, wall_ms: u64) {
    loop {
        let now = clock.wall_ms();
        if now >= wall_ms {
            return;
        }
        tokio::time::sleep(Duration::from_millis((wall_ms - now).min(100))).await;
    }
}

/// Prepares every distinct tree of `plan` once, waits for the start time, then runs the clients at
/// once: client `i` is client `client_base + i` of the run and fetches tree `trees[(client_base +
/// i) % trees.len()]` into `cache/c<client_base + i>`, which is removed afterwards unless the plan
/// keeps it. The records come back in client order.
///
/// # Errors
/// The plan has no tree or no client, a tree could not be prepared, or a client failed (the error
/// names the client and its tree).
#[expect(
    clippy::too_many_arguments,
    reason = "the signature the harness's command line and tests agree on"
)]
pub async fn run(
    store: Arc<Store>,
    plan: &LoadPlan,
    knobs: &Knobs,
    run: &str,
    node: &str,
    cache: &Path,
    sidecars: &Path,
    clock: Arc<dyn Clock>,
    rt: &Handle,
) -> Result<Vec<ClientRun>, Error> {
    if plan.trees.is_empty() || plan.clients == 0 {
        return Err(Error::Usage(
            "a load needs at least one tree and one client".to_owned(),
        ));
    }
    let mut prepared: BTreeMap<String, Arc<Prepared>> = BTreeMap::new();
    for tree in &plan.trees {
        if !prepared.contains_key(tree) {
            let p = prepare(&store, tree, sidecars, knobs.walk_parallel, clock.as_ref()).await?;
            prepared.insert(tree.clone(), Arc::new(p));
        }
    }
    let mut knobs = knobs.clone();
    knobs.budget.topology.clients = plan.total_clients.max(1);
    let knobs = Arc::new(knobs);
    if let Some(at) = plan.start_at_wall_ms {
        wait_until(clock.as_ref(), at).await;
    }
    let mut running = Vec::new();
    for i in 0..plan.clients {
        let global = plan.client_base + i;
        let tree = &plan.trees[global as usize % plan.trees.len()];
        let job = Job {
            store: store.clone(),
            p: prepared[tree].clone(),
            knobs: knobs.clone(),
            client: Client {
                run: run.to_owned(),
                node: node.to_owned(),
                client: global,
                clients: plan.total_clients,
                job: u64::from(global),
            },
            cache: cache.join(format!("c{global}")),
            mode: plan.mode,
            keep: plan.keep,
            clock: clock.clone(),
            rt: rt.clone(),
        };
        running.push((global, tree.clone(), rt.spawn(job.run())));
    }
    let mut records = Vec::new();
    for (global, tree, handle) in running {
        let done = handle.await.map_err(|e| e.to_string());
        let record = done
            .and_then(|r| r.map_err(|e| e.to_string()))
            .map_err(|why| Error::ReadPath {
                what: "client",
                why: format!("client {global} of tree {tree}: {why}"),
            })?;
        records.push(record);
    }
    Ok(records)
}

/// One client's task.
struct Job {
    store: Arc<Store>,
    p: Arc<Prepared>,
    knobs: Arc<Knobs>,
    client: Client,
    cache: std::path::PathBuf,
    mode: Mode,
    keep: bool,
    clock: Arc<dyn Clock>,
    rt: Handle,
}

impl Job {
    async fn run(self) -> Result<ClientRun, Error> {
        let record = match self.mode {
            Mode::Engine => {
                run_engine(
                    &self.store,
                    &self.p,
                    &self.knobs,
                    &self.client,
                    &self.cache,
                    self.clock.clone(),
                    &self.rt,
                )
                .await
            }
            Mode::Shipped => {
                run_shipped(
                    &self.store,
                    &self.p,
                    &self.knobs,
                    &self.client,
                    self.clock.clone(),
                )
                .await
            }
        };
        if !self.keep {
            // Housekeeping: a directory that is not there (the shipped way makes none) or that
            // cannot be removed leaves nothing the record depends on.
            let _ = std::fs::remove_dir_all(&self.cache);
        }
        record
    }
}
