//! The plan of trees without fetching them, through the library and through `readbench plan`,
//! against the in-process fake store.

#![cfg(target_os = "linux")]
#![expect(
    clippy::unwrap_used,
    reason = "the helpers of this file are part of its tests: their failure is the failure reported"
)]

#[cfg(test)]
#[expect(dead_code, reason = "this test file uses part of the fake")]
mod support;

use readbench::cli::run;
use readbench::http::HttpClient;
use readbench::planrun::{Variant, plan_tree, plan_trees};
use readbench::record::{PlanRecord, read_lines};
use readbench::swfs::FilerFetch;
use serde_json::json;
use support::{Fake, Kind};
use tierstore_testkit::dir::TestDir;

const BUCKET: &str = "packs";
const MIB: u64 = 1 << 20;
/// One file of 1 MiB at the start of a pack of 10 MiB.
const SPARSE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// One file of 100 bytes at the end of a pack of 2 MiB and 100 bytes.
const TAIL: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// A file in a pack the store lacks.
const MISSING_PACK: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
/// A file of 1 MiB in a pack of 100 bytes.
const SHORT: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
/// A tree the store lacks.
const ABSENT: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn data(n: u64) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + (i >> 20)) as u8).collect()
}

fn manifest(file: &str, size: u64, pack: &str, offset: u64) -> Vec<u8> {
    json!({"v": 1, "children": [{"name": file, "kind": "file", "sha256": hex::encode([5u8; 32]),
        "size": size, "exec": false, "source": {"key": pack, "offset": offset}}]})
    .to_string()
    .into_bytes()
}

async fn store() -> Fake {
    Fake::builder()
        .chunk_size(4 << 20)
        .object(BUCKET, "cas/b/d/pack", data(10 * MIB))
        .object(BUCKET, "cas/b/e/pack", data(2 * MIB + 100))
        .object(BUCKET, "cas/b/short/pack", data(100))
        .object(
            BUCKET,
            &format!("cas/l/{SPARSE}"),
            manifest("x", MIB, "cas/b/d/pack", 0),
        )
        .object(
            BUCKET,
            &format!("cas/l/{TAIL}"),
            manifest("y", 100, "cas/b/e/pack", 2 * MIB),
        )
        .object(
            BUCKET,
            &format!("cas/l/{MISSING_PACK}"),
            manifest("z", 5, "cas/b/missing/pack", 0),
        )
        .object(
            BUCKET,
            &format!("cas/l/{SHORT}"),
            manifest("w", MIB, "cas/b/short/pack", 0),
        )
        .start()
        .await
}

fn filer(fake: &Fake) -> FilerFetch {
    FilerFetch {
        http: HttpClient::new(10_000),
        base: fake.filer(0).url("/buckets/packs"),
    }
}

fn variants() -> Vec<Variant> {
    ["design=50,64,20", "greedy=50,64,5", "strict=100,0,100"]
        .into_iter()
        .map(|v| Variant::parse(v).unwrap())
        .collect()
}

fn record(
    tree: &str,
    variant: &str,
    (files, packs): (u64, u64),
    (needed, planned): (u64, u64),
    (whole, runs, ensures): (u64, u64, u64),
) -> PlanRecord {
    PlanRecord {
        run: "r".to_owned(),
        tree: tree.to_owned(),
        variant: variant.to_owned(),
        files,
        packs,
        bytes_needed: needed,
        bytes_planned: planned,
        packs_whole: whole,
        packs_runs: runs,
        ensures,
    }
}

#[tokio::test]
async fn a_sparse_tree_is_planned_as_runs_or_whole_as_the_variant_says() {
    let fake = store().await;
    let got = plan_tree(&filer(&fake), "r", SPARSE, &variants(), 4, 4)
        .await
        .unwrap();
    assert_eq!(
        got,
        [
            // 1 of 10 cells is 10%: below the 20% a small pack needs.
            record(SPARSE, "design", (1, 1), (MIB, MIB), (0, 1, 1)),
            record(SPARSE, "greedy", (1, 1), (MIB, 10 * MIB), (1, 0, 1)),
            record(SPARSE, "strict", (1, 1), (MIB, MIB), (0, 1, 1)),
        ]
    );
    // The packs' sizes came from the filer's entries: one per pack, none of the packs' bytes.
    assert_eq!(fake.filer(0).count(Kind::FilerMetadata), 1);
    assert_eq!(fake.volume(0).count(Kind::VolumeGet), 0);
}

#[tokio::test]
async fn the_last_cell_of_a_pack_is_counted_at_its_size() {
    let fake = store().await;
    let got = plan_tree(&filer(&fake), "r", TAIL, &variants(), 4, 4)
        .await
        .unwrap();
    assert_eq!(
        got,
        [
            // 1 of 3 cells is 33%: whole, all 2 MiB and 100 bytes of the pack.
            record(TAIL, "design", (1, 1), (100, 2 * MIB + 100), (1, 0, 1)),
            record(TAIL, "greedy", (1, 1), (100, 2 * MIB + 100), (1, 0, 1)),
            // As runs: the one short cell.
            record(TAIL, "strict", (1, 1), (100, 100), (0, 1, 1)),
        ]
    );
}

#[tokio::test]
async fn a_tree_that_fails_is_listed_with_its_error_and_the_rest_are_planned() {
    let fake = store().await;
    let trees: Vec<String> = [SPARSE, MISSING_PACK, ABSENT, SHORT, TAIL]
        .map(str::to_owned)
        .to_vec();
    let got = plan_trees(&filer(&fake), "r", &trees, &variants()[..1], 4, 4).await;
    assert_eq!(
        got.records,
        [
            record(SPARSE, "design", (1, 1), (MIB, MIB), (0, 1, 1)),
            record(TAIL, "design", (1, 1), (100, 2 * MIB + 100), (1, 0, 1)),
        ]
    );
    let failed: Vec<(&str, String)> = got
        .failed
        .iter()
        .map(|(tree, e)| (tree.as_str(), e.to_string()))
        .collect();
    assert_eq!(failed.len(), 3);
    assert_eq!(
        failed[0],
        (
            MISSING_PACK,
            "cas/b/missing/pack: the filer has no such pack".to_owned()
        )
    );
    assert_eq!(failed[1].0, ABSENT);
    assert!(failed[1].1.ends_with("not found"), "{}", failed[1].1);
    assert_eq!(failed[2].0, SHORT);
    assert!(failed[2].1.starts_with("sidecar: "), "{}", failed[2].1);
}

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

fn plan_args(fake: &Fake, out: &std::path::Path, more: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "plan",
        "--filer",
        &fake.filer(0).url(""),
        "--run",
        "r1",
        "--out",
        &out.display().to_string(),
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend(more.iter().map(|a| (*a).to_owned()));
    args
}

#[tokio::test]
async fn plan_writes_a_record_per_tree_and_variant_and_says_which_trees_failed() {
    let fake = store().await;
    let dir = scratch("plan-cli");
    let out = dir.path().join("plans.jsonl");
    let args = plan_args(
        &fake,
        &out,
        &[
            "--tree",
            SPARSE,
            MISSING_PACK,
            TAIL,
            "--variant",
            "design=50,64,20",
            "--variant",
            "greedy=50,64,5",
            "--parallel",
            "2",
        ],
    );
    let (code, stdout, err) = cli(&args).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(stdout, "");
    assert_eq!(
        err,
        format!(
            "tree {MISSING_PACK}: cas/b/missing/pack: the filer has no such pack\n\
             2 of 3 trees planned, 1 failed\n"
        )
    );
    let got: Vec<PlanRecord> = read_lines(&out).unwrap();
    assert_eq!(
        got,
        [
            record(SPARSE, "design", (1, 1), (MIB, MIB), (0, 1, 1)),
            record(SPARSE, "greedy", (1, 1), (MIB, 10 * MIB), (1, 0, 1)),
            record(TAIL, "design", (1, 1), (100, 2 * MIB + 100), (1, 0, 1)),
            record(TAIL, "greedy", (1, 1), (100, 2 * MIB + 100), (1, 0, 1)),
        ]
        .map(|mut r| {
            r.run = "r1".to_owned();
            r
        })
    );
}

#[tokio::test]
async fn plan_without_a_variant_is_the_design_and_takes_trees_from_a_file_after_the_flags() {
    let fake = store().await;
    let dir = scratch("plan-file");
    let out = dir.path().join("plans.jsonl");
    let list = dir.path().join("trees");
    std::fs::write(&list, format!("\n{SPARSE}\n  \n")).unwrap();
    let args = plan_args(
        &fake,
        &out,
        &["--tree", TAIL, "--trees-file", &list.display().to_string()],
    );
    let (code, _, err) = cli(&args).await;
    assert_eq!(
        (code, err.as_str()),
        (0, "2 of 2 trees planned, 0 failed\n")
    );
    let got: Vec<PlanRecord> = read_lines(&out).unwrap();
    let seen: Vec<(&str, &str)> = got
        .iter()
        .map(|r| (r.tree.as_str(), r.variant.as_str()))
        .collect();
    assert_eq!(seen, [(TAIL, "design"), (SPARSE, "design")]);
}

#[tokio::test]
async fn plan_fails_only_when_every_tree_failed() {
    let fake = store().await;
    let dir = scratch("plan-fail");
    let out = dir.path().join("plans.jsonl");
    let args = plan_args(&fake, &out, &["--tree", MISSING_PACK, ABSENT]);
    let (code, _, err) = cli(&args).await;
    assert_eq!(code, 1);
    assert!(err.contains("0 of 2 trees planned, 2 failed\n"), "{err}");
    assert!(
        err.ends_with("readbench: plan: every tree failed\n"),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "");
}

#[tokio::test]
async fn plan_refuses_arguments_that_name_no_tree_or_a_bad_variant_or_a_missing_file() {
    let fake = store().await;
    let dir = scratch("plan-usage");
    let out = dir.path().join("plans.jsonl");
    let (code, _, err) = cli(&plan_args(&fake, &out, &[])).await;
    assert_eq!(
        (code, err.as_str()),
        (2, "readbench: plan needs a tree: --tree or --trees-file\n")
    );
    let (code, _, err) = cli(&plan_args(
        &fake,
        &out,
        &["--tree", TAIL, "--variant", "oops"],
    ))
    .await;
    assert_eq!(code, 2);
    assert!(err.contains("--variant \"oops\""), "{err}");
    let none = dir.path().join("none").display().to_string();
    let (code, _, err) = cli(&plan_args(&fake, &out, &["--trees-file", &none])).await;
    assert_eq!(code, 1);
    assert!(err.starts_with("readbench: read "), "{err}");
    assert_eq!(fake.total(Kind::FilerGet), 0);
}

#[tokio::test]
async fn report_adds_the_table_of_plans_when_given_the_file() {
    let fake = store().await;
    let dir = scratch("plan-report");
    let out = dir.path().join("plans.jsonl");
    let args = plan_args(
        &fake,
        &out,
        &["--tree", SPARSE, "--variant", "greedy=50,64,5"],
    );
    assert_eq!(cli(&args).await.0, 0);
    let runs = dir.path().join("runs.jsonl");
    std::fs::write(&runs, "").unwrap();
    let report = |plans: Option<&std::path::Path>| {
        let mut args = vec![
            "report".to_owned(),
            "--runs".to_owned(),
            runs.display().to_string(),
        ];
        if let Some(p) = plans {
            args.extend(["--plans".to_owned(), p.display().to_string()]);
        }
        args
    };
    let (code, without, _) = cli(&report(None)).await;
    assert_eq!(code, 0);
    assert!(!without.contains("### Plans"), "{without}");
    let (code, with, _) = cli(&report(Some(&out))).await;
    assert_eq!(code, 0);
    assert!(
        with.ends_with(
            "### Plans\n\n\
| variant | trees | aggregate amplification | tree p50 | tree p90 | tree p99 | tree max | trees above 1.50 | packs whole |\n\
| --- | --- | --- | --- | --- | --- | --- | --- | --- |\n\
| greedy | 1 | 10.00 | 10.00 | 10.00 | 10.00 | 10.00 | 1 (100.0%) | 100.0% |\n\n"
        ),
        "{with}"
    );
    let none = dir.path().join("none");
    let (code, _, err) = cli(&report(Some(&none))).await;
    assert_eq!(code, 1);
    assert!(err.starts_with("readbench: open "), "{err}");
}
