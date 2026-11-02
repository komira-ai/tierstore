//! The host of one run, inside the host namespace: the scratch root of the hook (the shared
//! host.rs of the conformance rig), two staged releases, the configuration of the two slots, the
//! service manager, and the real `nft`.
//!
//! Each release is a directory whose `tierstore` is the launcher (double/main.rs): it answers
//! `--capabilities` with `handover-v1` and otherwise becomes the server binary the run names, so
//! the hook sees two releases that can be handed over to without the shipped `CAPABILITIES`
//! changing. A run that wants a restart says so in the params file (`mode = "restart"`).
//!
//! The slots are 9301 and 9302 and the public port is 9300, as in the design: the namespace is the
//! run's own, so the numbers are free. A build node (`Node::Cache`) serves a cache directory that
//! both slots share (the first holds `<cache>/.lock`, the second is read-only until it can take
//! it) in front of a `dir:` upstream, with its writes through to the upstream; a storage node
//! (`Node::Storage`) has a `dir:` hot tier and a `dir:` cold tier and the lock file of its
//! reconciler. Each slot's OCI front listens on the slot's port plus 100, on both wildcards.

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tierstore::server::build_info::VERSION;
use tierstore::server::update_hook::{FORWARD_TABLE, HANDOVER_CAPABILITY, redirect_target};
use tierstore_testkit::dir::TestDir;

use crate::host::{Host, Ran, mode, write};
use crate::netns::nft_program;
use crate::services::{Exec, Services};
use crate::signer::{Request, sign};
use crate::wire::{Answer, Broke, Conn};
use std::process::Child;

/// The slot that serves at first.
pub const A: u16 = 9301;
/// The idle slot.
pub const B: u16 = 9302;
/// The public port.
pub const PUBLIC: u16 = 9300;
/// The bucket of the S3 front.
pub const BUCKET: &str = "t1-bucket";
/// The access key of the clients.
pub const KEY_ID: &str = "AKIDHANDOVER";
/// Its secret.
pub const SECRET: &str = "handover-secret";
/// The double, which is each stand-in by its name.
const DOUBLE: &str = env!("CARGO_BIN_EXE_tierstore-handover-double");
/// The settle of each server's drain (`--drain-lb-wait`): short, so that a stop ends soon, and
/// never zero (design section 7). No assertion depends on it.
pub const SETTLE: &str = "1s";
/// What a hook step waits between two looks at an instance.
const POLL_MS: u64 = 100;

/// The port of the OCI front of `slot`.
pub fn oci_port(slot: u16) -> u16 {
    slot + 100
}

/// What the slots run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Node {
    /// A build node over a shared cache directory.
    Cache,
    /// A storage node over a `dir:` hot tier.
    Storage,
}

/// What a run is made with.
pub struct Plan {
    /// The name of its scratch directory.
    pub name: &'static str,
    pub node: Node,
    /// The params file says `mode = "restart"`.
    pub restart: bool,
    /// The server binary the releases launch.
    pub server: PathBuf,
    /// The hook's `nft` is the double, which can kill the hook after a ruleset; else the real
    /// `nft` itself.
    pub nft_double: bool,
}

/// The host of a run.
pub struct Site {
    pub host: Host,
    pub services: Services,
    pub node: Node,
    pub base: PathBuf,
    pub tools: PathBuf,
    /// The cache directory (build node) or the hot tier (storage node).
    pub hot: PathBuf,
    pub server: PathBuf,
    restart: bool,
    /// Dropped last.
    _dir: TestDir,
}

/// Copies the double to `to`, as a file owned by the user the test runs as, mode 0755: a staged
/// release's binary must be a regular file that only its owner may write.
fn place_double(to: &Path) {
    fs::create_dir_all(to.parent().expect("a parent")).expect("a directory");
    fs::copy(DOUBLE, to).expect("copy the double");
    mode(to, 0o755);
}

/// A staged release in `dir` that launches `server` and can be handed over to.
fn release(dir: &Path, server: &Path) {
    place_double(&dir.join("tierstore"));
    write(&dir.join("real"), &format!("{}\n", server.display()));
    write(
        &dir.join("capabilities"),
        &format!("{HANDOVER_CAPABILITY}\n"),
    );
}

/// The configuration file of a build node on `port`.
fn cache_config(port: u16, hot: &Path, cold: &Path, key: &Path) -> String {
    let oci = oci_port(port);
    format!(
        "[server]\n\
         listen = [\"0.0.0.0:{port}\", \"[::]:{port}\"]\n\
         proxy_id = \"tierstore-{port}\"\n\
         slot = {port}\n\
         bucket = \"{BUCKET}\"\n\
         access_key_file = \"{key}\"\n\
         [local_cache]\n\
         path = \"{hot}\"\n\
         budget = \"256MiB\"\n\
         write_through = true\n\
         [cold_store]\n\
         kind = \"dir\"\n\
         path = \"{cold}\"\n\
         [oci]\n\
         listen = [\"0.0.0.0:{oci}\", \"[::]:{oci}\"]\n\
         [[policy]]\n\
         prefix = \"t1/\"\n\
         cache = true\n\
         [[policy]]\n\
         prefix = \"oci/\"\n\
         cache = true\n",
        key = key.display(),
        hot = hot.display(),
        cold = cold.display(),
    )
}

/// The command line of a storage node on `port`: the slot template's flags.
fn storage_args(base: &Path, port: u16, hot: &Path, cold: &Path, key: &Path) -> Vec<String> {
    let oci = oci_port(port);
    [
        format!("--listen=0.0.0.0:{port}"),
        format!("--listen=[::]:{port}"),
        format!("--slot={port}"),
        format!("--proxy-id=tierstore-{port}"),
        format!("--hot=dir:{}", hot.display()),
        format!("--cold=dir:{}", cold.display()),
        "--hot-bytes=268435456".to_owned(),
        format!("--bucket={BUCKET}"),
        format!("--access-key-file={}", key.display()),
        format!("--lock-file={}", base.join("instance.lock").display()),
        format!("--oci-listen=0.0.0.0:{oci}"),
        format!("--oci-listen=[::]:{oci}"),
        format!("--drain-lb-wait={SETTLE}"),
        "--drain-deadline=60s".to_owned(),
    ]
    .into()
}

impl Site {
    /// Lays the host of `plan` down: no process runs yet, the record says no update is in
    /// flight and `A` serves, and `A`'s link names the first release.
    pub fn new(plan: &Plan) -> Site {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), plan.name);
        let base = fs::canonicalize(dir.path()).expect("the scratch directory");
        let tools = base.join("tools");
        place_double(&tools.join("systemctl"));
        let nft = if plan.nft_double {
            place_double(&tools.join("nft"));
            let real = nft_program();
            write(&tools.join("real-nft"), &format!("{}\n", real.display()));
            tools.join("nft")
        } else {
            nft_program()
        };
        let (old, new) = (base.join("releases/one"), base.join("releases/two"));
        release(&old, &plan.server);
        release(&new, &plan.server);
        let (hot, cold) = (base.join("hot"), base.join("cold"));
        fs::create_dir_all(&hot).expect("the hot directory");
        fs::create_dir_all(&cold).expect("the cold directory");
        let key = base.join("keys/access");
        write(&key, &format!("{KEY_ID}\n{SECRET}\n"));
        mode(&key, 0o600);
        let exec: Exec = match plan.node {
            Node::Cache => {
                for port in [A, B] {
                    let config = cache_config(port, &hot, &cold, &key);
                    write(&base.join(format!("conf/{port}.toml")), &config);
                }
                let conf = base.join("conf");
                Box::new(move |port| {
                    vec![
                        "--config".to_owned(),
                        conf.join(format!("{port}.toml")).display().to_string(),
                        format!("--drain-lb-wait={SETTLE}"),
                        "--drain-deadline=60s".to_owned(),
                    ]
                })
            }
            Node::Storage => {
                let (base, hot, cold, key) = (base.clone(), hot.clone(), cold.clone(), key.clone());
                Box::new(move |port| storage_args(&base, port, &hot, &cold, &key))
            }
        };
        let host = Host::new(
            &base,
            [A, B],
            PUBLIC,
            new,
            VERSION,
            nft,
            tools.join("systemctl"),
            POLL_MS,
        );
        host.lay_down_files();
        symlink(&old, host.state.join("slots").join(A.to_string())).expect("a link");
        host.lay_down_idle_record();
        let services = Services::start(&tools, &host.state, [A, B], exec, &base.join("logs"));
        Site {
            host,
            services,
            node: plan.node,
            base,
            tools,
            hot,
            server: plan.server.clone(),
            restart: plan.restart,
            _dir: dir,
        }
    }

    /// What the params file has besides the update: `mode = "restart"` in a restart.
    fn extra(&self) -> &'static str {
        if self.restart {
            "mode = \"restart\"\n"
        } else {
            ""
        }
    }

    /// Starts the step `step` as the updater would, and returns at once.
    pub fn spawn_step(&self, step: &str) -> Child {
        self.host.spawn_step_with(step, self.extra())
    }

    /// Waits for a step [`Site::spawn_step`] started, printing what it said.
    pub fn finish_step(&self, step: &str, child: Child) -> Ran {
        Self::print(step, Host::finish(child))
    }

    /// Returns once the settle of the drain of `slot` is over: its S3 and its OCI listener refuse
    /// connections. Looks again until they do, as the repository's tests wait for a `/readyz`
    /// they expect; never by a timer.
    pub fn wait_settled(&self, slot: u16) {
        for port in [slot, oci_port(slot)] {
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            while !matches!(Conn::open(addr), Err(Broke::Refused)) {
                std::thread::yield_now();
            }
        }
    }

    /// Runs the step `step` as the updater would, printing what it said.
    pub fn step(&self, step: &str) -> Ran {
        Self::print(step, self.host.step_with(step, self.extra()))
    }

    fn print(step: &str, ran: Ran) -> Ran {
        eprintln!(
            "T1: {step}: exit {:?} {}",
            ran.code.or(ran.signal.map(|signal| -signal)),
            ran.line
        );
        ran
    }

    /// The boot of the host: the boot entry `restore` applies the guard and the redirect for
    /// `A` and starts it, and `A` then listens and is read-write.
    pub fn boot(&self) {
        self.step("restore").ok();
        self.services.wait_listening(A);
        let ready = self.get(A, "/readyz").expect("an answer");
        let expected = match self.node {
            Node::Cache => "ready read-write",
            Node::Storage => "ready",
        };
        assert_eq!(String::from_utf8_lossy(&ready.body), expected);
    }

    /// The hook's `nft` kills the hook right after it applied a ruleset that contains `text`.
    pub fn kill_hook_after(&self, text: &str) {
        write(&self.tools.join("kill-after"), &format!("{text}\n"));
    }

    /// The port the redirect names, from the real `nft`'s listing.
    pub fn redirect(&self) -> Option<u16> {
        let out = std::process::Command::new(nft_program())
            .args(["list", "table", "inet", FORWARD_TABLE])
            .output()
            .expect("run nft");
        redirect_target(&String::from_utf8_lossy(&out.stdout))
    }

    /// `GET path` on `127.0.0.1:port`, unsigned, on a connection of its own.
    pub fn get(&self, port: u16, path: &str) -> Result<Answer, Broke> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let host = vec![("host".to_owned(), addr.to_string())];
        Conn::open(addr)?.exchange("GET", path, &host, b"", false)
    }

    /// The text of `/metricz` on `127.0.0.1:port`.
    pub fn metricz(&self, port: u16) -> String {
        let answer = self.get(port, "/metricz").expect("an answer");
        assert_eq!(answer.status, 200);
        String::from_utf8_lossy(&answer.body).into_owned()
    }

    /// A signed S3 request for `key` to the public port, on a connection of its own.
    pub fn s3(&self, method: &str, key: &str, body: &[u8]) -> Result<Answer, Broke> {
        let addr = SocketAddr::from(([127, 0, 0, 1], PUBLIC));
        let headers = signed(method, &addr.to_string(), key, body);
        Conn::open(addr)?.exchange(method, &format!("/{BUCKET}/{key}"), &headers, body, false)
    }
}

impl Drop for Site {
    /// On a failed run, prints the end of every server's stdout and stderr before the scratch
    /// directory goes, so that a CI log says what the servers said.
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let logs = fs::read_dir(self.base.join("logs")).into_iter().flatten();
        for entry in logs.flatten() {
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(40)..].join("\n");
            eprintln!("T1: {}:\n{tail}", entry.path().display());
        }
    }
}

/// The headers of a signed S3 request: a create-only put (`If-None-Match: *`) for `PUT`.
pub fn signed(method: &str, host: &str, key: &str, body: &[u8]) -> Vec<(String, String)> {
    let create: &[(&str, &str)] = if method == "PUT" {
        &[("if-none-match", "*")]
    } else {
        &[]
    };
    let stamp = amz_now();
    let path = format!("/{BUCKET}/{key}");
    let request = Request {
        method,
        host,
        path: &path,
        query: &[],
        headers: create,
        body,
        stamp: &stamp,
    };
    sign(&request, KEY_ID, SECRET)
}

/// The time now as SigV4 writes it (`20270115T080000Z`): the servers run on the real clock, and
/// refuse a request whose time is far from theirs.
fn amz_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since the epoch to a civil date, in the proleptic Gregorian calendar (eras of 400
    // years from 0000-03-01).
    let z = i64::try_from(days).expect("a day") + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}
