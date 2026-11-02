//! A host for the real `tierstore update-hook`: a scratch root with the declaration, `etc/group`,
//! `etc/passwd`, `proc/sys` and the state directory; two staged releases; the fake programs; and
//! the stub instances on ports of their own. The hook is the binary itself, run with `--root`,
//! `--root-uid`, `--nft`, `--systemctl`, `--public-port` and `--poll-interval-ms` pointing at the
//! scratch host (flags of the conformance build only).
//!
//! At the start one slot (`a`, the lower port) runs the old release, its unit active and the
//! redirect naming it, as a host is after a first install; the other (`b`) has no link and no
//! unit. The record says no update is in flight. No guard table is loaded yet, so an update
//! places it.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::ops::Deref;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tierstore::server::update_hook::fake::{NFT, RELEASE, SYSTEMCTL, WORLD, World};
use tierstore::server::update_hook::machine::LoopbackInstances;
use tierstore::server::update_hook::{Instances, redirect_ruleset, unit_name};
use tierstore_testkit::dir::TestDir;
use tierstore_testkit::port::Reserved;

pub use crate::host::{BOOT, DECLARED_STATE, HOOK, Host, uid};
use crate::host::{mode, write};
use crate::stub::{SlotScript, Stub};

/// The stand-in programs.
pub const FAKE: &str = env!("CARGO_BIN_EXE_tierstore-hook-fake");
/// The version of the release that runs at first.
pub const OLD_VERSION: &str = "1.0.0";
/// The version of the release the update installs.
pub const NEW_VERSION: &str = "2.0.0";
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
}

impl Setup {
    /// Both releases can hand over.
    pub fn handover() -> Setup {
        Setup {
            old_handover: true,
            new_handover: true,
        }
    }

    /// The old release cannot hand over: the update is a restart.
    pub fn restart() -> Setup {
        Setup {
            old_handover: false,
            new_handover: true,
        }
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
    host: Host,
    pub stub: Stub,
    /// The fakes' world.
    pub world: World,
    /// The release that runs at first.
    pub old: PathBuf,
    /// Dropped last: the stub and the processes are gone before the files are.
    _dir: TestDir,
}

impl Deref for Rig {
    type Target = Host;

    fn deref(&self) -> &Host {
        &self.host
    }
}

/// Puts the fake program at `to`: a hard link, or a copy across file systems.
fn place_fake(to: &Path) {
    if fs::hard_link(FAKE, to).is_err() {
        fs::copy(FAKE, to).expect("copy the fake");
    }
    mode(to, 0o755);
}

/// A staged release: the fake's `tierstore`, its capabilities and its version.
fn release(dir: &Path, version: &str, handover: bool) {
    fs::create_dir_all(dir).expect("a release");
    place_fake(&dir.join(RELEASE));
    let caps = if handover {
        format!("{HANDOVER}\n")
    } else {
        String::new()
    };
    write(&dir.join("capabilities"), &caps);
    write(&dir.join("version"), &format!("{version}\n"));
}

impl Rig {
    /// A scratch host as `setup` says.
    pub fn new(name: &str, setup: Setup) -> Rig {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let base = fs::canonicalize(dir.path()).expect("the scratch directory");
        let (mut held_a, mut held_b) = (Reserved::new(), Reserved::new());
        if held_a.port() > held_b.port() {
            std::mem::swap(&mut held_a, &mut held_b);
        }
        let held_public = Reserved::new();
        let (a, b, public) = (held_a.port(), held_b.port(), held_public.port());
        let old = base.join("releases/old");
        let new = base.join("releases/new");
        release(&old, OLD_VERSION, setup.old_handover);
        release(&new, NEW_VERSION, setup.new_handover);
        let tools = base.join("tools");
        let world_dir = tools.join(WORLD);
        fs::create_dir_all(&world_dir).expect("the fakes' world");
        place_fake(&tools.join(NFT));
        place_fake(&tools.join(SYSTEMCTL));
        let host = Host::new(
            &base,
            [a, b],
            public,
            new,
            NEW_VERSION,
            tools.join(NFT),
            tools.join(SYSTEMCTL),
            1,
        );
        let rig = Rig {
            host,
            stub: Stub::start(world_dir.clone(), [held_a, held_b], held_public),
            world: World::new(world_dir),
            old,
            _dir: dir,
        };
        rig.lay_down();
        rig
    }

    /// The files of the host, the record of a host with no update in flight, and the old slot
    /// running behind the redirect.
    fn lay_down(&self) {
        self.lay_down_files();
        symlink(&self.old, self.state.join("slots").join(self.a.to_string())).expect("a link");
        self.lay_down_idle_record();
        self.world
            .set_exec(&format!("{}/slots/%i/{RELEASE}", self.state.display()));
        let apply = redirect_ruleset(self.public, self.a);
        self.world.call(NFT, &["-f", "-"], &mut apply.as_bytes());
        let unit = unit_name(self.a);
        self.world
            .call(SYSTEMCTL, &["start", "--", &unit], &mut io::empty());
        assert_eq!(self.running(), [self.a], "the old slot runs");
        self.world.clear_logs();
    }

    /// The calls the fakes have logged.
    pub fn calls(&self) -> Vec<String> {
        self.world.lines("calls")
    }

    /// The calls logged since there were `mark`.
    pub fn calls_since(&self, mark: usize) -> Vec<String> {
        self.calls()[mark..].to_vec()
    }

    /// The changes the fakes have made.
    pub fn effects(&self) -> Vec<String> {
        self.world.lines("effects")
    }

    /// What the fakes hold.
    pub fn machine(&self) -> Machine {
        Machine {
            units: self
                .world
                .units()
                .into_iter()
                .map(|(port, unit)| (port, unit.release))
                .collect(),
            tables: self.world.tables(),
        }
    }

    /// The ports of the active units, in order.
    pub fn running(&self) -> Vec<u16> {
        self.world.units().into_keys().collect()
    }

    /// The port the redirect table names.
    pub fn redirect(&self) -> Option<u16> {
        self.world.redirect()
    }

    /// Whether the table `name` is loaded.
    pub fn has_table(&self, name: &str) -> bool {
        self.world.tables().contains_key(name)
    }

    /// A reboot: every unit stopped, every table gone (the rules are not kept across a boot),
    /// and a new boot id.
    pub fn reboot(&self, boot: &str) {
        self.world.reboot();
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
