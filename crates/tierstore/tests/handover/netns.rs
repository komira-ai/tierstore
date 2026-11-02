//! The two network namespaces of a run: `local`, the host, where the servers, the hook, the real
//! `nft` and the local clients are; and `remote`, the far end of a veth pair, where the remote
//! clients are. Both are made with `ip netns add` and the veth is made inside them, so nothing is
//! added to the namespace the test was started in. Dropping [`Namespaces`] deletes every nftables
//! table of `local` and then both namespaces, on a passing run and on a panic alike.
//!
//! A thread moves into a namespace with [`enter`]: the sockets it opens and the processes it starts
//! from then on are in it. The test's own thread never moves, so that it can read the ruleset of
//! the namespace it started in before and after a run ([`root_ruleset`]) and see that the run left
//! it as it was.

use std::fs;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};

/// The address of the host on the veth.
pub const LOCAL_V4: &str = "10.203.0.1";
/// The address of the remote end.
pub const REMOTE_V4: &str = "10.203.0.2";
/// The IPv6 address of the host on the veth.
pub const LOCAL_V6: &str = "fd00:203::1";
/// The IPv6 address of the remote end.
pub const REMOTE_V6: &str = "fd00:203::2";

/// The bit of `CAP_NET_ADMIN` in a capability set.
const CAP_NET_ADMIN: u32 = 12;

/// Panics, saying why, unless this process may administer the network: the test makes
/// namespaces and loads nftables rules, and must never pass without having done so.
pub fn require_net_admin() {
    let status = fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|bits| u64::from_str_radix(bits.trim(), 16).ok())
        .unwrap_or(0);
    assert!(
        effective & (1 << CAP_NET_ADMIN) != 0,
        "T1 needs CAP_NET_ADMIN: it makes network namespaces and loads nftables rules in them. \
         Build it as yourself and run the test binary with sudo (docs/design/tierstore-handover.md \
         section 10 has the command); without the capability it fails, it never passes"
    );
}

/// The real `nft`.
pub fn nft_program() -> PathBuf {
    ["/usr/sbin/nft", "/sbin/nft", "/usr/bin/nft"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .expect("T1 needs the nft program (the nftables package)")
}

/// Runs `program` with `args` and panics, with what it said, unless it succeeds.
fn run(program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The ruleset of the namespace the test started in, as `nft -s list ruleset` prints it: without
/// the counters, which the host's own traffic moves.
pub fn root_ruleset() -> String {
    let nft = nft_program();
    run(
        nft.to_str().expect("a UTF-8 path"),
        &["-s", "list", "ruleset"],
    )
}

/// Moves the calling thread into the namespace `name`.
pub fn enter(name: &str) {
    let file = fs::File::open(Path::new("/run/netns").join(name))
        .unwrap_or_else(|e| panic!("the namespace {name}: {e}"));
    move_into_link_name_space(file.as_fd(), Some(LinkNameSpaceType::Network))
        .unwrap_or_else(|e| panic!("enter the namespace {name}: {e}"));
}

/// The names of the two namespaces of a run.
#[derive(Clone, Debug)]
pub struct Names {
    pub local: String,
    pub remote: String,
}

/// The two namespaces, deleted on drop.
pub struct Namespaces {
    pub names: Names,
}

impl Namespaces {
    /// Makes the two namespaces of the run `tag`, with loopback up in each and a veth between
    /// them carrying an IPv4 and an IPv6 address at each end (no duplicate address detection,
    /// so the addresses are usable at once).
    pub fn create(tag: &str) -> Namespaces {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        // One at a time: the first `ip netns add` of a host mounts /run/netns, and two at once
        // could both try.
        static MAKING: Mutex<()> = Mutex::new(());
        let _one = MAKING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = RUNS.fetch_add(1, Ordering::SeqCst);
        let id = format!("t1-{tag}-{}-{n}", std::process::id());
        let names = Names {
            local: format!("{id}-l"),
            remote: format!("{id}-r"),
        };
        let made = Namespaces { names };
        let (l, r) = (made.names.local.as_str(), made.names.remote.as_str());
        run("ip", &["netns", "add", l]);
        run("ip", &["netns", "add", r]);
        run("ip", &["-n", l, "link", "set", "lo", "up"]);
        run("ip", &["-n", r, "link", "set", "lo", "up"]);
        run(
            "ip",
            &[
                "-n", l, "link", "add", "t1host", "type", "veth", "peer", "name", "t1far", "netns",
                r,
            ],
        );
        let host_v4 = format!("{LOCAL_V4}/24");
        let far_v4 = format!("{REMOTE_V4}/24");
        let host_v6 = format!("{LOCAL_V6}/64");
        let far_v6 = format!("{REMOTE_V6}/64");
        run("ip", &["-n", l, "addr", "add", &host_v4, "dev", "t1host"]);
        run("ip", &["-n", r, "addr", "add", &far_v4, "dev", "t1far"]);
        run(
            "ip",
            &[
                "-n", l, "-6", "addr", "add", &host_v6, "dev", "t1host", "nodad",
            ],
        );
        run(
            "ip",
            &[
                "-n", r, "-6", "addr", "add", &far_v6, "dev", "t1far", "nodad",
            ],
        );
        run("ip", &["-n", l, "link", "set", "t1host", "up"]);
        run("ip", &["-n", r, "link", "set", "t1far", "up"]);
        made
    }

    /// The tables loaded in the host's namespace, as `family name`.
    fn tables(&self) -> Vec<String> {
        let nft = nft_program();
        let out = Command::new("ip")
            .args(["netns", "exec", &self.names.local])
            .arg(nft)
            .args(["list", "tables"])
            .output();
        let Ok(out) = out else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix("table "))
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for Namespaces {
    fn drop(&mut self) {
        let nft = nft_program();
        for table in self.tables() {
            let mut words = vec!["netns", "exec", &self.names.local];
            let nft = nft.to_str().unwrap_or("nft");
            words.extend([nft, "delete", "table"]);
            words.extend(table.split_whitespace());
            let deleted = Command::new("ip").args(&words).status();
            if !deleted.is_ok_and(|status| status.success()) {
                eprintln!(
                    "T1: could not delete the table {table} of {}",
                    self.names.local
                );
            }
        }
        for name in [&self.names.local, &self.names.remote] {
            let deleted = Command::new("ip").args(["netns", "del", name]).status();
            if !deleted.is_ok_and(|status| status.success()) {
                eprintln!("T1: could not delete the namespace {name}");
            }
        }
    }
}
