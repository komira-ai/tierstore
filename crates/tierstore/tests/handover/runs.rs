//! The runs of T1. Each makes its own two namespaces, runs on a thread inside the host namespace,
//! and leaves the namespace it started in as it found it ([`t1`]). Between two steps of the hook
//! every client finishes [`PHASE`] more requests (or an OCI client has ended), so that every step
//! runs under the whole load; nothing waits on a clock.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::PathBuf;
use std::thread;

use tierstore::server::update_hook::{Marker, Mode};

use crate::checks;
use crate::guard;
use crate::held::{self, BIG_KEY, Held, Idle, Released};
use crate::host::HOOK;
use crate::load::{self, Fault, Kind, Load, Registry, Report, hex_sha256};
use crate::netns::{self, Names, Namespaces};
use crate::site::{A, B, Node, Plan, Site};
use crate::wire::Broke;

/// The requests each client finishes between two steps.
const PHASE: u64 = 20;
/// The line a server built with `test-drain-off` says at its start.
const DRAIN_OFF: &str = "a test build (test-drain-off)";

/// Runs `scenario` on a thread in the host namespace of two namespaces of its own, which are
/// deleted afterwards with every table in them, on a panic as well; asserts that a run that
/// completed could delete them all, and that the ruleset of the namespace the test started in is
/// as it was.
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
    let problems = namespaces.close();
    let ran = ran.unwrap_or_else(|panic| {
        for problem in &problems {
            eprintln!("T1: cleanup: {problem}");
        }
        resume_unwind(panic)
    });
    assert!(problems.is_empty(), "the cleanup failed: {problems:?}");
    assert_eq!(
        netns::root_ruleset(),
        before,
        "the run changed the root namespace's ruleset"
    );
    ran
}

/// The server of the test build: the drain as shipped.
fn server() -> PathBuf {
    PathBuf::from(HOOK)
}

/// The server built with `test-drain-off`, where the CI job builds it: `target/drain-off`, beside
/// this build's `target/tmp`.
fn drain_off_server() -> PathBuf {
    let target = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let binary = target
        .parent()
        .expect("the target directory")
        .join("drain-off/debug/tierstore");
    assert!(
        binary.is_file(),
        "{} is missing: build it with `cargo build -p tierstore --features \
         server,test-drain-off --bin tierstore --target-dir target/drain-off`",
        binary.display()
    );
    binary
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

/// Puts the registry content (a config blob, a layer and the manifest that names them), and the
/// large object of the held transfers (as an S3 object and as a blob), through the public port.
/// The OCI front is read-only: its content comes in by S3. Returns the registry and the digest of
/// the large object.
fn seed(site: &Site) -> (Registry, String) {
    let config = br#"{"architecture":"amd64","os":"linux"}"#.to_vec();
    let layer = load::object("layer", 2);
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
    let big = held::big("held-big");
    let big_hex = hex_sha256(&big);
    let mut puts = vec![(BIG_KEY.to_owned(), &big)];
    for bytes in [&config, &layer, &manifest, &big] {
        puts.push((format!("oci/blobs/sha256/{}", hex_sha256(bytes)), bytes));
    }
    for (key, bytes) in puts {
        let answer = site.s3("PUT", &key, bytes).expect("seed");
        assert_eq!(answer.status, 200, "seed {key}");
    }
    let registry = Registry {
        manifest: (hex_sha256(&manifest), manifest.len()),
        blobs,
    };
    (registry, big_hex)
}

/// A run under way: the host, the clients, and the digest of the held transfers' object.
struct Run {
    site: Site,
    load: Load,
    big: String,
}

/// Starts the host and the clients: `A` serves, the clients call it.
fn start(names: &Names, plan: &Plan) -> Run {
    let site = Site::new(plan);
    site.boot();
    let (registry, big) = seed(&site);
    let load = Load::start(load::clients(names, A), &registry);
    load.wait_more(PHASE);
    Run { site, load, big }
}

/// What a drain of v-current came to.
struct Drained {
    released: Released,
    /// What went wrong with the idle probes, from their side.
    idle: Vec<String>,
}

impl Run {
    /// `prepare` of a hand-over: the record says `handover`, the new slot listens, read-only.
    fn prepare_handover(&self) {
        let ran = self.site.step("prepare");
        ran.ok();
        assert!(
            ran.detail().contains("mode handover (handover_capable)"),
            "{ran:?}"
        );
        assert_eq!(self.site.host.record().mode, Mode::Handover);
        let ready = self.site.get(B, "/readyz").expect("an answer");
        assert_eq!(String::from_utf8_lossy(&ready.body), "ready read-only");
        self.load.wait_more(PHASE);
    }

    /// `cutover`, then `verify` before the commit, under load.
    fn cutover_and_verify(&self) {
        self.site.step("cutover").ok();
        assert_eq!(self.site.redirect(), Some(B));
        self.load.wait_more(PHASE);
        let ran = self.site.step("verify");
        ran.ok();
        assert_eq!(ran.line["promotion_pending"], true, "{ran:?}");
        self.load.wait_more(PHASE);
    }

    /// Runs `step`, which drains v-current (`A`): tells the clients the drain is coming, opens
    /// the idle probes on `A`, starts the step, and once `A`'s settle is over lets the `held`
    /// transfers go on; then waits for the probes and the step.
    fn drain_current(&self, step: &str, held: Held) -> Drained {
        self.load.draining();
        let idle = Idle::open(A);
        let child = self.site.spawn_step(step);
        self.site.wait_settled(A);
        let released = held.release();
        let idle = idle.join();
        self.site.finish_step(step, child).ok();
        Drained { released, idle }
    }

    /// `cleanup`, which drains v-current with the `held` transfers across the end of its settle,
    /// then `verify` until the new slot has taken the lock by itself.
    fn commit(&self, held: Held) -> Drained {
        let drained = self.drain_current("cleanup", held);
        self.load.wait_more(PHASE);
        let ran = self.site.step("verify");
        ran.ok();
        assert_eq!(ran.line["promotion_pending"], false, "{ran:?}");
        self.load.wait_more(PHASE);
        drained
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

/// A run that has ended: the host, what the clients did, and what the held transfers and the
/// idle probes came to.
struct Ended {
    site: Site,
    reports: Vec<Report>,
    drained: Drained,
}

/// A hand-over of build nodes under load, through `prepare`, the guard's check, `cutover` (with
/// transfers held across it) and `verify`, ending as `end` says.
fn handover(names: &Names, plan: &Plan, end: End) -> Ended {
    let run = start(names, plan);
    run.prepare_handover();
    guard::drops_direct_connects(names);
    let held = Held::begin(A, &run.big);
    run.cutover_and_verify();
    let drained = match end {
        End::Cleanup => run.commit(held),
        End::Rollback => {
            run.site.step("rollback").ok();
            run.load.wait_more(PHASE);
            Drained {
                released: held.release(),
                idle: Vec::new(),
            }
        }
    };
    let reports = run.load.finish();
    checks::print(&reports);
    Ended {
        site: run.site,
        reports,
        drained,
    }
}

impl Ended {
    /// No request failed, the held transfers completed, every acknowledged write reads back, and
    /// the slots answered as [`checks::both_slots`] says.
    fn served_everything(&self) {
        checks::no_failures(&self.reports);
        assert!(
            self.drained.released.failures.is_empty() && self.drained.idle.is_empty(),
            "{:?} {:?}",
            self.drained.released.failures,
            self.drained.idle
        );
        checks::both_slots(&self.reports);
        let acked: Vec<_> = self.drained.released.acked.iter().cloned().collect();
        assert_eq!(acked.len(), 1, "the held put was acknowledged");
        checks::read_back(&self.site, &self.reports, &acked);
    }
}

/// After a hand-over that committed: the new slot alone serves, through the redirect, read-write,
/// its promotion having discarded nothing; the old slot's link is gone; the record has no update
/// in flight; the old slot was stopped once, after the new one started. Then the new slot is
/// stopped; every stop is clean, and the old slot's drain read exactly the probes' late requests.
/// Then a fresh start on the cache directory discards nothing.
fn committed_host(site: &Site, calls: &[&str], exits: &[(u16, &str)]) {
    assert_eq!(site.services.running(), [B]);
    assert_eq!(site.services.calls(), calls);
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
    checks::exits_are(site, exits);
    checks::scan_discards_nothing(site);
}

/// T1 (a). Asserts that a hand-over of build nodes, end to end through `cleanup`, under twelve
/// clients that never retry (S3 puts, empty objects among them, and gets through the redirect of
/// the real `nft`, kept-alive and new connections, from the host and from the far end of a veth,
/// IPv4 and IPv6; OCI reads on the old slot's OCI front), and with a put and two gets held
/// part-way across `cutover` and across the end of the old slot's settle, fails no request;
/// every acknowledged write reads back; every client on new connections met both slots; the guard
/// drops direct connects from the far end; the old slot's drain read the idle probes' late
/// requests and no other; the new slot took the lock by itself; and the cache directory passes
/// the server's scan. Each OCI client ends at the old slot's drain: the hook does not move the
/// OCI front.
#[test]
#[cfg_attr(
    not(feature = "handover-t1"),
    ignore = "needs CAP_NET_ADMIN: run with --features handover-t1 under sudo"
)]
fn a_handover_under_load_fails_no_request() {
    t1("handover", |names| {
        let ended = handover(names, &plan("t1-handover", server()), End::Cleanup);
        ended.served_everything();
        checks::oci_ends(&ended.reports, true);
        let exits = ended.site.services.exits();
        assert!(!exits[0].stderr.contains(DRAIN_OFF), "{exits:?}");
        committed_host(
            &ended.site,
            &["start 9301", "start 9302", "stop 9301"],
            &[(A, &checks::probes_note()), (B, "")],
        );
    });
}

/// T1 (b). Asserts that the same hand-over ending in `rollback` before `cleanup` fails no
/// request, completes the held transfers, and leaves the old slot serving through the redirect,
/// read-write, with no late request on either front, the new slot stopped and its link removed,
/// and the record with no update in flight.
#[test]
#[cfg_attr(
    not(feature = "handover-t1"),
    ignore = "needs CAP_NET_ADMIN: run with --features handover-t1 under sudo"
)]
fn a_handover_rolled_back_under_load_fails_no_request() {
    t1("rollback", |names| {
        let ended = handover(names, &plan("t1-rollback", server()), End::Rollback);
        let site = &ended.site;
        ended.served_everything();
        checks::oci_ends(&ended.reports, false);
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
        checks::no_late_live(site, A);
        site.services.stop_slot(A);
        checks::exits_are(site, &[(B, ""), (A, "")]);
        checks::scan_discards_nothing(site);
    });
}

/// T1 (c). Asserts that an update in restart mode (`mode = "restart"` in the params) of storage
/// nodes over a `dir:` hot tier stays within the restart's bound: `cutover` stops the old slot
/// before it starts the new one, and the only failed requests are S3 connects into that gap,
/// refused, or (queued at the end of the old slot's settle) reset before any answer byte; no
/// failure outside the gap or of an OCI client, no 5xx, no reset or cut answer on an established
/// connection, no acknowledged write lost. The transfers held across the old slot's settle
/// complete, and its drain read the idle probes' late requests (the storage node's exit note).
/// The new slot's reconciler holds the lease; the journals are intact, with no two writers, and
/// once both slots have stopped no lease is left.
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
        let run = start(names, &plan);
        let ran = run.site.step("prepare");
        ran.ok();
        assert!(
            ran.detail().contains("mode restart (declared_restart)"),
            "{ran:?}"
        );
        assert_eq!(run.site.host.record().mode, Mode::Restart);
        run.load.wait_more(PHASE);
        let held = Held::begin(A, &run.big);
        run.load.gap(true);
        let drained = run.drain_current("cutover", held);
        run.load.gap(false);
        assert_eq!(run.site.redirect(), Some(B));
        run.load.wait_more(PHASE);
        run.site.step("verify").ok();
        run.load.wait_more(PHASE);
        run.site.step("cleanup").ok();
        run.load.wait_more(PHASE);
        let reports = run.load.finish();
        let site = &run.site;
        checks::print(&reports);
        checks::restart_bound(&reports);
        assert!(drained.released.failures.is_empty() && drained.idle.is_empty());
        checks::oci_ends(&reports, true);
        checks::both_slots(&reports);
        let acked: Vec<_> = drained.released.acked.into_iter().collect();
        checks::read_back(site, &reports, &acked);
        // `cleanup` stops the old slot again: a stop of a stopped unit does nothing.
        assert_eq!(
            site.services.calls(),
            ["start 9301", "stop 9301", "start 9302", "stop 9301"]
        );
        checks::lease_held_by(site, B);
        site.services.stop_slot(B);
        checks::exits_are(site, &[(A, &checks::probes_note()), (B, "")]);
        checks::journals_and_lease(site, &reports, &acked);
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
        let run = start(names, &plan);
        let site = &run.site;
        run.prepare_handover();
        site.kill_hook_after(&format!("redirect to :{B}"));
        assert!(site.step("cutover").killed());
        assert_eq!(site.redirect(), Some(B), "the swap was applied");
        assert!(!site.host.record().markers.contains(&Marker::Swapped));
        run.load.wait_more(PHASE);
        site.step("restore").ok();
        assert_eq!(
            site.redirect(),
            Some(A),
            "restore names the slot before the update"
        );
        run.load.wait_more(PHASE);
        site.step("rollback").ok();
        assert_eq!(site.redirect(), Some(A));
        assert_eq!(site.services.running(), [A]);
        run.load.wait_more(PHASE);
        run.prepare_handover();
        let held = Held::begin(A, &run.big);
        run.cutover_and_verify();
        let drained = run.commit(held);
        let reports = run.load.finish();
        checks::print(&reports);
        let ended = Ended {
            site: run.site,
            reports,
            drained,
        };
        ended.served_everything();
        checks::oci_ends(&ended.reports, true);
        committed_host(
            &ended.site,
            &[
                "start 9301",
                "start 9302",
                "stop 9302",
                "start 9302",
                "stop 9301",
            ],
            &[(B, ""), (A, &checks::probes_note()), (B, "")],
        );
    });
}

/// T1 without P1 and P2. Asserts that the hand-over of (a), run with servers built with
/// `test-drain-off` (no `Connection: close` in the drain, a plain close at its end), trips the
/// zero-failure check: kept-alive S3 connections to the old slot end, at its drain, without an
/// answer. And, apart from that, that the old slot's drain read none of the idle probes' late
/// requests: those connections never had an answer in the drain, so `Connection: close` has
/// nothing to do with them, and only the lingering close reads what they send after the end of
/// the stream. The binary is the one the CI job built into `target/drain-off`; its start line on
/// stderr says it is that build.
#[test]
#[cfg_attr(
    not(feature = "handover-t1"),
    ignore = "needs CAP_NET_ADMIN: run with --features handover-t1 under sudo"
)]
fn without_the_drain_a_handover_under_load_fails_requests() {
    let server = drain_off_server();
    t1("drain-off", move |names| {
        let ended = handover(names, &plan("t1-drain-off", server), End::Cleanup);
        let exits = ended.site.services.exits();
        assert!(exits[0].stderr.contains(DRAIN_OFF), "{exits:?}");
        assert_eq!(
            checks::late_lines(&exits[0].stderr),
            "",
            "without the lingering close no late request is read"
        );
        let kept_s3_broke = ended
            .reports
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
        let caught = catch_unwind(AssertUnwindSafe(|| checks::no_failures(&ended.reports)));
        let Err(panic) = caught else {
            panic!("T1 passed without P1 and P2: it cannot tell a drain from none");
        };
        let said = panic.downcast_ref::<String>().cloned().unwrap_or_default();
        eprintln!("T1 without P1 and P2, as expected:\n{said}");
    });
}
