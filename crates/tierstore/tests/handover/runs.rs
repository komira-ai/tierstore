//! The runs of T1. Each makes its own two namespaces, runs on a thread inside the host namespace,
//! and leaves the namespace it started in as it found it ([`t1`]). Between two steps of the hook
//! the S3 clients finish [`PHASE`] more requests, so that every step runs under load; nothing
//! waits on a clock.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::PathBuf;
use std::thread;

use tierstore::server::update_hook::{Marker, Mode};

use crate::checks;
use crate::host::HOOK;
use crate::load::{self, Fault, Kind, Load, Registry, Report, hex_sha256};
use crate::netns::{self, Names, Namespaces};
use crate::site::{A, B, Node, Plan, Site};
use crate::wire::Broke;

/// The S3 requests finished between two steps: thirty for each S3 client.
const PHASE: u64 = 240;
/// The line a server built with `test-drain-off` says at its start.
const DRAIN_OFF: &str = "a test build (test-drain-off)";
/// Where the job finds the server built with `test-drain-off`.
const DRAIN_OFF_ENV: &str = "TIERSTORE_T1_DRAIN_OFF";

/// Runs `scenario` on a thread in the host namespace of two namespaces of its own, which are
/// deleted afterwards, with every table in them, on a panic as well; and asserts that the ruleset
/// of the namespace the test started in is as it was.
fn t1<R: Send + 'static>(tag: &str, scenario: impl FnOnce(&Names) -> R + Send + 'static) -> R {
    netns::require_net_admin();
    let before = netns::root_ruleset();
    let namespaces = Namespaces::create(tag);
    let names = namespaces.names.clone();
    let ran = thread::spawn(move || {
        netns::enter(&names.local);
        scenario(&names)
    })
    .join();
    drop(namespaces);
    assert_eq!(
        netns::root_ruleset(),
        before,
        "the run changed the root namespace's ruleset"
    );
    ran.unwrap_or_else(|panic| resume_unwind(panic))
}

/// The server of the test build: the drain as shipped.
fn server() -> PathBuf {
    PathBuf::from(HOOK)
}

/// A run of build nodes handed over with the real `nft`.
fn plan(name: &'static str, server: PathBuf) -> Plan {
    Plan {
        name,
        node: Node::Cache,
        restart: false,
        server,
        nft_double: false,
    }
}

/// Puts the registry content (a config blob, a layer and the manifest that names them) through
/// the public port, at the keys the OCI front reads: the OCI front is read-only.
fn seed(site: &Site) -> Registry {
    let config = br#"{"architecture":"amd64","os":"linux"}"#.to_vec();
    let layer = load::object("layer", 1);
    let blobs: Vec<(String, usize)> = [&config, &layer]
        .iter()
        .map(|bytes| (hex_sha256(bytes), bytes.len()))
        .collect();
    let manifest = format!(
        "{{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\",\
         \"config\":{{\"mediaType\":\"application/vnd.oci.image.config.v1+json\",\
         \"digest\":\"sha256:{}\",\"size\":{}}},\
         \"layers\":[{{\"mediaType\":\"application/vnd.oci.image.layer.v1.tar\",\
         \"digest\":\"sha256:{}\",\"size\":{}}}]}}",
        blobs[0].0, blobs[0].1, blobs[1].0, blobs[1].1
    )
    .into_bytes();
    for bytes in [&config, &layer, &manifest] {
        let key = format!("oci/blobs/sha256/{}", hex_sha256(bytes));
        let answer = site.s3("PUT", &key, bytes).expect("seed the registry");
        assert_eq!(answer.status, 200, "seed {key}");
    }
    Registry {
        manifest: (hex_sha256(&manifest), manifest.len()),
        blobs,
    }
}

/// How a hand-over ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    /// `cleanup`, and `verify` until the new slot is read-write.
    Cleanup,
    /// `rollback` before `cleanup`.
    Rollback,
}

/// Starts the host and the clients: `A` serves, the clients call it.
fn start(names: &Names, plan: &Plan) -> (Site, Load) {
    let site = Site::new(plan);
    site.boot();
    let registry = seed(&site);
    let load = Load::start(load::clients(names, A), &registry);
    load.wait_more(PHASE);
    (site, load)
}

/// `prepare` of a hand-over: the record says `handover`, the new slot listens, read-only.
fn prepare_handover(site: &Site, load: &Load) {
    let ran = site.step("prepare");
    ran.ok();
    assert!(
        ran.detail().contains("mode handover (handover_capable)"),
        "{ran:?}"
    );
    assert_eq!(site.host.record().mode, Mode::Handover);
    let ready = site.get(B, "/readyz").expect("an answer");
    assert_eq!(String::from_utf8_lossy(&ready.body), "ready read-only");
    load.wait_more(PHASE);
}

/// `cutover`, then `verify` before the commit, under load.
fn cutover_and_verify(site: &Site, load: &Load) {
    site.step("cutover").ok();
    assert_eq!(site.redirect(), Some(B));
    load.wait_more(PHASE);
    let ran = site.step("verify");
    ran.ok();
    assert_eq!(ran.line["promotion_pending"], true, "{ran:?}");
    load.wait_more(PHASE);
}

/// `cleanup`, then `verify` until the new slot has taken the lock by itself.
fn commit(site: &Site, load: &Load) {
    site.step("cleanup").ok();
    load.wait_more(PHASE);
    let ran = site.step("verify");
    ran.ok();
    assert_eq!(ran.line["promotion_pending"], false, "{ran:?}");
    load.wait_more(PHASE);
}

/// A hand-over of build nodes under load, through `prepare`, `cutover` and `verify`, ending as
/// `end` says. Returns the host and what the clients did.
fn handover(names: &Names, plan: &Plan, end: End) -> (Site, Vec<Report>) {
    let (site, load) = start(names, plan);
    prepare_handover(&site, &load);
    cutover_and_verify(&site, &load);
    match end {
        End::Cleanup => commit(&site, &load),
        End::Rollback => {
            site.step("rollback").ok();
            load.wait_more(PHASE);
        }
    }
    let reports = load.finish();
    checks::print(&reports);
    (site, reports)
}

/// After a hand-over that committed: the new slot alone serves, through the redirect, read-write,
/// its promotion having discarded nothing; the old slot's link is gone; the record has no update
/// in flight; the old slot was stopped once, after the new one started. Then the new slot is
/// stopped, every stop is clean, and a fresh start on the cache directory discards nothing.
fn committed_host(site: &Site) {
    assert_eq!(site.services.running(), [B]);
    assert_eq!(
        site.services.calls(),
        ["start 9301", "start 9302", "stop 9301"]
    );
    assert_eq!(site.redirect(), Some(B));
    assert_eq!(site.host.link(A), None);
    assert_eq!(site.host.record().previous, None);
    let text = site.metricz(B);
    assert!(
        text.contains("tierstore_cache_mode{mode=\"read_write\"} 1\n"),
        "{text}"
    );
    assert_eq!(
        checks::counter(&text, "tierstore_cache_discarded_at_start_total"),
        Some(0)
    );
    site.services.stop_slot(B);
    assert_eq!(checks::exits_clean(site), 2);
    checks::scan_discards_nothing(site);
}

/// T1 (a). Asserts that a hand-over of build nodes, end to end through `cleanup`, under twelve
/// clients that never retry (S3 puts and gets through the redirect of the real `nft`, kept-alive
/// and new connections, from the host and from the far end of a veth, IPv4 and IPv6; OCI reads on
/// the old slot's OCI front), fails no request; every acknowledged write reads back; both slots
/// answered; no request came late to either front of the old slot; the new slot took the lock by
/// itself; and the cache directory passes the server's scan. Each OCI client ends at the old
/// slot's drain: the hook does not move the OCI front.
#[test]
#[cfg_attr(
    not(feature = "handover-t1"),
    ignore = "needs CAP_NET_ADMIN: run with --features handover-t1 under sudo"
)]
fn a_handover_under_load_fails_no_request() {
    t1("handover", |names| {
        let (site, reports) = handover(names, &plan("t1-handover", server()), End::Cleanup);
        checks::no_failures(&reports);
        checks::oci_ends(&reports, true);
        checks::both_slots(&reports);
        checks::read_back(&site, &reports);
        let exits = site.services.exits();
        assert!(!exits[0].stderr.contains(DRAIN_OFF), "{exits:?}");
        committed_host(&site);
    });
}

/// T1 (b). Asserts that the same hand-over ending in `rollback` before `cleanup` fails no request,
/// and leaves the old slot serving through the redirect, read-write, with nothing late on either
/// front, the new slot stopped and its link removed, and the record with no update in flight.
#[test]
#[cfg_attr(
    not(feature = "handover-t1"),
    ignore = "needs CAP_NET_ADMIN: run with --features handover-t1 under sudo"
)]
fn a_handover_rolled_back_under_load_fails_no_request() {
    t1("rollback", |names| {
        let (site, reports) = handover(names, &plan("t1-rollback", server()), End::Rollback);
        checks::no_failures(&reports);
        checks::oci_ends(&reports, false);
        checks::both_slots(&reports);
        checks::read_back(&site, &reports);
        assert_eq!(site.services.running(), [A]);
        assert_eq!(
            site.services.calls(),
            ["start 9301", "start 9302", "stop 9302"]
        );
        assert_eq!(site.redirect(), Some(A));
        assert_eq!(site.host.link(B), None);
        assert_eq!(site.host.record().previous, None);
        let ready = site.get(A, "/readyz").expect("an answer");
        assert_eq!(String::from_utf8_lossy(&ready.body), "ready read-write");
        checks::no_late_live(&site, A);
        site.services.stop_slot(A);
        assert_eq!(checks::exits_clean(&site), 2);
        checks::scan_discards_nothing(&site);
    });
}

/// T1 (c). Asserts that an update in restart mode (`mode = "restart"` in the params) of storage
/// nodes over a `dir:` hot tier stays within the restart's bound: `cutover` stops the old slot
/// before it starts the new one, and the only failed requests are connects in that gap, on new
/// connections, before any byte of an answer; no 5xx, no reset or cut answer on an established
/// connection, no acknowledged write lost. The new slot's reconciler holds the lease; the journals
/// are intact, with no two writers, and once both slots have stopped no lease is left.
#[test]
#[cfg_attr(
    not(feature = "handover-t1"),
    ignore = "needs CAP_NET_ADMIN: run with --features handover-t1 under sudo"
)]
fn a_restart_under_load_fails_only_connects_in_its_gap() {
    t1("restart", |names| {
        let plan = Plan {
            name: "t1-restart",
            node: Node::Storage,
            restart: true,
            server: server(),
            nft_double: false,
        };
        let (site, load) = start(names, &plan);
        let ran = site.step("prepare");
        ran.ok();
        assert!(
            ran.detail().contains("mode restart (declared_restart)"),
            "{ran:?}"
        );
        assert_eq!(site.host.record().mode, Mode::Restart);
        load.wait_more(PHASE);
        site.step("cutover").ok();
        assert_eq!(site.redirect(), Some(B));
        load.wait_more(PHASE);
        site.step("verify").ok();
        load.wait_more(PHASE);
        site.step("cleanup").ok();
        load.wait_more(PHASE);
        let reports = load.finish();
        checks::print(&reports);
        checks::restart_bound(&reports);
        checks::oci_ends(&reports, true);
        checks::both_slots(&reports);
        checks::read_back(&site, &reports);
        // `cleanup` stops the old slot again: a stop of a stopped unit does nothing.
        assert_eq!(
            site.services.calls(),
            ["start 9301", "stop 9301", "start 9302", "stop 9301"]
        );
        checks::lease_held_by(&site, B);
        site.services.stop_slot(B);
        assert_eq!(checks::exits_clean(&site), 2);
        checks::journals_and_lease(&site, &reports);
    });
}

/// T1, the crash. Asserts that a hook killed right after `cutover` applied the swap, before it
/// recorded `swapped`, loses no request: the boot entry `restore` puts the rule back on the slot
/// the record names as the one before the update, the updater's `rollback` of the pending
/// `cutover` succeeds, and a whole hand-over after it fails no request either, with the same
/// checks as (a).
#[test]
#[cfg_attr(
    not(feature = "handover-t1"),
    ignore = "needs CAP_NET_ADMIN: run with --features handover-t1 under sudo"
)]
fn a_hook_killed_after_the_swap_then_restored_fails_no_request() {
    t1("killed", |names| {
        let plan = Plan {
            nft_double: true,
            ..plan("t1-killed", server())
        };
        let (site, load) = start(names, &plan);
        prepare_handover(&site, &load);
        site.kill_hook_after(&format!("redirect to :{B}"));
        assert!(site.step("cutover").killed());
        assert_eq!(site.redirect(), Some(B), "the swap was applied");
        assert!(!site.host.record().markers.contains(&Marker::Swapped));
        load.wait_more(PHASE);
        site.step("restore").ok();
        assert_eq!(
            site.redirect(),
            Some(A),
            "restore names the slot before the update"
        );
        load.wait_more(PHASE);
        site.step("rollback").ok();
        assert_eq!(site.redirect(), Some(A));
        assert_eq!(site.services.running(), [A]);
        load.wait_more(PHASE);
        prepare_handover(&site, &load);
        cutover_and_verify(&site, &load);
        commit(&site, &load);
        let reports = load.finish();
        checks::print(&reports);
        checks::no_failures(&reports);
        checks::oci_ends(&reports, true);
        checks::both_slots(&reports);
        checks::read_back(&site, &reports);
        assert_eq!(site.services.running(), [B]);
        assert_eq!(
            site.services.calls(),
            [
                "start 9301",
                "start 9302",
                "stop 9302",
                "start 9302",
                "stop 9301"
            ]
        );
        assert_eq!(site.redirect(), Some(B));
        site.services.stop_slot(B);
        assert_eq!(checks::exits_clean(&site), 3);
        checks::scan_discards_nothing(&site);
    });
}

/// T1 without P1 and P2. Asserts that the hand-over of (a), run with servers built with
/// `test-drain-off` (no `Connection: close` in the drain, a plain close at its end), trips the
/// zero-failure check: the test can fail, and what it catches is the drain. The binary is the one
/// the CI job built into its own target directory, named by `TIERSTORE_T1_DRAIN_OFF`; its start
/// line on stderr says it is that build. What breaks is the one thing P1 and P2 protect: a kept-alive
/// S3 connection to the old slot ends, at its drain, without an answer.
#[test]
#[cfg_attr(
    not(feature = "handover-t1"),
    ignore = "needs CAP_NET_ADMIN: run with --features handover-t1 under sudo"
)]
fn without_the_drain_a_handover_under_load_fails_requests() {
    let server = std::env::var_os(DRAIN_OFF_ENV).map(PathBuf::from).unwrap_or_else(|| {
        panic!(
            "{DRAIN_OFF_ENV} names no binary: build one with `cargo build -p tierstore --features \
             server,test-drain-off --bin tierstore --target-dir target/drain-off`"
        )
    });
    t1("drain-off", move |names| {
        let (site, reports) = handover(names, &plan("t1-drain-off", server), End::Cleanup);
        let exits = site.services.exits();
        assert!(exits[0].stderr.contains(DRAIN_OFF), "{exits:?}");
        let kept_s3_broke = reports
            .iter()
            .filter(|report| report.kind == Kind::S3)
            .flat_map(|report| &report.failures)
            .any(|failure| {
                matches!(
                    failure.fault,
                    Fault::Broke(Broke::BeforeAnswer { fresh: false, .. })
                )
            });
        assert!(
            kept_s3_broke,
            "no kept-alive S3 connection broke at the drain"
        );
        let caught = catch_unwind(AssertUnwindSafe(|| checks::no_failures(&reports)));
        let Err(panic) = caught else {
            panic!("T1 passed without P1 and P2: it cannot tell a drain from none");
        };
        let said = panic.downcast_ref::<String>().cloned().unwrap_or_default();
        eprintln!("T1 without P1 and P2, as expected:\n{said}");
    });
}
