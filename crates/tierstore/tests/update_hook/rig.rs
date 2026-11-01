//! A host for the real `tierstore update-hook`: a scratch root with the declaration, `etc/group`,
//! `etc/passwd`, `proc/sys` and the state directory; two staged releases; the fake programs; and
//! the stub instances. The hook is the binary itself, run with `--root`, `--root-uid`, `--nft`
//! and `--systemctl` pointing at the scratch host.
//!
//! At the start one slot (`a`) runs the old release, its unit active and the redirect naming it,
//! as a host is after a first install; the other (`b`) has no link and no unit. The record says
//! no update is in flight. No guard table is loaded yet, so an update places it.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;
use tierstore::server::update_hook::machine::LoopbackInstances;
use tierstore::server::update_hook::{
    DECLARATION_PATH, Instances, PUBLIC_PORT, Record, redirect_ruleset, unit_name,
};
use tierstore_testkit::dir::TestDir;
use tierstore_testkit::port::Reserved;

use crate::stub::{SlotScript, Stub};
use crate::world::{self, Unit};

/// The binary under test.
pub const HOOK: &str = env!("CARGO_BIN_EXE_tierstore");
/// The stand-in programs.
pub const FAKE: &str = env!("CARGO_BIN_EXE_tierstore-hook-fake");
/// The service account the declaration names.
pub const SERVICE: &str = "tierstore";
/// The version of the release that runs at first.
pub const OLD_VERSION: &str = "1.0.0";
/// The version of the release the update installs.
pub const NEW_VERSION: &str = "2.0.0";
/// The boot the host is in at first.
pub const BOOT: &str = "boot-one";
/// The capability that lets a release be handed over to.
pub const HANDOVER: &str = "handover-v1";

/// `nft list table inet kbf_guard_tierstore`, as the fake logs it.
pub const LIST_GUARD: &str = "nft list table inet kbf_guard_tierstore";
/// `nft list table inet kbf_forward_tierstore`, as the fake logs it.
pub const LIST_FORWARD: &str = "nft list table inet kbf_forward_tierstore";
/// `nft -f -`, a ruleset applied in one transaction.
pub const APPLY: &str = "nft -f -";
/// `nft delete table inet kbf_guard_tierstore`.
pub const DELETE_GUARD: &str = "nft delete table inet kbf_guard_tierstore";

/// `systemctl start` of the slot unit of `port`, as the fake logs it.
pub fn start(port: u16) -> String {
    format!("systemctl start -- {}", unit_name(port))
}

/// `systemctl stop` of the slot unit of `port`.
pub fn stop(port: u16) -> String {
    format!("systemctl stop -- {}", unit_name(port))
}

/// What the scratch host is made with.
#[derive(Clone, Copy, Debug)]
pub struct Setup {
    /// The old release lists `handover-v1`.
    pub old_handover: bool,
    /// The new release lists `handover-v1`.
    pub new_handover: bool,
    /// The stub serves the public port (and the test holds its lock).
    pub public: bool,
}

impl Setup {
    /// Both releases can hand over, and the public port is served.
    pub fn handover() -> Setup {
        Setup {
            old_handover: true,
            new_handover: true,
            public: true,
        }
    }

    /// Both releases can hand over; the public port is not served (a test that never probes it).
    pub fn private() -> Setup {
        Setup {
            public: false,
            ..Setup::handover()
        }
    }
}

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

/// What the fakes hold: the active units (with the release each runs) and the loaded tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub units: BTreeMap<u16, PathBuf>,
    pub tables: BTreeMap<String, Vec<String>>,
}

/// The scratch host and its stub.
pub struct Rig {
    pub stub: Stub,
    /// The slot that runs at first.
    pub a: u16,
    /// The idle slot.
    pub b: u16,
    pub root: PathBuf,
    pub state: PathBuf,
    /// The directory of the fake programs.
    pub tools: PathBuf,
    /// The fakes' `state` directory.
    pub world: PathBuf,
    /// The release that runs at first.
    pub old: PathBuf,
    /// The staged release.
    pub new: PathBuf,
    /// The version the params file names as the target.
    target: RefCell<String>,
    base: PathBuf,
    /// Dropped last: the stub and the processes are gone before the files are.
    _dir: TestDir,
}

fn mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set a mode");
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("a parent")).expect("make the parents");
    fs::write(path, text).expect("write a file");
}

/// Puts the fake program at `to`: a hard link, or a copy across file systems.
pub fn place_fake(to: &Path) {
    if fs::hard_link(FAKE, to).is_err() {
        fs::copy(FAKE, to).expect("copy the fake");
    }
    mode(to, 0o755);
}

/// A staged release: the fake's `tierstore`, its capabilities and its version.
fn release(dir: &Path, version: &str, handover: bool) {
    fs::create_dir_all(dir).expect("a release");
    place_fake(&dir.join("tierstore"));
    let caps = if handover {
        format!("{HANDOVER}\n")
    } else {
        String::new()
    };
    write(&dir.join("capabilities"), &caps);
    write(&dir.join("version"), &format!("{version}\n"));
}

/// The uid the tests run as, which owns every file of the scratch host.
pub fn uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn gid() -> u32 {
    rustix::process::getegid().as_raw()
}

impl Rig {
    /// A scratch host as `setup` says.
    pub fn new(name: &str, setup: Setup) -> Rig {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let base = fs::canonicalize(dir.path()).expect("the scratch directory");
        // `a` is the lower port, so a list of both slots in port order is `[a, b]`.
        let (mut held_a, mut held_b) = (Reserved::new(), Reserved::new());
        if held_a.port() > held_b.port() {
            std::mem::swap(&mut held_a, &mut held_b);
        }
        let (a, b) = (held_a.port(), held_b.port());
        let old = base.join("releases/old");
        let new = base.join("releases/new");
        release(&old, OLD_VERSION, setup.old_handover);
        release(&new, NEW_VERSION, setup.new_handover);
        let tools = base.join("tools");
        let world = tools.join("state");
        fs::create_dir_all(&world).expect("the fakes' world");
        place_fake(&tools.join("nft"));
        place_fake(&tools.join("systemctl"));
        let root = base.join("root");
        let state = root.join("var/lib/updater/hooks/tierstore");
        let rig = Rig {
            stub: Stub::start(world.clone(), [held_a, held_b], setup.public),
            a,
            b,
            root,
            state,
            tools,
            world,
            old,
            new,
            target: RefCell::new(NEW_VERSION.to_owned()),
            base,
            _dir: dir,
        };
        rig.lay_down();
        rig
    }

    /// The files of the host, the record of a host with no update in flight, and the old slot
    /// running behind the redirect.
    fn lay_down(&self) {
        let declaration = self.root.join(DECLARATION_PATH.trim_start_matches('/'));
        write(
            &declaration,
            &self.declaration_text(&self.declared_state_dir()),
        );
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
        symlink(&self.old, self.state.join("slots").join(self.a.to_string())).expect("a link");
        self.set_record_bytes(&Record::idle(self.a).encode(), 0o600);
        write(
            &self.world.join("exec"),
            &format!("{}/slots/%i/tierstore", self.state.display()),
        );
        self.fake("nft", &["-f", "-"], &redirect_ruleset(PUBLIC_PORT, self.a));
        self.fake("systemctl", &["start", "--", &unit_name(self.a)], "");
        for log in ["calls", "effects"] {
            fs::remove_file(self.world.join(log)).expect("a log of the fakes");
        }
    }

    /// The `state_dir` the declaration names: the state directory itself, which the hook opens
    /// by this path.
    pub fn declared_state_dir(&self) -> String {
        self.state.display().to_string()
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

    /// Runs a fake program directly, as the test, with `input` on its standard input.
    fn fake(&self, program: &str, args: &[&str], input: &str) {
        let mut child = Command::new(self.tools.join(program))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("run a fake");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write the input");
        assert!(child.wait().expect("wait").success(), "{program} {args:?}");
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

    /// Runs `tierstore update-hook` with the flags of this host, then `words`.
    pub fn hook(&self, words: &[&str]) -> Ran {
        self.hook_trusting(Some(uid()), words)
    }

    /// Runs `tierstore update-hook` with the flags of this host, `--root-uid` only when
    /// `root_uid` is given, then `words`.
    pub fn hook_trusting(&self, root_uid: Option<u32>, words: &[&str]) -> Ran {
        let mut command = Command::new(HOOK);
        command.arg("update-hook").arg("--root").arg(&self.root);
        if let Some(root_uid) = root_uid {
            command.args(["--root-uid", &root_uid.to_string()]);
        }
        let out = command
            .arg("--nft")
            .arg(self.tools.join("nft"))
            .arg("--systemctl")
            .arg(self.tools.join("systemctl"))
            .args(words)
            .current_dir(&self.base)
            .output()
            .expect("run the hook");
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

    /// Runs `step` as the updater would: `prepare` with the staged release, the five steps with
    /// the params file (its text from [`Rig::params_text`] with `extra`), `restore` with nothing.
    pub fn step_with(&self, step: &str, extra: &str) -> Ran {
        if step == "restore" {
            return self.hook(&["restore"]);
        }
        let params = self.base.join("params.toml");
        write(&params, &self.params_text(extra));
        let params = params.to_str().expect("a path");
        if step == "prepare" {
            let new = self.new.to_str().expect("a path");
            self.hook(&["prepare", new, "--params", params])
        } else {
            self.hook(&[step, "--params", params])
        }
    }

    /// Runs `step` with the plain params file.
    pub fn step(&self, step: &str) -> Ran {
        self.step_with(step, "")
    }

    /// The calls the fakes have logged.
    pub fn calls(&self) -> Vec<String> {
        world::lines(&self.world, "calls")
    }

    /// The calls logged since there were `mark`.
    pub fn calls_since(&self, mark: usize) -> Vec<String> {
        self.calls()[mark..].to_vec()
    }

    /// The changes the fakes have made.
    pub fn effects(&self) -> Vec<String> {
        world::lines(&self.world, "effects")
    }

    /// What the fakes hold.
    pub fn machine(&self) -> Machine {
        Machine {
            units: world::units(&self.world)
                .into_iter()
                .map(|(port, Unit { release, .. })| (port, release))
                .collect(),
            tables: world::tables(&self.world),
        }
    }

    /// The ports of the active units.
    pub fn running(&self) -> Vec<u16> {
        world::units(&self.world).into_keys().collect()
    }

    /// The port the redirect table names.
    pub fn redirect(&self) -> Option<u16> {
        world::redirect(&self.world)
    }

    /// Whether the table `name` is loaded.
    pub fn has_table(&self, name: &str) -> bool {
        world::tables(&self.world).contains_key(name)
    }

    /// The record.
    pub fn record(&self) -> Record {
        let bytes = fs::read(self.state.join("state.json")).expect("the record");
        Record::decode(&bytes).expect("a record")
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

    /// Makes the fake kill the hook after its `n`-th call from now on; `None` stops that.
    pub fn kill_after(&self, n: Option<usize>) {
        let file = self.world.join("kill-after");
        match n {
            Some(n) => write(&file, &format!("{}\n", self.calls().len() + n)),
            None => {
                let _ = fs::remove_file(file);
            }
        }
    }

    /// Makes the fake `systemctl` check `path` (`%i` the port) as the slot unit's
    /// `ConditionPathExists=`.
    pub fn set_condition(&self, path: &str) {
        write(&self.world.join("condition"), path);
    }

    /// Writes the boot id.
    pub fn set_boot(&self, boot: &str) {
        write(
            &self.root.join("proc/sys/kernel/random/boot_id"),
            &format!("{boot}\n"),
        );
    }

    /// A reboot: every unit stopped, every table gone (the rules are not kept across a boot),
    /// and a new boot id.
    pub fn reboot(&self, boot: &str) {
        for dir in ["units", "tables"] {
            let _ = fs::remove_dir_all(self.world.join(dir));
        }
        self.set_boot(boot);
    }

    /// Scripts the instance on `port`.
    pub fn script(&self, port: u16, script: SlotScript) {
        self.stub.script(port, script);
    }

    /// `GET path` on `127.0.0.1:port`, as the hook asks it: the status and the body, or `None`
    /// for no answer.
    pub fn get(&self, port: u16, path: &str) -> Option<(u16, String)> {
        let client = LoopbackInstances::new(Duration::from_secs(5), Duration::from_secs(30));
        client
            .get(port, path)
            .ok()
            .map(|reply| (reply.status, reply.body))
    }
}
