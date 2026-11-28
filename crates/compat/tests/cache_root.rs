//! The new cache of `[cache]` across the release before this one and this branch: the file that
//! turns it on, and the directory it writes `packs-v2/` into, which may be the directory of the
//! release before's cache (`local_cache.path`). The release before is the real `tierstore`
//! library of its commit (`tierstore_prev`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::ffi::OsString;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use tierstore_compat::patterned;
use tierstore_entry::inproc::InProcStore;
use tierstore_fetch_api::entry::{EntrySpec, EntryStore};
use tierstore_fetch_api::ids::{CellRun, PackId, PackKey, PackRoot};
use tierstore_format::ids::Generation;
use tierstore_journal::BootId;
use tierstore_testkit::dir::TestDir;

/// One cell of an entry.
const CELL: usize = 1 << 20;

/// The boot both stores of the on-disk test run in.
const BOOT: BootId = BootId([5; 16]);

/// A build node's file under `dir`, over an S3 cold store, with `cache` as the body of a
/// `[cache]` table (none if it is empty), written; its path.
fn build_node_file(dir: &Path, cache: &str) -> OsString {
    let mut paths = Vec::new();
    for (name, text) in [
        ("access.key", "AKID\n0123456789abcdef\n"),
        ("cold.credentials", "AKID\n0123456789abcdef\n"),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        paths.push(path.display().to_string());
    }
    let table = if cache.is_empty() {
        String::new()
    } else {
        format!("[cache]\n{cache}\n")
    };
    let text = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nproxy_id = \"c1\"\nbucket = \"tier-bucket\"\n\
         access_key_file = \"{key}\"\n\n\
         [local_cache]\npath = \"{cache_dir}\"\nbudget = \"1GiB\"\n\n\
         [cold_store]\nkind = \"s3\"\nbucket = \"cold\"\nendpoint = \"http://cold.invalid\"\n\
         credentials_file = \"{credentials}\"\n\n{table}\n\
         [[policy]]\nprefix = \"cas/\"\ncache = true\n",
        key = paths[0],
        credentials = paths[1],
        cache_dir = dir.join("cache").display(),
    );
    let path = dir.join("tierstore.toml");
    std::fs::write(&path, text).unwrap();
    path.into_os_string()
}

/// The status each release exits with when it checks `file` (`--check-config`): this branch's,
/// then the release before's.
fn both(file: &OsString) -> (String, String) {
    let args = || {
        vec![
            OsString::from("tierstore"),
            OsString::from("--config"),
            file.clone(),
            OsString::from("--check-config"),
        ]
    };
    let now = tierstore::server::cli::main(args());
    let before = tierstore_prev::server::cli::main(args());
    (format!("{now:?}"), format!("{before:?}"))
}

/// Asserts that a build node's file with `[cache]` is read by this branch and refused by the
/// release before with status 2 (a table it does not know), so a host that may roll back takes
/// the table out first; and that the same file without the table is read by both.
#[test]
fn the_cache_table_is_read_here_and_refused_by_the_release_before() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-cache-table");
    let ok = format!("{:?}", ExitCode::SUCCESS);
    let refused = format!("{:?}", ExitCode::from(2));
    let root = dir.path().join("cache");
    let table = format!("root = \"{}\"\nbudget = \"1GiB\"", root.display());
    let with = build_node_file(dir.path(), &table);
    assert_eq!(both(&with), (ok.clone(), refused));
    let without = build_node_file(dir.path(), "");
    assert_eq!(both(&without), (ok.clone(), ok));
}

/// The entry of `key` that the new cache keeps, of one cell.
fn spec(key: &str) -> EntrySpec {
    EntrySpec {
        pack: PackId {
            key: PackKey(key.to_owned()),
            root: PackRoot::Etag(Generation([7; 16])),
        },
        size: CELL as u64,
    }
}

/// Every file under `dir` with its bytes, in path order.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(next) = dirs.pop() {
        for entry in std::fs::read_dir(&next).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                found.push((path, bytes));
            }
        }
    }
    found.sort();
    found
}

/// Asserts that the release before opens a cache directory in which this branch's new cache
/// wrote `packs-v2/` (an entry with one committed cell) and leaves every file of it as it was,
/// with its own cache working beside it; and that this branch then opens the entry again and
/// reads the cell it committed.
#[tokio::test]
async fn the_release_before_opens_a_cache_directory_with_packs_v2_and_leaves_it() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-cache-packs");
    let root = dir.path().join("cache");
    let cell = patterned(3, CELL);
    {
        let store = InProcStore::new(&root, Some(BOOT)).unwrap();
        let entry = store.open(&spec("cas/big")).unwrap();
        let won = entry.claim(CellRun { first: 0, count: 1 }).pop().unwrap();
        entry.publish(won, &cell).unwrap();
        entry.commit_batch().unwrap();
        store.close().unwrap();
    }
    let packs = root.join("packs-v2");
    let written = files(&packs);
    assert!(
        written
            .iter()
            .any(|(path, _)| path.extension().is_some_and(|e| e == "pack"))
    );

    let config = tierstore_prev::server::local_cache::CacheConfig::new(
        root.clone(),
        64 << 20,
        vec![("cas/".to_owned(), true)],
    );
    let upstream: Arc<dyn tierstore_prev::Backend> = Arc::new(tierstore_prev::MemoryBackend::new());
    let disk = Arc::new(tierstore_prev::server::local_cache::SystemDisk);
    let clock = Arc::new(tierstore_prev::SystemClock);
    let before = tierstore_prev::server::local_cache::LocalCache::open(
        config,
        upstream.clone(),
        disk,
        clock,
    )
    .unwrap();
    upstream
        .put("cas/small", bytes::Bytes::from_static(b"small"))
        .await
        .unwrap();
    let read = tierstore_prev::Backend::get(&before, "cas/small", None)
        .await
        .unwrap();
    assert_eq!(read, "small");
    drop(before);
    assert_eq!(files(&packs), written);

    let store = InProcStore::new(&root, Some(BOOT)).unwrap();
    let entry = store.open(&spec("cas/big")).unwrap();
    assert_eq!(entry.read_cell(0).unwrap(), cell);
}
