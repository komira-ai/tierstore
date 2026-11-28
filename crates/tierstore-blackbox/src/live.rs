//! The rig of a running node: `fake-s3` started as its upstream, the node file written for it
//! ([`crate::config`]), the node started from the command the driver gives (the binary of the
//! package under test with `--config <file>`), and everything a case asks done from outside: the
//! provider's socket, the mount, `/metricz` on the node's S3 front, the placements in its cache
//! directory, `/proc/<pid>/status`, the upstream's control socket, a ballast file on its disk.
//!
//! A node counts as started once it has printed its start line and its provider's socket takes a
//! connection.

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use hyper::Method;
use objstore_s3::s3::fake::Serving;
use tierstore_trees_api::provider::v1::Source;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

use crate::clock::Stopwatch;
use crate::config::{Node, Paths};
use crate::metrics::Gauges;
use crate::os::{self, Process};
use crate::provider::{Bounds, Provider, Refusal};
use crate::rig::Rig;
use crate::tree::{Tree, TreeFault};

/// How often a start looks at the provider's socket.
const SOCKET_LOOK: Duration = Duration::from_millis(50);

/// How long a started program may take to print its start line (a node mounts first).
const START_WAIT: Duration = Duration::from_secs(60);

/// How long a stopped program may take to end after SIGTERM before it is killed: the node's
/// drain (`server.drain_lb_wait` and `server.drain_deadline` of its file) and the unmount.
const STOP_GRACE: Duration = Duration::from_secs(30);

/// The command that runs a node with the node file at a path.
pub type NodeCommand = Box<dyn Fn(&Path) -> Command + Send>;

/// What a case's rig is made of.
#[derive(Clone, Debug)]
pub struct Setup {
    /// The case's directory, which holds every file of the case.
    pub dir: PathBuf,
    /// The `fake-s3` program.
    pub fake_s3: PathBuf,
    /// A file system of its own for the cache, which the case may fill; none: the cache is in the
    /// case's directory.
    pub small_disk: Option<PathBuf>,
    /// The cache's budget, in MiB.
    pub budget_mib: u64,
    /// `trees.max_placements`, when the case sets it.
    pub max_placements: Option<u32>,
    /// How long a started node may take to take a connection on its provider's socket.
    pub socket_wait: Duration,
}

/// The rig of a running node (module docs).
pub struct Live {
    paths: Paths,
    file: PathBuf,
    command: NodeCommand,
    node: Option<Process>,
    metrics: String,
    upstream: Process,
    upstream_address: String,
    control: PathBuf,
    provider: Provider,
    cache: PathBuf,
    ballast: Option<PathBuf>,
    socket_wait: Duration,
    clock: Stopwatch,
}

impl Live {
    /// Starts the upstream and the node of `setup`, the node by `command`.
    ///
    /// # Errors
    /// What did not start, and why.
    pub async fn start(setup: Setup, command: NodeCommand) -> Result<Self, String> {
        let clock = Stopwatch::start();
        let paths = Paths::of(&setup.dir);
        std::fs::create_dir_all(&paths.logs)
            .map_err(|why| format!("{}: {why}", paths.logs.display()))?;
        let control = setup.dir.join("upstream.sock");
        let mut fake = Command::new(&setup.fake_s3);
        fake.arg("--control").arg(&control);
        let upstream =
            Process::start(fake, "fake-s3", &paths.logs, START_WAIT).map_err(|e| e.to_string())?;
        let upstream_address = upstream.listen().to_owned();
        let cache = setup
            .small_disk
            .as_ref()
            .map_or_else(|| setup.dir.join("cache"), |disk| disk.join("cache"));
        let node = Node {
            cache: cache.clone(),
            budget_mib: setup.budget_mib,
            upstream: upstream_address.clone(),
            max_placements: setup.max_placements,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
        };
        let file = node.write(&paths)?;
        let mut live = Self {
            provider: Provider::new(&paths.socket, Bounds::default()),
            paths,
            file,
            command,
            node: None,
            metrics: String::new(),
            upstream,
            upstream_address,
            control,
            cache,
            ballast: setup.small_disk.map(|disk| disk.join("ballast")),
            socket_wait: setup.socket_wait,
            clock,
        };
        live.start_node().await?;
        Ok(live)
    }

    /// Starts the node and waits until its provider's socket takes a connection.
    async fn start_node(&mut self) -> Result<(), String> {
        let command = (self.command)(&self.file);
        let started = Process::start(command, "tierstore", &self.paths.logs, START_WAIT)
            .map_err(|e| e.to_string())?;
        self.metrics = started.listen().to_owned();
        self.node = Some(started);
        let since = self.clock.elapsed();
        while std::os::unix::net::UnixStream::connect(&self.paths.socket).is_err() {
            if self.clock.elapsed().saturating_sub(since) >= self.socket_wait {
                return Err(format!(
                    "the node's provider socket {} took no connection within {:?}",
                    self.paths.socket.display(),
                    self.socket_wait
                ));
            }
            tokio::time::sleep(SOCKET_LOOK).await;
        }
        self.provider = Provider::new(&self.paths.socket, Bounds::default());
        Ok(())
    }

    /// Stops the node (SIGTERM, so that it takes its mount down) and the upstream.
    ///
    /// # Errors
    /// A process that could not be stopped, or a node that did not end as asked: with status 0,
    /// or by the SIGTERM itself.
    pub fn stop(mut self) -> Result<(), String> {
        let node = self.node.take().map(|node| node.terminate(STOP_GRACE));
        let upstream = self.upstream.terminate(STOP_GRACE);
        match node.transpose().map_err(|e| e.to_string())? {
            Some(status) if !status.success() && status.signal() != Some(15) => {
                Err(format!("the node ended {status} when asked to stop"))
            }
            _ => upstream.map(drop).map_err(|e| e.to_string()),
        }
    }

    fn node(&mut self) -> Result<&mut Process, String> {
        self.node
            .as_mut()
            .ok_or_else(|| "the node is not running".to_owned())
    }

    /// Sends `line` to the upstream's control socket: its answer.
    async fn control(&self, line: &str) -> Result<String, String> {
        let said = |why: std::io::Error| format!("fake-s3's control socket, {line:?}: {why}");
        let mut stream = UnixStream::connect(&self.control).await.map_err(said)?;
        stream
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(said)?;
        let mut answer = String::new();
        BufReader::new(stream)
            .read_line(&mut answer)
            .await
            .map_err(said)?;
        Ok(answer.trim_end().to_owned())
    }
}

/// The control command of `serving`.
fn command_of(serving: &Serving) -> String {
    match serving {
        Serving::All => "serve".to_owned(),
        Serving::Stalled { prefix } => format!("stall {prefix}"),
        Serving::Failing { prefix, status } => format!("fail {status} {prefix}"),
    }
}

/// The placements in the `placements` directory of the cache `cache`: its files whose names are
/// placement ids (32 hex digits); none when there is no such directory.
fn placements_in(cache: &Path) -> Result<usize, String> {
    let dir = cache.join("placements");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(why) => return Err(format!("{}: {why}", dir.display())),
    };
    let is_id = |name: &str| name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit());
    Ok(entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_str().is_some_and(is_id))
        .count())
}

impl Rig for Live {
    async fn prepare(&mut self, source: &Source, tree: &str) -> Result<(), Refusal> {
        self.provider.prepare(source, tree).await
    }

    async fn mount(&mut self, source: &Source) -> Result<PathBuf, Refusal> {
        self.provider.mount(source).await
    }

    async fn release(&mut self, source: &Source) -> Result<(), Refusal> {
        self.provider.release(source).await
    }

    async fn read(&mut self, at: &Path, tree: &Tree) -> Result<(), TreeFault> {
        tokio::task::block_in_place(|| tree.check(at))
    }

    fn placements(&mut self) -> Result<usize, String> {
        placements_in(&self.cache)
    }

    fn resident(&mut self) -> Result<u64, String> {
        self.node()?.resident_bytes().map_err(|e| e.to_string())
    }

    async fn gauges(&mut self) -> Result<Gauges, String> {
        let (status, body) =
            crate::http::call(&self.metrics, Method::GET, "/metricz", Vec::new()).await?;
        (status == 200)
            .then(|| Gauges::of(&String::from_utf8_lossy(&body)))
            .ok_or_else(|| format!("GET http://{}/metricz answered {status}", self.metrics))
    }

    async fn put(&mut self, tree: &Tree) -> Result<(), String> {
        for (key, bytes) in tree.objects() {
            let path = format!("/cas/{key}");
            let (status, _) =
                crate::http::call(&self.upstream_address, Method::PUT, &path, bytes.clone())
                    .await?;
            if status != 200 {
                return Err(format!("PUT {path} to fake-s3 answered {status}"));
            }
        }
        Ok(())
    }

    async fn serve_as(&mut self, serving: Serving) -> Result<(), String> {
        let line = command_of(&serving);
        let answer = self.control(&line).await?;
        (answer == "ok")
            .then_some(())
            .ok_or_else(|| format!("fake-s3 answered {line:?} with {answer:?}"))
    }

    async fn stalled(&mut self) -> Result<u64, String> {
        let answer = self.control("stats").await?;
        answer
            .split_whitespace()
            .find_map(|word| word.strip_prefix("stalled="))
            .and_then(|count| count.parse().ok())
            .ok_or_else(|| format!("fake-s3 answered stats with {answer:?}"))
    }

    async fn restart(&mut self) -> Result<(), String> {
        let node = self.node.take().ok_or("the node is not running")?;
        node.kill().map_err(|e| e.to_string())?;
        self.start_node().await
    }

    fn alive(&mut self) -> bool {
        self.node.as_mut().is_some_and(Process::alive)
    }

    fn fill_disk(&mut self) -> Result<u64, String> {
        let ballast = self
            .ballast
            .as_ref()
            .ok_or("this case's cache has no disk of its own")?;
        os::fill(ballast).map_err(|e| e.to_string())
    }

    fn free_disk(&mut self) -> Result<(), String> {
        let ballast = self
            .ballast
            .as_ref()
            .ok_or("this case's cache has no disk of its own")?;
        std::fs::remove_file(ballast).map_err(|why| format!("{}: {why}", ballast.display()))
    }

    fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    async fn pause(&mut self, time: Duration) {
        tokio::time::sleep(time).await;
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
