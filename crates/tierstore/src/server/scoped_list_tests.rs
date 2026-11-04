//! The list right of a scoped key, every right on every call, and the key files of the release
//! before: through `ScopedKey::from_file`, `GcKey::from_file` and `Server::handle`.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use hyper::StatusCode;
use tierstore_testkit::dir::TestDir;

use super::Prefixes;
use super::gc::gc_keys;
use super::scoped::{ScopedKey, scoped_keys};
use super::testing::{BUCKET, Reply, Rig, texts};

const MEBIBYTE: u64 = 1 << 20;
const FLAG: &str = "--scoped-key-file";

/// The headers of a create-only put.
const CREATE: &[(&str, &str)] = &[("if-none-match", "*")];

/// Each key has one right, on one prefix: `a/` read, `b/` list, `c/` write, `d/` delete.
const READ: (&str, &str) = ("k-read", "read-secret-0123456789");
const LIST: (&str, &str) = ("k-list", "list-secret-0123456789");
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
        config.scoped_keys = vec![
            only(
                READ,
                Prefixes {
                    read: &["a/"],
                    ..none
                },
            ),
            only(
                LIST,
                Prefixes {
                    list: &["b/"],
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
        "abort" => {
            let uri = format!("/{BUCKET}/{prefix}up?uploadId={upload}");
            rig.send_as(key, "DELETE", &uri, &[], b"").await
        }
        "delete" => rig.send_as(key, "DELETE", &object, &[], b"").await,
        other => panic!("no call {other}"),
    }
}

/// Asserts every right on every call and every prefix: of four keys that each have one right on
/// one prefix, a key does exactly the calls its right gives, on its own prefix, and every other
/// pair of key, call and prefix is 403 `AccessDenied` with the full out-of-scope message and
/// changes nothing. Read gives get, head, list and list of uploads; list gives head, list and
/// list of uploads and never get; write gives a create-only put, the start of an upload and its
/// abort; delete gives a delete and the abort of an upload. The status of each call that is
/// allowed is the one S3 answers.
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
        (LIST, "b/", vec!["head", "list", "list_uploads"]),
        (WRITE, "c/", vec!["put", "start_upload", "abort"]),
        (DELETE, "d/", vec!["delete", "abort"]),
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
    assert_eq!(rig.get("d/obj", None).await.status, StatusCode::NOT_FOUND);
    for prefix in ["a/", "b/", "d/"] {
        let stored = rig.get(&format!("{prefix}new"), None).await;
        assert_eq!(stored.status, StatusCode::NOT_FOUND, "{prefix}");
    }
}

/// Asserts the boundaries of a list prefix: a list and a head are allowed for a prefix that is a
/// list prefix or deeper (a list may also end inside a name), and refused, with the full message,
/// for no prefix, an empty one, a shorter one, a sibling that shares the characters, a path that
/// climbs out, the prefix itself as the key of a head, and the keys of the proxy; and a list
/// prefix never lets the key read.
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
    let head = rig.send_as(LIST, "HEAD", &format!("/{BUCKET}/b/x"), &[], b"");
    assert_eq!(head.await.status, StatusCode::OK);
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

/// Asserts what the list lists of a key file are: `list_prefixes` is optional and empty when it is
/// absent, a file with it reads back as written, a key may have only a list prefix, and a list
/// prefix is held to the rules of every prefix (no closing slash, reserved) and to being a list
/// of strings.
#[test]
fn a_scoped_key_file_may_give_list_prefixes_and_may_leave_them_out() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-scoped-list");
    let with = |list: &str| {
        format!(
            "{{\"access_key_id\": \"packs\", \"secret_access_key\": \"s-0123456789\", \
             \"read_prefixes\": [], \"write_prefixes\": [], \"delete_prefixes\": [], \
             \"list_prefixes\": {list}}}"
        )
    };
    let without = "{\"access_key_id\": \"packs\", \"secret_access_key\": \"s-0123456789\", \
                   \"read_prefixes\": [\"r/\"], \"write_prefixes\": [\"w/\"], \
                   \"delete_prefixes\": [\"d/\"]}";
    let read = ScopedKey::from_file(FLAG, &file(&dir, "without", without)).unwrap();
    assert!(read.list_prefixes().is_empty());
    assert_eq!(read.read_prefixes(), ["r/"]);
    assert_eq!(read.write_prefixes(), ["w/"]);
    assert_eq!(read.delete_prefixes(), ["d/"]);
    let only = ScopedKey::from_file(FLAG, &file(&dir, "only", &with(r#"["l/", "m/"]"#)));
    assert_eq!(only.unwrap().list_prefixes(), ["l/", "m/"]);
    for (name, list) in [
        ("empty", "[]"),
        ("noslash", r#"["l"]"#),
        ("reserved", r#"["_journal/"]"#),
        ("tiering", r#"["x/_tiering/"]"#),
        ("type", r#""l/""#),
        ("item", "[7]"),
    ] {
        let path = file(&dir, name, &with(list));
        let refused = ScopedKey::from_file(FLAG, &path).unwrap_err();
        assert!(
            refused.starts_with("--scoped-key-file "),
            "{name}: {refused}"
        );
        assert!(!refused.contains("s-0123456789"), "{name}: {refused}");
    }
}

/// Runs the previous release's key files through this release (the files as that release wrote
/// them: a scoped key file of the five fields and a GC key file of `prefix`) and asserts they
/// serve as they did: the scoped key reads and lists under its read prefix, writes under its write
/// prefix and deletes under its delete prefix; the GC key lists, heads and deletes under its
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
