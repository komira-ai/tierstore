//! The guard in the real kernel: a peer on the far side of the veth that connects straight to a
//! private slot, on IPv4 and on IPv6, is dropped, while the public port's redirected connections
//! pass. Observed by counters of a watch table of the test's own, in the host namespace, with one
//! chain before the guard's (input, priority -10) and one after it (priority 10). The probe waits
//! until each direct connect's SYN has been counted twice before the guard (the first one and its
//! retransmission, so the peer had no SYN-ACK), and then asserts that the chain after the guard
//! never saw one, and that it did see redirected traffic. Catches a guard that is missing or does
//! not drop.

use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::process::Command;
use std::thread;

use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketType, connect, socket};

use crate::netns::{self, LOCAL_V4, LOCAL_V6, Names, nft_program};
use crate::site::A;

/// The watch table.
const TABLE: &str = "t1_watch";

fn nft(args: &[&str], input: Option<&str>) -> String {
    let mut command = Command::new(nft_program());
    command.args(args);
    let output = match input {
        Some(text) => {
            use std::io::Write;
            let mut child = command
                .arg("-f")
                .arg("-")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("run nft");
            child
                .stdin
                .take()
                .expect("a pipe")
                .write_all(text.as_bytes())
                .expect("the ruleset");
            child.wait_with_output().expect("nft")
        }
        None => command.output().expect("run nft"),
    };
    assert!(
        output.status.success(),
        "nft {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The packets the counter `name` of the watch table has counted.
fn packets(name: &str) -> u64 {
    let listing = nft(&["list", "counter", "inet", TABLE, name], None);
    listing
        .split_whitespace()
        .skip_while(|word| *word != "packets")
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no packet count in {listing:?}"))
}

/// A connect to `addr` from the calling thread's namespace that does not wait for its answer.
fn connect_in_background(addr: SocketAddr) -> OwnedFd {
    let family = if addr.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    };
    let fd = socket(family, SocketType::STREAM, None).expect("a socket");
    rustix::io::ioctl_fionbio(&fd, true).expect("non-blocking");
    match connect(&fd, &addr) {
        Err(Errno::INPROGRESS) => fd,
        other => panic!("connect {addr}: {other:?}"),
    }
}

/// Asserts that the guard drops a direct connect from the far end to the private slot `A` on both
/// families, and lets the redirected traffic of the running clients through. Run in the host
/// namespace, while the clients run and the guard is loaded.
pub fn drops_direct_connects(names: &Names) {
    let a = A.to_string();
    nft(
        &[],
        Some(&format!(
            "add table inet {TABLE}\n\
             add counter inet {TABLE} before_v4\n\
             add counter inet {TABLE} before_v6\n\
             add counter inet {TABLE} after_direct\n\
             add counter inet {TABLE} after_redirected\n\
             add chain inet {TABLE} before {{ type filter hook input priority -10; policy accept; }}\n\
             add chain inet {TABLE} after {{ type filter hook input priority 10; policy accept; }}\n\
             add rule inet {TABLE} before iifname \"t1host\" meta nfproto ipv4 tcp dport {a} ct original proto-dst {a} counter name \"before_v4\"\n\
             add rule inet {TABLE} before iifname \"t1host\" meta nfproto ipv6 tcp dport {a} ct original proto-dst {a} counter name \"before_v6\"\n\
             add rule inet {TABLE} after iifname \"t1host\" tcp dport {a} ct original proto-dst {a} counter name \"after_direct\"\n\
             add rule inet {TABLE} after iifname \"t1host\" ct original proto-dst 9300 counter name \"after_redirected\"\n"
        )),
    );
    let remote = names.remote.clone();
    let sockets = thread::spawn(move || {
        netns::enter(&remote);
        let v4: SocketAddr = format!("{LOCAL_V4}:{A}").parse().expect("an address");
        let v6: SocketAddr = format!("[{LOCAL_V6}]:{A}").parse().expect("an address");
        [connect_in_background(v4), connect_in_background(v6)]
    })
    .join()
    .expect("the far end's connects");
    // A SYN and its retransmission: the first was neither answered nor let through.
    while packets("before_v4") < 2 || packets("before_v6") < 2 {
        thread::yield_now();
    }
    assert_eq!(
        packets("after_direct"),
        0,
        "a direct connect passed the guard"
    );
    assert!(
        packets("after_redirected") > 0,
        "no redirected packet was seen after the guard"
    );
    drop(sockets);
    nft(&["delete", "table", "inet", TABLE], None);
    eprintln!("T1: the guard dropped direct connects to {A} from the far end, IPv4 and IPv6");
}
