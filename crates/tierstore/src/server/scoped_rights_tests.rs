//! Every right of a scoped key and of a GC key on every call, the list right of a GC key, and the
//! key files of the release before: through `ScopedKey::from_file`, `GcKey::from_file` and
//! `Server::handle`.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use hyper::{Request, StatusCode};
use tierstore_testkit::dir::TestDir;

use super::Prefixes;
use super::gc::{GcKey, gc_keys};
use super::request::{Query, Target};
use super::scoped::{OUT_OF_SCOPE as REFUSED, ScopedKey, permit, scoped_keys};
use super::testing::{BUCKET, Reply, Rig, texts};

const MEBIBYTE: u64 = 1 << 20;
const FLAG: &str = "--scoped-key-file";

/// The headers of a create-only put.
const CREATE: &[(&str, &str)] = &[("if-none-match", "*")];

/// Each key has its rights on one prefix: `a/` read, `b/` list, delete and abort (a GC key), `c/`
/// write, `d/` delete.
const READ: (&str, &str) = ("k-read", "read-secret-0123456789");
const LIST: (&str, &str) = ("k-gc", "gc-secret-0123456789");
const WRITE: (&str, &str) = ("k-write", "write-secret-0123456789");
const DELETE: (&str, &str) = ("k-delete", "delete-secret-0123456789");

/// The message of a call outside the prefixes a key has for it.
const OUT_OF_SCOPE: &str = "Access Denied: the key, or the listing prefix, is outside the prefixes this access key has for this call.";

fn file(dir: &TestDir, name: &str, text: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    path
}

fn message(reply: &Reply) -> String {
    texts(&reply.body, "Message").concat()
}

async fn rig() -> Rig {
    let made = Rig::with(MEBIBYTE, |config| {
        let only = |key: (&str, &str), prefixes: Prefixes<'_>| {
            ScopedKey::new(key.0, key.1, &prefixes).unwrap()
        };
        let none = Prefixes::default();
        config.gc_keys = vec![GcKey::new(LIST.0, LIST.1, "b/").unwrap()];
        config.scoped_keys = vec![
            only(
                READ,
                Prefixes {
                    read: &["a/"],
                    ..none
                },
            ),
            only(
                WRITE,
                Prefixes {
                    write: &["c/"],
                    ..none
                },
            ),
            only(
                DELETE,
                Prefixes {
                    delete: &["d/"],
                    ..none
                },
            ),
        ];
    });
    made.await
}

/// The encoding of a prefix in a query.
fn encoded(prefix: &str) -> String {
    prefix.replace('/', "%2F")
}

/// One call by `key` on `prefix`: `op` names it.
async fn attempt(rig: &Rig, key: (&str, &str), op: &str, prefix: &str, upload: &str) -> Reply {
    let object = format!("/{BUCKET}/{prefix}obj");
    let listing = |query: &str| format!("/{BUCKET}?{query}&prefix={}", encoded(prefix));
    match op {
        "get" => rig.send_as(key, "GET", &object, &[], b"").await,
        "head" => rig.send_as(key, "HEAD", &object, &[], b"").await,
        "list" => {
            let uri = listing("list-type=2");
            rig.send_as(key, "GET", &uri, &[], b"").await
        }
        "list_uploads" => {
            let uri = listing("uploads");
            rig.send_as(key, "GET", &uri, &[], b"").await
        }
        "put" => {
            let uri = format!("/{BUCKET}/{prefix}new");
            rig.send_as(key, "PUT", &uri, CREATE, b"x").await
        }
        "start_upload" => {
            let uri = format!("/{BUCKET}/{prefix}start?uploads");
            rig.send_as(key, "POST", &uri, &[], b"").await
        }
        "list_parts" => {
            let uri = format!("/{BUCKET}/{prefix}up?uploadId={upload}");
            rig.send_as(key, "GET", &uri, &[], b"").await
        }
        "abort" => {
            let uri = format!("/{BUCKET}/{prefix}up?uploadId={upload}");
            rig.send_as(key, "DELETE", &uri, &[], b"").await
        }
        "delete" => rig.send_as(key, "DELETE", &object, &[], b"").await,
        other => panic!("no call {other}"),
    }
}

/// Asserts every right on every call and every prefix: of four keys (a read key, a GC key, a write
/// key and a delete key) each on one prefix, a key does exactly the calls its rights give, on its
/// own prefix, and every other pair of key, call and prefix is 403 `AccessDenied` with the full
/// out-of-scope message and changes nothing. Read gives get, head, list and list of uploads; the
/// GC key gives head, list, list of uploads, delete and abort and never get; write gives a
/// create-only put, the start of an upload, the listing of its parts and its abort; a delete prefix gives a delete and not an
/// abort (as before: a key that deletes cannot cancel another client's upload). The status of each
/// call that is allowed is the one S3 answers.
#[tokio::test(start_paused = true)]
async fn each_right_allows_its_calls_on_its_own_prefix_and_no_other() {
    let rig = rig().await;
    let prefixes = ["a/", "b/", "c/", "d/"];
    let mut uploads = Vec::new();
    for prefix in prefixes {
        rig.put(&format!("{prefix}obj"), b"BODY").await;
        let started = rig.call("POST", &format!("{prefix}up?uploads"), b"").await;
        uploads.push(texts(&started.body, "UploadId").concat());
    }
    let keys = [
        (READ, "a/", vec!["get", "head", "list", "list_uploads"]),
        (
            LIST,
            "b/",
            vec!["head", "list", "list_uploads", "delete", "abort"],
        ),
        (
            WRITE,
            "c/",
            vec!["put", "start_upload", "list_parts", "abort"],
        ),
        (DELETE, "d/", vec!["delete"]),
    ];
    let status = |op: &str| match op {
        "abort" | "delete" => StatusCode::NO_CONTENT,
        _ => StatusCode::OK,
    };
    let ops = [
        "get",
        "head",
        "list",
        "list_uploads",
        "put",
        "start_upload",
        "list_parts",
        "abort",
        "delete",
    ];
    for (key, own, rights) in keys {
        for (prefix, upload) in prefixes.iter().zip(&uploads) {
            for op in ops {
                let reply = attempt(&rig, key, op, prefix, upload).await;
                let what = format!("{} {op} {prefix}", key.0);
                if *prefix == own && rights.contains(&op) {
                    assert_eq!(reply.status, status(op), "{what}: {}", reply.body);
                } else {
                    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{what}");
                    assert_eq!(reply.code(), "AccessDenied", "{what}");
                    assert_eq!(message(&reply), OUT_OF_SCOPE, "{what}");
                }
            }
        }
    }
    assert!(rig.get("c/new", None).await.status.is_success());
    assert!(rig.get("a/obj", None).await.status.is_success());
    for gone in ["b/obj", "d/obj"] {
        assert_eq!(rig.get(gone, None).await.status, StatusCode::NOT_FOUND);
    }
    let open = rig.list("uploads&prefix=d%2F").await;
    assert_eq!(texts(&open.body, "UploadId"), [uploads[3].as_str()]);
    for prefix in ["a/", "b/", "d/"] {
        let stored = rig.get(&format!("{prefix}new"), None).await;
        assert_eq!(stored.status, StatusCode::NOT_FOUND, "{prefix}");
    }
}

/// Asserts that only `uploadId` makes a GET of a key a listing of parts: a GET with a part number
/// (the read of one part of an object, which this server does not do) or `uploads` on a key is
/// 403 `AccessDenied` for the key that writes and for the key that reads, on their own prefixes.
#[tokio::test(start_paused = true)]
async fn a_get_of_a_key_with_a_part_number_is_not_a_listing_of_parts() {
    let rig = rig().await;
    for (key, prefix) in [(WRITE, "c/"), (READ, "a/")] {
        for query in ["partNumber=1", "uploads"] {
            let uri = format!("/{BUCKET}/{prefix}obj?{query}");
            let reply = rig.send_as(key, "GET", &uri, &[], b"").await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{} {query}", key.0);
            assert_eq!(reply.code(), "AccessDenied", "{} {query}", key.0);
        }
    }
}

/// Asserts the boundaries of the list right of a GC key: a list and a head are allowed for a
/// prefix that is its prefix or deeper (a list may also end inside a name), and refused, with the
/// full message, for no prefix, an empty one, a shorter one, a sibling that shares the characters,
/// a path that climbs out, the prefix itself as the key of a head, and the keys of the proxy; and
/// the list right never lets the key read.
#[tokio::test(start_paused = true)]
async fn the_list_right_stops_at_its_prefix() {
    let rig = rig().await;
    rig.put("b/x", b"CONTENT-OF-B").await;
    rig.put("bb/x", b"two").await;
    for (prefix, keys) in [("b%2F", vec!["b/x"]), ("b%2Fx", vec!["b/x"])] {
        let uri = format!("/{BUCKET}?list-type=2&prefix={prefix}");
        let listed = rig.send_as(LIST, "GET", &uri, &[], b"").await;
        assert_eq!(listed.status, StatusCode::OK, "{prefix}");
        assert_eq!(texts(&listed.body, "Key"), keys, "{prefix}");
    }
    let uri = format!("/{BUCKET}/b/x");
    let head = rig.send_as(LIST, "HEAD", &uri, &[], b"").await;
    assert_eq!(head.status, StatusCode::OK);
    for query in [
        "list-type=2",
        "list-type=2&prefix=",
        "list-type=2&prefix=b",
        "list-type=2&prefix=bb%2F",
        "list-type=2&prefix=b%2F..%2F",
        "list-type=2&prefix=_journal%2F",
        "uploads&prefix=b",
        "uploads",
    ] {
        let uri = format!("/{BUCKET}?{query}");
        let reply = rig.send_as(LIST, "GET", &uri, &[], b"").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{query}");
        assert_eq!(message(&reply), OUT_OF_SCOPE, "{query}");
    }
    for key in [
        "b/",
        "b",
        "bb/x",
        "b/../bb/x",
        "b/%2e%2e/bb/x",
        "_journal/p1/x",
    ] {
        let uri = format!("/{BUCKET}/{key}");
        let reply = rig.send_as(LIST, "HEAD", &uri, &[], b"").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{key}");
        assert_eq!(message(&reply), OUT_OF_SCOPE, "{key}");
    }
    let read = rig
        .send_as(LIST, "GET", &format!("/{BUCKET}/b/x"), &[], b"")
        .await;
    assert_eq!(read.status, StatusCode::FORBIDDEN);
    assert_eq!(message(&read), OUT_OF_SCOPE);
    assert!(!read.body.contains("CONTENT-OF-B"));
}

/// Asserts that a HEAD under a list prefix is for a header-signed request only: for the scopes of
/// a GC key, a HEAD with an `authorization` header is allowed and the same HEAD without one (as a
/// presigned URL is) is out of scope, so a bearer URL never reaches what only a list prefix covers.
#[test]
fn a_presigned_head_is_held_to_the_read_prefixes() {
    let scopes = GcKey::new("k-gc", "gc-secret-0123456789", "b/")
        .unwrap()
        .scopes();
    let head = |signed: bool| {
        let mut request = Request::builder().method("HEAD").uri("/tier-bucket/b/x");
        if signed {
            request = request.header("authorization", "AWS4-HMAC-SHA256 x");
        }
        let (parts, ()) = request.body(()).unwrap().into_parts();
        let query = Query::parse(None).unwrap();
        permit(&scopes, &parts, &Target::Object("b/x".to_owned()), &query)
    };
    assert_eq!(head(true), Ok(()));
    assert_eq!(head(false), Err(REFUSED));
}

/// Asserts what a scoped key file may hold: exactly the five fields. A sixth, such as
/// `list_prefixes` (a right only a GC key has), is refused with the full message, which names the
/// flag and the file and carries no part of the secret.
#[test]
fn a_scoped_key_file_has_the_five_fields_and_no_list_or_abort_right() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-scoped-list");
    let base = "\"access_key_id\": \"packs\", \"secret_access_key\": \"s-0123456789\", \
                \"read_prefixes\": [\"r/\"], \"write_prefixes\": [], \"delete_prefixes\": []";
    let loaded = ScopedKey::from_file(FLAG, &file(&dir, "five", &format!("{{{base}}}"))).unwrap();
    assert_eq!(loaded.read_prefixes(), ["r/"]);
    assert!(loaded.write_prefixes().is_empty());
    for (name, extra) in [
        ("list", ", \"list_prefixes\": [\"l/\"]"),
        ("abort", ", \"abort_prefixes\": [\"a/\"]"),
    ] {
        let path = file(&dir, name, &format!("{{{base}{extra}}}"));
        let refused = ScopedKey::from_file(FLAG, &path).unwrap_err();
        let wanted = format!(
            "{FLAG} {}: not JSON of exactly access_key_id, secret_access_key, read_prefixes, \
             write_prefixes and delete_prefixes (the last three lists of strings)",
            path.display()
        );
        assert_eq!(refused, wanted, "{name}");
    }
}

/// Runs the previous release's key files through this release (the files as that release wrote
/// them: a scoped key file of the five fields and a GC key file of `prefix`) and asserts they
/// serve as they did: the scoped key reads and lists under its read prefix, writes under its write
/// prefix and deletes under its delete prefix (and cannot abort an upload); the GC key lists, heads and deletes under its
/// prefix and is refused a get, a put and anything outside it.
#[tokio::test(start_paused = true)]
async fn the_key_files_of_the_release_before_serve_as_they_did() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-scoped-before");
    let old_scoped = file(
        &dir,
        "scoped.key",
        "{\"access_key_id\": \"old-scoped\", \"secret_access_key\": \"old-scoped-secret-01\", \
         \"read_prefixes\": [\"r/\"], \"write_prefixes\": [\"w/\"], \"delete_prefixes\": [\"d/\"]}",
    );
    let old_gc = file(
        &dir,
        "gc.key",
        "{\"access_key_id\": \"old-gc\", \"secret_access_key\": \"old-gc-secret-0123\", \
         \"prefix\": \"g/\"}",
    );
    let scoped = scoped_keys(&[old_scoped]).unwrap();
    let gc = gc_keys(&[old_gc]).unwrap();
    let rig = Rig::with(MEBIBYTE, |config| {
        config.scoped_keys = scoped;
        config.gc_keys = gc;
    })
    .await;
    for key in ["r/o", "d/o", "g/o", "g/p", "x/o"] {
        rig.put(key, b"BODY").await;
    }
    let s = ("old-scoped", "old-scoped-secret-01");
    let g = ("old-gc", "old-gc-secret-0123");
    let call = |key,
                method: &'static str,
                path: String,
                extra: &'static [(&'static str, &'static str)]| {
        let rig = &rig;
        async move {
            let uri = format!("/{BUCKET}{path}");
            rig.send_as(key, method, &uri, extra, b"x").await.status
        }
    };
    assert_eq!(call(s, "GET", "/r/o".into(), &[]).await, StatusCode::OK);
    let listed = call(s, "GET", "?list-type=2&prefix=r%2F".into(), &[]).await;
    assert_eq!(listed, StatusCode::OK);
    assert_eq!(call(s, "PUT", "/w/n".into(), CREATE).await, StatusCode::OK);
    let deleted = call(s, "DELETE", "/d/o".into(), &[]).await;
    assert_eq!(deleted, StatusCode::NO_CONTENT);
    let started = rig.call("POST", "d/up?uploads", b"").await;
    let upload = texts(&started.body, "UploadId").concat();
    let abort = format!("/d/up?uploadId={upload}");
    let refused = call(s, "DELETE", abort, &[]).await;
    assert_eq!(
        refused,
        StatusCode::FORBIDDEN,
        "a delete prefix aborts nothing"
    );
    let outside = call(s, "GET", "/x/o".into(), &[]).await;
    assert_eq!(outside, StatusCode::FORBIDDEN);
    let listed = call(g, "GET", "?list-type=2&prefix=g%2F".into(), &[]).await;
    assert_eq!(listed, StatusCode::OK);
    assert_eq!(call(g, "HEAD", "/g/o".into(), &[]).await, StatusCode::OK);
    let deleted = call(g, "DELETE", "/g/o".into(), &[]).await;
    assert_eq!(deleted, StatusCode::NO_CONTENT);
    for (method, path) in [("GET", "/g/p"), ("PUT", "/g/n"), ("DELETE", "/x/o")] {
        let refused = call(g, method, path.into(), &[]).await;
        assert_eq!(refused, StatusCode::FORBIDDEN, "{method} {path}");
    }
    assert_eq!(rig.get("g/p", None).await.body, "BODY");
}
