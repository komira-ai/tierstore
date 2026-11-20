//! The sample of trees from the filer's listing of `cas/l/`, through the library and through
//! `readbench sample-trees`, and the fake's listing itself, against the in-process fake store.

#![expect(
    clippy::unwrap_used,
    reason = "the helpers of this file are part of its tests: their failure is the failure reported"
)]

#[cfg(test)]
#[expect(dead_code, reason = "this test file uses part of the fake")]
mod support;

use readbench::cli::run;
use readbench::http::HttpClient;
use readbench::sampletrees::{Query, Sampled, sample_trees};
use readbench::swfs::FilerFetch;
use serde_json::json;
use support::{Fake, Kind};
use tierstore_testkit::dir::TestDir;

const BUCKET: &str = "packs";

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// A manifest of `dirs` directories and one file.
fn manifest(dirs: usize) -> Vec<u8> {
    let mut children = vec![
        json!({"name": "f", "kind": "file", "sha256": hex::encode([1u8; 32]),
        "size": 0, "exec": false}),
    ];
    for i in 0..dirs {
        children.push(json!({"name": format!("d{i}"), "kind": "dir",
            "sha256": hex::encode([2u8; 32]), "size": 1}));
    }
    json!({"v": 1, "children": children})
        .to_string()
        .into_bytes()
}

/// The size of `manifest(dirs)`.
fn size(dirs: usize) -> u64 {
    manifest(dirs).len() as u64
}

/// The entries of `cas/l/`, in name order:
///
/// | name | modified   | content                     |
/// |------|------------|-----------------------------|
/// | t01  | 2026-09-01 | manifest, 2 directories     |
/// | t02  | 2026-11-01 | manifest, 0 directories     |
/// | t03  | 2026-11-02 | manifest, 1 directory       |
/// | t04  | 2026-11-03 | an `!<arch>` archive        |
/// | t05  | 2026-11-04 | manifest, 2 directories     |
/// | t06  | 2026-11-05 | manifest, 3 directories     |
/// | t07  | 2026-11-06 | manifest, 2 directories     |
///
/// and `sub/t99` under `cas/l/sub/`, which is not an entry of `cas/l/`.
async fn store() -> Fake {
    let mut builder = Fake::builder();
    for (name, mtime, content) in [
        ("t01", "2026-09-01T00:00:00Z", manifest(2)),
        ("t02", "2026-11-01T00:00:00Z", manifest(0)),
        ("t03", "2026-11-02T00:00:00Z", manifest(1)),
        (
            "t04",
            "2026-11-03T00:00:00Z",
            b"!<arch>\nsome archive".to_vec(),
        ),
        ("t05", "2026-11-04T00:00:00Z", manifest(2)),
        ("t06", "2026-11-05T00:00:00Z", manifest(3)),
        ("t07", "2026-11-06T00:00:00Z", manifest(2)),
        ("sub/t99", "2026-11-07T00:00:00Z", manifest(5)),
    ] {
        builder = builder.object_at(BUCKET, &format!("cas/l/{name}"), content, mtime);
    }
    builder.start().await
}

fn filer(fake: &Fake) -> FilerFetch {
    FilerFetch {
        http: HttpClient::new(10_000),
        base: fake.filer(0).url("/buckets/packs"),
    }
}

fn query() -> Query {
    Query {
        after: String::new(),
        scan: 1000,
        since: String::new(),
        min_bytes: 0,
        max_bytes: u64::MAX,
        min_dirs: 0,
        count: 100,
    }
}

fn roots(found: &Sampled) -> Vec<&str> {
    found.roots.iter().map(String::as_str).collect()
}

#[tokio::test]
async fn every_manifest_of_the_listing_is_kept_and_the_archive_is_counted_not_kept() {
    let fake = store().await;
    let found = sample_trees(&filer(&fake), &query()).await.unwrap();
    assert_eq!(roots(&found), ["t01", "t02", "t03", "t05", "t06", "t07"]);
    assert_eq!((found.scanned, found.unparsed), (7, 1));
    assert_eq!(fake.filer(0).count(Kind::FilerList), 1);
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 7);
    // The same store gives the same sample.
    assert_eq!(sample_trees(&filer(&fake), &query()).await.unwrap(), found);
}

#[tokio::test]
async fn the_time_and_size_bounds_leave_entries_unread() {
    let fake = store().await;
    let f = filer(&fake);
    let q = Query {
        since: "2026-11-03T00:00:00Z".to_owned(),
        ..query()
    };
    let found = sample_trees(&f, &q).await.unwrap();
    assert_eq!(roots(&found), ["t05", "t06", "t07"]);
    assert_eq!((found.scanned, found.unparsed), (7, 1));
    // Entries below the least size or above the greatest are not read: 2 and 3 directories.
    let q = Query {
        min_bytes: size(2),
        max_bytes: size(2),
        ..query()
    };
    let before = fake.filer(0).count(Kind::FilerGet);
    let found = sample_trees(&f, &q).await.unwrap();
    assert_eq!(roots(&found), ["t01", "t05", "t07"]);
    assert_eq!((found.scanned, found.unparsed), (7, 0));
    assert_eq!(fake.filer(0).count(Kind::FilerGet) - before, 3);
}

#[tokio::test]
async fn directories_count_start_after_a_key_and_stop_at_the_number_of_trees() {
    let fake = store().await;
    let f = filer(&fake);
    let q = Query {
        min_dirs: 2,
        ..query()
    };
    assert_eq!(
        roots(&sample_trees(&f, &q).await.unwrap()),
        ["t01", "t05", "t06", "t07"]
    );
    let q = Query {
        min_dirs: 2,
        count: 2,
        ..query()
    };
    let found = sample_trees(&f, &q).await.unwrap();
    // The scan stops at the second tree: t06 and t07 are not looked at.
    assert_eq!((roots(&found), found.scanned), (vec!["t01", "t05"], 5));
    let q = Query {
        after: "t03".to_owned(),
        count: 2,
        ..query()
    };
    let found = sample_trees(&f, &q).await.unwrap();
    assert_eq!((roots(&found), found.unparsed), (vec!["t05", "t06"], 1));
}

#[tokio::test]
async fn the_scan_bound_limits_the_entries_looked_at() {
    let fake = store().await;
    let q = Query { scan: 3, ..query() };
    let found = sample_trees(&filer(&fake), &q).await.unwrap();
    assert_eq!(
        (roots(&found), found.scanned),
        (vec!["t01", "t02", "t03"], 3)
    );
    assert_eq!(fake.filer(0).count(Kind::FilerList), 1);
    // The count reached by the last entry of a page that has more after it ends the sample.
    let q = Query {
        scan: 3,
        count: 3,
        ..query()
    };
    let found = sample_trees(&filer(&fake), &q).await.unwrap();
    assert_eq!((found.roots.len(), found.scanned), (3, 3));
    assert_eq!(fake.filer(0).count(Kind::FilerList), 2);
    let q = Query { scan: 0, ..query() };
    let found = sample_trees(&filer(&fake), &q).await.unwrap();
    assert_eq!(found, Sampled::default());
    assert_eq!(fake.filer(0).count(Kind::FilerList), 2);
}

#[tokio::test]
async fn a_long_listing_is_read_in_pages_of_a_thousand() {
    let mut builder = Fake::builder();
    for i in 0..2500 {
        builder = builder.object(BUCKET, &format!("cas/l/z{i:04}"), vec![0]);
    }
    let fake = builder.start().await;
    // Nothing is large enough to be read: the listing alone is walked.
    let q = Query {
        min_bytes: 2,
        scan: 100_000,
        ..query()
    };
    let found = sample_trees(&filer(&fake), &q).await.unwrap();
    assert_eq!(
        (found.roots.len(), found.scanned, found.unparsed),
        (0, 2500, 0)
    );
    assert_eq!(fake.filer(0).count(Kind::FilerList), 3);
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 0);
    // A scan of 1500 asks for 1000 and then for 500.
    let q = Query { scan: 1500, ..q };
    let found = sample_trees(&filer(&fake), &q).await.unwrap();
    assert_eq!(found.scanned, 1500);
    assert_eq!(fake.filer(0).count(Kind::FilerList), 5);
}

#[tokio::test]
async fn a_listing_or_a_manifest_that_cannot_be_read_is_an_error_naming_it() {
    let fake = store().await;
    let f = filer(&fake);
    fake.filer(0)
        .fail_status("/buckets/packs/cas/l/t03", 500, None);
    let e = sample_trees(&f, &query()).await.unwrap_err().to_string();
    assert!(e.ends_with("/cas/l/t03: status 500"), "{e}");
    fake.filer(0).clear_rules("/buckets/packs/cas/l/t03");
    // An answer that is not a listing.
    fake.filer(0)
        .fail_status("/buckets/packs/cas/l/", 200, None);
    let e = sample_trees(&f, &query()).await.unwrap_err().to_string();
    assert!(e.contains("/cas/l/?limit=1000&lastFileName=: "), "{e}");
    fake.filer(0)
        .fail_status("/buckets/packs/cas/l/", 404, None);
    let e = sample_trees(&f, &query()).await.unwrap_err().to_string();
    assert!(e.ends_with("status 404") || e.ends_with("not found"), "{e}");
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

fn args(fake: &Fake, more: &[&str]) -> Vec<String> {
    let mut args = vec![
        "sample-trees".to_owned(),
        "--filer".to_owned(),
        fake.filer(0).url(""),
    ];
    args.extend(more.iter().map(|a| (*a).to_owned()));
    args
}

#[tokio::test]
async fn sample_trees_prints_one_root_per_line_and_says_what_it_looked_at() {
    let fake = store().await;
    let (code, out, err) = cli(&args(
        &fake,
        &[
            "--min-dirs",
            "2",
            "--count",
            "3",
            "--since",
            "2026-10-01T00:00:00Z",
        ],
    ))
    .await;
    assert_eq!((code, out.as_str()), (0, "t05\nt06\nt07\n"));
    assert_eq!(err, "3 trees of 7 entries looked at, 1 not manifests\n");
}

#[tokio::test]
async fn sample_trees_writes_the_roots_to_the_file_it_is_given() {
    let fake = store().await;
    let dir = scratch("sample-out");
    let out = dir.path().join("roots");
    std::fs::write(&out, "old\n").unwrap();
    let out_arg = out.display().to_string();
    let (code, stdout, err) = cli(&args(
        &fake,
        &[
            "--out",
            &out_arg,
            "--count",
            "2",
            "--scan",
            "50",
            "--after",
            "t01",
            "--max-bytes",
            "100000",
        ],
    ))
    .await;
    assert_eq!((code, stdout.as_str()), (0, ""));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "t02\nt03\n");
    assert_eq!(err, "2 trees of 2 entries looked at, 0 not manifests\n");
    // No tree at all: an empty file, not a blank line.
    let (code, _, err) = cli(&args(&fake, &["--out", &out_arg, "--min-dirs", "9"])).await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "");
    // Over the default size bound nothing is read; the file cannot be written to a directory.
    let (code, _, err) = cli(&args(&fake, &["--out", &dir.path().display().to_string()])).await;
    assert_eq!(code, 1);
    assert!(err.starts_with("readbench: write "), "{err}");
}

#[tokio::test]
async fn sample_trees_of_a_listing_that_fails_is_exit_1() {
    let fake = store().await;
    fake.filer(0)
        .fail_status("/buckets/packs/cas/l/", 500, None);
    let (code, out, err) = cli(&args(&fake, &[])).await;
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.starts_with("readbench: http://"), "{err}");
    assert!(err.ends_with("status 500\n"), "{err}");
}
