//! The sidecar of a walked tree, made from packs read through the in-process fake filer.

#![expect(
    clippy::unwrap_used,
    reason = "the helpers of this file are part of its tests: their failure is the failure reported"
)]

#[cfg(test)]
#[expect(dead_code, reason = "this test file uses part of the fake")]
mod support;

use std::collections::BTreeMap;

use readbench::http::HttpClient;
use readbench::manifest::{encode_v2, walk};
use readbench::sidecar::{build, pack_cells, read_or_none};
use readbench::swfs::FilerFetch;
use serde_json::json;
use support::{Fake, Kind};
use tierstore_list::{PackCells, TreeListL2};
use tierstore_testkit::dir::TestDir;

const BUCKET: &str = "packs";
const ROOT: &str = "1111111111111111111111111111111111111111111111111111111111111111";

/// `n` bytes that differ from cell to cell.
fn data(n: usize, salt: usize) -> Vec<u8> {
    (0..n).map(|i| ((i >> 7) + salt + i) as u8).collect()
}

fn filer(fake: &Fake) -> FilerFetch {
    FilerFetch {
        http: HttpClient::new(10_000),
        base: fake.filer(0).url("/buckets/packs"),
    }
}

fn file(name: &str, size: u64, pack: &str, offset: u64) -> serde_json::Value {
    json!({"name": name, "kind": "file", "sha256": hex::encode([1u8; 32]), "size": size,
        "exec": false, "source": {"key": pack, "offset": offset}})
}

/// A fake holding two packs (2.5 MiB and 100 bytes), chunked in 1 MiB, and a root manifest of two
/// files that cover them.
async fn tree() -> (Fake, Vec<u8>, BTreeMap<String, u64>) {
    let (a, b) = (data(2_621_440, 1), data(100, 2));
    let manifest = json!({"v": 1, "children": [
        file("a", 2_621_440, "cas/b/a/pack", 0),
        file("b", 100, "cas/b/b/pack", 0),
    ]});
    let fake = Fake::builder()
        .chunk_size(1 << 20)
        .object(BUCKET, "cas/b/a/pack", a)
        .object(BUCKET, "cas/b/b/pack", b)
        .object(
            BUCKET,
            &format!("cas/l/{ROOT}"),
            manifest.to_string().into_bytes(),
        )
        .start()
        .await;
    let walked = walk(ROOT, &filer(&fake), 2).await.unwrap();
    let v2 = encode_v2(&walked.list).unwrap();
    let sizes = BTreeMap::from([
        ("cas/b/a/pack".to_owned(), 2_621_440),
        ("cas/b/b/pack".to_owned(), 100),
    ]);
    (fake, v2, sizes)
}

#[tokio::test]
async fn pack_cells_are_the_digests_of_the_packs_bytes_read_once() {
    let (fake, _, _) = tree().await;
    let cells = pack_cells(&filer(&fake), "cas/b/a/pack", 2_621_440)
        .await
        .unwrap();
    assert_eq!(cells, PackCells::of(&data(2_621_440, 1)));
    assert_eq!(cells.digests.len(), 3);
    assert_eq!(fake.filer(0).path_count("/buckets/packs/cas/b/a/pack"), 1);
}

#[tokio::test]
async fn a_pack_of_no_bytes_has_no_cells_and_is_not_read() {
    let (fake, _, _) = tree().await;
    let cells = pack_cells(&filer(&fake), "cas/b/none", 0).await.unwrap();
    assert_eq!(cells, PackCells::default());
    // Only the root manifest was read.
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 1);
}

#[tokio::test]
async fn a_pack_shorter_than_its_size_is_refused_naming_it() {
    let (fake, _, _) = tree().await;
    let err = pack_cells(&filer(&fake), "cas/b/b/pack", 150)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "cas/b/b/pack: read 100 bytes of a pack of 150"
    );
}

#[tokio::test]
async fn a_pack_cut_off_mid_body_or_missing_fails_the_read() {
    let (fake, _, _) = tree().await;
    fake.filer(0).cut_after("/buckets/packs/cas/b/a/pack", 10);
    let err = pack_cells(&filer(&fake), "cas/b/a/pack", 2_621_440)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("body"), "{err}");
    let err = pack_cells(&filer(&fake), "cas/b/none", 5)
        .await
        .unwrap_err();
    assert!(err.to_string().ends_with("not found"), "{err}");
}

#[tokio::test]
async fn the_sidecar_of_a_walked_tree_opens_with_the_cells_of_its_packs() {
    let (fake, v2, sizes) = tree().await;
    let sidecar = build(&v2, &filer(&fake), &sizes, 2).await.unwrap();
    let l2 = TreeListL2::open(sidecar).unwrap();
    assert_eq!(l2.pack_key(0), Some("cas/b/a/pack"));
    assert_eq!(l2.pack_key(1), Some("cas/b/b/pack"));
    assert_eq!(l2.pack_key(2), None);
    // Each pack was read once, whole.
    assert_eq!(fake.filer(0).path_count("/buckets/packs/cas/b/a/pack"), 1);
    assert_eq!(fake.filer(0).path_count("/buckets/packs/cas/b/b/pack"), 1);
}

#[tokio::test]
async fn a_sidecar_with_parallel_zero_still_reads_every_pack() {
    let (fake, v2, sizes) = tree().await;
    assert!(build(&v2, &filer(&fake), &sizes, 0).await.is_ok());
}

#[tokio::test]
async fn a_pack_the_list_names_but_the_sizes_lack_is_a_sidecar_error() {
    let (fake, v2, mut sizes) = tree().await;
    sizes.remove("cas/b/b/pack");
    let err = build(&v2, &filer(&fake), &sizes, 2).await.unwrap_err();
    assert!(err.to_string().starts_with("sidecar: "), "{err}");
    assert!(err.to_string().contains("cas/b/b/pack"), "{err}");
}

#[tokio::test]
async fn a_list_that_is_not_version_2_is_a_sidecar_error() {
    let (fake, _, sizes) = tree().await;
    let err = build(b"nope", &filer(&fake), &sizes, 2).await.unwrap_err();
    assert!(err.to_string().starts_with("sidecar: "), "{err}");
}

#[tokio::test]
async fn an_unreadable_pack_fails_the_build() {
    let (fake, v2, mut sizes) = tree().await;
    sizes.insert("cas/b/none".to_owned(), 5);
    let err = build(&v2, &filer(&fake), &sizes, 2).await.unwrap_err();
    assert!(err.to_string().ends_with("not found"), "{err}");
}

#[test]
fn a_file_is_read_and_a_missing_one_is_none() {
    let dir = TestDir::under(std::env::temp_dir(), "readbench-sidecar");
    let path = dir.path().join("x.l2");
    assert_eq!(read_or_none(&path).unwrap(), None);
    std::fs::write(&path, b"abc").unwrap();
    assert_eq!(read_or_none(&path).unwrap(), Some(b"abc".to_vec()));
}

#[test]
fn a_path_that_cannot_be_read_is_named() {
    let dir = TestDir::under(std::env::temp_dir(), "readbench-sidecar-dir");
    let err = read_or_none(dir.path()).unwrap_err();
    assert!(
        err.to_string()
            .starts_with(&format!("read {}: ", dir.path().display())),
        "{err}"
    );
}
