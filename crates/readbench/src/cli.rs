//! The command line of `readbench`. [`run`] takes the arguments (without the program's name) and
//! the two streams it answers on, so the tests drive every verb against the in-process store;
//! [`main`] only starts a runtime and calls it.
//!
//! The verbs write their records, one JSON object per line, to the file `--out` names (appending)
//! and say what they did in words on the error stream; the verbs that are answers (`walk`,
//! `audit`, `report`) write to the output stream.
//!
//! Exit codes: 0 when the verb did what was asked, 1 when it failed, 2 when the arguments are not
//! a command.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use futures::FutureExt as _;
use tokio::runtime::Handle;

use crate::Error;
use crate::clock::MachineClock;
use crate::http::HttpClient;
use crate::load::{self, LoadPlan};
use crate::manifest::walk;
use crate::planrun::{Variant, parse_trees, plan_trees};
use crate::record::{
    ClientRun, DemandRead, DiskSample, MetricDelta, Mode, PlanRecord, read_lines, write_line,
};
use crate::replay::{self, Client, Knobs, Store};
use crate::report::{
    demand_table, disks_table, drive_classes_table, metrics_table, parse_class, plan_table,
    runs_table, trees_table,
};
use crate::sample::{Series, deltas, parse_metrics, sample_disks, to_f64};
use crate::sampletrees::{Query, sample_trees};
use crate::swfs::{FilerFetch, load_map, parse_server};

const MIB: u64 = 1 << 20;

#[derive(Parser)]
#[command(
    name = "readbench",
    about = "Farm validation of tierstore's read path: replays job trees against the object store, samples the drives and the nodes' metrics, and tables the results."
)]
struct Cli {
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// Reads a tree's manifests and prints what it holds.
    Walk(WalkArgs),
    /// Makes the L2 sidecars of trees (reads their packs for the cells), without fetching them.
    Prepare(PrepareArgs),
    /// Fetches trees with one or many clients and records each client's run.
    Replay(ReplayArgs),
    /// Reads single files of a tree with nothing fetched ahead and records each wait.
    Demand(DemandArgs),
    /// Samples block devices from /proc/diskstats.
    Disks(DisksArgs),
    /// Reads metrics pages twice, a window apart, and records what changed.
    Scrape(ScrapeArgs),
    /// Prints how many copies each volume of the store has.
    Audit(AuditArgs),
    /// Prints the Markdown tables of the records of a run.
    Report(ReportArgs),
    /// Plans trees without fetching them and records how many bytes the plan would fetch.
    Plan(PlanArgs),
    /// Lists the store's trees and prints a sample of them.
    SampleTrees(SampleTreesArgs),
}

#[derive(Args)]
struct FilerArgs {
    /// The filer's URL, such as http://192.0.2.22:8888.
    #[arg(long)]
    filer: String,
    /// The bucket the trees and packs are in.
    #[arg(long, default_value = "packs")]
    bucket: String,
    /// Milliseconds a request waits for its answer's headers.
    #[arg(long, default_value_t = 30_000)]
    timeout_ms: u64,
}

#[derive(Args)]
struct ServerArgs {
    /// The volume servers, `<node id>=<host:port>`.
    #[arg(long = "volume-server", num_args = 1..)]
    volume_server: Vec<String>,
}

#[derive(Args)]
struct KnobArgs {
    /// Streams each drive admits at once.
    #[arg(long, default_value_t = 1)]
    drive_streams: u32,
    /// The steady part size of each size class, in MiB.
    #[arg(long, value_delimiter = ',', default_value = "32,32,64")]
    part_sizes: Vec<u64>,
    /// Microseconds of one positioning of a cold drive.
    #[arg(long, default_value_t = 12_000)]
    cold_positioning_us: u32,
    /// MB/s a cold drive streams.
    #[arg(long, default_value_t = 190)]
    cold_media_mb_per_s: u32,
    /// Coverage in percent from which a pack is fetched whole.
    #[arg(long, default_value_t = 50)]
    whole_cov_percent: u8,
    /// The largest pack, in MiB, fetched whole from the lower coverage.
    #[arg(long, default_value_t = 64)]
    small_pack_mib: u64,
    /// Coverage in percent from which a small pack is fetched whole.
    #[arg(long, default_value_t = 20)]
    small_cov_percent: u8,
    /// Hash threads; the core is measured and the pool sized for 500 MB/s when absent.
    #[arg(long)]
    hash_threads: Option<std::num::NonZeroUsize>,
    /// Manifests read at once.
    #[arg(long, default_value_t = 16)]
    walk_parallel: usize,
    /// The shipped fill's piece, in MiB.
    #[arg(long, default_value_t = 8)]
    fill_chunk_mib: u64,
    /// The shipped fill's reads in flight.
    #[arg(long, default_value_t = 16)]
    fill_concurrency: usize,
    /// Cold drives of the store.
    #[arg(long, default_value_t = 12)]
    cold_drives: u32,
    /// Hot devices of the store.
    #[arg(long, default_value_t = 0)]
    hot_devices: u32,
}

#[derive(Args)]
struct WalkArgs {
    #[command(flatten)]
    filer: FilerArgs,
    /// The tree: the hex digest of its root manifest.
    #[arg(long)]
    tree: String,
    /// Manifests read at once.
    #[arg(long, default_value_t = 16)]
    parallel: usize,
}

#[derive(Args)]
struct PrepareArgs {
    #[command(flatten)]
    filer: FilerArgs,
    #[command(flatten)]
    servers: ServerArgs,
    /// The trees.
    #[arg(long, num_args = 1.., required = true)]
    tree: Vec<String>,
    /// The directory the sidecars are kept in.
    #[arg(long)]
    sidecars: PathBuf,
    /// Manifests and packs read at once.
    #[arg(long, default_value_t = 16)]
    parallel: usize,
}

#[derive(Args)]
struct ReplayArgs {
    #[command(flatten)]
    filer: FilerArgs,
    #[command(flatten)]
    servers: ServerArgs,
    #[command(flatten)]
    knobs: KnobArgs,
    /// The trees the clients take in turn.
    #[arg(long, num_args = 1.., required = true)]
    tree: Vec<String>,
    /// The directory the sidecars are kept in.
    #[arg(long)]
    sidecars: PathBuf,
    /// The directory the clients' caches are made in.
    #[arg(long)]
    cache: PathBuf,
    /// How the clients fetch.
    #[arg(long, value_parser = ["engine", "shipped"], default_value = "engine")]
    mode: String,
    /// The clients this node runs.
    #[arg(long, default_value_t = 1)]
    clients: u32,
    /// The clients of the run on every node (this node's, when absent).
    #[arg(long)]
    total_clients: Option<u32>,
    /// The index of this node's first client.
    #[arg(long, default_value_t = 0)]
    client_base: u32,
    /// The node's name in the records.
    #[arg(long)]
    node: String,
    /// The run's name in the records.
    #[arg(long)]
    run: String,
    /// Wait for this wall-clock time (milliseconds since the epoch) before the clients start.
    #[arg(long)]
    start_at: Option<u64>,
    /// The file the records are appended to.
    #[arg(long)]
    out: PathBuf,
    /// Keep the clients' cache directories.
    #[arg(long)]
    keep: bool,
    /// Prepare the tree without reading its packs and verify no cell: the first read of a tree
    /// then reaches the store's drives (the sidecars directory is not used).
    #[arg(long)]
    unverified: bool,
}

#[derive(Args)]
struct DemandArgs {
    #[command(flatten)]
    filer: FilerArgs,
    #[command(flatten)]
    servers: ServerArgs,
    #[command(flatten)]
    knobs: KnobArgs,
    /// The tree.
    #[arg(long)]
    tree: String,
    /// The directory the sidecars are kept in.
    #[arg(long)]
    sidecars: PathBuf,
    /// The directory the probe's cache is made in.
    #[arg(long)]
    cache: PathBuf,
    /// The files read.
    #[arg(long, default_value_t = 20)]
    probes: usize,
    /// The clients fetching at the same time on every node, as the records say.
    #[arg(long, default_value_t = 0)]
    load_clients: u32,
    /// The node's name in the records.
    #[arg(long)]
    node: String,
    /// The run's name in the records.
    #[arg(long)]
    run: String,
    /// The file the records are appended to.
    #[arg(long)]
    out: PathBuf,
    /// Prepare the tree without reading its packs and verify no cell: the first read of a tree
    /// then reaches the store's drives (the sidecars directory is not used).
    #[arg(long)]
    unverified: bool,
}

#[derive(Args)]
struct DisksArgs {
    /// The devices, such as sda,sdb.
    #[arg(long, value_delimiter = ',', required = true)]
    devices: Vec<String>,
    /// Milliseconds between readings.
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,
    /// The intervals sampled.
    #[arg(long)]
    count: u64,
    /// The run's name in the records.
    #[arg(long)]
    run: String,
    /// The node's name in the records.
    #[arg(long)]
    node: String,
    /// The file the records are appended to.
    #[arg(long)]
    out: PathBuf,
    /// The file read for the counters.
    #[arg(long, default_value = "/proc/diskstats")]
    diskstats: PathBuf,
}

#[derive(Args)]
struct ScrapeArgs {
    /// The metrics pages, `<node name>=<url>`.
    #[arg(long, num_args = 1.., required = true)]
    metrics: Vec<String>,
    /// Milliseconds between the two readings.
    #[arg(long)]
    window_ms: u64,
    /// The run's name in the records.
    #[arg(long)]
    run: String,
    /// The file the records are appended to.
    #[arg(long)]
    out: PathBuf,
    /// Milliseconds a request waits for its answer's headers.
    #[arg(long, default_value_t = 30_000)]
    timeout_ms: u64,
}

#[derive(Args)]
struct AuditArgs {
    #[command(flatten)]
    servers: ServerArgs,
    /// Milliseconds a request waits for its answer's headers.
    #[arg(long, default_value_t = 30_000)]
    timeout_ms: u64,
}

#[derive(Args)]
struct ReportArgs {
    /// The client runs.
    #[arg(long)]
    runs: PathBuf,
    /// The demand reads.
    #[arg(long)]
    demand: Option<PathBuf>,
    /// The drive samples.
    #[arg(long)]
    disks: Option<PathBuf>,
    /// The metric deltas.
    #[arg(long)]
    metrics: Option<PathBuf>,
    /// The drives the table keeps (all when absent).
    #[arg(long, value_delimiter = ',')]
    devices: Vec<String>,
    /// A class of drives, `NAME=node:dev,node:dev,...` (repeated): the drives table is then a
    /// row for each run and class instead of each device.
    #[arg(long = "class")]
    classes: Vec<String>,
    /// The `plan` records: adds the table of the plans' amplification.
    #[arg(long)]
    plans: Option<PathBuf>,
}

#[derive(Args)]
struct PlanArgs {
    #[command(flatten)]
    filer: FilerArgs,
    /// The trees to plan (hex digests of their root manifests).
    #[arg(long, num_args = 1..)]
    tree: Vec<String>,
    /// A file of trees, one per line, planned after the `--tree` ones.
    #[arg(long)]
    trees_file: Option<PathBuf>,
    /// A variant of the planner's constants, `NAME=WHOLE_COV,SMALL_MIB,SMALL_COV`; without one
    /// there is the variant `design` of the defaults.
    #[arg(long)]
    variant: Vec<String>,
    /// Pack sizes read at once.
    #[arg(long, default_value_t = 32)]
    parallel: usize,
    /// Manifests read at once when a tree is walked.
    #[arg(long, default_value_t = 16)]
    walk_parallel: usize,
    #[arg(long)]
    run: String,
    #[arg(long)]
    out: PathBuf,
}

#[derive(Args)]
struct SampleTreesArgs {
    #[command(flatten)]
    filer: FilerArgs,
    /// List the entries named after this key.
    #[arg(long, default_value = "")]
    after: String,
    /// Look at this many entries of the listing at most.
    #[arg(long, default_value_t = 100_000)]
    scan: u64,
    /// Keep entries modified at or after this RFC 3339 time.
    #[arg(long, default_value = "")]
    since: String,
    #[arg(long, default_value_t = 0)]
    min_bytes: u64,
    /// Entries larger than this are not read (the default is 16 MiB: a manifest is small, a
    /// loose blob may not be).
    #[arg(long, default_value_t = 16 * MIB)]
    max_bytes: u64,
    /// Keep manifests with at least this many directories.
    #[arg(long, default_value_t = 0)]
    min_dirs: usize,
    /// Stop after this many trees.
    #[arg(long, default_value_t = 20)]
    count: usize,
    /// The file the roots are written to, one per line; the output stream without it.
    #[arg(long)]
    out: Option<PathBuf>,
}

/// Says `line` on `w`. A reader that went away (a closed pipe) is not a failure of the verb.
fn say(w: &mut dyn Write, line: &str) {
    let _ = writeln!(w, "{line}");
}

/// The file `path`, opened to append records to.
fn open_out(path: &Path) -> Result<File, Error> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| Error::io("open", path, e))
}

/// Appends every record of `records` to the file `path`.
fn write_all<T: serde::Serialize>(path: &Path, records: &[T]) -> Result<(), Error> {
    let mut file = open_out(path)?;
    records
        .iter()
        .try_for_each(|r| write_line(&mut file, path, r))
}

impl KnobArgs {
    /// The knobs these flags make for a run of `total_clients` clients.
    fn knobs(&self, total_clients: u32) -> Result<Knobs, Error> {
        let [a, b, c] = self.part_sizes[..] else {
            return Err(Error::Usage(
                "--part-sizes takes three sizes in MiB, such as 32,32,64".to_owned(),
            ));
        };
        let mut k = Knobs::new(total_clients);
        k.budget.topology.drive_streams = self.drive_streams;
        k.budget.drive_permits = self.drive_streams.max(1);
        k.budget.topology.cold_drives = self.cold_drives;
        k.budget.topology.hot_devices = self.hot_devices;
        k.budget.part_sizes = [
            a.saturating_mul(MIB),
            b.saturating_mul(MIB),
            c.saturating_mul(MIB),
        ];
        k.plan.cold.positioning_us = self.cold_positioning_us;
        k.plan.cold.media_mb_per_s = self.cold_media_mb_per_s;
        k.plan.whole_cov_percent = self.whole_cov_percent;
        k.plan.small_pack_bytes = self.small_pack_mib.saturating_mul(MIB);
        k.plan.small_whole_cov_percent = self.small_cov_percent;
        k.hash_threads = self.hash_threads;
        k.walk_parallel = self.walk_parallel;
        k.fill_chunk = self.fill_chunk_mib.saturating_mul(MIB);
        k.fill_concurrency = self.fill_concurrency;
        k.budget
            .validate()
            .map_err(|e| Error::Usage(e.to_string()))?;
        Ok(k)
    }
}

/// The filer of `args`.
fn filer_of(args: &FilerArgs, http: &HttpClient) -> FilerFetch {
    FilerFetch {
        http: http.clone(),
        base: format!(
            "{}/buckets/{}",
            args.filer.trim_end_matches('/'),
            args.bucket
        ),
    }
}

/// The store of the arguments: the filer, and the volume map of the servers that answered (the
/// ones that did not are said on `err`). With `need_volumes` a map with no volume is an error:
/// the engine could not read a part.
async fn open_store(
    filer: &FilerArgs,
    servers: &ServerArgs,
    need_volumes: bool,
    err: &mut dyn Write,
) -> Result<Store, Error> {
    let listed = servers
        .volume_server
        .iter()
        .map(|a| parse_server(a))
        .collect::<Result<Vec<_>, _>>()?;
    let http = HttpClient::new(filer.timeout_ms);
    let (map, failed) = load_map(&http, &listed).await;
    for (server, e) in &failed {
        say(err, &format!("volume server {}: {e}", server.addr));
    }
    if need_volumes && map.volumes() == 0 {
        return Err(Error::ReadPath {
            what: "volume map",
            why: "no volume server gave a status page (--volume-server)".to_owned(),
        });
    }
    Ok(Store {
        fetch: filer_of(filer, &http),
        map: Arc::new(map),
        http,
    })
}

/// A client's run in words.
fn describe(r: &ClientRun) -> String {
    let mode = match r.mode {
        Mode::Engine => "engine",
        Mode::Shipped => "shipped",
    };
    format!(
        "{mode} client {} tree {}: ready in {:.3} s, {:.2} of {:.2} MiB fetched, {} GETs, {} failed",
        r.client,
        r.tree,
        to_f64(r.tree_ready_us) / 1e6,
        to_f64(r.bytes_fetched) / to_f64(MIB),
        to_f64(r.bytes_needed) / to_f64(MIB),
        r.total_gets(),
        r.get_errors,
    )
}

async fn verb(verb: Verb, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), Error> {
    let clock = MachineClock::shared();
    match verb {
        Verb::Walk(a) => {
            let fetch = filer_of(&a.filer, &HttpClient::new(a.filer.timeout_ms));
            let w = walk(&a.tree, &fetch, a.parallel).await?;
            say(
                out,
                &format!(
                    "tree {}: {} manifests, {} files, {} packs, {} bytes",
                    a.tree, w.manifests, w.files, w.packs, w.bytes
                ),
            );
        }
        Verb::Prepare(a) => {
            let store = open_store(&a.filer, &a.servers, false, err).await?;
            for tree in &a.tree {
                let p =
                    replay::prepare(&store, tree, &a.sidecars, a.parallel, clock.as_ref()).await?;
                let how = if p.sidecar_built { "made" } else { "found" };
                say(
                    err,
                    &format!(
                        "tree {tree}: {} manifests, {} files, {} packs, {} bytes; sidecar {how}",
                        p.walked.manifests, p.walked.files, p.walked.packs, p.walked.bytes
                    ),
                );
            }
        }
        Verb::Replay(a) => {
            let mode = if a.mode == "shipped" {
                Mode::Shipped
            } else {
                Mode::Engine
            };
            let total = a.total_clients.unwrap_or(a.clients);
            let knobs = a.knobs.knobs(total)?;
            let store = open_store(&a.filer, &a.servers, mode == Mode::Engine, err).await?;
            let plan = LoadPlan {
                trees: a.tree,
                clients: a.clients,
                client_base: a.client_base,
                total_clients: total,
                mode,
                start_at_wall_ms: a.start_at,
                keep: a.keep,
                unverified: a.unverified,
            };
            let runs = load::run(
                Arc::new(store),
                &plan,
                &knobs,
                &a.run,
                &a.node,
                &a.cache,
                &a.sidecars,
                clock,
                &Handle::current(),
            )
            .await?;
            write_all(&a.out, &runs)?;
            for r in &runs {
                say(err, &describe(r));
            }
        }
        Verb::Demand(a) => {
            let knobs = a.knobs.knobs(a.load_clients.max(1))?;
            let store = open_store(&a.filer, &a.servers, true, err).await?;
            let p = if a.unverified {
                replay::prepare_unverified(&store, &a.tree, knobs.walk_parallel, clock.as_ref())
                    .await?
            } else {
                replay::prepare(
                    &store,
                    &a.tree,
                    &a.sidecars,
                    knobs.walk_parallel,
                    clock.as_ref(),
                )
                .await?
            };
            let client = Client {
                run: a.run,
                node: a.node,
                client: 0,
                clients: a.load_clients.max(1),
                job: 0,
            };
            let cache = a.cache.join("demand");
            let reads = replay::run_demand(
                &store,
                &p,
                &knobs,
                &client,
                &cache,
                a.probes,
                a.load_clients,
                clock,
                &Handle::current(),
            )
            .await;
            // Housekeeping: nothing the records depend on is in the probe's cache.
            let _ = std::fs::remove_dir_all(&cache);
            let reads = reads?;
            write_all(&a.out, &reads)?;
            say(err, &format!("{} demand reads", reads.len()));
        }
        Verb::Disks(a) => {
            let mut file = open_out(&a.out)?;
            let mut pause = |ms: u64| tokio::time::sleep(Duration::from_millis(ms)).boxed();
            let n = sample_disks(
                &a.diskstats,
                &a.devices,
                &a.run,
                &a.node,
                a.interval_ms,
                a.count,
                clock.as_ref(),
                &mut pause,
                &mut file,
                &a.out,
            )
            .await?;
            say(err, &format!("{n} samples"));
        }
        Verb::Scrape(a) => scrape(&a, err).await?,
        Verb::Audit(a) => {
            let http = HttpClient::new(a.timeout_ms);
            let listed = a
                .servers
                .volume_server
                .iter()
                .map(|s| parse_server(s))
                .collect::<Result<Vec<_>, _>>()?;
            let (map, failed) = load_map(&http, &listed).await;
            say(out, &format!("{} volumes", map.volumes()));
            for (copies, volumes) in map.histogram() {
                say(out, &format!("{copies} copies: {volumes} volumes"));
            }
            for (server, e) in &failed {
                say(out, &format!("unreachable {}: {e}", server.addr));
            }
        }
        Verb::Plan(a) => plan(&a, err).await?,
        Verb::SampleTrees(a) => {
            let fetch = filer_of(&a.filer, &HttpClient::new(a.filer.timeout_ms));
            let query = Query {
                after: a.after,
                scan: a.scan,
                since: a.since,
                min_bytes: a.min_bytes,
                max_bytes: a.max_bytes,
                min_dirs: a.min_dirs,
                count: a.count,
            };
            let found = sample_trees(&fetch, &query).await?;
            let mut text = found.roots.join("\n");
            if !found.roots.is_empty() {
                text.push('\n');
            }
            match &a.out {
                Some(path) => {
                    std::fs::write(path, &text).map_err(|e| Error::io("write", path, e))?
                }
                None => {
                    let _ = out.write_all(text.as_bytes());
                }
            }
            say(
                err,
                &format!(
                    "{} trees of {} entries looked at, {} not manifests",
                    found.roots.len(),
                    found.scanned,
                    found.unparsed
                ),
            );
        }
        Verb::Report(a) => {
            let runs: Vec<ClientRun> = read_lines(&a.runs)?;
            let reads: Vec<DemandRead> = optional(a.demand.as_deref())?;
            let samples: Vec<DiskSample> = optional(a.disks.as_deref())?;
            let changes: Vec<MetricDelta> = optional(a.metrics.as_deref())?;
            let classes = a
                .classes
                .iter()
                .map(|c| parse_class(c))
                .collect::<Result<Vec<_>, _>>()?;
            let drives = if classes.is_empty() {
                disks_table(&samples, &a.devices)
            } else {
                drive_classes_table(&samples, &classes)
            };
            let mut tables = vec![
                ("Runs", runs_table(&runs)),
                ("Trees", trees_table(&runs)),
                ("Demand reads", demand_table(&reads)),
                ("Drives", drives),
                ("Node metrics", metrics_table(&changes, &[])),
            ];
            if let Some(path) = &a.plans {
                let plans: Vec<PlanRecord> = read_lines(path)?;
                tables.push(("Plans", plan_table(&plans)));
            }
            let mut text = String::new();
            for (heading, body) in tables {
                // Writing to a String does not fail.
                let _ = write!(text, "### {heading}\n\n{body}\n");
            }
            let _ = out.write_all(text.as_bytes());
        }
    }
    Ok(())
}

/// The records of the file `path`, none when no file is named.
fn optional<T: serde::de::DeserializeOwned>(path: Option<&Path>) -> Result<Vec<T>, Error> {
    path.map_or_else(|| Ok(Vec::new()), read_lines)
}

/// The series of each page, by the node's name.
async fn read_pages(
    http: &HttpClient,
    pages: &[(&str, &str)],
) -> Result<Vec<(String, Vec<Series>)>, Error> {
    let mut series = Vec::new();
    for (node, url) in pages {
        let body = http.get_bytes(url, false).await?;
        let text = String::from_utf8_lossy(&body);
        series.push(((*node).to_owned(), parse_metrics(&text)));
    }
    Ok(series)
}

/// Plans the trees of `a` and appends one record per tree and variant to its `--out`; a tree
/// that fails is named on `err` and left out, and only every tree failing fails the verb.
async fn plan(a: &PlanArgs, err: &mut dyn Write) -> Result<(), Error> {
    let mut trees = a.tree.clone();
    if let Some(path) = &a.trees_file {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io("read", path, e))?;
        trees.extend(parse_trees(&text));
    }
    if trees.is_empty() {
        return Err(Error::Usage(
            "plan needs a tree: --tree or --trees-file".to_owned(),
        ));
    }
    let variants = if a.variant.is_empty() {
        vec![Variant::design()]
    } else {
        a.variant
            .iter()
            .map(|v| Variant::parse(v))
            .collect::<Result<Vec<_>, _>>()?
    };
    let fetch = filer_of(&a.filer, &HttpClient::new(a.filer.timeout_ms));
    let planned = plan_trees(
        &fetch,
        &a.run,
        &trees,
        &variants,
        a.walk_parallel,
        a.parallel,
    )
    .await;
    write_all(&a.out, &planned.records)?;
    for (tree, e) in &planned.failed {
        say(err, &format!("tree {tree}: {e}"));
    }
    say(
        err,
        &format!(
            "{} of {} trees planned, {} failed",
            trees.len() - planned.failed.len(),
            trees.len(),
            planned.failed.len()
        ),
    );
    if planned.failed.len() == trees.len() {
        return Err(Error::ReadPath {
            what: "plan",
            why: "every tree failed".to_owned(),
        });
    }
    Ok(())
}

/// Two readings of each metrics page, `window_ms` apart, and the change of each series.
async fn scrape(a: &ScrapeArgs, err: &mut dyn Write) -> Result<(), Error> {
    let http = HttpClient::new(a.timeout_ms);
    let mut pages = Vec::new();
    for arg in &a.metrics {
        let (node, url) = arg.split_once('=').ok_or_else(|| {
            Error::Usage(format!("--metrics {arg:?}: expected <node name>=<url>"))
        })?;
        pages.push((node, url));
    }
    let before = read_pages(&http, &pages).await?;
    tokio::time::sleep(Duration::from_millis(a.window_ms)).await;
    let after = read_pages(&http, &pages).await?;
    let mut changes = Vec::new();
    for ((node, first), (_, second)) in before.iter().zip(&after) {
        changes.extend(deltas(&a.run, node, first, second, a.window_ms));
    }
    write_all(&a.out, &changes)?;
    say(err, &format!("{} series changed", changes.len()));
    Ok(())
}

/// Runs the command `args` (the arguments after the program's name), answering on `out` and
/// `err`; the exit code.
pub async fn run(args: &[&str], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let parsed = Cli::try_parse_from(std::iter::once("readbench").chain(args.iter().copied()));
    let cli = match parsed {
        Ok(cli) => cli,
        Err(e) => {
            if e.use_stderr() {
                let _ = write!(err, "{}", e.render());
                return 2;
            }
            let _ = write!(out, "{}", e.render());
            return 0;
        }
    };
    match verb(cli.verb, out, err).await {
        Ok(()) => 0,
        Err(e) => {
            say(err, &format!("readbench: {e}"));
            match e {
                Error::Usage(_) => 2,
                _ => 1,
            }
        }
    }
}

/// [`run`] on the runtime `start` makes; 1 and a word on `err` when it does not start.
pub fn run_with(
    args: &[&str],
    out: &mut dyn Write,
    err: &mut dyn Write,
    start: impl FnOnce() -> std::io::Result<tokio::runtime::Runtime>,
) -> u8 {
    match start() {
        Ok(rt) => rt.block_on(run(args, out, err)),
        Err(e) => {
            say(err, &format!("readbench: the runtime did not start: {e}"));
            1
        }
    }
}

/// The program: [`run`] on a multi-thread runtime, with the process's arguments and streams.
#[must_use]
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = run_with(
        &args,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
        tokio::runtime::Runtime::new,
    );
    ExitCode::from(code)
}
