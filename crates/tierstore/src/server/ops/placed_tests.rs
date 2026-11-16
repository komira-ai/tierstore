//! The S3 front under the placement of a node-local tree, through `Server::handle`: what a read of
//! a key under the placement answers, what a write there answers, and what a listing shows. The
//! tree's files are members of a pack in the upstream of a local cache; the first read of a file
//! fills its bytes into the cache, and the progress a test waits for is never a time.

use futures::StreamExt as _;
use http_body_util::BodyExt;
use hyper::StatusCode;
use hyper::header::HeaderMap;

use super::placed::{file_body, span_body, unanswered, unfilled};
use crate::server::auth::sign;
use crate::server::delete_objects_tests::{document, errors};
use crate::server::local_cache::pack_testing::Pack;
use crate::server::local_cache::testing::Rig as CacheRig;
use crate::server::local_cache::trees::{
    CreateRequest, FetchError, Location, OpenFile, PlacementId, ReadError, Resolved, TreeStore,
    TreesConfig,
};
use crate::server::local_cache::{RangeError, Span};
use crate::server::request::Query;
use crate::server::testing::{BUCKET, Rig, checksum, texts};
use crate::server::time::http_date;
use crate::{BlobSource, Mode, NewEntry, NewKind, Op, TreeDigest, TreeLimits, build_tree};

/// The key prefix the small tree is placed at.
const ROOT: &str = "jobs/1/root/";

/// The key of the pack in the upstream.
const PACK: &str = "cas/pack/one";

/// The SHA-256 of no bytes: the tag of a symbolic link and of an empty file.
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// What the code says of a write under a placement.
const READ_ONLY: &str =
    "A tree is placed at this key on this node: it cannot be written or deleted.";

/// A server whose bucket has a tree store over a cache whose upstream holds the pack.
struct Fixture {
    cache: CacheRig,
    pack: Pack,
    store: TreeStore,
    rig: Rig,
}

impl Fixture {
    async fn new(name: &str) -> Self {
        let cache = CacheRig::new(name);
        let pack = Pack::put(&cache, PACK, &[1000, 2000, 3000]).await;
        let store = TreeStore::open(cache.cache.clone(), TreesConfig::default()).expect("a store");
        let held = store.clone();
        let rig = Rig::with(1 << 20, move |config| config.trees = Some(held)).await;
        Self {
            cache,
            pack,
            store,
            rig,
        }
    }

    /// The file `path` whose bytes are the blob `blob` of the pack.
    fn member(&self, path: &str, blob: usize, mode: Option<Mode>) -> NewEntry {
        let (sha256, offset, size) = &self.pack.blobs[blob];
        NewEntry {
            path: path.to_owned(),
            kind: NewKind::File {
                mode,
                sha256: TreeDigest::from_hex(sha256).expect("a digest"),
                size: *size,
                from: Some(BlobSource {
                    key: PACK.into(),
                    offset: *offset,
                }),
            },
        }
    }

    /// The small tree: bin/tool (mode 4755, blob 0), bin/other (blob 1), lib/deep/data (blob 2),
    /// an empty directory, a symbolic link and an empty file.
    fn entries(&self) -> Vec<NewEntry> {
        let setuid = Mode::new(0o4755).expect("a mode");
        vec![
            self.member("bin/tool", 0, Some(setuid)),
            self.member("bin/other", 1, None),
            self.member("lib/deep/data", 2, None),
            NewEntry {
                path: "empty".to_owned(),
                kind: NewKind::Dir { mode: None },
            },
            link("link", "bin/tool"),
            NewEntry {
                path: "zero".to_owned(),
                kind: NewKind::File {
                    mode: None,
                    sha256: TreeDigest::of(b""),
                    size: 0,
                    from: None,
                },
            },
        ]
    }

    /// Places the tree of `entries` at the prefix `prefix` of the bucket.
    fn place(&self, prefix: &str, entries: Vec<NewEntry>) -> PlacementId {
        let built = build_tree(None, entries, &TreeLimits::default()).expect("a tree");
        let at = vec![Location::s3(BUCKET, prefix).expect("a location")];
        let request = CreateRequest {
            built,
            at,
            ttl: None,
            ready: false,
        };
        self.store.create(request).expect("a placement").placement
    }

    /// The small tree at [`ROOT`].
    fn placed(&self) -> PlacementId {
        self.place(ROOT, self.entries())
    }

    /// When the placement was made, in milliseconds.
    fn made(&self, id: &PlacementId) -> u64 {
        self.store
            .placement(id)
            .expect("a live placement")
            .created_ms
    }

    /// Whether the cache holds the bytes of the blob `blob`.
    fn held(&self, blob: usize) -> bool {
        let (_, offset, size) = &self.pack.blobs[blob];
        self.cache.cache.span_of(PACK, *offset, *size).is_some()
    }

    /// The tag a file of the blob `blob` is shown with: its SHA-256 in quotes.
    fn tag(&self, blob: usize) -> String {
        format!("\"{}\"", self.pack.blobs[blob].0)
    }

    /// Gets of the pack the upstream has had.
    fn pack_reads(&self) -> u64 {
        self.cache.calls_on(Op::Get, PACK)
    }
}

fn link(path: &str, target: &str) -> NewEntry {
    NewEntry {
        path: path.to_owned(),
        kind: NewKind::Symlink {
            target: target.to_owned(),
        },
    }
}

/// The key of `path` under [`ROOT`].
fn at(path: &str) -> String {
    format!("{ROOT}{path}")
}

/// An answer with its body as bytes, not as text.
struct Raw {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Raw {
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }

    fn code(&self) -> String {
        texts(&String::from_utf8_lossy(&self.body), "Code").concat()
    }
}

/// A request for the object `key` with the headers `extra`.
async fn raw(rig: &Rig, method: &str, key: &str, extra: &[(&str, &str)]) -> Raw {
    let uri = format!("/{BUCKET}/{key}");
    let request = sign(&rig.config, method, &uri, extra, b"");
    let (parts, body) = rig.server.handle(request).await.into_parts();
    let bytes = body.collect().await.expect("a whole body").to_bytes();
    Raw {
        status: parts.status,
        headers: parts.headers,
        body: bytes.to_vec(),
    }
}

async fn get(rig: &Rig, key: &str) -> Raw {
    raw(rig, "GET", key, &[]).await
}

async fn head(rig: &Rig, key: &str) -> Raw {
    raw(rig, "HEAD", key, &[]).await
}

/// The `Prefix` of each common prefix of a listing.
fn common_prefixes(body: &str) -> Vec<String> {
    let doc = roxmltree::Document::parse(body).expect("well-formed XML");
    doc.descendants()
        .filter(|node| node.tag_name().name() == "CommonPrefixes")
        .flat_map(|node| node.children())
        .filter(|node| node.tag_name().name() == "Prefix")
        .map(|node| node.text().unwrap_or_default().to_owned())
        .collect()
}

/// A ListObjects (version 1) with this query string.
async fn old(rig: &Rig, query: &str) -> crate::server::testing::Reply {
    let uri = format!("/{BUCKET}?{query}");
    rig.send("GET", &uri, &[], b"").await
}

/// The keys of every page of a ListObjectsV2 of `query` of at most `max` entries, and how many
/// pages it took.
async fn walk(rig: &Rig, query: &str, max: usize) -> (Vec<String>, usize) {
    let mut keys = Vec::new();
    let mut pages = 0;
    let mut token = String::new();
    loop {
        let mut asked = format!("{query}&max-keys={max}");
        if !token.is_empty() {
            asked.push_str(&format!("&continuation-token={token}"));
        }
        let reply = rig.list(&asked).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        pages += 1;
        keys.extend(texts(&reply.body, "Key"));
        token = texts(&reply.body, "NextContinuationToken").concat();
        if token.is_empty() {
            return (keys, pages);
        }
        assert!(pages < 50, "the walk does not end");
    }
}

/// The keys of the small tree under [`ROOT`], in key order.
fn tree_keys() -> Vec<String> {
    ["bin/other", "bin/tool", "lib/deep/data", "link", "zero"]
        .iter()
        .map(|path| at(path))
        .collect()
}

/// Asserts that a GET of a file under the placement is 200 with the file's bytes (as many as its
/// size), its SHA-256 in quotes as `ETag`, the time the placement was made as
/// `Last-Modified`, its mode in four octal digits as `x-amz-meta-mode` (the setuid bit kept), and
/// the headers of any object; for each of the three files.
#[tokio::test]
async fn a_file_under_a_placement_is_got_as_an_object() {
    let fx = Fixture::new("placed-get").await;
    let id = fx.placed();
    let modified = http_date(fx.made(&id));
    for (path, blob, mode) in [
        ("bin/tool", 0, "4755"),
        ("bin/other", 1, "0644"),
        ("lib/deep/data", 2, "0644"),
    ] {
        let reply = get(&fx.rig, &at(path)).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
        assert_eq!(reply.body, fx.pack.content(blob), "{path}");
        let size = usize::try_from(fx.pack.blobs[blob].2).unwrap();
        assert_eq!(reply.body.len(), size, "{path}");
        assert_eq!(reply.header("etag"), fx.tag(blob), "{path}");
        assert_eq!(reply.header("last-modified"), modified, "{path}");
        assert_eq!(reply.header("x-amz-meta-mode"), mode, "{path}");
        assert_eq!(reply.header("accept-ranges"), "bytes", "{path}");
        assert_eq!(
            reply.header("content-type"),
            "application/octet-stream",
            "{path}"
        );
    }
}

/// Asserts that a `Range` is served as for an object: a closed range, an open one and a suffix are
/// 206 with exactly those bytes and `Content-Range`; a range that runs past the end
/// is cut at the end; and a range that starts at or after the end is 416 `InvalidRange` with the
/// size in `Content-Range`.
#[tokio::test]
async fn a_range_of_a_file_is_served_as_for_an_object() {
    let fx = Fixture::new("placed-range").await;
    fx.placed();
    let key = at("bin/tool");
    let whole = fx.pack.content(0);
    for (range, from, to) in [
        ("bytes=10-19", 10, 19),
        ("bytes=990-", 990, 999),
        ("bytes=-5", 995, 999),
        ("bytes=0-0", 0, 0),
        ("bytes=990-5000", 990, 999),
    ] {
        let reply = raw(&fx.rig, "GET", &key, &[("range", range)]).await;
        assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(reply.body, &whole[from..=to], "{range}");
        let said = format!("bytes {from}-{to}/1000");
        assert_eq!(reply.header("content-range"), said, "{range}");
        assert_eq!(reply.header("etag"), fx.tag(0), "{range}");
    }
    for range in ["bytes=1000-", "bytes=5000-6000"] {
        let reply = raw(&fx.rig, "GET", &key, &[("range", range)]).await;
        assert_eq!(reply.status, StatusCode::RANGE_NOT_SATISFIABLE, "{range}");
        assert_eq!(reply.code(), "InvalidRange", "{range}");
        assert_eq!(reply.header("content-range"), "bytes */1000", "{range}");
    }
}

/// Asserts that a HEAD of a file answers the headers of the GET (the length, `ETag`,
/// `Last-Modified`, mode, `Accept-Ranges`) with no body, and fetches nothing: the cache does not
/// hold the file's bytes afterwards and the upstream was not read.
#[tokio::test]
async fn a_head_answers_the_headers_and_fetches_nothing() {
    let fx = Fixture::new("placed-head").await;
    let id = fx.placed();
    let reply = head(&fx.rig, &at("bin/tool")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.is_empty());
    assert_eq!(reply.header("content-length"), "1000");
    assert_eq!(reply.header("etag"), fx.tag(0));
    assert_eq!(reply.header("last-modified"), http_date(fx.made(&id)));
    assert_eq!(reply.header("x-amz-meta-mode"), "4755");
    assert_eq!(reply.header("accept-ranges"), "bytes");
    assert_eq!(reply.header("content-type"), "application/octet-stream");
    assert!(!fx.held(0));
    assert_eq!(fx.pack_reads(), 0);
}

/// Asserts that a directory (with and without the closing `/`, the placement's root and an empty
/// directory), a path that is not in the tree and a path below a file are 404 `NoSuchKey` for GET
/// and for HEAD.
#[tokio::test]
async fn a_directory_or_a_missing_path_is_no_such_key() {
    let fx = Fixture::new("placed-missing").await;
    fx.placed();
    for key in [
        at("bin"),
        at("bin/"),
        ROOT.to_owned(),
        at("empty"),
        at("lib/deep"),
        at("nope"),
        at("bin/nope"),
        at("bin/tool/below"),
    ] {
        let reply = get(&fx.rig, &key).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{key}");
        assert_eq!(reply.code(), "NoSuchKey", "{key}");
        let reply = head(&fx.rig, &key).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{key}");
    }
}

/// Asserts that a symbolic link is an object of no bytes whose `ETag` is the SHA-256 of nothing and
/// whose `x-amz-meta-symlink-target` is its target (a byte that is not printable ASCII, and `%`,
/// written `%XX`), and that an empty file is 200 with no body; and that none of them reads the pack.
#[tokio::test]
async fn a_link_and_an_empty_file_are_objects_of_no_bytes() {
    let fx = Fixture::new("placed-link").await;
    fx.place("jobs/2/", {
        let mut entries = fx.entries();
        entries.push(link("odd", "x y%\u{e9}"));
        entries
    });
    for method in ["GET", "HEAD"] {
        let reply = raw(&fx.rig, method, "jobs/2/link", &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{method}");
        assert!(reply.body.is_empty(), "{method}");
        if method == "HEAD" {
            assert_eq!(reply.header("content-length"), "0", "{method}");
        }
        assert_eq!(reply.header("etag"), format!("\"{EMPTY}\""), "{method}");
        assert_eq!(
            reply.header("x-amz-meta-symlink-target"),
            "bin/tool",
            "{method}"
        );
        let odd = raw(&fx.rig, method, "jobs/2/odd", &[]).await;
        assert_eq!(odd.status, StatusCode::OK, "{method}");
        let escaped = odd.header("x-amz-meta-symlink-target");
        assert_eq!(escaped, "x y%25%C3%A9", "{method}");
        let zero = raw(&fx.rig, method, "jobs/2/zero", &[]).await;
        assert_eq!(zero.status, StatusCode::OK, "{method}");
        assert!(zero.body.is_empty(), "{method}");
        if method == "HEAD" {
            assert_eq!(zero.header("content-length"), "0", "{method}");
        }
        assert_eq!(zero.header("etag"), format!("\"{EMPTY}\""), "{method}");
        assert_eq!(zero.header("x-amz-meta-mode"), "0644", "{method}");
    }
    assert_eq!(fx.pack_reads(), 0);
}

/// Asserts that the conditions of a read are judged on the file's tag: `If-None-Match` with the tag
/// is 304 (no body, nothing fetched), `If-Match` with another tag is 412 `PreconditionFailed`, and
/// `If-Match` with the tag and `If-None-Match` with another tag are 200.
#[tokio::test]
async fn the_conditions_of_a_read_are_judged_on_the_files_tag() {
    let fx = Fixture::new("placed-conditions").await;
    fx.placed();
    let key = at("bin/other");
    let tag = fx.tag(1);
    let not_modified = raw(&fx.rig, "GET", &key, &[("if-none-match", tag.as_str())]).await;
    assert_eq!(not_modified.status, StatusCode::NOT_MODIFIED);
    assert!(not_modified.body.is_empty());
    assert_eq!(not_modified.header("etag"), tag);
    assert!(!fx.held(1));
    let other = "\"0000\"";
    let failed = raw(&fx.rig, "GET", &key, &[("if-match", other)]).await;
    assert_eq!(failed.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(failed.code(), "PreconditionFailed");
    let kept = raw(&fx.rig, "GET", &key, &[("if-match", tag.as_str())]).await;
    assert_eq!(kept.status, StatusCode::OK);
    assert_eq!(kept.body, fx.pack.content(1));
    let changed = raw(&fx.rig, "HEAD", &key, &[("if-none-match", other)]).await;
    assert_eq!(changed.status, StatusCode::OK);
}

/// Asserts that a key that is under no placement (a sibling that only begins like the prefix, the
/// prefix without its `/`, another prefix) is an ordinary object: a put through the server is read
/// back; and that once the placement is released the same key under the old prefix is ordinary
/// again: 404 `NoSuchKey` before a put, and the put bytes after.
#[tokio::test]
async fn a_key_outside_the_placements_is_ordinary_and_so_is_one_after_the_release() {
    let fx = Fixture::new("placed-ordinary").await;
    let id = fx.placed();
    for key in ["jobs/1/rootx/a", "jobs/2/a", "jobs/1/root", "jobs/1/a"] {
        assert_eq!(get(&fx.rig, key).await.code(), "NoSuchKey", "{key}");
        assert_eq!(
            fx.rig.put(key, key.as_bytes()).await.status,
            StatusCode::OK,
            "{key}"
        );
        let reply = get(&fx.rig, key).await;
        assert_eq!(reply.status, StatusCode::OK, "{key}");
        assert_eq!(reply.body, key.as_bytes(), "{key}");
    }
    fx.store.release(&id).expect("a release");
    let key = at("bin/tool");
    let gone = get(&fx.rig, &key).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.code(), "NoSuchKey");
    assert_eq!(fx.rig.put(&key, b"fresh").await.status, StatusCode::OK);
    assert_eq!(get(&fx.rig, &key).await.body, b"fresh");
    assert_eq!(fx.rig.put(&at("new"), b"made").await.status, StatusCode::OK);
}

/// Asserts that every write under a placement is 409 `TreeLocationReadOnly` with the fixed message:
/// a create-only put, a plain put, a delete, the start of an upload, a part and the completion of
/// an upload; also for a key the tree does not have; and that nothing was written.
#[tokio::test]
async fn a_write_under_a_placement_is_refused() {
    let fx = Fixture::new("placed-writes").await;
    fx.placed();
    for path in ["bin/tool", "new/key", "empty"] {
        let key = at(path);
        let done = r#"<CompleteMultipartUpload></CompleteMultipartUpload>"#;
        let attempts = [
            fx.rig.put(&key, b"x").await,
            fx.rig.call("PUT", &key, b"x").await,
            fx.rig.call("DELETE", &key, b"").await,
            fx.rig.call("POST", &format!("{key}?uploads"), b"").await,
            fx.rig
                .call("PUT", &format!("{key}?partNumber=1&uploadId=x"), b"p")
                .await,
            fx.rig
                .call("POST", &format!("{key}?uploadId=x"), done.as_bytes())
                .await,
        ];
        for (number, reply) in attempts.iter().enumerate() {
            assert_eq!(reply.status, StatusCode::CONFLICT, "{key} {number}");
            assert_eq!(reply.code(), "TreeLocationReadOnly", "{key} {number}");
            assert_eq!(texts(&reply.body, "Message"), [READ_ONLY], "{key} {number}");
        }
    }
    assert_eq!(get(&fx.rig, &at("new/key")).await.code(), "NoSuchKey");
    assert_eq!(get(&fx.rig, &at("bin/tool")).await.body, fx.pack.content(0));
}

/// Asserts that the abort of an upload under a placement is not refused as a write: it answers what
/// the abort of an upload that is not open answers, 404 `NoSuchUpload`.
#[tokio::test]
async fn the_abort_of_an_upload_is_not_refused() {
    let fx = Fixture::new("placed-abort").await;
    fx.placed();
    let reply = fx
        .rig
        .call("DELETE", &format!("{}?uploadId=x", at("bin/tool")), b"")
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    assert_eq!(reply.code(), "NoSuchUpload");
}

/// Asserts that a DeleteObjects naming a key under a placement and an ordinary key answers an
/// `Error` with the code and the fixed message for the first and a `Deleted` for the second, and
/// deletes only the second: the tree's file is still served.
#[tokio::test]
async fn a_delete_objects_refuses_the_placed_key_alone() {
    let fx = Fixture::new("placed-delete-many").await;
    fx.rig.put("plain", b"body").await;
    fx.placed();
    let placed = at("bin/tool");
    let body = document(&[placed.as_str(), "plain"], None);
    let sum = checksum("content-md5", body.as_bytes());
    let uri = format!("/{BUCKET}?delete");
    let reply = fx
        .rig
        .send(
            "POST",
            &uri,
            &[("content-md5", sum.as_str())],
            body.as_bytes(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let refused = (
        placed.clone(),
        "TreeLocationReadOnly".to_owned(),
        READ_ONLY.to_owned(),
    );
    assert_eq!(errors(&reply.body), [refused]);
    let doc = roxmltree::Document::parse(&reply.body).expect("well-formed XML");
    let deleted: Vec<_> = doc
        .descendants()
        .filter(|node| node.tag_name().name() == "Deleted")
        .flat_map(|node| node.children())
        .filter(|node| node.tag_name().name() == "Key")
        .map(|node| node.text().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(deleted, ["plain"]);
    assert_eq!(get(&fx.rig, "plain").await.code(), "NoSuchKey");
    assert_eq!(get(&fx.rig, &placed).await.body, fx.pack.content(0));
}

/// Asserts that a ListObjectsV2 inside the placement lists the files and links of the tree (not the
/// empty directory) in key order with their sizes and tags; that the prefix, a part of it, and
/// `start-after` cut the listing; and that paging by one and by two reaches each key once.
#[tokio::test]
async fn a_listing_inside_a_placement_shows_the_trees_keys() {
    let fx = Fixture::new("placed-list-inside").await;
    fx.placed();
    let reply = fx.rig.list(&format!("prefix={ROOT}")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(texts(&reply.body, "Key"), tree_keys());
    assert_eq!(
        texts(&reply.body, "Size"),
        ["2000", "1000", "3000", "0", "0"]
    );
    let tags = [
        fx.tag(1),
        fx.tag(0),
        fx.tag(2),
        format!("\"{EMPTY}\""),
        format!("\"{EMPTY}\""),
    ];
    assert_eq!(texts(&reply.body, "ETag"), tags);
    assert_eq!(texts(&reply.body, "IsTruncated"), ["false"]);
    let keys = |from: usize, to: usize| tree_keys()[from..to].to_vec();
    for (query, expected) in [
        (format!("prefix={ROOT}bin/"), keys(0, 2)),
        (format!("prefix={ROOT}bin/t"), keys(1, 2)),
        (format!("prefix={ROOT}lib/de"), keys(2, 3)),
        (format!("prefix={ROOT}z"), keys(4, 5)),
        (format!("prefix={ROOT}nothing"), Vec::new()),
        ("prefix=jobs/1/ro".to_owned(), keys(0, 5)),
        ("prefix=jobs/1/root".to_owned(), keys(0, 5)),
        (format!("prefix={ROOT}&start-after={ROOT}bin"), keys(0, 5)),
        (
            format!("prefix={ROOT}&start-after={ROOT}bin/tool"),
            keys(2, 5),
        ),
        (
            format!("prefix={ROOT}&start-after={ROOT}lib/deep/data"),
            keys(3, 5),
        ),
        (format!("prefix={ROOT}&start-after={ROOT}zz"), Vec::new()),
    ] {
        let reply = fx.rig.list(&query).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.body);
        assert_eq!(texts(&reply.body, "Key"), expected, "{query}");
    }
    let prefix = format!("prefix={ROOT}");
    assert_eq!(walk(&fx.rig, &prefix, 1).await, (tree_keys(), 5));
    assert_eq!(walk(&fx.rig, &prefix, 2).await, (tree_keys(), 3));
}

/// Asserts that with `delimiter=/` a listing inside the placement folds the directories of the tree
/// into common prefixes (`bin/`, `lib/`, not the empty directory) beside the files and links at its
/// top, and that one level down the files of the directory are listed.
#[tokio::test]
async fn a_delimited_listing_folds_the_trees_directories() {
    let fx = Fixture::new("placed-list-delimiter").await;
    fx.placed();
    let reply = fx.rig.list(&format!("prefix={ROOT}&delimiter=/")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(common_prefixes(&reply.body), [at("bin/"), at("lib/")]);
    assert_eq!(texts(&reply.body, "Key"), [at("link"), at("zero")]);
    let reply = fx.rig.list(&format!("prefix={ROOT}lib/&delimiter=/")).await;
    assert_eq!(common_prefixes(&reply.body), [at("lib/deep/")]);
    assert!(texts(&reply.body, "Key").is_empty());
    let reply = fx.rig.list(&format!("prefix={ROOT}bin/&delimiter=/")).await;
    assert!(common_prefixes(&reply.body).is_empty());
    assert_eq!(texts(&reply.body, "Key"), [at("bin/other"), at("bin/tool")]);
}

/// Asserts that a listing above the placement merges the ordinary objects with the tree's keys in
/// key order, leaves out the ordinary objects whose keys are under the placement, folds the placement
/// into one common prefix with a delimiter, and that paging by two (through pages whose backend
/// keys are all under the placement) reaches each key once, none repeated or missed.
#[tokio::test]
async fn a_listing_above_a_placement_merges_the_ordinary_objects() {
    let fx = Fixture::new("placed-list-above").await;
    for key in [
        "jobs/0/a",
        "jobs/1/a",
        "jobs/1/root/aaa",
        "jobs/1/root/bbb",
        "jobs/1/root/ccc",
        "jobs/9/z",
    ] {
        assert_eq!(
            fx.rig.put(key, b"body").await.status,
            StatusCode::OK,
            "{key}"
        );
    }
    fx.placed();
    let mut expected = vec!["jobs/0/a".to_owned(), "jobs/1/a".to_owned()];
    expected.extend(tree_keys());
    expected.push("jobs/9/z".to_owned());
    let reply = fx.rig.list("prefix=jobs/").await;
    assert_eq!(texts(&reply.body, "Key"), expected);
    let sizes = ["4", "4", "2000", "1000", "3000", "0", "0", "4"];
    assert_eq!(texts(&reply.body, "Size"), sizes);
    assert_eq!(
        walk(&fx.rig, "prefix=jobs/", 2).await,
        (expected.clone(), 4)
    );
    assert_eq!(
        walk(&fx.rig, "prefix=jobs/", 1).await,
        (expected.clone(), 8)
    );
    assert_eq!(
        walk(&fx.rig, "prefix=jobs/", 3).await,
        (expected.clone(), 3)
    );
    assert_eq!(walk(&fx.rig, "prefix=jobs/", 1000).await, (expected, 1));
    let reply = fx.rig.list("prefix=jobs/&delimiter=/").await;
    let folded = ["jobs/0/", "jobs/1/", "jobs/9/"];
    assert_eq!(common_prefixes(&reply.body), folded);
    let reply = fx.rig.list("prefix=jobs/1/&delimiter=/").await;
    assert_eq!(common_prefixes(&reply.body), [ROOT]);
    assert_eq!(texts(&reply.body, "Key"), ["jobs/1/a"]);
}

/// Asserts that an ordinary object put under the placement's prefix before the placement was made
/// is not listed, and is not read: a GET of its key serves the tree's file when the tree has the
/// key and is 404 `NoSuchKey` when it does not.
#[tokio::test]
async fn an_ordinary_object_under_a_placement_is_shadowed() {
    let fx = Fixture::new("placed-shadow").await;
    fx.rig.put(&at("bin/tool"), b"old").await;
    fx.rig.put(&at("stray"), b"stray").await;
    fx.placed();
    let reply = fx.rig.list(&format!("prefix={ROOT}")).await;
    assert_eq!(texts(&reply.body, "Key"), tree_keys());
    assert_eq!(
        texts(&reply.body, "Size"),
        ["2000", "1000", "3000", "0", "0"]
    );
    assert_eq!(get(&fx.rig, &at("bin/tool")).await.body, fx.pack.content(0));
    let stray = get(&fx.rig, &at("stray")).await;
    assert_eq!(stray.status, StatusCode::NOT_FOUND);
    assert_eq!(stray.code(), "NoSuchKey");
}

/// Asserts that ListObjects (version 1, with `marker`) and ListObjectVersions (with `key-marker`)
/// show the tree's keys in key order and page through them: after the marker `bin/tool` the rest
/// is listed, and a page of two ends at the key that is the next marker.
#[tokio::test]
async fn the_older_listings_show_the_trees_keys() {
    let fx = Fixture::new("placed-list-old").await;
    fx.placed();
    let all = old(&fx.rig, &format!("prefix={ROOT}")).await;
    assert_eq!(all.status, StatusCode::OK, "{}", all.body);
    assert_eq!(texts(&all.body, "Key"), tree_keys());
    assert_eq!(texts(&all.body, "Size"), ["2000", "1000", "3000", "0", "0"]);
    let rest = old(&fx.rig, &format!("prefix={ROOT}&marker={}", at("bin/tool"))).await;
    assert_eq!(texts(&rest.body, "Key"), tree_keys()[2..].to_vec());
    let uri = format!("/{BUCKET}?versions&prefix={ROOT}&max-keys=2");
    let first = fx.rig.send("GET", &uri, &[], b"").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(texts(&first.body, "Key"), tree_keys()[..2].to_vec());
    assert_eq!(texts(&first.body, "VersionId"), ["null", "null"]);
    assert_eq!(texts(&first.body, "Size"), ["2000", "1000"]);
    assert_eq!(texts(&first.body, "NextKeyMarker"), [at("bin/tool")]);
    let uri = format!(
        "/{BUCKET}?versions&prefix={ROOT}&key-marker={}",
        at("bin/tool")
    );
    let rest = fx.rig.send("GET", &uri, &[], b"").await;
    assert_eq!(texts(&rest.body, "Key"), tree_keys()[2..].to_vec());
}

/// Asserts that the keys of a tree are listed in the order of the keys, not of the names: a file
/// `a-b` and a directory `a` with the file `a/x` list as `a-b` then `a/x` (`-` is below `/`), in
/// one page, in pages of one, and after a `start-after` of either.
#[tokio::test]
async fn the_keys_of_a_tree_are_listed_in_key_order() {
    let fx = Fixture::new("placed-list-order").await;
    let entries = vec![fx.member("a/x", 0, None), fx.member("a-b", 1, None)];
    fx.place("k/", entries);
    let both = ["k/a-b".to_owned(), "k/a/x".to_owned()];
    let reply = fx.rig.list("prefix=k/").await;
    assert_eq!(texts(&reply.body, "Key"), both);
    assert_eq!(texts(&reply.body, "Size"), ["2000", "1000"]);
    assert_eq!(walk(&fx.rig, "prefix=k/", 1).await, (both.to_vec(), 2));
    let reply = fx.rig.list("prefix=k/&start-after=k/a-b").await;
    assert_eq!(texts(&reply.body, "Key"), ["k/a/x"]);
    let reply = fx.rig.list("prefix=k/&start-after=k/a/x").await;
    assert!(texts(&reply.body, "Key").is_empty());
    let reply = fx.rig.list("prefix=k/a-").await;
    assert_eq!(texts(&reply.body, "Key"), ["k/a-b"]);
    let reply = fx.rig.list("prefix=k/&delimiter=/").await;
    assert_eq!(texts(&reply.body, "Key"), ["k/a-b"]);
    assert_eq!(common_prefixes(&reply.body), ["k/a/"]);
}

/// Asserts that the first GET of a file whose bytes are not in the cache fills them from the
/// upstream (the answer has the right bytes and the cache holds the range afterwards), that a range
/// of a file that is not cached is served from the fill, and that a second GET is served from the
/// cache without another read of the pack.
#[tokio::test]
async fn the_first_get_fills_the_files_bytes_into_the_cache() {
    let fx = Fixture::new("placed-fill").await;
    fx.placed();
    assert!(!fx.held(1));
    let reply = get(&fx.rig, &at("bin/other")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, fx.pack.content(1));
    assert!(fx.held(1));
    assert!(!fx.held(2));
    let reads = fx.pack_reads();
    assert!(reads > 0);
    let again = get(&fx.rig, &at("bin/other")).await;
    assert_eq!(again.body, fx.pack.content(1));
    assert_eq!(fx.pack_reads(), reads);
    let ranged = raw(
        &fx.rig,
        "GET",
        &at("lib/deep/data"),
        &[("range", "bytes=10-19")],
    )
    .await;
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.body, &fx.pack.content(2)[10..20]);
    assert!(fx.held(2));
}

/// Asserts that a file whose source does not hash to the SHA-256 the tree names (the right size,
/// at a real offset) is 500 `InternalError` on every GET, and that nothing is cached for it, while
/// a good file of the same tree is still served.
#[tokio::test]
async fn a_source_that_does_not_hash_to_the_digest_is_an_error_and_caches_nothing() {
    let fx = Fixture::new("placed-mismatch").await;
    let (_, offset, size) = fx.pack.blobs[0].clone();
    let bad = NewEntry {
        path: "bad".to_owned(),
        kind: NewKind::File {
            mode: None,
            sha256: TreeDigest::of(b"other bytes"),
            size,
            from: Some(BlobSource {
                key: PACK.into(),
                offset,
            }),
        },
    };
    fx.place("jobs/3/", vec![bad, fx.member("good", 1, None)]);
    for _ in 0..2 {
        let reply = get(&fx.rig, "jobs/3/bad").await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.code(), "InternalError");
        assert!(!fx.held(0));
        assert!(fx.cache.cache.span_of(PACK, offset, size).is_none());
    }
    let good = get(&fx.rig, "jobs/3/good").await;
    assert_eq!(good.status, StatusCode::OK);
    assert_eq!(good.body, fx.pack.content(1));
}

/// The status and the `Retry-After` an error is answered with.
fn told(error: crate::server::reply::S3Error) -> (StatusCode, String) {
    let answer = error.into_answer("/key", "0");
    let retry = answer.headers().get("retry-after");
    let retry = retry
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    (answer.status(), retry.to_owned())
}

/// Asserts what a path the store does not answer is told as: none (an ordinary key) for a
/// placement that ended, `404 NoSuchKey` for a path not in the tree or below a file, and `500
/// InternalError` for a damaged tree.
#[test]
fn a_path_the_store_does_not_answer_is_told_by_why() {
    assert!(unanswered("k", ReadError::NoPlacement).is_none());
    for error in [ReadError::NotFound, ReadError::NotADirectory] {
        let refused = unanswered("k", error).expect("an answer");
        assert_eq!(refused.kind(), (StatusCode::NOT_FOUND, "NoSuchKey"));
    }
    let damaged = unanswered("k", ReadError::Corrupt("record x".into())).expect("an answer");
    assert_eq!(
        damaged.kind(),
        (StatusCode::INTERNAL_SERVER_ERROR, "InternalError")
    );
    assert_eq!(
        damaged.detail(),
        Some("backend error: the tree placed at k: record x")
    );
}

/// Asserts that a fill a retry can mend is `503 SlowDown` with `Retry-After: 1`, and that a source
/// that does not hold the file's bytes is `500 InternalError` naming the key and the reason; for
/// every reason a fill gives.
#[test]
fn a_failed_fill_is_slow_down_when_a_retry_can_mend_it_and_an_error_otherwise() {
    let retried = [
        RangeError::Upstream(500),
        RangeError::Upstream(503),
        RangeError::Busy,
        RangeError::ReadOnly,
        RangeError::NoSpace,
        RangeError::Io,
        RangeError::Changed,
        RangeError::Aborted,
    ];
    for error in retried {
        let said = told(unfilled("k", &error));
        assert_eq!(
            said,
            (StatusCode::SERVICE_UNAVAILABLE, "1".to_owned()),
            "{error}"
        );
        assert_eq!(unfilled("k", &error).code(), "SlowDown", "{error}");
    }
    let failed = [
        RangeError::Upstream(404),
        RangeError::NotKept,
        RangeError::Empty,
        RangeError::Past,
        RangeError::Mismatch,
        RangeError::Short,
    ];
    for error in failed {
        let refused = unfilled("k", &error);
        let kind = (StatusCode::INTERNAL_SERVER_ERROR, "InternalError");
        assert_eq!(refused.kind(), kind, "{error}");
        let detail = format!(
            "backend error: the bytes of the tree file k cannot be read from its source: {error}"
        );
        assert_eq!(refused.detail(), Some(detail.as_str()), "{error}");
    }
}

/// Asserts how a body is made of what the store gives for a file: none (an ordinary key) for a
/// placement that ended; `500` for a damaged tree; `404 NoSuchKey` for a path that is not a file;
/// the fill's answer for a fill that failed; `503 SlowDown` for a range gone from the cache, or a
/// cache file gone before it was opened; and no bytes for a file with no span.
#[tokio::test]
async fn a_file_body_answers_each_way_a_file_is_not_had() {
    let fx = Fixture::new("placed-body").await;
    let id = fx.placed();
    let gone = file_body("k", Err(FetchError::Read(ReadError::NoPlacement)), 0, 1);
    assert!(gone.expect("no error").is_none());
    let damaged = Err(FetchError::Read(ReadError::Corrupt("x".into())));
    let damaged = file_body("k", damaged, 0, 1).err().expect("an error");
    assert_eq!(damaged.code(), "InternalError");
    let dir = Resolved::Dir {
        mode: Mode::DIR,
        entries: 0,
    };
    let dir = file_body("k", Err(FetchError::NotAFile(dir)), 0, 1).err();
    assert_eq!(dir.expect("an error").code(), "NoSuchKey");
    let mismatch = Err(FetchError::Fill(RangeError::Mismatch));
    let mismatch = file_body("k", mismatch, 0, 1).err().expect("an error");
    assert_eq!(mismatch.code(), "InternalError");
    let lost = file_body("k", Err(FetchError::Lost), 0, 1).err();
    assert_eq!(lost.expect("an error").code(), "SlowDown");
    let open = |span| OpenFile {
        mode: Mode::FILE,
        size: 10,
        sha256: TreeDigest::of(b""),
        span,
        hold: fx.store.open_read(&id).expect("a hold"),
    };
    let none = file_body("k", Ok(open(None)), 0, 10)
        .expect("a body")
        .expect("some");
    assert!(none.collect::<Vec<_>>().await.is_empty());
    let missing = Span {
        path: fx.cache.config.path.join("no-such-file"),
        offset: 0,
        whole: true,
    };
    let missing = file_body("k", Ok(open(Some(missing))), 0, 10).err();
    assert_eq!(missing.expect("an error").code(), "SlowDown");
}

/// Asserts that a body read from a cache file shorter than the span ends with an error after the
/// bytes it had, so the answer is cut and not completed with other bytes.
#[tokio::test]
async fn a_short_cache_file_ends_the_body_with_an_error() {
    let fx = Fixture::new("placed-short").await;
    let id = fx.placed();
    let path = fx.cache.config.path.join("short");
    std::fs::write(&path, b"0123456789").expect("a file");
    let hold = fx.store.open_read(&id).expect("a hold");
    let body = span_body(&path, 2, 20, hold).expect("an open file");
    let pieces: Vec<_> = body.collect().await;
    assert_eq!(pieces.len(), 1);
    assert!(
        matches!(&pieces[0], Err(crate::Error::Other(why)) if why.starts_with("reading a tree file: "))
    );
}

/// Asserts that a placement released and swept after a key was located under it, and before the
/// read is answered, leaves the key to be answered as an ordinary one: no answer of the tree.
#[tokio::test]
async fn a_placement_that_ends_after_the_lookup_leaves_an_ordinary_key() {
    let fx = Fixture::new("placed-race").await;
    let id = fx.placed();
    let key = at("bin/tool");
    let located = fx.store.locate_s3(BUCKET, &key).expect("located");
    fx.store.release(&id).expect("a release");
    fx.store.sweep();
    let (parts, ()) = hyper::Request::builder()
        .uri(format!("/{BUCKET}/{key}"))
        .body(())
        .expect("a request")
        .into_parts();
    let query = Query::parse(None).expect("a query");
    for body in [true, false] {
        let read = fx
            .rig
            .server
            .read_located(&parts, &query, &key, &fx.store, &located, body)
            .await;
        assert!(read.expect("no error").is_none(), "{body}");
    }
}

/// Asserts that a listing that meets a placement whose records file is damaged is `500
/// InternalError`, not a listing that leaves the tree's keys out, and that one that does not meet
/// it is served.
#[tokio::test]
async fn a_listing_that_meets_a_damaged_tree_is_an_internal_error() {
    let fx = Fixture::new("placed-damaged").await;
    let id = fx.placed();
    let tree = fx.store.placement(&id).expect("a placement").tree;
    let records = fx
        .cache
        .config
        .path
        .join("trees")
        .join(tree.to_string())
        .join("records");
    std::fs::write(&records, b"").expect("a cut file");
    let reply = fx.rig.list("prefix=jobs/").await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.code(), "InternalError");
    let other = fx.rig.list("prefix=other/").await;
    assert_eq!(other.status, StatusCode::OK);
}
