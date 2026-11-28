//! A build node started from its file, with `[cache]`, over the loopback S3 service as its cold
//! store: a GET of a large object through its S3 front goes through the read path the start
//! built (`local_cache::large::stack`), every part of it a ranged GET with `If-Match`, and none
//! of it is kept whole in `obj/`.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::s3::{Addressing, Credentials, S3Config, S3Store};
use objstore_s3::{ByteRange, Capabilities, ObjectKey, ObjectStore as _};
use tierstore_testkit::dir::TestDir;
use tokio::sync::oneshot;

use crate::server::cache_cli::run_cache;
use crate::server::cache_cli_tests::probe;
use crate::server::cli::Addresses;
use crate::server::local_cache::large::testing::{MIB, build_node_file, cache_settings, object};
use crate::server::testing::{BUCKET, KEY_ID, SECRET};

/// A client of the S3 front at `addr`, signing with the key of the node's file.
fn front(addr: SocketAddr) -> S3Store {
    let config = S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".to_owned(),
        bucket: BUCKET.to_owned(),
        addressing: Addressing::Auto,
        ca_pem: None,
        credentials: Credentials::new(KEY_ID, SECRET),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    S3Store::new(config).unwrap()
}

/// Every file under `dir`, at any depth (none if it is not there).
fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// Asserts that a node started from a file whose cold store holds an object larger than its
/// `whole_fill_limit` serves a ranged GET of it through the engine: the bytes of the range, every
/// GET the cold store saw of the object a ranged one with `If-Match` of the object's tag, no file
/// of the object in `obj/`, its pack under `packs-v2/`, and the families of the read path on
/// `/metricz`.
#[tokio::test]
async fn s3_get_of_a_large_object_goes_through_the_engine_in_a_started_server() {
    let dir = TestDir::under(std::env::temp_dir(), "tss3large");
    let fake = FakeS3::start().await;
    let direct = fake.store(Capabilities {
        conditional_put: true,
        object_lock: false,
    });
    let key = ObjectKey::new("cas/big/object").unwrap();
    let bytes = object(3 * MIB + 11, 1);
    direct.put_new(&key, bytes.clone(), None).await.unwrap();
    let etag = direct.head(&key).await.unwrap().etag;
    // The design's example: the new cache's root is the directory of the old one.
    let root = dir.path().join("cache");
    let table = format!("root = \"{}\"\nbudget = \"64MiB\"", root.display());
    let mut settings = cache_settings(&build_node_file(dir.path(), fake.endpoint(), &table));
    let boot = dir.path().join("boot_id");
    std::fs::write(&boot, "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0\n").unwrap();
    settings.read_path.as_mut().unwrap().boot_id_file = boot;
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let running = run_cache(
        &settings,
        async move {
            let _ = stopped.await;
        },
        move |at| drop(told.send(at)),
    );
    let (first, len) = (MIB - 3, MIB + 10);
    let driving = async move {
        let at = bound.await.unwrap();
        let asked = ByteRange::new(first, len).unwrap();
        let got: Bytes = front(at.s3).get_range(&key, asked).await.unwrap();
        let (_, page) = probe(at.s3, "/metricz").await;
        let _ = stop.send(());
        (got, page)
    };
    let (ran, (got, page)) = tokio::join!(running, driving);
    ran.unwrap();
    let start = usize::try_from(first).unwrap();
    let end = start + usize::try_from(len).unwrap();
    assert_eq!(got, bytes.slice(start..end));
    let reads: Vec<_> = fake
        .reads()
        .into_iter()
        .filter(|read| read.key == "cas/big/object")
        .collect();
    assert!(!reads.is_empty());
    for read in &reads {
        assert_eq!(read.if_match.as_deref(), Some(etag.as_str()), "{read:?}");
        assert!(read.range.is_some(), "{read:?}");
    }
    assert_eq!(
        files_under(&root.join("obj")),
        Vec::<std::path::PathBuf>::new()
    );
    let packs = files_under(&root.join("packs-v2"));
    let pack = packs
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "pack"))
        .count();
    assert_eq!(pack, 1, "{packs:?}");
    assert!(
        page.contains("# TYPE tierstore_gets_total counter\n"),
        "{page}"
    );
    assert_eq!(
        page.matches("# TYPE tierstore_build_info ").count(),
        1,
        "{page}"
    );
}

/// Asserts that a node whose file has no `[cache]` serves the same GET through `obj/` as
/// before: the cold store sees no read with `If-Match`, and no `packs-v2/` is made.
#[tokio::test]
async fn a_file_without_a_cache_table_builds_no_read_path() {
    let dir = TestDir::under(std::env::temp_dir(), "tss3large-none");
    let fake = FakeS3::start().await;
    let direct = fake.store(Capabilities::default());
    let key = ObjectKey::new("cas/big/plain").unwrap();
    let bytes = object(3 * MIB, 2);
    direct.put_new(&key, bytes.clone(), None).await.unwrap();
    let settings = cache_settings(&build_node_file(dir.path(), fake.endpoint(), ""));
    assert!(settings.read_path.is_none());
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let running = run_cache(
        &settings,
        async move {
            let _ = stopped.await;
        },
        move |at| drop(told.send(at)),
    );
    let driving = async move {
        let at = bound.await.unwrap();
        let asked = ByteRange::new(0, 10).unwrap();
        let got: Bytes = front(at.s3).get_range(&key, asked).await.unwrap();
        let _ = stop.send(());
        got
    };
    let (ran, got) = tokio::join!(running, driving);
    ran.unwrap();
    assert_eq!(got, bytes.slice(0..10));
    assert!(fake.reads().iter().all(|read| read.if_match.is_none()));
    assert!(!dir.path().join("cache/packs-v2").exists());
}
