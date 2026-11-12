//! The `put` request of the fill socket: the answer for each outcome of the put, what the staged
//! file must be, and what the cache keeps.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use futures::StreamExt as _;
use tokio::io::{AsyncRead, ReadBuf};
use tracing::Level;
use tracing::subscriber::{DefaultGuard, set_default};

use super::{answer, chunks, send};
use crate::server::build_info::stderr_log::LineLog;
use crate::server::local_cache::testing::{Rig, until};
use crate::{Backend as _, Error, Op, TreeDigest};

const CREATED: &str = r#"{"ok":true,"created":true}"#;
const EXISTED: &str = r#"{"ok":true,"created":false}"#;

/// The answer that names `word`.
fn failed(word: &str) -> String {
    format!(r#"{{"ok":false,"error":"{word}"}}"#)
}

/// A subscriber that drops what it is told, so that the warnings of a failed put are made, as they
/// are when the server runs, and are not shown.
fn quiet() -> DefaultGuard {
    set_default(LineLog::new(std::io::sink(), Level::INFO))
}

fn sha(bytes: &[u8]) -> TreeDigest {
    TreeDigest::of(bytes)
}

/// A file of `bytes` named `name` in the rig's scratch directory.
fn staged(rig: &Rig, name: &str, bytes: &[u8]) -> PathBuf {
    let path = rig.dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// The answer to a put of the file at `path`, named as `bytes`, under `key`.
async fn put(rig: &Rig, key: &str, path: &Path, bytes: &[u8]) -> String {
    let len = bytes.len() as u64;
    answer(&rig.cache, key, sha(bytes), len, path).await
}

/// Asserts that a put of a staged file makes the object (`created: true`), that the same put
/// again is `created: false` and writes nothing, and that other bytes under the key are
/// `exists_different`, leaving the first object as it was.
#[tokio::test]
async fn a_put_makes_the_object_and_the_same_put_again_is_not_a_failure() {
    let rig = Rig::writing("tfs-put-made");
    let file = staged(&rig, "pack.part", b"pack bytes");
    assert_eq!(
        put(&rig, "cas/b/1/n/pack", &file, b"pack bytes").await,
        CREATED
    );
    let got = rig.tamper.get("cas/b/1/n/pack", None).await.unwrap();
    assert_eq!(got, "pack bytes");
    assert_eq!(
        put(&rig, "cas/b/1/n/pack", &file, b"pack bytes").await,
        EXISTED
    );
    let _log = quiet();
    let other = staged(&rig, "other.part", b"pack bytez");
    let answer = put(&rig, "cas/b/1/n/pack", &other, b"pack bytez").await;
    assert_eq!(answer, failed("exists_different"));
    let got = rig.tamper.get("cas/b/1/n/pack", None).await.unwrap();
    assert_eq!(got, "pack bytes");
}

/// Asserts that a file whose bytes are not the digest named, or that is not the size named
/// (shorter or longer: a short or unflushed file), is `checksum` and makes no object.
#[tokio::test]
async fn a_file_that_is_not_its_digest_or_its_size_is_checksum() {
    let rig = Rig::writing("tfs-put-checksum");
    let _log = quiet();
    let file = staged(&rig, "a.part", b"abc");
    let wrong = answer(&rig.cache, "cas/b/w", sha(b"abd"), 3, &file).await;
    assert_eq!(wrong, failed("checksum"));
    let short = answer(&rig.cache, "cas/b/s", sha(b"abcd"), 4, &file).await;
    assert_eq!(short, failed("checksum"));
    let long = answer(&rig.cache, "cas/b/l", sha(b"ab"), 2, &file).await;
    assert_eq!(long, failed("checksum"));
    for key in ["cas/b/w", "cas/b/s", "cas/b/l"] {
        let missing = rig.tamper.get(key, None).await.unwrap_err();
        assert_eq!(missing, Error::NotFound(key.to_owned()), "{key}");
    }
}

/// Asserts that a staged path that is not a regular file the cache may open is `stage` and sends
/// nothing upstream: no file, a link (not followed, even to a good file), a directory and a FIFO
/// (opened without waiting for a writer).
#[tokio::test]
async fn a_path_that_is_not_a_regular_file_is_stage() {
    let rig = Rig::writing("tfs-put-stage");
    let _log = quiet();
    let good = staged(&rig, "good.part", b"abc");
    let link = rig.dir.path().join("link.part");
    std::os::unix::fs::symlink(&good, &link).unwrap();
    let dir = rig.dir.path().join("dir.part");
    std::fs::create_dir(&dir).unwrap();
    let fifo = rig.dir.path().join("fifo.part");
    let mode = rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR;
    rustix::fs::mknodat(rustix::fs::CWD, &fifo, rustix::fs::FileType::Fifo, mode, 0).unwrap();
    let missing = rig.dir.path().join("missing.part");
    for path in [missing, link, dir, fifo] {
        let got = put(&rig, "cas/b/k", &path, b"abc").await;
        assert_eq!(got, failed("stage"), "{}", path.display());
    }
    assert_eq!(rig.calls(Op::Put), 0);
}

/// Asserts the words for the upstream's refusals: an object too large is `toolarge`, and a store
/// that does not answer is `upstream`, as is a cache that does not write through.
#[tokio::test]
async fn the_refusals_of_the_upstream_have_their_words() {
    let rig = Rig::writing("tfs-put-upstream");
    let _log = quiet();
    let file = staged(&rig, "a.part", b"abc");
    let too_large = Error::TooLarge("cas/b/k".to_owned());
    rig.upstream.fail_next_with(Op::Put, too_large);
    assert_eq!(
        put(&rig, "cas/b/k", &file, b"abc").await,
        failed("toolarge")
    );
    rig.upstream.fail_next(Op::Put, 1);
    assert_eq!(
        put(&rig, "cas/b/k", &file, b"abc").await,
        failed("upstream")
    );
    let reading = Rig::new("tfs-put-read-only");
    let file = staged(&reading, "a.part", b"abc");
    let refused = put(&reading, "cas/b/k", &file, b"abc").await;
    assert_eq!(refused, failed("upstream"));
}

/// Asserts that the put goes through the cache as a create-only put does, with no HEAD before
/// it (a key that exists is compared, not refused), and that the cache keeps the copy of what it
/// made under a prefix it keeps, so the node reads it without the upstream; a put that found the
/// object makes no second copy.
#[tokio::test]
async fn the_put_is_sent_without_a_head_first_and_the_cache_keeps_its_copy() {
    let rig = Rig::writing("tfs-put-keep");
    let file = staged(&rig, "a.part", b"kept bytes");
    assert_eq!(put(&rig, "cas/b/k", &file, b"kept bytes").await, CREATED);
    let log = rig.upstream.call_log();
    assert_eq!(
        log.first(),
        Some(&(Op::Put, "cas/b/k".to_owned())),
        "{log:?}"
    );
    assert!(rig.has_file("cas/b/k"), "the copy is kept");
    let gets = rig.calls(Op::Get);
    assert_eq!(rig.get("cas/b/k").await.unwrap(), "kept bytes");
    assert_eq!(rig.calls(Op::Get), gets, "read from the cache");
    assert_eq!(put(&rig, "cas/b/k", &file, b"kept bytes").await, EXISTED);
    // The copy a put that found the object began is dropped; its writer removes the stage file
    // when the runtime next runs it.
    until("the stage file was removed", || rig.staged_files() == 0).await;
}

/// A reader that gives `first` and then fails.
struct Failing {
    first: Option<&'static [u8]>,
}

impl AsyncRead for Failing {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.first.take() {
            Some(first) => {
                buf.put_slice(first);
                Poll::Ready(Ok(()))
            }
            None => Poll::Ready(Err(io::Error::other("the disk failed"))),
        }
    }
}

/// Asserts that a read of the staged file that fails ends the body with an error after the bytes
/// read before it, says so, and makes the answer `stage` with no object made.
#[tokio::test]
async fn a_read_of_the_staged_file_that_fails_is_stage() {
    let broken = AtomicBool::new(false);
    let items: Vec<_> = chunks(Failing { first: Some(b"ab") }, &broken)
        .collect()
        .await;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].as_ref().unwrap(), "ab");
    assert!(items[1].is_err());
    assert!(broken.load(Ordering::Relaxed));

    let rig = Rig::writing("tfs-put-broken");
    let _log = quiet();
    let reader = Failing { first: Some(b"ab") };
    let got = send(&rig.cache, "cas/b/k", sha(b"abcd"), 4, reader).await;
    assert_eq!(got, failed("stage"));
    let missing = rig.tamper.get("cas/b/k", None).await.unwrap_err();
    assert_eq!(missing, Error::NotFound("cas/b/k".to_owned()));
}

/// Asserts that an empty staged file with the digest of no bytes is put as an empty object
/// (`created: true`, then `created: false`), which reads back empty through the cache.
#[tokio::test]
async fn an_empty_file_is_put_as_an_empty_object() {
    let rig = Rig::writing("tfs-put-empty");
    let file = staged(&rig, "empty.part", b"");
    assert_eq!(put(&rig, "cas/b/e", &file, b"").await, CREATED);
    assert_eq!(put(&rig, "cas/b/e", &file, b"").await, EXISTED);
    assert_eq!(rig.get("cas/b/e").await.unwrap(), "");
}

/// Asserts the words of what the upstream itself answers: a refusal of the checksum is `checksum`,
/// and an object deleted while the answer was decided (`Conflict`) is `upstream`.
#[tokio::test]
async fn a_checksum_refused_upstream_and_a_conflict_have_their_words() {
    let rig = Rig::writing("tfs-put-upstream-words");
    let _log = quiet();
    let file = staged(&rig, "a.part", b"abc");
    let refused = Error::BadDigest("cas/b/k".to_owned());
    rig.upstream.fail_next_with(Op::Put, refused);
    assert_eq!(
        put(&rig, "cas/b/k", &file, b"abc").await,
        failed("checksum")
    );
    let conflict = Error::Conflict("cas/b/k".to_owned());
    rig.upstream.fail_next_with(Op::Put, conflict);
    assert_eq!(
        put(&rig, "cas/b/k", &file, b"abc").await,
        failed("upstream")
    );
}
