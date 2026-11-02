//! The local cache's directory: `obj/<aa>/<bb>/<hex>` holds an object's bytes and
//! `obj/<aa>/<bb>/<hex>.meta` the sidecar, JSON, that says what the bytes are.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tierstore::server::local_cache::{CacheConfig, LocalCache, SystemDisk};
use tierstore::{Backend, MemoryBackend, TestClock};
use tierstore_testkit::TestDir;

const SIDECAR: &[u8] = include_bytes!("fixtures/cache_sidecar.json");
const SIDECAR_NEWER: &[u8] = include_bytes!("fixtures/cache_sidecar_newer.json");

const KEY: &str = "models/a.bin";
/// The SHA-256 of the key, which names the files of an object that is not named by its content.
const HEX: &str = "bad625a3890ea33470a191b8cf197639d3565ab738e4d040304dbf4c1707d7af";
const SHA256_OF_HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const MODIFIED: u64 = 1_700_000_000_000;

fn data_path(root: &Path) -> PathBuf {
    root.join("obj").join("ba").join("d6").join(HEX)
}

fn meta_path(root: &Path) -> PathBuf {
    root.join("obj")
        .join("ba")
        .join("d6")
        .join(format!("{HEX}.meta"))
}

/// A directory with the bytes `hello` and `sidecar` as the sidecar of the object.
fn directory_with(scratch: &Path, sidecar: &[u8]) -> PathBuf {
    let root = scratch.join("cache");
    fs::create_dir_all(data_path(&root).parent().unwrap()).unwrap();
    fs::write(data_path(&root), b"hello").unwrap();
    fs::write(meta_path(&root), sidecar).unwrap();
    root
}

fn open(root: &Path, upstream: Arc<MemoryBackend>, clock: Arc<TestClock>) -> LocalCache {
    let config = CacheConfig::new(root, 1 << 20, vec![("models/".to_owned(), true)]);
    LocalCache::open(config, upstream, Arc::new(SystemDisk), clock).unwrap()
}

/// Asserts that a fill of an object of 5 bytes, made by an upstream whose clock says the time of
/// the fixture, writes the data file and the sidecar of the fixture byte for byte, in the
/// directories the hex of the key names. Catches a renamed, removed or re-meant sidecar field, or
/// a changed name for a file.
#[tokio::test]
async fn a_fill_writes_the_sidecar_of_the_fixture() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "stored-sidecar-write");
    let clock = Arc::new(TestClock::new());
    clock.set(MODIFIED);
    let upstream = Arc::new(MemoryBackend::with_clock(clock.clone()));
    upstream.put(KEY, "hello".into()).await.unwrap();
    let root = scratch.path().join("cache");
    let cache = open(&root, upstream, clock);

    assert_eq!(cache.get(KEY, None).await.unwrap(), "hello");
    assert_eq!(fs::read(meta_path(&root)).unwrap(), SIDECAR);
    assert_eq!(fs::read(data_path(&root)).unwrap(), b"hello");
}

/// Asserts that a cache that opens over a directory holding the files of the fixture reads the
/// sidecar: it holds one object of 5 bytes, says the size, tag and time of the fixture for a
/// HEAD, and serves the bytes with the upstream empty, so that nothing came from it. Catches a
/// sidecar field that is read as another.
#[tokio::test]
async fn a_cache_reads_the_sidecar_of_the_fixture() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "stored-sidecar-read");
    let root = directory_with(scratch.path(), SIDECAR);
    let clock = Arc::new(TestClock::new());
    let upstream = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cache = open(&root, upstream, clock);

    assert_eq!(cache.held(), (5, 1));
    let meta = cache.head(KEY).await.unwrap();
    assert_eq!(meta.size, 5);
    assert_eq!(meta.tag, SHA256_OF_HELLO);
    assert_eq!(meta.modified, MODIFIED);
    assert_eq!(cache.get(KEY, None).await.unwrap(), "hello");
}

/// Asserts that a sidecar with a field this reader does not know is read as the fixture is: the
/// object is kept and served.
#[tokio::test]
async fn a_sidecar_with_an_added_field_is_still_read() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "stored-sidecar-newer");
    let root = directory_with(scratch.path(), SIDECAR_NEWER);
    let clock = Arc::new(TestClock::new());
    let upstream = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cache = open(&root, upstream, clock);

    assert_eq!(cache.held(), (5, 1));
    assert_eq!(cache.get(KEY, None).await.unwrap(), "hello");
}

/// Asserts that a sidecar that lacks a field the reader needs (here the size, spelled another
/// way) does not make a whole object: the cache holds nothing and removes the files, so a
/// renamed field is not read as a default. This is the failure the two fixture tests above
/// turn into a red test.
#[tokio::test]
async fn a_sidecar_that_lacks_a_field_is_not_a_whole_object() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "stored-sidecar-renamed");
    let text = String::from_utf8(SIDECAR.to_vec()).unwrap();
    let renamed = text.replace("\"size\"", "\"bytes\"");
    let root = directory_with(scratch.path(), renamed.as_bytes());
    let clock = Arc::new(TestClock::new());
    let upstream = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cache = open(&root, upstream, clock);

    assert_eq!(cache.held(), (0, 0));
    assert!(!data_path(&root).exists());
    assert!(!meta_path(&root).exists());
}
