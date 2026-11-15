//! The local cache's directory `extents-v1`: a sparse file of an object's full size and a JSON
//! sidecar `<hex>.ext` that lists the filled ranges. `obj/` holds whole objects only, so a release
//! that does not know the directory ignores it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tierstore::server::local_cache::{CacheConfig, LocalCache, RangeWant, SystemDisk};
use tierstore::{Backend, ByteRange, FaultyBackend, MemoryBackend, Op, TestClock};
use tierstore_testkit::dir::TestDir;

const SIDECAR: &[u8] = include_bytes!("fixtures/cache_extents.json");
const SIDECAR_NEWER: &[u8] = include_bytes!("fixtures/cache_extents_newer.json");

const KEY: &str = "models/pack.bin";
const BODY: &[u8] = b"0123456789abcdef";
/// The SHA-256 of the key, which names the files of an object that is not named by its content.
const HEX: &str = "12b3b178996cd91e0e6dfd04499b007e2f3ac35fd4291836e55fa677a2051e99";
/// The SHA-256 of `BODY[4..9]` and of `BODY[10..13]`, the two ranges of the fixture.
const SHA_FIRST: &str = "f43640d7c16bae51d37d5d50d6d3dc534b433347d879d683e5a3c7ae36c4009c";
const SHA_SECOND: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const MODIFIED: u64 = 1_790_000_000_000;

fn data_path(root: &Path) -> PathBuf {
    root.join("extents-v1").join("12").join("b3").join(HEX)
}

fn meta_path(root: &Path) -> PathBuf {
    root.join("extents-v1")
        .join("12")
        .join("b3")
        .join(format!("{HEX}.ext"))
}

/// A directory whose entry by extents has the bytes of `BODY` in its two ranges and zeros
/// elsewhere, with `sidecar` as its sidecar.
fn directory_with(scratch: &Path, sidecar: &[u8]) -> PathBuf {
    let root = scratch.join("cache");
    fs::create_dir_all(data_path(&root).parent().unwrap()).unwrap();
    let mut bytes = vec![0; BODY.len()];
    bytes[4..9].copy_from_slice(&BODY[4..9]);
    bytes[10..13].copy_from_slice(&BODY[10..13]);
    fs::write(data_path(&root), bytes).unwrap();
    fs::write(meta_path(&root), sidecar).unwrap();
    root
}

fn open(root: &Path, upstream: Arc<dyn Backend>, clock: Arc<TestClock>) -> LocalCache {
    let config = CacheConfig::new(root, 1 << 20, vec![("models/".to_owned(), true)]);
    LocalCache::open(config, upstream, Arc::new(SystemDisk), clock).unwrap()
}

/// An upstream that holds `BODY` at `KEY`, counting its calls.
async fn upstream(clock: &Arc<TestClock>) -> Arc<FaultyBackend> {
    let memory = Arc::new(MemoryBackend::with_clock(clock.clone()));
    memory.put(KEY, BODY.to_vec().into()).await.unwrap();
    Arc::new(FaultyBackend::new(memory))
}

/// Asserts that a fill of two ranges, made by an upstream whose clock says the time of the
/// fixture, writes the sparse file and the sidecar of the fixture byte for byte, in the directories
/// the hex of the key names. Catches a renamed, removed or re-meant sidecar field, a changed name,
/// or a range written at another offset.
#[tokio::test]
async fn a_fill_writes_the_entry_of_the_fixture() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "stored-extents-write");
    let clock = Arc::new(TestClock::new());
    clock.set(MODIFIED);
    let root = scratch.path().join("cache");
    let cache = open(&root, upstream(&clock).await, clock);
    for (offset, len, sha256) in [(4, 5, SHA_FIRST), (10, 3, SHA_SECOND)] {
        let want = RangeWant {
            pack: KEY,
            known: None,
            offset,
            len,
            sha256,
        };
        cache.fill_range(&want).await.unwrap();
    }
    assert_eq!(fs::read(meta_path(&root)).unwrap(), SIDECAR);
    let file = fs::read(data_path(&root)).unwrap();
    assert_eq!(file.len(), BODY.len());
    assert_eq!(file[4..9], BODY[4..9]);
    assert_eq!(file[10..13], BODY[10..13]);
    assert!(
        file[..4]
            .iter()
            .chain(&file[9..10])
            .chain(&file[13..])
            .all(|b| *b == 0)
    );
}

/// Asserts that a cache that opens over a directory holding the entry of the fixture reads it:
/// 8 bytes held by one entry and no object, a span of a held range in the entry's file, none for a
/// range that is not held; and that a ranged read of a held range is answered from the entry (the
/// upstream is not asked), while a read of a range that is not held, and one of the whole object,
/// give the upstream's bytes, never the hole.
#[tokio::test]
async fn a_cache_reads_the_entry_of_the_fixture_and_never_serves_a_hole() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "stored-extents-read");
    let root = directory_with(scratch.path(), SIDECAR);
    let clock = Arc::new(TestClock::new());
    let upstream = upstream(&clock).await;
    let cache = open(&root, upstream.clone(), clock);

    assert_eq!(cache.held(), (8, 0));
    let span = cache.span_of(KEY, 5, 3).unwrap();
    assert_eq!(
        (span.path, span.offset, span.whole),
        (data_path(&root), 5, false)
    );
    assert!(cache.span_of(KEY, 8, 2).is_none());
    assert_eq!(
        cache.get(KEY, Some(ByteRange::new(5, 3))).await.unwrap(),
        "567"
    );
    assert_eq!(upstream.calls(Op::Get), 0);
    assert_eq!(
        cache.get(KEY, Some(ByteRange::new(0, 4))).await.unwrap(),
        "0123"
    );
    assert_eq!(cache.get(KEY, None).await.unwrap(), BODY);
    assert!(upstream.calls(Op::Get) > 0);
}

/// Asserts that a sidecar with a field this reader does not know is read as the fixture is, and
/// that one whose ranges are damaged (out of order) is not an entry: the files are removed.
#[tokio::test]
async fn an_added_field_is_read_and_a_damaged_list_is_removed() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "stored-extents-newer");
    let root = directory_with(scratch.path(), SIDECAR_NEWER);
    let clock = Arc::new(TestClock::new());
    let cache = open(&root, upstream(&clock).await, clock.clone());
    assert_eq!(cache.held(), (8, 0));
    drop(cache);

    let text = String::from_utf8(SIDECAR.to_vec()).unwrap();
    let damaged = text.replace("[[4,5],[10,3]]", "[[10,3],[4,5]]");
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "stored-extents-damaged");
    let root = directory_with(scratch.path(), damaged.as_bytes());
    let cache = open(&root, upstream(&clock).await, clock);
    assert_eq!(cache.held(), (0, 0));
    assert!(!data_path(&root).exists());
    assert!(!meta_path(&root).exists());
}

/// Asserts that the release before the extents reads the directory this one wrote: a cache that
/// holds a whole object and an entry by extents keeps the whole object's two files in `obj/` and
/// the entry's two only in `extents-v1/`, so a scan of `obj/` (which is all that release does)
/// finds the whole object and nothing it would take for another; and that this release reads
/// a directory of the earlier one, which has no `extents-v1/`, and makes none until a range is filled.
#[tokio::test]
async fn obj_holds_only_whole_objects_and_extents_v1_is_made_when_used() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "stored-extents-beside");
    let clock = Arc::new(TestClock::new());
    let upstream = upstream(&clock).await;
    upstream
        .put("models/other.bin", "hello".into())
        .await
        .unwrap();
    let root = scratch.path().join("cache");
    let cache = open(&root, upstream, clock);
    assert_eq!(cache.get("models/other.bin", None).await.unwrap(), "hello");
    assert!(!root.join("extents-v1").exists());
    let want = RangeWant {
        pack: KEY,
        known: None,
        offset: 4,
        len: 5,
        sha256: SHA_FIRST,
    };
    cache.fill_range(&want).await.unwrap();

    let mut in_obj = names_under(&root.join("obj"));
    in_obj.sort();
    assert_eq!(in_obj.len(), 2);
    assert!(in_obj[0].ends_with(".meta") != in_obj[1].ends_with(".meta"));
    assert!(in_obj.iter().all(|name| !name.contains(HEX)));
    let mut in_extents = names_under(&root.join("extents-v1"));
    in_extents.sort();
    assert_eq!(in_extents, [HEX.to_owned(), format!("{HEX}.ext")]);
}

/// The file names under `dir`, at any depth.
fn names_under(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            names.extend(names_under(&path));
        } else {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names
}
