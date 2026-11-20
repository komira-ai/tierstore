//! Every verb of the command line, driven through `readbench::cli::run` against the in-process
//! fake store.

#![cfg(target_os = "linux")]
#![expect(
    clippy::unwrap_used,
    reason = "the helpers of this file are part of its tests: their failure is the failure reported"
)]

#[cfg(test)]
#[expect(dead_code, reason = "this test file uses part of the fake")]
mod support;
#[cfg(test)]
#[expect(dead_code, reason = "this test file uses part of the trees")]
mod tree;

use std::path::Path;

use readbench::cli::{run, run_with};
use readbench::record::{ClientRun, DemandRead, DiskSample, MetricDelta, Mode, read_lines};
use support::{Fake, Kind};
use tierstore_testkit::dir::TestDir;
use tokio::net::{TcpListener, TcpStream};
use tree::{PACK_B, ROOT, ROOT_BYTES, ROOT2, Variant, fake};

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// Runs the command; its exit code, output and error text.
async fn cli(args: &[String]) -> (u8, String, String) {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err).await;
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

fn path(p: &Path) -> String {
    p.display().to_string()
}

/// `--filer` and `--volume-server` of the fake.
fn store_args(fake: &Fake) -> Vec<String> {
    vec![
        "--filer".to_owned(),
        fake.filer(0).url(""),
        "--volume-server".to_owned(),
        format!("2={}", fake.volume(0).addr()),
        format!("3={}", fake.volume(1).addr()),
    ]
}

fn with(mut base: Vec<String>, more: &[&str]) -> Vec<String> {
    base.extend(strings(more));
    base
}

#[tokio::test]
async fn walk_prints_what_the_tree_holds() {
    let fake = fake(Variant::Whole).await;
    let args = strings(&["walk", "--filer", &fake.filer(0).url("/"), "--tree", ROOT]);
    let (code, out, err) = cli(&args).await;
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            0,
            format!("tree {ROOT}: 2 manifests, 4 files, 3 packs, {ROOT_BYTES} bytes\n").as_str(),
            ""
        )
    );
}

#[tokio::test]
async fn walk_of_a_tree_the_store_lacks_fails() {
    let fake = fake(Variant::Whole).await;
    let args = strings(&[
        "walk",
        "--filer",
        &fake.filer(0).url(""),
        "--tree",
        &"9".repeat(64),
    ]);
    let (code, out, err) = cli(&args).await;
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.starts_with("readbench: http://"), "{err}");
    assert!(err.ends_with("not found\n"), "{err}");
}

#[tokio::test]
async fn prepare_makes_the_sidecars_once_and_then_finds_them() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-prepare");
    let args = with(with(vec!["prepare".to_owned()], &[]), &[]);
    let args = with(
        [args, store_args(&fake)].concat(),
        &[
            "--tree",
            ROOT,
            ROOT2,
            "--sidecars",
            &path(&dir.path().join("s")),
        ],
    );
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        err,
        format!(
            "tree {ROOT}: 2 manifests, 4 files, 3 packs, {ROOT_BYTES} bytes; sidecar made\n\
             tree {ROOT2}: 1 manifests, 1 files, 1 packs, 100 bytes; sidecar made\n"
        )
    );
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(err.matches("sidecar found").count(), 2, "{err}");
}

fn replay_args(fake: &Fake, dir: &TestDir, more: &[&str]) -> Vec<String> {
    let mut args = vec!["replay".to_owned()];
    args.extend(store_args(fake));
    args.extend(strings(&[
        "--tree",
        ROOT,
        ROOT2,
        "--sidecars",
        &path(&dir.path().join("sidecars")),
        "--cache",
        &path(&dir.path().join("cache")),
        "--node",
        "n1",
        "--run",
        "r1",
        "--out",
        &path(&dir.path().join("runs.jsonl")),
        "--hash-threads",
        "1",
    ]));
    args.extend(strings(more));
    args
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_records_each_engine_client_and_says_so() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-replay");
    let (code, out, err) = cli(&replay_args(
        &fake,
        &dir,
        &[
            "--clients",
            "2",
            "--total-clients",
            "4",
            "--client-base",
            "1",
        ],
    ))
    .await;
    assert_eq!((code, out.as_str()), (0, ""), "{err}");
    let runs: Vec<ClientRun> = read_lines(&dir.path().join("runs.jsonl")).unwrap();
    assert_eq!(
        runs.iter()
            .map(|r| (r.client, r.clients, r.tree.as_str(), r.mode))
            .collect::<Vec<_>>(),
        vec![(1, 4, ROOT2, Mode::Engine), (2, 4, ROOT, Mode::Engine)]
    );
    assert_eq!(err.lines().count(), 2, "{err}");
    assert!(
        err.starts_with(&format!("engine client 1 tree {ROOT2}: ready in ")),
        "{err}"
    );
    // A second run appends.
    let (code, _, _) = cli(&replay_args(&fake, &dir, &["--start-at", "0", "--keep"])).await;
    assert_eq!(code, 0);
    let runs: Vec<ClientRun> = read_lines(&dir.path().join("runs.jsonl")).unwrap();
    assert_eq!(runs.len(), 3);
    assert!(dir.path().join("cache/c0").is_dir());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_the_shipped_way_needs_no_volume_server() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-shipped");
    let mut args = vec![
        "replay".to_owned(),
        "--filer".to_owned(),
        fake.filer(0).url(""),
    ];
    args.extend(strings(&[
        "--tree",
        ROOT,
        "--sidecars",
        &path(&dir.path().join("s")),
        "--cache",
        &path(&dir.path().join("c")),
        "--mode",
        "shipped",
        "--node",
        "n1",
        "--run",
        "r1",
        "--out",
        &path(&dir.path().join("o")),
        "--fill-chunk-mib",
        "1",
        "--fill-concurrency",
        "2",
    ]));
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 0, "{err}");
    let runs: Vec<ClientRun> = read_lines(&dir.path().join("o")).unwrap();
    assert_eq!(runs[0].mode, Mode::Shipped);
    assert_eq!(runs[0].gets["chunk"], 8);
    assert!(err.starts_with("shipped client 0 tree "), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_of_the_engine_says_which_servers_did_not_answer_and_needs_one_that_did() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-noservers");
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let mut args = vec![
        "replay".to_owned(),
        "--filer".to_owned(),
        fake.filer(0).url(""),
    ];
    args.extend(strings(&["--volume-server", &format!("2={closed}")]));
    args.extend(strings(&[
        "--tree",
        ROOT,
        "--sidecars",
        &path(&dir.path().join("s")),
        "--cache",
        &path(&dir.path().join("c")),
        "--node",
        "n1",
        "--run",
        "r1",
        "--out",
        &path(&dir.path().join("o")),
    ]));
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 1);
    assert!(
        err.starts_with(&format!("volume server {closed}: ")),
        "{err}"
    );
    assert!(
        err.ends_with(
            "readbench: volume map: no volume server gave a status page (--volume-server)\n"
        ),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_of_a_lost_pack_fails_and_writes_nothing() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-lost");
    // The sidecar is made against the whole store; the run then reads a store without pack B.
    let (code, _, err) = cli(&replay_args(&fake, &dir, &["--mode", "shipped"])).await;
    assert_eq!(code, 0, "{err}");
    std::fs::remove_file(dir.path().join("runs.jsonl")).unwrap();
    let lost = tree::fake(Variant::Without(PACK_B)).await;
    let (code, _, err) = cli(&replay_args(&lost, &dir, &["--mode", "engine"])).await;
    assert_eq!(code, 1);
    assert!(err.contains("readbench: client: client 0 of tree"), "{err}");
    assert!(
        err.contains(&format!("fill: {PACK_B}: the fill is lost")),
        "{err}"
    );
    assert!(!dir.path().join("runs.jsonl").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demand_records_the_waits_of_single_files() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-demand");
    let mut args = vec!["demand".to_owned()];
    args.extend(store_args(&fake));
    args.extend(strings(&[
        "--tree",
        ROOT,
        "--sidecars",
        &path(&dir.path().join("s")),
        "--cache",
        &path(&dir.path().join("c")),
        "--node",
        "n1",
        "--run",
        "r1",
        "--out",
        &path(&dir.path().join("o")),
        "--probes",
        "2",
        "--load-clients",
        "8",
        "--hash-threads",
        "1",
    ]));
    let (code, _, err) = cli(&args).await;
    assert_eq!((code, err.as_str()), (0, "2 demand reads\n"));
    let reads: Vec<DemandRead> = read_lines(&dir.path().join("o")).unwrap();
    assert_eq!(reads.len(), 2);
    assert!(
        reads
            .iter()
            .all(|r| r.outcome == "ready" && r.load_clients == 8 && r.node == "n1")
    );
    // The probe's cache is gone.
    assert!(!dir.path().join("c/demand").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demand_that_cannot_make_its_cache_fails_and_writes_nothing() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-demand-fail");
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, b"x").unwrap();
    let mut args = vec!["demand".to_owned()];
    args.extend(store_args(&fake));
    args.extend(strings(&[
        "--tree",
        ROOT,
        "--sidecars",
        &path(&dir.path().join("s")),
        "--cache",
        &path(&blocker),
        "--node",
        "n1",
        "--run",
        "r1",
        "--out",
        &path(&dir.path().join("o")),
        "--hash-threads",
        "1",
    ]));
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 1);
    assert!(err.contains("readbench: entry store: "), "{err}");
    assert!(!dir.path().join("o").exists());
}

#[tokio::test]
async fn disks_samples_each_interval_of_each_device() {
    let dir = scratch("cli-disks");
    let stats = dir.path().join("diskstats");
    std::fs::write(
        &stats,
        "   8       0 sda 100 0 2000 50 0 0 0 0 0 40 60 0 0 0 0 0 0\n   8      16 sdb 1 0 8 1 0 0 0 0 0 1 1 0 0 0 0 0 0\n",
    )
    .unwrap();
    let args = strings(&[
        "disks",
        "--devices",
        "sda,sdb",
        "--interval-ms",
        "0",
        "--count",
        "2",
        "--run",
        "r1",
        "--node",
        "n1",
        "--out",
        &path(&dir.path().join("o")),
        "--diskstats",
        &path(&stats),
    ]);
    let (code, _, err) = cli(&args).await;
    assert_eq!((code, err.as_str()), (0, "4 samples\n"));
    let samples: Vec<DiskSample> = read_lines(&dir.path().join("o")).unwrap();
    assert_eq!(
        samples
            .iter()
            .map(|s| s.device.as_str())
            .collect::<Vec<_>>(),
        vec!["sda", "sdb", "sda", "sdb"]
    );
    assert!(
        samples
            .iter()
            .all(|s| s.reads == 0 && s.run == "r1" && s.node == "n1")
    );
}

#[tokio::test]
async fn an_out_file_that_cannot_be_opened_is_named() {
    let dir = scratch("cli-out");
    let stats = dir.path().join("diskstats");
    std::fs::write(&stats, "").unwrap();
    let out = dir.path().join("missing/o");
    let args = strings(&[
        "disks",
        "--devices",
        "sda",
        "--count",
        "1",
        "--run",
        "r",
        "--node",
        "n",
        "--out",
        &path(&out),
        "--diskstats",
        &path(&stats),
    ]);
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 1);
    assert!(
        err.starts_with(&format!("readbench: open {}: ", out.display())),
        "{err}"
    );
}

#[tokio::test]
async fn disks_of_a_file_that_is_not_there_fails() {
    let dir = scratch("cli-disks-missing");
    let args = strings(&[
        "disks",
        "--devices",
        "sda",
        "--count",
        "1",
        "--run",
        "r",
        "--node",
        "n",
        "--out",
        &path(&dir.path().join("o")),
        "--diskstats",
        &path(&dir.path().join("nope")),
    ]);
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 1);
    assert!(err.starts_with("readbench: "), "{err}");
}

/// A page server that answers its n-th request with `pages[n]` (the last one for the rest).
async fn metrics_server(pages: &'static [&'static str]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut n = 0;
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let page = pages[n.min(pages.len() - 1)];
            n += 1;
            serve(&stream, page).await;
        }
    });
    format!("http://{addr}/metrics")
}

async fn serve(stream: &TcpStream, page: &str) {
    let mut buf = [0u8; 2048];
    stream.readable().await.unwrap();
    let _ = stream.try_read(&mut buf);
    let answer = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    stream.writable().await.unwrap();
    let _ = stream.try_write(answer.as_bytes());
}

#[tokio::test]
async fn scrape_records_what_changed_between_two_readings() {
    let url = metrics_server(&[
        "up 1\nreq_total{op=\"get\"} 5\nsame_total 3\n",
        "up 1\nreq_total{op=\"get\"} 12\nsame_total 3\n",
    ])
    .await;
    let dir = scratch("cli-scrape");
    let args = strings(&[
        "scrape",
        "--metrics",
        &format!("n1={url}"),
        "--window-ms",
        "1",
        "--run",
        "r1",
        "--out",
        &path(&dir.path().join("o")),
    ]);
    let (code, _, err) = cli(&args).await;
    assert_eq!((code, err.as_str()), (0, "1 series changed\n"));
    let deltas: Vec<MetricDelta> = read_lines(&dir.path().join("o")).unwrap();
    assert_eq!(deltas.len(), 1);
    let d = &deltas[0];
    assert_eq!(
        (
            d.run.as_str(),
            d.node.as_str(),
            d.name.as_str(),
            d.window_ms
        ),
        ("r1", "n1", "req_total", 1)
    );
    assert!((d.delta - 7.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn scrape_of_a_page_that_is_not_named_or_not_there_fails() {
    let dir = scratch("cli-scrape-bad");
    let out = path(&dir.path().join("o"));
    let args = strings(&[
        "scrape",
        "--metrics",
        "no-equals",
        "--window-ms",
        "1",
        "--run",
        "r",
        "--out",
        &out,
    ]);
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 2);
    assert_eq!(
        err,
        "readbench: --metrics \"no-equals\": expected <node name>=<url>\n"
    );
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let url = format!("n1=http://{closed}/metrics");
    let args = strings(&[
        "scrape",
        "--metrics",
        &url,
        "--window-ms",
        "1",
        "--run",
        "r",
        "--out",
        &out,
    ]);
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 1);
    assert!(err.starts_with("readbench: http://"), "{err}");
}

#[tokio::test]
async fn audit_prints_the_copies_histogram_and_the_servers_that_did_not_answer() {
    let fake = fake(Variant::Whole).await;
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let mut args = vec!["audit".to_owned()];
    args.extend(strings(&[
        "--volume-server",
        &format!("2={}", fake.volume(0).addr()),
        &format!("3={}", fake.volume(1).addr()),
        &format!("4={closed}"),
    ]));
    let (code, out, err) = cli(&args).await;
    assert_eq!((code, err.as_str()), (0, ""));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(&lines[..2], ["2 volumes", "2 copies: 2 volumes"]);
    assert!(
        lines[2].starts_with(&format!("unreachable {closed}: ")),
        "{out}"
    );
    assert_eq!(lines.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_makes_the_tables_of_the_files_it_is_given() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-report");
    let (code, _, err) = cli(&replay_args(&fake, &dir, &[])).await;
    assert_eq!(code, 0, "{err}");
    let runs = path(&dir.path().join("runs.jsonl"));
    let (code, out, err) = cli(&strings(&["report", "--runs", &runs])).await;
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.starts_with("### Runs\n\n| run | mode |"), "{out}");
    for heading in [
        "### Runs",
        "### Trees",
        "### Demand reads",
        "### Drives",
        "### Node metrics",
    ] {
        assert!(out.contains(heading), "{out}");
    }
    assert!(
        out.contains("| 111111111111 | engine | 1 | 4 | 3 | 2 | 7/0/0/0 |"),
        "{out}"
    );
    // With the other files, and a device list.
    let empty = dir.path().join("empty");
    std::fs::write(&empty, "").unwrap();
    let e = path(&empty);
    let (code, _, _) = cli(&strings(&[
        "report",
        "--runs",
        &runs,
        "--demand",
        &e,
        "--disks",
        &e,
        "--metrics",
        &e,
        "--devices",
        "sda,sdb",
    ]))
    .await;
    assert_eq!(code, 0);
    let (code, _, err) = cli(&strings(&[
        "report",
        "--runs",
        &path(&dir.path().join("none")),
    ]))
    .await;
    assert_eq!(code, 1);
    assert!(err.starts_with("readbench: open "), "{err}");
}

#[tokio::test]
async fn report_with_classes_tables_drives_by_class() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-report-class");
    let (code, _, err) = cli(&replay_args(&fake, &dir, &[])).await;
    assert_eq!(code, 0, "{err}");
    let runs = path(&dir.path().join("runs.jsonl"));
    let disks = dir.path().join("disks.jsonl");
    let sample = |node: &str, dev: &str| {
        format!(
            "{{\"run\":\"r1\",\"node\":\"{node}\",\"device\":\"{dev}\",\"t_ms\":0,\"reads\":2,\"rkb_s\":2048.0,\"r_await_ms\":5.0,\"aqu_sz\":1.0,\"util_pct\":50.0}}\n"
        )
    };
    std::fs::write(&disks, sample("n1", "sda") + &sample("n1", "sdb")).unwrap();
    let disks = path(&disks);
    let (code, out, err) = cli(&strings(&[
        "report",
        "--runs",
        &runs,
        "--disks",
        &disks,
        "--class",
        "hdd=n1:sda",
        "--class",
        "other=n1:sdb",
    ]))
    .await;
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(
        out.contains("| r1 | hdd | 1 | 1 | 2.0 | 5.0 | 5.0 | 1.00 | 50.0 | 50.0 |\n"),
        "{out}"
    );
    assert!(out.contains("| r1 | other | 1 | 1 | 2.0 |"), "{out}");
    assert!(!out.contains("| node | device |"), "{out}");
    let (code, _, err) = cli(&strings(&["report", "--runs", &runs, "--class", "hdd"])).await;
    assert_eq!(code, 2);
    assert!(
        err.starts_with("readbench: --class \"hdd\": expected NAME="),
        "{err}"
    );
}

#[tokio::test]
async fn arguments_that_are_not_a_command_are_exit_2() {
    let (code, out, err) = cli(&strings(&["bogus"])).await;
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("unrecognized subcommand"), "{err}");
    let (code, out, err) = cli(&strings(&["--help"])).await;
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.contains("Usage: readbench"), "{out}");
    assert!(out.contains("replay"), "{out}");
}

#[tokio::test]
async fn knobs_and_servers_that_make_no_sense_are_usage_errors() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-usage");
    for (more, said) in [
        (
            &["--part-sizes", "1,2"][..],
            "--part-sizes takes three sizes in MiB",
        ),
        (
            &["--drive-streams", "0"][..],
            "budget: topology.drive_streams is 0",
        ),
    ] {
        let (code, _, err) = cli(&replay_args(&fake, &dir, more)).await;
        assert_eq!(code, 2, "{err}");
        assert!(err.contains(said), "{err}");
    }
    let mut args = replay_args(&fake, &dir, &[]);
    args.push("--volume-server".to_owned());
    args.push("nonsense".to_owned());
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 2);
    assert!(err.contains("volume server \"nonsense\""), "{err}");
    let (code, _, err) = cli(&strings(&["audit", "--volume-server", "nonsense"])).await;
    assert_eq!(code, 2);
    assert!(
        err.starts_with("readbench: volume server \"nonsense\""),
        "{err}"
    );
}

#[test]
fn a_runtime_that_does_not_start_is_exit_1_and_one_that_does_runs_the_command() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with(&["--help"], &mut out, &mut err, || {
        Err(std::io::Error::other("no threads"))
    });
    assert_eq!(code, 1);
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "readbench: the runtime did not start: no threads\n"
    );
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with(
        &["--help"],
        &mut out,
        &mut err,
        tokio::runtime::Runtime::new,
    );
    assert_eq!(code, 0);
    assert!(String::from_utf8(out).unwrap().contains("Usage: readbench"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_unverified_reads_no_pack_first_and_makes_no_sidecar() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-replay-unverified");
    let (code, _, err) = cli(&replay_args(&fake, &dir, &["--unverified"])).await;
    assert_eq!(code, 0, "{err}");
    assert!(!dir.path().join("sidecars").exists());
    let runs: Vec<ClientRun> = read_lines(&dir.path().join("runs.jsonl")).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].config["verified"], "false");
    assert_eq!(runs[0].config["hash_threads"], "none");
    // Each pack was read once, by the client; nothing read it before.
    assert_eq!(runs[0].total_gets(), fake.total(Kind::VolumeGet));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demand_unverified_makes_no_sidecar() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-demand-unverified");
    let mut args = vec!["demand".to_owned()];
    args.extend(store_args(&fake));
    args.extend(strings(&[
        "--tree",
        ROOT,
        "--sidecars",
        &path(&dir.path().join("s")),
        "--cache",
        &path(&dir.path().join("c")),
        "--node",
        "n1",
        "--run",
        "r1",
        "--out",
        &path(&dir.path().join("o")),
        "--probes",
        "2",
        "--unverified",
    ]));
    let (code, _, err) = cli(&args).await;
    assert_eq!((code, err.as_str()), (0, "2 demand reads\n"));
    assert!(!dir.path().join("s").exists());
    let reads: Vec<DemandRead> = read_lines(&dir.path().join("o")).unwrap();
    assert!(reads.iter().all(|r| r.outcome == "ready"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demand_unverified_of_a_tree_the_store_lacks_fails_and_writes_nothing() {
    let fake = fake(Variant::Whole).await;
    let dir = scratch("cli-demand-unverified-missing");
    let mut args = vec!["demand".to_owned()];
    args.extend(store_args(&fake));
    args.extend(strings(&[
        "--tree",
        &"4".repeat(64),
        "--sidecars",
        &path(&dir.path().join("s")),
        "--cache",
        &path(&dir.path().join("c")),
        "--node",
        "n1",
        "--run",
        "r1",
        "--out",
        &path(&dir.path().join("o")),
        "--unverified",
    ]));
    let (code, out, err) = cli(&args).await;
    assert_eq!((code, out.as_str()), (1, ""), "{err}");
    assert!(err.contains("not found"), "{err}");
    assert!(!dir.path().join("o").exists());
}
