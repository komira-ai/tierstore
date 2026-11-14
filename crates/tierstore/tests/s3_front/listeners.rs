//! Several addresses of the `s3` and `oci` interfaces: one server over every address, in this
//! process and as the binary.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::Client;
use rustix::process::Signal;
use tierstore::server::Drained;
use tierstore::server::cli::{Addresses, Cli, run_serving};
use tokio::sync::oneshot;

use crate::binary::{run, start};
use crate::files::{Files, Tables, oci_interface};

/// Whether this host has IPv6 loopback, decided by trying to bind it.
fn has_v6() -> bool {
    std::net::TcpListener::bind("[::1]:0").is_ok()
}

/// The address a client dials to reach a socket bound at `at`: a wildcard by way of loopback.
fn reach(at: SocketAddr) -> SocketAddr {
    let ip = match at.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, at.port())
}

/// A client that keeps no connection after an answer, so that nothing of it is open at the drain.
fn client() -> Client {
    Client::builder().pool_max_idle_per_host(0).build().unwrap()
}

/// The settings of a start with these tables, with a drain that waits no longer than it must.
fn cli(files: &Files, tables: &Tables) -> Cli {
    let mut cli = Cli::from_config_text(&files.text(tables)).expect("a file");
    cli.drain_lb_wait = Duration::from_millis(50);
    cli
}

/// Asserts that `run_serving` binds a socket for each address of the `s3` interfaces and of the
/// `oci` interfaces, says every address to `ready` (the first as `s3` and `oci`), serves the S3
/// and the registry calls on all of them, and that the stop ends the one drain with nothing cut and
/// leaves no task alive that could still hold a socket.
#[tokio::test(flavor = "current_thread")]
async fn every_address_is_served_and_drained_together() {
    let files = Files::new("listeners-all");
    let mut s3 = vec!["127.0.0.1:0", "0.0.0.0:0"];
    let mut oci = vec!["127.0.0.1:0", "0.0.0.0:0"];
    if has_v6() {
        s3.push("[::1]:0");
        oci.push("[::1]:0");
    }
    let tables = Tables {
        listen: s3.iter().map(|address| (*address).to_owned()).collect(),
        more: oci_interface(&oci, ""),
        ..Tables::default()
    };
    let (s3_asked, oci_asked) = (s3.len(), oci.len());
    let cli = cli(&files, &tables);
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let running = run_serving(
        &cli,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound);
        },
    );
    let driving = async {
        let bound = bound.await.unwrap();
        assert_eq!(bound.s3_all.len(), s3_asked, "{bound:?}");
        assert_eq!(bound.oci_all.len(), oci_asked, "{bound:?}");
        assert_eq!(bound.s3, bound.s3_all[0]);
        assert_eq!(bound.oci, Some(bound.oci_all[0]));
        let client = client();
        for at in &bound.s3_all {
            let answer = client.get(format!("http://{}/healthz", reach(*at)));
            let status = answer.send().await.unwrap().status();
            assert_eq!(status, 200, "S3 at {at}");
        }
        for at in &bound.oci_all {
            let answer = client.get(format!("http://{}/v2/", reach(*at)));
            let status = answer.send().await.unwrap().status();
            assert_eq!(status, 200, "OCI at {at}");
        }
        let alive = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        assert!(
            alive > 0,
            "no accept task is running, so the end proves nothing"
        );
        stop.send(()).unwrap();
        bound
    };
    let (drained, bound) = tokio::join!(running, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    // A listener is owned by the serving future and the accept tasks it spawned, and this test's
    // runtime is its own: once the drain has returned and the tasks that were ending have run, a
    // task that is still alive would be one still holding a socket. The kernel's refusal of a new
    // connection is not asked: a port that was freed can be taken at once by a socket of another
    // test, or by the dial itself, and then the connect succeeds.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    let alive = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    assert_eq!(alive, 0, "tasks still alive after the drain, {bound:?}");
}

/// Asserts that an address given twice is refused before anything is bound or served, for an
/// `s3` interface and for an `oci` one, with status 2, nothing on stdout and the exact message
/// that names the entry, and that two entries on one address are refused naming both.
#[test]
fn an_address_given_twice_is_refused() {
    let files = Files::new("listeners-twice");
    let twice = Tables {
        listen: vec!["127.0.0.1:7".to_owned(), "127.0.0.1:7".to_owned()],
        ..Tables::default()
    };
    let oci_twice = Tables {
        more: oci_interface(&["127.0.0.1:6", "127.0.0.1:6"], ""),
        listen: vec!["127.0.0.1:5".to_owned()],
        ..Tables::default()
    };
    let entries = Tables {
        more: oci_interface(&["127.0.0.1:0"], ""),
        listen: vec!["127.0.0.1:5".to_owned()],
        ..Tables::default()
    };
    let oci_on_the_s3_port = Tables {
        more: oci_interface(&["127.0.0.1:5"], ""),
        listen: vec!["127.0.0.1:5".to_owned()],
        ..Tables::default()
    };
    let cases = [
        (
            twice,
            "interface[0].params: the address 127.0.0.1:7 is listed twice",
        ),
        (
            oci_twice,
            "interface[1].params: the address 127.0.0.1:6 is listed twice",
        ),
        (
            oci_on_the_s3_port,
            "interface[1].params: the address 127.0.0.1:5 is already used by interface[0]",
        ),
    ];
    for (tables, want) in cases {
        let flags = files.flags_of(&tables, &["--drain-lb-wait", "50ms"]);
        let (code, out, err) = run(&flags);
        assert_eq!((code, out.as_str()), (2, ""), "{want}");
        let expected = format!("tierstore: config file {}: {want}\n", flags[1]);
        assert_eq!(err, expected);
    }
    // Both entries on different addresses are fine.
    assert!(Cli::from_config_text(&files.text(&entries)).is_ok());
}

/// Asserts that the binary's start line names every address of the S3 front, separated by
/// commas in the one word `listen=`, that each of them answers, and that SIGTERM ends it with
/// status 0.
#[tokio::test]
async fn the_start_line_names_every_address() {
    let files = Files::new("listeners-line");
    let tables = Tables {
        listen: vec!["127.0.0.1:0".to_owned(), "0.0.0.0:0".to_owned()],
        ..Tables::default()
    };
    let mut running = start(&files.flags_of(&tables, &["--drain-lb-wait", "50ms"]));
    let named: Vec<SocketAddr> = running
        .addr()
        .split(',')
        .map(|word| word.parse().expect("an address"))
        .collect();
    assert_eq!(named.len(), 2, "{}", running.addr());
    let client = client();
    for at in &named {
        assert_ne!(at.port(), 0, "{at}");
        let answer = client.get(format!("http://{}/healthz", reach(*at)));
        assert_eq!(answer.send().await.unwrap().status(), 200, "{at}");
    }
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that a start given the wildcard of both families on one port, for an `s3` interface
/// and for an `oci` one, is refused when the IPv4 one is taken, and is never ready: the refusal
/// names `0.0.0.0:P`, the address that is taken, so the IPv6 socket on that port, asked first, was
/// bound (`IPV6_V6ONLY`; without it the refusal would name `[::]:P`, its socket holding the IPv4
/// port too). It would catch a start that serves on one family when the other cannot be bound,
/// which refuses the redirected connections of the other.
#[tokio::test]
async fn a_start_that_cannot_bind_both_wildcards_is_refused() {
    // A host without IPv6 cannot have the two wildcards of a port: the test says so on stderr,
    // with the reason the system gave, and asserts nothing.
    if let Err(why) = std::net::TcpListener::bind("[::1]:0") {
        eprintln!("skipped: this host has no IPv6 loopback ({why}); there is no [::]:P to bind");
        return;
    }
    let files = Files::new("listeners-both");
    let taken = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let (v6, v4) = (format!("[::]:{port}"), format!("0.0.0.0:{port}"));
    let both = [v6.as_str(), v4.as_str()];
    let s3 = Tables {
        listen: both.map(str::to_owned).to_vec(),
        ..Tables::default()
    };
    let oci = Tables {
        more: oci_interface(&both, ""),
        ..Tables::default()
    };
    for (kind, tables) in [("s3", s3), ("oci", oci)] {
        let cli = cli(&files, &tables);
        let ready = AtomicBool::new(false);
        let told = |_: Addresses| ready.store(true, Ordering::SeqCst);
        let refused = run_serving(&cli, async {}, told).await.err();
        let why = refused.expect("a refused start").to_string();
        assert!(why.starts_with(&format!("binding {v4}: ")), "{kind}: {why}");
        assert!(!ready.load(Ordering::SeqCst), "{kind}");
    }
}
