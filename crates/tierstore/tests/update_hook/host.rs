//! The scratch host the real `tierstore update-hook` runs against, apart from what acts on it: the
//! root with the declaration, `etc/group`, `etc/passwd`, `proc/sys` and the state directory; the
//! params file the updater would write; and the hook's command line, with the flags of the
//! conformance build (`--root`, `--root-uid`, `--nft`, `--systemctl`, `--public-port`,
//! `--poll-interval-ms`) pointing it at the host, its programs and its ports.
//!
//! Two rigs use it: the conformance test (rig.rs, against fake `nft` and `systemctl` and a stub
//! of the instances), and the hand-over test under load (tests/handover, against the real `nft`
//! in a network namespace and the real servers), which takes this file by its path. Not every
//! helper is used by both.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use serde_json::Value;
use tierstore::server::update_hook::{DECLARATION_PATH, Record, unit_name};

/// The binary under test.
pub const HOOK: &str = env!("CARGO_BIN_EXE_tierstore");
/// The service account the declaration names.
pub const SERVICE: &str = "tierstore";
/// The boot the host is in at first.
pub const BOOT: &str = "boot-one";
/// The state directory the declaration names, under the scratch root.
pub const DECLARED_STATE: &str = "/var/lib/updater/hooks/tierstore";

/// How a run of the hook ended.
#[derive(Debug)]
pub struct Ran {
    /// The exit status; `None` when a signal ended it.
    pub code: Option<i32>,
    /// The signal that ended it.
    pub signal: Option<i32>,
    /// The last line of its standard output; `null` when there is none.
    pub line: Value,
    /// Its standard error.
    pub stderr: String,
}

impl Ran {
    fn of(out: &Output) -> Ran {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout.lines().last().map_or(Value::Null, |line| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{line:?} is not JSON: {e}"))
        });
        Ran {
            code: out.status.code(),
            signal: out.status.signal(),
            line,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Asserts exit 0.
    pub fn ok(&self) -> &Ran {
        assert_eq!(self.code, Some(0), "{self:?}");
        self
    }

    /// Asserts exit 1 with `reason`.
    pub fn fails(&self, reason: &str) -> &Ran {
        assert_eq!(self.code, Some(1), "{self:?}");
        assert_eq!(self.line["reason"], reason, "{self:?}");
        self
    }

    /// Whether the run was killed with SIGKILL.
    pub fn killed(&self) -> bool {
        self.signal == Some(9)
    }

    /// The `detail` of the last line.
    pub fn detail(&self) -> String {
        self.line["detail"].as_str().unwrap_or_default().to_owned()
    }
}

/// Gives `path` the permission bits `mode`.
pub fn mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set a mode");
}

/// Writes `text` to `path`, making its parents.
pub fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("a parent")).expect("make the parents");
    fs::write(path, text).expect("write a file");
}

/// The uid the tests run as, which owns every file of the scratch host.
pub fn uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// The gid the tests run as.
pub fn gid() -> u32 {
    rustix::process::getegid().as_raw()
}

/// A scratch host: its two slots, its public port, its directories and the programs the hook
/// is pointed at.
pub struct Host {
    /// The slot that runs at first, the lower of the two ports.
    pub a: u16,
    /// The idle slot.
    pub b: u16,
    /// The public port of this host.
    pub public: u16,
    pub root: PathBuf,
    pub state: PathBuf,
    /// The staged release the params file names.
    pub new: PathBuf,
    /// The `nft` the hook runs.
    pub nft: PathBuf,
    /// The `systemctl` the hook runs.
    pub systemctl: PathBuf,
    /// What `--poll-interval-ms` says.
    pub poll_ms: u64,
    /// The version the params file names as the target.
    target: RefCell<String>,
    /// The directory the params files are written to and the hook runs in.
    base: PathBuf,
}

impl Host {
    /// A host under `base` (its root is `base/root`) with the slots `a` and `b` and the port
    /// `public`, whose update stages `new` at the version `target`, and whose hook runs `nft`
    /// and `systemctl` and looks at an instance every `poll_ms` milliseconds. Nothing is
    /// written: see [`Host::lay_down_files`].
    #[expect(clippy::too_many_arguments, reason = "one host, said once")]
    pub fn new(
        base: &Path,
        [a, b]: [u16; 2],
        public: u16,
        new: PathBuf,
        target: &str,
        nft: PathBuf,
        systemctl: PathBuf,
        poll_ms: u64,
    ) -> Host {
        let root = base.join("root");
        let state = root.join(DECLARED_STATE.trim_start_matches('/'));
        Host {
            a,
            b,
            public,
            root,
            state,
            new,
            nft,
            systemctl,
            poll_ms,
            target: RefCell::new(target.to_owned()),
            base: base.to_owned(),
        }
    }

    /// The files of the host: the declaration, `etc/group` (the service account in the group
    /// of the state directory), `etc/passwd`, the boot id, the connection-tracking bounds of a
    /// host that is ready, and the state directory with its `slots`, all owned by the user the
    /// tests run as. No record and no link.
    pub fn lay_down_files(&self) {
        let declaration = self.root.join(DECLARATION_PATH.trim_start_matches('/'));
        write(&declaration, &self.declaration_text(DECLARED_STATE));
        mode(&declaration, 0o644);
        mode(&self.root.join("etc"), 0o755);
        mode(&self.root.join("etc/updater.d"), 0o755);
        write(
            &self.root.join("etc/group"),
            &format!("tierstore-update:x:{}:{SERVICE}\n", gid()),
        );
        write(
            &self.root.join("etc/passwd"),
            &format!(
                "{SERVICE}:x:{}:{}::/:/usr/sbin/nologin\n",
                uid() + 1,
                gid() + 1
            ),
        );
        self.set_boot(BOOT);
        let netfilter = self.root.join("proc/sys/net/netfilter");
        write(
            &netfilter.join("nf_conntrack_tcp_timeout_established"),
            "432000\n",
        );
        write(&netfilter.join("nf_conntrack_count"), "12\n");
        write(&netfilter.join("nf_conntrack_max"), "262144\n");
        fs::create_dir_all(self.state.join("slots")).expect("the state directory");
        mode(&self.state, 0o750);
        mode(&self.state.join("slots"), 0o750);
    }

    /// The record of a host with no update in flight, `a` serving.
    pub fn lay_down_idle_record(&self) {
        self.set_record_bytes(&Record::idle(self.a).encode(), 0o600);
    }

    /// A declaration of the two slots with `state_dir`.
    pub fn declaration_text(&self, state_dir: &str) -> String {
        let (a, b) = (self.a, self.b);
        format!(
            "uid = \"{SERVICE}\"\n\
             state_dir = \"{state_dir}\"\n\
             health_ports = [{a}, {b}]\n\
             \n\
             [hook]\n\
             units = [\"{}\", \"{}\"]\n",
            unit_name(a),
            unit_name(b)
        )
    }

    /// Replaces the declaration's text.
    pub fn set_declaration(&self, text: &str) {
        write(
            &self.root.join(DECLARATION_PATH.trim_start_matches('/')),
            text,
        );
    }

    /// Makes the params file name `version` as the target version.
    pub fn set_target(&self, version: &str) {
        version.clone_into(&mut self.target.borrow_mut());
    }

    /// The text of the params file the updater would write: an update to the new release,
    /// then `extra`.
    pub fn params_text(&self, extra: &str) -> String {
        format!(
            "promotion_bound_secs = 900\n\
             \"updater.staged_release\" = \"{}\"\n\
             \"updater.target_version\" = \"{}\"\n\
             \"updater.promotion_ceiling_secs\" = 3600\n\
             {extra}",
            self.new.display(),
            self.target.borrow()
        )
    }

    /// `tierstore update-hook` with the flags of this host, `--root-uid` only when `root_uid` is
    /// given, then `words`.
    fn command(&self, root_uid: Option<u32>, words: &[&str]) -> Command {
        let mut command = Command::new(HOOK);
        command.arg("update-hook").arg("--root").arg(&self.root);
        if let Some(root_uid) = root_uid {
            command.args(["--root-uid", &root_uid.to_string()]);
        }
        command
            .arg("--nft")
            .arg(&self.nft)
            .arg("--systemctl")
            .arg(&self.systemctl)
            .args(["--public-port", &self.public.to_string()])
            .args(["--poll-interval-ms", &self.poll_ms.to_string()])
            .args(words)
            .current_dir(&self.base);
        command
    }

    /// Runs the hook with the flags of this host, then `words`.
    pub fn hook(&self, words: &[&str]) -> Ran {
        self.hook_trusting(Some(uid()), words)
    }

    /// Runs the hook with `--root-uid` only when `root_uid` is given.
    pub fn hook_trusting(&self, root_uid: Option<u32>, words: &[&str]) -> Ran {
        Ran::of(
            &self
                .command(root_uid, words)
                .output()
                .expect("run the hook"),
        )
    }

    /// The words of `step` as the updater gives them: `prepare` with the staged release, the
    /// five steps with the params file (its text from [`Host::params_text`] with `extra`),
    /// `restore` with nothing.
    fn words(&self, step: &str, extra: &str) -> Vec<String> {
        if step == "restore" {
            return vec![step.to_owned()];
        }
        let params = self.base.join(format!("params-{step}.toml"));
        write(&params, &self.params_text(extra));
        let params = params.display().to_string();
        let mut words = vec![step.to_owned()];
        if step == "prepare" {
            words.push(self.new.display().to_string());
        }
        words.extend(["--params".to_owned(), params]);
        words
    }

    /// Runs `step` as the updater would, with `extra` in the params file.
    pub fn step_with(&self, step: &str, extra: &str) -> Ran {
        let words = self.words(step, extra);
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        self.hook(&words)
    }

    /// Runs `step` with the plain params file.
    pub fn step(&self, step: &str) -> Ran {
        self.step_with(step, "")
    }

    /// Starts `step` and returns at once: the running hook, its output piped.
    pub fn spawn_step(&self, step: &str) -> Child {
        self.spawn_step_with(step, "")
    }

    /// Starts `step` with `extra` in the params file, and returns at once.
    pub fn spawn_step_with(&self, step: &str, extra: &str) -> Child {
        let words = self.words(step, extra);
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        self.command(Some(uid()), &words)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the hook")
    }

    /// Waits for a hook [`Host::spawn_step`] started.
    pub fn finish(child: Child) -> Ran {
        Ran::of(&child.wait_with_output().expect("wait for the hook"))
    }

    /// The bytes of the record.
    pub fn record_bytes(&self) -> Vec<u8> {
        fs::read(self.state.join("state.json")).expect("the record")
    }

    /// The record.
    pub fn record(&self) -> Record {
        Record::decode(&self.record_bytes()).expect("a record")
    }

    /// Replaces the record with `bytes` and gives it `mode`.
    pub fn set_record_bytes(&self, bytes: &[u8], file_mode: u32) {
        let path = self.state.join("state.json");
        fs::write(&path, bytes).expect("write the record");
        mode(&path, file_mode);
    }

    /// Where the link of `port` points.
    pub fn link(&self, port: u16) -> Option<PathBuf> {
        fs::read_link(self.state.join("slots").join(port.to_string())).ok()
    }

    /// Writes the boot id.
    pub fn set_boot(&self, boot: &str) {
        write(
            &self.root.join("proc/sys/kernel/random/boot_id"),
            &format!("{boot}\n"),
        );
    }
}
