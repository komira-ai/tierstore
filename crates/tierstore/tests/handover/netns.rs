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

/// The address of the host on the veth: a documentation address (RFC 5737), which the run's own
/// namespaces hold and nothing routes.
pub const LOCAL_V4: &str = "192.0.2.1";
/// The address of the remote end.
pub const REMOTE_V4: &str = "192.0.2.2";
/// The IPv6 address of the host on the veth: a documentation address (RFC 3849).
pub const LOCAL_V6: &str = "2001:db8:203::1";
/// The IPv6 address of the remote end.
pub const REMOTE_V6: &str = "2001:db8:203::2";

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

/// The two namespaces, deleted by [`Namespaces::close`], or on drop.
pub struct Namespaces {
    pub names: Names,
    closed: bool,
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
        let made = Namespaces {
            names,
            closed: false,
        };
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

    /// Deletes every table of the host namespace and then both namespaces, and says what could
    /// not be done: a completed run asserts it is nothing.
    pub fn close(mut self) -> Vec<String> {
        self.closed = true;
        clean(&self.names, &nft_path(), &mut run_checked)
    }
}

/// Runs `ip` with `args`: what it printed, or why it failed.
fn run_checked(args: &[&str]) -> Result<String, String> {
    let out = Command::new("ip")
        .args(args)
        .output()
        .map_err(|e| format!("ip: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "ip {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The real `nft`, as a string for an `ip netns exec` command line.
fn nft_path() -> String {
    nft_program().to_string_lossy().into_owned()
}

/// The cleanup of the namespaces `names`, through `ip` (given as `run`, so that a test can make it
/// fail) and the `nft` at `nft`: list the tables of the host namespace, delete each, delete both
/// namespaces, then list the namespaces and find neither. Every failure is a problem, and none
/// stops the rest.
fn clean(
    names: &Names,
    nft: &str,
    run: &mut dyn FnMut(&[&str]) -> Result<String, String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let local = names.local.as_str();
    match run(&["netns", "exec", local, nft, "list", "tables"]) {
        Ok(listing) => {
            for table in listing
                .lines()
                .filter_map(|line| line.strip_prefix("table "))
            {
                let mut words = vec!["netns", "exec", local, nft, "delete", "table"];
                words.extend(table.split_whitespace());
                if let Err(why) = run(&words) {
                    problems.push(why);
                }
            }
        }
        Err(why) => problems.push(why),
    }
    for name in [local, names.remote.as_str()] {
        if let Err(why) = run(&["netns", "del", name]) {
            problems.push(why);
        }
    }
    match run(&["netns", "list"]) {
        Ok(listing) => {
            for name in [local, names.remote.as_str()] {
                if listing.split_whitespace().any(|word| word == name) {
                    problems.push(format!("the namespace {name} is still there"));
                }
            }
        }
        Err(why) => problems.push(why),
    }
    problems
}

impl Drop for Namespaces {
    /// The cleanup of a run that did not get to [`Namespaces::close`] (it panicked): best effort,
    /// each problem printed.
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        for problem in clean(&self.names, &nft_path(), &mut run_checked) {
            eprintln!("T1: cleanup: {problem}");
        }
    }
}

#[cfg(test)]
mod tests {
    //! The cleanup, through a stand-in for `ip` that fails where it is told to.

    use super::{Names, clean};

    /// A path no host has: the unit test needs no nftables.
    const NFT: &str = "/nonexistent/nft";

    fn names() -> Names {
        Names {
            local: "t1-l".to_owned(),
            remote: "t1-r".to_owned(),
        }
    }

    /// Runs the cleanup with an `ip` that answers `listing` to `list tables`, an empty list to
    /// `netns list`, and fails each call whose words contain `fail`.
    fn with(listing: &str, fail: &str) -> (Vec<String>, Vec<String>) {
        let mut calls = Vec::new();
        let problems = clean(&names(), NFT, &mut |args| {
            let line = args.join(" ");
            calls.push(line.clone());
            if !fail.is_empty() && line.contains(fail) {
                return Err(format!("failed: {line}"));
            }
            Ok(if line.ends_with("list tables") {
                listing.to_owned()
            } else {
                String::new()
            })
        });
        (problems, calls)
    }

    /// Asserts that a cleanup that works deletes every table and both namespaces and reports
    /// nothing, and that a failure to list the tables, to delete a table or to delete a namespace
    /// is reported (and the rest is still done), as is a namespace still listed afterwards. It
    /// would catch a cleanup whose failures a passing run could hide.
    #[test]
    fn every_failure_of_the_cleanup_is_reported() {
        let tables = "table inet kbf_forward_tierstore\ntable inet kbf_guard_tierstore\n";
        let (problems, calls) = with(tables, "");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            calls.iter().filter(|c| c.contains("delete table")).count(),
            2
        );
        assert_eq!(calls.iter().filter(|c| c.contains("netns del")).count(), 2);
        assert!(
            calls[0].contains("/nonexistent/nft list tables"),
            "{calls:?}"
        );
        for fail in [
            "list tables",
            "delete table inet kbf_guard",
            "netns del t1-r",
            "netns list",
        ] {
            let (problems, calls) = with(tables, fail);
            assert_eq!(problems.len(), 1, "{fail}: {problems:?}");
            assert!(calls.iter().any(|c| c.contains("netns del t1-l")), "{fail}");
        }
        let mut problems = clean(&names(), NFT, &mut |args| {
            Ok(if args == ["netns", "list"] {
                "t1-l (id: 3)\n".to_owned()
            } else {
                String::new()
            })
        });
        assert_eq!(
            problems.pop(),
            Some("the namespace t1-l is still there".to_owned())
        );
    }
}
