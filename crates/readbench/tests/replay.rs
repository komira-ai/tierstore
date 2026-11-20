//! The three replays and the preparation of a tree, against the in-process fake store.

#![expect(
    clippy::unwrap_used,
    reason = "the helpers of this file are part of its tests: their failure is the failure reported"
)]

#[cfg(test)]
#[expect(dead_code, reason = "this test file uses part of the fake")]
mod support;
#[cfg(test)]
mod tree;

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use readbench::clock::{Clock, MachineClock};
use readbench::record::Mode;
use readbench::replay::{
    Client, Knobs, Prepared, check_outcome, outcome_label, pack_spec, prepare, prepare_unverified,
    run_demand, run_engine, run_shipped,
};
use sha2::{Digest as _, Sha256};
use support::Kind;
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::{PackKey, PackRoot};
use tierstore_testkit::dir::TestDir;
use tree::{
    PACK_A, PACK_B, PACK_C, ROOT, ROOT_BYTES, SIZE_A, SIZE_B, SIZE_C, Variant, data, fake, store,
};

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn knobs() -> Knobs {
    let mut knobs = Knobs::new(1);
    knobs.hash_threads = NonZeroUsize::new(2);
    knobs
}

fn client() -> Client {
    Client {
        run: "r1".to_owned(),
        node: "n1".to_owned(),
        client: 0,
        clients: 1,
        job: 4,
    }
}

fn clock() -> Arc<dyn Clock> {
    MachineClock::shared()
}

/// The tree `ROOT` prepared against the whole store, its sidecar kept in `dir`.
async fn prepared(dir: &Path) -> Prepared {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    prepare(&store, ROOT, dir, 4, clock().as_ref())
        .await
        .unwrap()
}

/// The bytes of every `.pack` file under `dir`, by size.
fn pack_files(dir: &Path) -> Vec<Vec<u8>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "pack") {
                out.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    walk(dir, &mut paths);
    let mut files: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    files.sort_by_key(Vec::len);
    files
}

#[tokio::test]
async fn a_tree_is_walked_and_its_sidecar_made_once_then_found() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let dir = scratch("replay-prepare");
    let first = prepare(&store, ROOT, dir.path(), 4, clock().as_ref())
        .await
        .unwrap();
    assert!(first.sidecar_built);
    assert_eq!(first.root, ROOT);
    assert_eq!(
        (
            first.walked.manifests,
            first.walked.files,
            first.walked.packs,
            first.walked.bytes
        ),
        (2, 4, 3, ROOT_BYTES)
    );
    assert_eq!(
        first.sizes,
        [
            (PACK_A.to_owned(), SIZE_A),
            (PACK_B.to_owned(), SIZE_B),
            (PACK_C.to_owned(), SIZE_C)
        ]
        .into_iter()
        .collect()
    );
    let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(files.len(), 1);
    let file = files[0].as_ref().unwrap().path();
    assert_eq!(std::fs::read(&file).unwrap(), first.l2.bytes());
    assert_eq!(
        file.file_name().unwrap().to_str().unwrap(),
        format!("{}.l2", hex::encode(sha256(&first.v2)))
    );
    let reads = |fake: &support::Fake| {
        (
            fake.filer(0).count(Kind::FilerGet),
            fake.filer(0).count(Kind::FilerMetadata),
        )
    };
    // Two manifests, three packs; one entry per pack.
    assert_eq!(reads(&fake), (5, 3));
    let second = prepare(&store, ROOT, dir.path(), 4, clock().as_ref())
        .await
        .unwrap();
    assert!(!second.sidecar_built);
    assert_eq!(second.l2.bytes(), first.l2.bytes());
    assert_eq!(second.v2, first.v2);
    assert_eq!(second.sizes, first.sizes);
    // The packs and their entries were not read again; the manifests were.
    assert_eq!(reads(&fake), (7, 3));
}

#[tokio::test]
async fn a_sidecar_that_is_not_the_trees_is_refused_naming_its_file() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let dir = scratch("replay-refused");
    prepare(&store, ROOT, dir.path(), 4, clock().as_ref())
        .await
        .unwrap();
    let file = std::fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&file, b"not a sidecar").unwrap();
    let err = prepare(&store, ROOT, dir.path(), 4, clock().as_ref())
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with(&format!("sidecar: {}: ", file.display())),
        "{err}"
    );
}

#[tokio::test]
async fn a_pack_the_filer_lacks_stops_the_preparation_naming_it() {
    let fake = fake(Variant::Without(PACK_C)).await;
    let store = store(&fake).await;
    let dir = scratch("replay-nopack");
    let err = prepare(&store, ROOT, dir.path(), 4, clock().as_ref())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("{PACK_C}: the filer has no such pack")
    );
}

#[tokio::test]
async fn sidecars_that_cannot_be_made_or_written_are_named() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let dir = scratch("replay-unwritable");
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, b"x").unwrap();
    let err = prepare(&store, ROOT, &blocker.join("sub"), 4, clock().as_ref())
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("create "), "{err}");
    assert!(
        err.to_string().contains(&blocker.display().to_string()),
        "{err}"
    );
    // A sidecar that is a link to nowhere reads as missing and cannot be written.
    let first = scratch("replay-unwritable-name");
    prepare(&store, ROOT, first.path(), 4, clock().as_ref())
        .await
        .unwrap();
    let name = std::fs::read_dir(first.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let linked = dir.path().join("linked");
    std::fs::create_dir(&linked).unwrap();
    let path = linked.join(name);
    std::os::unix::fs::symlink(dir.path().join("nowhere/x"), &path).unwrap();
    let err = prepare(&store, ROOT, &linked, 4, clock().as_ref())
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with(&format!("write {}: ", path.display())),
        "{err}"
    );
}

#[tokio::test]
async fn a_tree_that_is_not_there_fails_the_walk() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let dir = scratch("replay-notree");
    let err = prepare(&store, &"4".repeat(64), dir.path(), 4, clock().as_ref())
        .await
        .unwrap_err();
    assert!(err.to_string().ends_with("not found"), "{err}");
}

#[test]
fn a_wait_that_did_not_end_ready_names_the_pack() {
    let pack = PackKey("cas/b/x/pack".to_owned());
    assert!(check_outcome(&pack, WaitOutcome::Ready).is_ok());
    assert_eq!(
        check_outcome(&pack, WaitOutcome::Eio { cell: 3 })
            .unwrap_err()
            .to_string(),
        "fill: cas/b/x/pack: cell 3 failed (eio)"
    );
    assert_eq!(
        check_outcome(&pack, WaitOutcome::Lost)
            .unwrap_err()
            .to_string(),
        "fill: cas/b/x/pack: the fill is lost"
    );
}

#[test]
fn a_demand_read_is_labelled_by_how_its_wait_ended() {
    assert_eq!(outcome_label(WaitOutcome::Ready), "ready");
    assert_eq!(outcome_label(WaitOutcome::Eio { cell: 1 }), "eio");
    assert_eq!(outcome_label(WaitOutcome::Lost), "lost");
}

#[tokio::test]
async fn a_pack_the_list_does_not_have_is_an_error() {
    let dir = scratch("replay-spec");
    let p = prepared(dir.path()).await;
    let spec = pack_spec(&p, 0).unwrap();
    assert_eq!(spec.id.key.0, PACK_A);
    assert_eq!(spec.size, SIZE_A);
    assert!(spec.digests.is_some());
    assert_eq!(
        pack_spec(&p, 9).unwrap_err().to_string(),
        "fill: the list has no pack 9"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_engine_fetches_every_pack_whole_with_every_cell_present() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let dir = scratch("replay-engine");
    let p = prepare(
        &store,
        ROOT,
        &dir.path().join("sidecars"),
        4,
        clock().as_ref(),
    )
    .await
    .unwrap();
    let cache = dir.path().join("cache");
    let run = run_engine(
        &store,
        &p,
        &knobs(),
        &client(),
        &cache,
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            run.run.as_str(),
            run.node.as_str(),
            run.tree.as_str(),
            run.mode
        ),
        ("r1", "n1", ROOT, Mode::Engine)
    );
    assert_eq!((run.client, run.clients), (0, 1));
    assert_eq!(
        (run.manifests, run.files, run.packs, run.bytes_needed),
        (2, 4, 3, ROOT_BYTES)
    );
    // Whole packs: every byte of the three packs, once, and only grid parts.
    assert_eq!(run.bytes_fetched, SIZE_A + SIZE_B + SIZE_C);
    assert_eq!(run.get_errors, 0);
    assert_eq!(
        run.gets["demand_sub"] + run.gets["retry"] + run.gets["hedge"],
        0
    );
    assert_eq!(run.gets.len(), 4);
    // Every GET is a part of the recorder's, and a request the volume servers got.
    assert_eq!(run.total_gets(), run.first_byte_us.n);
    assert_eq!(run.total_gets(), run.get_us.n);
    assert_eq!(run.total_gets(), fake.total(Kind::VolumeGet));
    // One part for each chunk of the packs (3 + 3 + 1): no pack is small enough to be one part.
    assert_eq!(run.gets["grid_part"], 7);
    let files = pack_files(&cache);
    assert_eq!(
        files,
        vec![data(SIZE_C, 3), data(SIZE_B, 2), data(SIZE_A, 1)]
    );
    assert_eq!(run.config["hash_threads"], "2");
    assert_eq!(run.config["sidecar_built"], "true");
    assert_eq!(run.config["part_sizes_mib"], "32,32,64");
    assert_eq!(run.config["cold_drives"], "12");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_measured_hash_pool_has_at_least_one_thread() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let dir = scratch("replay-measured");
    let p = prepare(
        &store,
        ROOT,
        &dir.path().join("sidecars"),
        4,
        clock().as_ref(),
    )
    .await
    .unwrap();
    let mut knobs = knobs();
    knobs.hash_threads = None;
    let run = run_engine(
        &store,
        &p,
        &knobs,
        &client(),
        &dir.path().join("cache"),
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert!(run.config["hash_threads"].parse::<usize>().unwrap() >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pack_the_store_lost_fails_the_run_naming_it() {
    let dir = scratch("replay-lost");
    let p = prepared(&dir.path().join("sidecars")).await;
    let fake = fake(Variant::Without(PACK_B)).await;
    let store = store(&fake).await;
    let err = run_engine(
        &store,
        &p,
        &knobs(),
        &client(),
        &dir.path().join("cache"),
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), format!("fill: {PACK_B}: the fill is lost"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_budget_the_topology_cannot_run_is_refused() {
    let dir = scratch("replay-budget");
    let p = prepared(&dir.path().join("sidecars")).await;
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let mut knobs = knobs();
    knobs.budget.topology.clients = 0;
    let err = run_engine(
        &store,
        &p,
        &knobs,
        &client(),
        &dir.path().join("cache"),
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("budget: budget: topology.clients is 0"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cache_that_cannot_be_made_is_an_entry_store_error() {
    let dir = scratch("replay-nocache");
    let p = prepared(&dir.path().join("sidecars")).await;
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, b"x").unwrap();
    let err = run_engine(
        &store,
        &p,
        &knobs(),
        &client(),
        &blocker,
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().starts_with("entry store: making "), "{err}");
}

#[tokio::test]
async fn the_shipped_way_reads_each_file_in_pieces_and_hashes_every_byte() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let dir = scratch("replay-shipped");
    let p = prepare(&store, ROOT, dir.path(), 4, clock().as_ref())
        .await
        .unwrap();
    let before = fake.total(Kind::FilerGet);
    let mut knobs = knobs();
    knobs.fill_chunk = 1 << 20;
    knobs.fill_concurrency = 3;
    let run = run_shipped(&store, &p, &knobs, &client(), clock())
        .await
        .unwrap();
    assert_eq!(run.mode, Mode::Shipped);
    // ceil(2621440 / 1 MiB) + ceil(2097252 / 1 MiB) + 1 + 1.
    assert_eq!(run.gets, [("chunk".to_owned(), 8)].into_iter().collect());
    assert_eq!(fake.total(Kind::FilerGet) - before, 8);
    assert_eq!(run.bytes_fetched, ROOT_BYTES);
    assert_eq!(run.bytes_needed, ROOT_BYTES);
    assert_eq!(run.config["hashed_bytes"], ROOT_BYTES.to_string());
    assert_eq!(run.config["fill_chunk"], (1u64 << 20).to_string());
    assert_eq!(
        (run.first_byte_us.n, run.get_us.n, run.get_errors),
        (8, 8, 0)
    );
}

#[tokio::test]
async fn a_shipped_fill_of_a_lost_cut_or_short_pack_fails_naming_it() {
    let dir = scratch("replay-shipped-bad");
    let p = prepared(&dir.path().join("sidecars")).await;
    let mut knobs = knobs();
    knobs.fill_chunk = 1 << 20;
    let lost = fake(Variant::Without(PACK_C)).await;
    let err = run_shipped(&store(&lost).await, &p, &knobs, &client(), clock())
        .await
        .unwrap_err();
    assert!(err.to_string().ends_with("not found"), "{err}");
    assert!(err.to_string().contains(PACK_C), "{err}");
    let short = fake(Variant::Short(PACK_B, 1000)).await;
    let err = run_shipped(&store(&short).await, &p, &knobs, &client(), clock())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("{PACK_B}: read 1000 bytes of the 1048576 at 0")
    );
    let cut = fake(Variant::Whole).await;
    cut.filer(0)
        .cut_after(&format!("/buckets/packs/{PACK_A}"), 10);
    let err = run_shipped(&store(&cut).await, &p, &knobs, &client(), clock())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("body"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demand_reads_wait_for_one_file_each_from_distinct_packs() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let dir = scratch("replay-demand");
    let p = prepare(
        &store,
        ROOT,
        &dir.path().join("sidecars"),
        4,
        clock().as_ref(),
    )
    .await
    .unwrap();
    let reads = run_demand(
        &store,
        &p,
        &knobs(),
        &client(),
        &dir.path().join("cache"),
        20,
        6,
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    // The first file of each pack by path: a (pack A), b (pack B), sub/d (pack C).
    assert_eq!(
        reads.iter().map(|r| r.len).collect::<Vec<_>>(),
        vec![2_621_440, SIZE_B, 100]
    );
    assert!(reads.iter().all(|r| r.outcome == "ready"), "{reads:?}");
    assert!(
        reads
            .iter()
            .all(|r| (r.run.as_str(), r.node.as_str(), r.load_clients) == ("r1", "n1", 6))
    );
    let one = run_demand(
        &store,
        &p,
        &knobs(),
        &client(),
        &dir.path().join("cache2"),
        1,
        0,
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].len, 2_621_440);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_demand_read_of_a_lost_pack_is_recorded_lost() {
    let dir = scratch("replay-demand-lost");
    let p = prepared(&dir.path().join("sidecars")).await;
    let fake = fake(Variant::Without(PACK_A)).await;
    let store = store(&fake).await;
    let reads = run_demand(
        &store,
        &p,
        &knobs(),
        &client(),
        &dir.path().join("cache"),
        1,
        0,
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].outcome, "lost");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_demand_run_whose_cache_cannot_be_made_fails() {
    let dir = scratch("replay-demand-nocache");
    let p = prepared(&dir.path().join("sidecars")).await;
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, b"x").unwrap();
    let err = run_demand(
        &store,
        &p,
        &knobs(),
        &client(),
        &blocker,
        1,
        0,
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().starts_with("entry store: "), "{err}");
}

#[test]
fn the_knobs_describe_themselves_as_text() {
    let knobs = Knobs::new(0);
    assert_eq!(knobs.budget.topology.clients, 1);
    let text = knobs.describe(Some(3), false, true);
    assert_eq!(text["hash_threads"], "3");
    assert_eq!(text["sidecar_built"], "false");
    assert_eq!(text["verified"], "true");
    assert_eq!(
        Knobs::new(1).describe(None, false, false)["hash_threads"],
        "none"
    );
    assert_eq!(text["whole_cov_percent"], "50");
    assert_eq!(text["cold_media_mb_per_s"], "190");
    assert_eq!(text["fill_concurrency"], "16");
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[tokio::test]
async fn an_unverified_preparation_reads_no_pack_and_writes_no_sidecar() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let p = prepare_unverified(&store, ROOT, 4, clock().as_ref())
        .await
        .unwrap();
    assert_eq!(p.root, ROOT);
    assert!(!p.sidecar_built);
    assert_eq!(
        (
            p.walked.manifests,
            p.walked.files,
            p.walked.packs,
            p.walked.bytes
        ),
        (2, 4, 3, ROOT_BYTES)
    );
    assert_eq!(
        p.sizes,
        [
            (PACK_A.to_owned(), SIZE_A),
            (PACK_B.to_owned(), SIZE_B),
            (PACK_C.to_owned(), SIZE_C)
        ]
        .into_iter()
        .collect()
    );
    // The two manifests were read through the filer, and each pack's entry (not its bytes).
    assert_eq!(fake.filer(0).count(Kind::FilerGet), 2);
    assert_eq!(fake.filer(0).count(Kind::FilerMetadata), 3);
    assert_eq!(fake.total(Kind::VolumeGet), 0);
    // Each pack is named by a generation, and its spec has no digests.
    let generations = p.unverified.as_ref().unwrap();
    assert_eq!(generations.len(), 3);
    let spec = pack_spec(&p, 0).unwrap();
    assert!(spec.digests.is_none());
    assert!(matches!(spec.id.root, PackRoot::Etag(_)));
    // The same tree prepared verified gives the same list.
    let dir = scratch("replay-unverified-same");
    let verified = prepare(&store, ROOT, dir.path(), 4, clock().as_ref())
        .await
        .unwrap();
    assert_eq!(p.v2, verified.v2);
    assert!(verified.unverified.is_none());
}

#[tokio::test]
async fn an_unverified_preparation_of_a_pack_the_filer_lacks_names_it() {
    let fake = fake(Variant::Without(PACK_C)).await;
    let store = store(&fake).await;
    let err = prepare_unverified(&store, ROOT, 4, clock().as_ref())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("{PACK_C}: the filer has no such pack")
    );
}

#[tokio::test]
async fn an_unverified_pack_without_a_generation_is_an_error() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let mut p = prepare_unverified(&store, ROOT, 4, clock().as_ref())
        .await
        .unwrap();
    p.unverified = Some(std::collections::BTreeMap::new());
    assert_eq!(
        pack_spec(&p, 0).unwrap_err().to_string(),
        format!("fill: no generation of {PACK_A}")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unverified_run_fetches_every_cell_with_the_gets_of_a_verified_run() {
    let dir = scratch("replay-unverified-run");
    let verified_fake = fake(Variant::Whole).await;
    let verified_store = store(&verified_fake).await;
    let p = prepare(
        &verified_store,
        ROOT,
        &dir.path().join("sidecars"),
        4,
        clock().as_ref(),
    )
    .await
    .unwrap();
    let verified = run_engine(
        &verified_store,
        &p,
        &knobs(),
        &client(),
        &dir.path().join("verified"),
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    let before = verified_fake.total(Kind::VolumeGet);

    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let p = prepare_unverified(&store, ROOT, 4, clock().as_ref())
        .await
        .unwrap();
    let cache = dir.path().join("unverified");
    let run = run_engine(
        &store,
        &p,
        &knobs(),
        &client(),
        &cache,
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(run.gets, verified.gets);
    assert_eq!(run.gets["grid_part"], 7);
    assert_eq!(fake.total(Kind::VolumeGet), before);
    assert_eq!(run.bytes_fetched, SIZE_A + SIZE_B + SIZE_C);
    assert_eq!(run.bytes_fetched, verified.bytes_fetched);
    assert_eq!(run.get_errors, 0);
    assert_eq!(
        pack_files(&cache),
        vec![data(SIZE_C, 3), data(SIZE_B, 2), data(SIZE_A, 1)]
    );
    assert_eq!(run.config["verified"], "false");
    assert_eq!(run.config["hash_threads"], "none");
    assert_eq!(run.config["sidecar_built"], "false");
    assert_eq!(verified.config["verified"], "true");
    assert_eq!(verified.config["hash_threads"], "2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupted_pack_byte_is_caught_verified_and_not_caught_unverified() {
    let dir = scratch("replay-unverified-corrupt");
    let p = prepared(&dir.path().join("sidecars")).await;
    let fake = fake(Variant::Corrupt(PACK_B)).await;
    let store = store(&fake).await;
    let err = run_engine(
        &store,
        &p,
        &knobs(),
        &client(),
        &dir.path().join("verified"),
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().starts_with("fill: "), "{err}");
    // Unverified, the same bytes are taken as they come.
    let p = prepare_unverified(&store, ROOT, 4, clock().as_ref())
        .await
        .unwrap();
    let cache = dir.path().join("unverified");
    run_engine(
        &store,
        &p,
        &knobs(),
        &client(),
        &cache,
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    let mut bad = data(SIZE_B, 2);
    bad[5] ^= 0xff;
    assert!(pack_files(&cache).contains(&bad));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unverified_demand_read_is_ready_without_a_hash_pool() {
    let fake = fake(Variant::Whole).await;
    let store = store(&fake).await;
    let p = prepare_unverified(&store, ROOT, 4, clock().as_ref())
        .await
        .unwrap();
    let dir = scratch("replay-unverified-demand");
    let reads = run_demand(
        &store,
        &p,
        &knobs(),
        &client(),
        &dir.path().join("cache"),
        2,
        1,
        clock(),
        &tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    assert_eq!(reads.len(), 2);
    assert!(reads.iter().all(|r| r.outcome == "ready"));
}
