//! What the S3 front of a node over the cold tier alone does with each call: create-only at the
//! cold store for a put and for the completion of an upload (over a memory, a directory and an
//! S3 cold store), the ranges, listings and multipart uploads of a client's session, each role of
//! a key, and presigned PUTs.

use std::fs::Permissions;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::s3::{Addressing, Credentials, S3Config, S3Store};
use objstore_s3::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ObjectKey, ObjectStore as _, ObjectStoreError,
    PageSize,
};
use tokio::sync::oneshot;

use super::{run_cold, serve_cold};
use crate::server::cache_cli_tests::{client, probe};
use crate::server::cli::{Addresses, Error};
use crate::server::combined_cli_tests::{Node, QUICK};
use crate::server::config::cold::ColdSettings;
use crate::server::config::settings_from_text;
use crate::server::presign::{PresignRequest, presign_put};
use crate::server::{AccessKey, Drained};
use crate::{Backend, Clock, DirBackend, MemoryBackend, SystemClock};

/// The bucket the node's file names and its clients address.
const BUCKET: &str = "tier-bucket";
const READ_ID: &str = "AKIDREAD";
const READ_SECRET: &str = "read-secret-0123456789";
const GC_ID: &str = "gc-key";
const GC_SECRET: &str = "gc-secret-0123456789";
const SCOPED_ID: &str = "scoped-key";
const SCOPED_SECRET: &str = "scoped-secret-0123456789";
const PRESIGN_ID: &str = "presign-key";
const PRESIGN_SECRET: &str = "presign-secret-0123456789";

/// Writes `text` to the file `name` of `node`'s directory, which only its owner can read, and
/// gives its path.
fn secret(node: &Node, name: &str, text: &str) -> PathBuf {
    let path = node.dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    path
}

/// The file of a node over the cold tier alone with a key file of every role besides the full
/// key (read-only, GC under `gc/`, scoped to read `r/`, write `w/` and delete `d/`, and a
/// presigning key under `p/`), and `cold` as its `[cold_store]` table.
fn text(node: &Node, cold: &str) -> String {
    let read = secret(node, "read", &format!("{READ_ID}\n{READ_SECRET}\n"));
    let gc = secret(
        node,
        "gc",
        &format!(
            r#"{{"access_key_id": "{GC_ID}", "secret_access_key": "{GC_SECRET}", "prefix": "gc/"}}"#
        ),
    );
    let scoped = secret(
        node,
        "scoped",
        &format!(
            r#"{{"access_key_id": "{SCOPED_ID}", "secret_access_key": "{SCOPED_SECRET}", "read_prefixes": ["r/"], "write_prefixes": ["w/"], "delete_prefixes": ["d/"]}}"#
        ),
    );
    let presign = secret(
        node,
        "presign",
        &format!(
            r#"{{"access_key_id": "{PRESIGN_ID}", "secret_access_key": "{PRESIGN_SECRET}", "ceiling": {{"prefix": "p/", "max_expires_secs": 600, "max_object_bytes": 1048576}}}}"#
        ),
    );
    format!(
        r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id            = "n1"
bucket              = "{BUCKET}"
access_key_file     = "{key}"
read_only_key_files = ["{read}"]
gc_key_files        = ["{gc}"]
scoped_key_files    = ["{scoped}"]
presign_key_files   = ["{presign}"]

[cold_store]
{cold}
"#,
        key = node.key.display(),
        read = read.display(),
        gc = gc.display(),
        scoped = scoped.display(),
        presign = presign.display(),
    )
}

/// The settings of [`text`].
fn settings(node: &Node, cold: &str) -> ColdSettings {
    settings_from_text(&text(node, cold), QUICK)
        .unwrap()
        .into_cold()
        .unwrap()
}

/// The settings of a node whose cold store is the memory tier the test passes to
/// [`serving_over`] (the `[cold_store]` table is read but not opened by `serve_cold`).
fn memory_settings(node: &Node) -> ColdSettings {
    settings(node, "kind = \"memory\"")
}

/// A client of the front at `addr` that signs with the key `id` and `secret`, sending
/// `If-None-Match: *` on a create-only put and completion.
fn client_as(addr: SocketAddr, id: &str, secret: &str) -> S3Store {
    let config = S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".to_owned(),
        bucket: BUCKET.to_owned(),
        addressing: Addressing::Auto,
        ca_pem: None,
        credentials: Credentials::new(id, secret),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    S3Store::new(config).unwrap()
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

/// The names of the objects under `prefix`, as `store` lists them.
async fn names(store: &S3Store, prefix: &str) -> Vec<String> {
    let under = KeyPrefix::new(prefix).unwrap();
    let page = store.list(&under, None, PageSize::MAX).await.unwrap();
    page.objects
        .into_iter()
        .map(|info| info.key.as_str().to_owned())
        .collect()
}

/// The ids of the uploads open under `prefix`, as `store` lists them.
async fn open_uploads(store: &S3Store, prefix: &str) -> Vec<String> {
    let under = KeyPrefix::new(prefix).unwrap();
    let page = store.list_multipart_uploads(&under, None, PageSize::MAX);
    let page = page.await.unwrap();
    page.uploads
        .into_iter()
        .map(|upload| upload.upload_id)
        .collect()
}

/// An upload of `key` begun through `store`, with `parts` sent to it: its id, and the numbers and
/// tags to complete it with.
async fn open_upload(
    store: &S3Store,
    key: &ObjectKey,
    parts: &[&'static [u8]],
) -> (String, Vec<(u32, String)>) {
    let id = store.create_multipart(key).await.unwrap();
    let mut done = Vec::new();
    for (at, &part) in parts.iter().enumerate() {
        let number = u32::try_from(at).unwrap() + 1;
        let body: BodyStream =
            futures::stream::once(async move { Ok(Bytes::from_static(part)) }).boxed();
        let len = u64::try_from(part.len()).unwrap();
        let tag = store.upload_part_stream(key, &id, number, len, body).await;
        done.push((number, tag.unwrap()));
    }
    (id, done)
}

/// Whether `result` is the refusal 403 `AccessDenied`.
fn denied<T: std::fmt::Debug>(result: &Result<T, ObjectStoreError>) -> bool {
    matches!(
        result,
        Err(ObjectStoreError::Service { status: 403, code, .. }) if code == "AccessDenied"
    )
}

/// Asserts what `store` answers a second create of `name` (which holds `first`) by a put and by
/// the completion of an upload: both are refused as a key that exists, the bytes are still
/// `first`, and the upload whose completion was refused is still open and can be aborted.
async fn create_only_holds(store: &S3Store, name: &str, first: &'static [u8]) {
    let x = key(name);
    store
        .put_new(&x, Bytes::from_static(first), None)
        .await
        .unwrap();
    let again = store.put_new(&x, Bytes::from_static(b"second"), None).await;
    assert!(
        matches!(again, Err(ObjectStoreError::AlreadyExists(_))),
        "{again:?}"
    );
    let (id, parts) = open_upload(store, &x, &[b"third"]).await;
    let done = store.complete_multipart(&x, &id, &parts).await;
    assert!(
        matches!(done, Err(ObjectStoreError::AlreadyExists(_))),
        "{done:?}"
    );
    let all = ByteRange::new(0, u64::try_from(first.len()).unwrap()).unwrap();
    assert_eq!(store.get_range(&x, all).await.unwrap(), first);
    assert_eq!(open_uploads(store, "cas/").await, [id.clone()]);
    store.abort_multipart(&x, &id).await.unwrap();
    assert!(open_uploads(store, "cas/").await.is_empty());
}

/// Runs `serve_cold` over `cold`, calls `work` with the S3 address once it serves, then stops it.
async fn serving_over<F, Fut>(
    settings: &ColdSettings,
    cold: Arc<dyn Backend>,
    work: F,
) -> Result<Drained, Error>
where
    F: FnOnce(SocketAddr) -> Fut,
    Fut: Future<Output = ()>,
{
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let running = serve_cold(
        settings,
        cold,
        async move {
            let _ = stopped.await;
        },
        move |at| drop(told.send(at)),
    );
    let driving = async move {
        let at = bound.await.unwrap();
        work(at.s3).await;
        let _ = stop.send(());
    };
    let (ran, ()) = tokio::join!(running, driving);
    ran
}

/// Runs `run_cold` (which opens the cold store the settings name), calls `work` with the S3
/// address once it serves, then stops it.
async fn serving_file<F, Fut>(settings: &ColdSettings, work: F) -> Result<Drained, Error>
where
    F: FnOnce(SocketAddr) -> Fut,
    Fut: Future<Output = ()>,
{
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let running = run_cold(
        settings,
        async move {
            let _ = stopped.await;
        },
        move |at| drop(told.send(at)),
    );
    let driving = async move {
        let at = bound.await.unwrap();
        work(at.s3).await;
        let _ = stop.send(());
    };
    let (ran, ()) = tokio::join!(running, driving);
    ran
}

/// Asserts the session of a client of a node over a memory cold store: a create-only put, a
/// whole and a ranged get, a HEAD, a listing, a delete, and a multipart upload that is listed
/// while it is open and then completed; the probes (`/readyz` ready, `/metricz` naming no proxy,
/// no OCI front and no cache); that the cold store then holds the completed object and nothing
/// else (the front writes no object of its own there), and that the drain cuts nothing.
#[tokio::test]
async fn a_node_over_the_cold_tier_serves_ranges_listings_and_uploads_from_the_cold_store() {
    let node = Node::new("tscold-front-session");
    let settings = memory_settings(&node);
    let memory = Arc::new(MemoryBackend::new());
    let cold: Arc<dyn Backend> = memory.clone();
    let work = |addr: SocketAddr| async move {
        let store = client(addr);
        let a = key("cas/a");
        store
            .put_new(&a, Bytes::from_static(b"hello"), None)
            .await
            .unwrap();
        let all = ByteRange::new(0, 5).unwrap();
        assert_eq!(store.get_range(&a, all).await.unwrap(), "hello");
        let middle = ByteRange::new(1, 3).unwrap();
        assert_eq!(store.get_range(&a, middle).await.unwrap(), "ell");
        assert_eq!(store.len(&a).await.unwrap(), Some(5));
        assert_eq!(names(&store, "cas/").await, ["cas/a"]);
        store.delete(&a).await.unwrap();
        assert_eq!(store.len(&a).await.unwrap(), None);

        let m = key("cas/m");
        let (id, parts) = open_upload(&store, &m, &[b"abc", b"def"]).await;
        assert_eq!(open_uploads(&store, "cas/").await, [id.clone()]);
        store.complete_multipart(&m, &id, &parts).await.unwrap();
        assert!(open_uploads(&store, "cas/").await.is_empty());
        let whole = ByteRange::new(0, 6).unwrap();
        assert_eq!(store.get_range(&m, whole).await.unwrap(), "abcdef");

        assert_eq!(probe(addr, "/readyz").await, (200, "ready".to_owned()));
        let (status, said) = probe(addr, "/metricz").await;
        assert_eq!(status, 200);
        let none = "# this server has no proxy, no OCI read front and no local cache\n";
        assert!(said.starts_with(none), "{said}");
    };
    let drained = serving_over(&settings, cold, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    let kept = memory.list("").await.unwrap();
    let kept: Vec<&str> = kept.iter().map(|listed| listed.key.as_str()).collect();
    assert_eq!(kept, ["cas/m"]);
    assert_eq!(memory.get("cas/m", None).await.unwrap(), "abcdef");
}

/// Asserts that create-only holds at a memory and at a directory cold store, for a put and for
/// the completion of an upload: a second put of a key that exists is refused as a key that
/// exists (412), the completion of an upload of that key is refused the same way, the bytes in
/// the cold store are the first put's, and the refused upload is still open and can be aborted.
/// Catches a front over the cold store that replaces an object on a second create.
#[tokio::test]
async fn create_only_holds_for_a_put_and_for_a_completion_at_a_memory_and_a_directory_store() {
    let node = Node::new("tscold-front-create-only");
    let memory = Arc::new(MemoryBackend::new());
    let cold: Arc<dyn Backend> = memory.clone();
    let work = |addr: SocketAddr| async move {
        let store = client(addr);
        create_only_holds(&store, "cas/x", b"first").await;
    };
    serving_over(&memory_settings(&node), cold, work)
        .await
        .unwrap();
    assert_eq!(memory.get("cas/x", None).await.unwrap(), "first");

    let directory = format!("kind = \"dir\"\npath = \"{}\"", node.cold().display());
    let on_disk = settings(&node, &directory);
    let work = |addr: SocketAddr| async move {
        let store = client(addr);
        create_only_holds(&store, "cas/y", b"kept").await;
    };
    serving_file(&on_disk, work).await.unwrap();
    let stored = DirBackend::open(node.cold()).unwrap();
    assert_eq!(stored.get("cas/y", None).await.unwrap(), "kept");
}

/// Asserts that create-only holds at an S3 cold store (the loopback S3 service) reached through
/// the file's `[cold_store]`, for a put and for the completion of an upload: both are refused as
/// a key that exists, the service holds the first put's bytes, and the refused upload is still
/// open at the service until it is aborted. The S3 backend sends `If-None-Match: *` on the put
/// and on the completion, and the service, which keeps the condition, refuses the second create.
/// Catches a front over an S3 cold store that sends a create unconditioned.
#[tokio::test]
async fn create_only_holds_for_a_put_and_for_a_completion_at_an_s3_cold_store() {
    let node = Node::new("tscold-front-s3");
    let fake = FakeS3::start().await;
    let credentials = secret(&node, "cold.credentials", "fake-key\nfake-secret\n");
    let s3 = format!(
        "kind             = \"s3\"\nbucket           = \"tierstore-fake\"\nendpoint         = \"{}\"\ncredentials_file = \"{}\"",
        fake.endpoint(),
        credentials.display()
    );
    let over_s3 = settings(&node, &s3);
    let work = |addr: SocketAddr| async move {
        let store = client(addr);
        create_only_holds(&store, "cas/z", b"original").await;
    };
    serving_file(&over_s3, work).await.unwrap();
    let direct = fake.store(Capabilities::default());
    let all = ByteRange::new(0, 8).unwrap();
    assert_eq!(
        direct.get_range(&key("cas/z"), all).await.unwrap(),
        "original"
    );
    assert_eq!(fake.incomplete_uploads(), 0);
}

/// Asserts that each role of a key is held to what it may do by a node over the cold tier alone:
/// a read-only key reads and lists and is refused a put and a delete; a GC key lists, heads and
/// deletes under its prefix, and is refused a get and a put even under it and a delete outside
/// it; a scoped key reads its read prefix, creates once under its write prefix (a second create
/// is refused as a key that exists), deletes under its delete prefix, and is refused the rest; a
/// presigning key is refused a call signed in its header. Each refusal is 403 `AccessDenied`,
/// and the cold store holds afterwards exactly what the allowed calls left.
#[tokio::test]
async fn each_key_role_is_held_to_what_it_may_do_over_the_cold_store() {
    let node = Node::new("tscold-front-roles");
    let settings = memory_settings(&node);
    let memory = Arc::new(MemoryBackend::new());
    for (name, body) in [
        ("cas/o", "one"),
        ("gc/o", "two"),
        ("r/o", "three"),
        ("d/o", "four"),
    ] {
        memory.put(name, Bytes::from(body)).await.unwrap();
    }
    let cold: Arc<dyn Backend> = memory.clone();
    let work = |addr: SocketAddr| async move {
        let all = ByteRange::new(0, 5).unwrap();
        let read = client_as(addr, READ_ID, READ_SECRET);
        assert_eq!(read.get_range(&key("cas/o"), all).await.unwrap(), "one");
        assert_eq!(names(&read, "gc/").await, ["gc/o"]);
        let put = read
            .put_new(&key("cas/n"), Bytes::from_static(b"x"), None)
            .await;
        assert!(denied(&put), "{put:?}");
        let gone = read.delete(&key("cas/o")).await;
        assert!(denied(&gone), "{gone:?}");

        let gc = client_as(addr, GC_ID, GC_SECRET);
        assert_eq!(names(&gc, "gc/").await, ["gc/o"]);
        assert_eq!(gc.len(&key("gc/o")).await.unwrap(), Some(3));
        let got = gc.get_range(&key("gc/o"), all).await;
        assert!(denied(&got), "{got:?}");
        let put = gc
            .put_new(&key("gc/n"), Bytes::from_static(b"x"), None)
            .await;
        assert!(denied(&put), "{put:?}");
        let gone = gc.delete(&key("cas/o")).await;
        assert!(denied(&gone), "{gone:?}");
        gc.delete(&key("gc/o")).await.unwrap();

        let scoped = client_as(addr, SCOPED_ID, SCOPED_SECRET);
        assert_eq!(scoped.get_range(&key("r/o"), all).await.unwrap(), "three");
        let got = scoped.get_range(&key("cas/o"), all).await;
        assert!(denied(&got), "{got:?}");
        let made = key("w/n");
        scoped
            .put_new(&made, Bytes::from_static(b"new"), None)
            .await
            .unwrap();
        let again = scoped
            .put_new(&made, Bytes::from_static(b"newer"), None)
            .await;
        assert!(
            matches!(again, Err(ObjectStoreError::AlreadyExists(_))),
            "{again:?}"
        );
        let got = scoped.get_range(&made, all).await;
        assert!(denied(&got), "{got:?}");
        let gone = scoped.delete(&made).await;
        assert!(denied(&gone), "{gone:?}");
        scoped.delete(&key("d/o")).await.unwrap();

        let signer = client_as(addr, PRESIGN_ID, PRESIGN_SECRET);
        let put = signer
            .put_new(&key("p/n"), Bytes::from_static(b"x"), None)
            .await;
        assert!(denied(&put), "{put:?}");
    };
    serving_over(&settings, cold, work).await.unwrap();
    let mut kept: Vec<String> = memory
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|listed| listed.key)
        .collect();
    kept.sort();
    assert_eq!(kept, ["cas/o", "r/o", "w/n"]);
    assert_eq!(memory.get("w/n", None).await.unwrap(), "new");
}

/// Asserts that a presigned PUT is taken by a node over the cold tier alone and lands in the cold
/// store, create-only (a storage node puts it in its hot tier; here the cold store is the one
/// store): a URL of the presigning key puts the object (200), the same key put again by a fresh
/// URL is 412 and the bytes are the first's, and a URL for a key outside the key's prefix is 403
/// and stores nothing.
#[tokio::test]
async fn a_presigned_put_lands_in_the_cold_store_create_only() {
    let node = Node::new("tscold-front-presign");
    let settings = memory_settings(&node);
    let memory = Arc::new(MemoryBackend::new());
    let cold: Arc<dyn Backend> = memory.clone();
    let work = |addr: SocketAddr| async move {
        let signer = AccessKey::new(PRESIGN_ID, PRESIGN_SECRET);
        let endpoint = format!("http://{addr}");
        let url = |name: &str| {
            let request = PresignRequest {
                endpoint: &endpoint,
                region: "us-east-1",
                bucket: BUCKET,
                key: name,
                expires_secs: 300,
                now: SystemClock.now(),
            };
            presign_put(&signer, &request).unwrap()
        };
        let http = tierstore_testkit::http::client();
        let put = |url: String, body: &'static str| http.put(url).body(body).send();
        let first = put(url("p/obj"), "payload").await.unwrap();
        assert_eq!(first.status().as_u16(), 200);
        let second = put(url("p/obj"), "other").await.unwrap();
        assert_eq!(second.status().as_u16(), 412);
        let outside = put(url("q/obj"), "payload").await.unwrap();
        assert_eq!(outside.status().as_u16(), 403);
    };
    serving_over(&settings, cold, work).await.unwrap();
    assert_eq!(memory.get("p/obj", None).await.unwrap(), "payload");
    assert_eq!(
        memory.head("q/obj").await.map(drop),
        Err(crate::Error::NotFound("q/obj".to_owned()))
    );
}
