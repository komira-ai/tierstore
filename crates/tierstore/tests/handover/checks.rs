//! What a run is judged by once its clients have stopped. Each check names the defect it catches.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Stdio};
use std::sync::Arc;

use tierstore::{Backend, DirBackend, decode_journal};
use tierstore_testkit::reaped::{first_line, spawn};

use crate::load::{Fault, Kind, Report, hex_sha256};
use crate::site::{A, B, Node, Site};
use crate::wire::Broke;

/// The failed requests of all clients.
pub fn failures(reports: &[Report]) -> Vec<String> {
    reports
        .iter()
        .flat_map(|report| report.failures.iter().map(ToString::to_string))
        .collect()
}

/// Prints what each client did.
pub fn print(reports: &[Report]) {
    for report in reports {
        eprintln!(
            "T1: {}: {} requests, {} answered, {} failed, {} writes acknowledged, slots {:?}{}",
            report.name,
            report.requests,
            report.answered,
            report.failures.len(),
            report.acked.len(),
            report.slots,
            report
                .ended
                .as_ref()
                .map_or(String::new(), |why| format!(", ended by {why}")),
        );
    }
}

/// Asserts that no request failed: no refused or reset connection, no cut answer, no timeout, no
/// 5xx, no wrong answer. Catches a redirect swap that breaks established connections, a drain
/// without its lingering close, and a v-current stopped too soon.
pub fn no_failures(reports: &[Report]) {
    let failed = failures(reports);
    assert!(
        failed.is_empty(),
        "T1: {} requests failed; the first ones:\n{}",
        failed.len(),
        failed
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Asserts the bound of a restart (design sections 3, 9 and 11 question 5): every failed request
/// is a connect that failed on a new connection, refused or broken before any byte of the answer,
/// in the gap between the stop of v-current and the start of v-next; no 5xx, no reset or cut
/// answer on a connection that had carried an answer, no wrong answer. Returns the number of the
/// connects that failed, and prints it.
pub fn restart_bound(reports: &[Report]) -> usize {
    let mut refused = 0;
    let mut broken = 0;
    let mut beyond = Vec::new();
    for failure in reports.iter().flat_map(|report| &report.failures) {
        match &failure.fault {
            Fault::Broke(Broke::Refused) => refused += 1,
            Fault::Broke(Broke::BeforeAnswer { fresh: true, .. }) => broken += 1,
            _ => beyond.push(failure.to_string()),
        }
    }
    eprintln!(
        "T1: restart gap: {} connects failed ({refused} refused, {broken} broken before any \
         answer byte on a new connection)",
        refused + broken
    );
    assert!(
        beyond.is_empty(),
        "T1: {} failures beyond the restart's bound; the first ones:\n{}",
        beyond.len(),
        beyond
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    refused + broken
}

/// Asserts that each OCI client ended by the drain of its slot, when `drained` says that slot
/// drained, and that it did not end otherwise.
pub fn oci_ends(reports: &[Report], drained: bool) {
    for report in reports.iter().filter(|report| report.kind == Kind::Oci) {
        assert!(report.answered > 0, "{} was never answered", report.name);
        assert_eq!(report.ended.is_some(), drained, "{report:?}");
    }
}

/// Asserts that both slots answered `/metricz` through the public port. Catches a swap that never
/// moved the traffic.
pub fn both_slots(reports: &[Report]) {
    let mut slots: BTreeMap<u16, u64> = BTreeMap::new();
    for (slot, count) in reports.iter().flat_map(|report| &report.slots) {
        *slots.entry(*slot).or_default() += count;
    }
    eprintln!("T1: /metricz answers through the public port by slot: {slots:?}");
    for slot in [A, B] {
        assert!(
            slots.get(&slot).is_some_and(|&n| n > 0),
            "{slot} never answered: {slots:?}"
        );
    }
}

/// Asserts that every object a client was told it wrote reads back through the public port with
/// the bytes it wrote. Catches a write lost or cut in the swap. Returns how many were read.
pub fn read_back(site: &Site, reports: &[Report]) -> usize {
    let mut read = 0;
    for acked in reports.iter().flat_map(|report| &report.acked) {
        let answer = site
            .s3("GET", &acked.key, b"")
            .unwrap_or_else(|broke| panic!("read back {}: {broke}", acked.key));
        assert_eq!(answer.status, 200, "read back {}", acked.key);
        assert_eq!(
            hex_sha256(&answer.body),
            acked.sha256,
            "read back {}",
            acked.key
        );
        read += 1;
    }
    eprintln!("T1: {read} acknowledged writes read back");
    assert!(read > 0, "no write was acknowledged");
    read
}

/// The value of the counter `name` (no labels) in a `/metricz` text.
pub fn counter(metricz: &str, name: &str) -> Option<u64> {
    metricz
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
}

/// Asserts that the late-request counters of the running slot `port` are 0, for the S3 front and
/// the OCI front.
pub fn no_late_live(site: &Site, port: u16) {
    let text = site.metricz(port);
    for name in [
        "tierstore_drain_late_requests_total",
        "tierstore_oci_drain_late_requests_total",
    ] {
        assert_eq!(counter(&text, name), Some(0), "{name} of {port}");
    }
}

/// Asserts that every server process that stopped exited 0 after its drain and left no note of a
/// late request (the exit note of `tierstore_drain_late_requests_total` and its OCI twin, which
/// cannot be scraped once the drain has closed `/metricz`) and none of a request cut at the
/// deadline. Catches requests that arrived after the drain boundary (P2). Returns the exits.
pub fn exits_clean(site: &Site) -> usize {
    let exits = site.services.exits();
    for exit in &exits {
        assert_eq!(exit.code, Some(0), "the stop of {}: {exit:?}", exit.port);
        assert!(!exit.stderr.contains("came too late"), "{exit:?}");
        assert!(
            !exit.stderr.contains("were cut at the drain deadline"),
            "{exit:?}"
        );
    }
    exits.len()
}

/// Asserts that the cache directory passes the server's own scan with nothing discarded: a fresh
/// read-write start on it, once every slot has stopped, takes the lock, empties `.stage/` and scans,
/// and reports what it discarded. Catches a promotion that raced the lock (a half-published
/// object), and a write left in the stage.
pub fn scan_discards_nothing(site: &Site) {
    assert_eq!(site.node, Node::Cache);
    assert!(
        site.services.running().is_empty(),
        "every slot stopped first"
    );
    let config = site.base.join("conf/scan.toml");
    let text = std::fs::read_to_string(site.base.join(format!("conf/{A}.toml")))
        .expect("a slot's configuration");
    let text = text
        .lines()
        .filter(|line| !line.starts_with("slot ") && !line.starts_with("listen "))
        .collect::<Vec<_>>()
        .join("\n")
        .replacen("[server]", "[server]\nlisten = \"127.0.0.1:0\"", 1)
        .replacen("[oci]", "[oci]\nlisten = \"127.0.0.1:0\"", 1);
    std::fs::write(&config, text).expect("the scan's configuration");
    let mut command = Command::new(&site.server);
    command
        .arg("--config")
        .arg(&config)
        .args(["--drain-lb-wait", "1ms"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut scan = spawn(&mut command, 3).expect("start the scan");
    let stdout = scan.stdout.take().expect("a pipe");
    let (line, _rest) =
        first_line(stdout, std::time::Duration::from_secs(120)).expect("the start line");
    let addr = line
        .split_whitespace()
        .find_map(|word| word.strip_prefix("listen="))
        .and_then(|addrs| addrs.split(',').next())
        .expect("an address");
    let port: u16 = addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("a port");
    let text = site.metricz(port);
    assert!(
        text.contains("tierstore_cache_mode{mode=\"read_write\"} 1\n"),
        "{text}"
    );
    assert_eq!(
        counter(&text, "tierstore_cache_discarded_at_start_total"),
        Some(0),
        "{text}"
    );
    eprintln!("T1: a fresh start on the cache directory discarded nothing");
}

/// The hot tier of a storage node, and a runtime to read it with.
fn hot_tier(site: &Site) -> (tokio::runtime::Runtime, Arc<dyn Backend>) {
    assert_eq!(site.node, Node::Storage);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let hot: Arc<dyn Backend> = Arc::new(DirBackend::open(&site.hot).expect("the hot tier"));
    (runtime, hot)
}

/// Waits until the reconciler of the slot `port`, the one running, holds the lease (its first
/// pass takes it, once the slot holds the lock file), as the repository's tests wait for a
/// `/readyz` they expect: by looking again, never by a timer. Asserts the lease names that slot
/// with a nonce and a generation. Catches a lease taken by a writer that is not the slot holding
/// the lock.
pub fn lease_held_by(site: &Site, port: u16) {
    let (runtime, hot) = hot_tier(site);
    let body = runtime.block_on(async {
        loop {
            if let Ok(body) = hot.get("_tiering/lease", None).await {
                return body;
            }
        }
    });
    let text = String::from_utf8_lossy(&body);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text:?}");
    assert_eq!(
        lines[0],
        format!("tierstore-{port}"),
        "the holder: {text:?}"
    );
    assert_eq!(lines[1].len(), 32, "the nonce: {text:?}");
    assert!(lines[1].bytes().all(|b| b.is_ascii_hexdigit()), "{text:?}");
    assert!(
        lines[2].parse::<u64>().is_ok_and(|n| n > 0),
        "the generation: {text:?}"
    );
    eprintln!("T1: the lease is held by tierstore-{port}");
}

/// Asserts, once both slots have stopped cleanly, that the hot tier of a storage node holds intact
/// journals and no lease, with no two writers: every journal object decodes, the journals are
/// those of the two slots' proxy ids and of nothing else, each acknowledged key is journaled
/// exactly once, and each holder gave the lease up as it stopped (a holder deletes the lease
/// object only if it is still the one it wrote, so a lease another writer overwrote would be
/// left). Catches two processes writing one journal, and a lease not given up or held by two.
pub fn journals_and_lease(site: &Site, reports: &[Report]) {
    assert!(
        site.services.running().is_empty(),
        "every slot stopped first"
    );
    let (runtime, hot) = hot_tier(site);
    let ids: BTreeSet<String> = [A, B].map(|port| format!("tierstore-{port}")).into();
    let mut writers = BTreeSet::new();
    let mut journaled: BTreeMap<String, u32> = BTreeMap::new();
    runtime.block_on(async {
        let keys = list(&*hot, "_journal/").await;
        for key in &keys {
            let id = key["_journal/".len()..].split('/').next().expect("an id");
            writers.insert(id.to_owned());
            let body = read(&*hot, key).await;
            let entries = decode_journal(&body).unwrap_or_else(|e| panic!("{key}: {e}"));
            for entry in entries {
                *journaled.entry(entry.key).or_default() += 1;
            }
        }
        assert!(
            hot.head("_tiering/lease").await.is_err(),
            "a lease is left after both slots stopped"
        );
    });
    assert_eq!(writers, ids, "the journals' writers");
    for acked in reports.iter().flat_map(|report| &report.acked) {
        assert_eq!(
            journaled.get(&acked.key),
            Some(&1),
            "{} in the journals",
            acked.key
        );
    }
    eprintln!(
        "T1: journals of {writers:?} intact, {} keys journaled once each, the lease given up",
        journaled.len()
    );
}

async fn list(hot: &dyn Backend, prefix: &str) -> Vec<String> {
    let listed = hot.list(prefix).await.expect("list the hot tier");
    listed.into_iter().map(|object| object.key).collect()
}

async fn read(hot: &dyn Backend, key: &str) -> Vec<u8> {
    hot.get(key, None)
        .await
        .unwrap_or_else(|e| panic!("{key}: {e}"))
        .to_vec()
}
