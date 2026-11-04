//! The keys under `_entries/` of the directory backend: their subdirectory, their listing and the
//! lock that makes their conditional writes atomic between processes. A second `DirBackend` over
//! the same directory, or a second open of the lock file, is a second open file description, which
//! `flock` treats as another process.

use std::fs::{File, Permissions, TryLockError};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use tierstore_testkit::dir::TestDir;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::{Backend, DirBackend, Error, PartBody};

use super::SyncDir;
use super::entries::{self, DIR};

fn backend() -> (TestDir, DirBackend) {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-entries");
    let backend = DirBackend::open(dir.path()).unwrap();
    (dir, backend)
}

fn body(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

fn part(text: &'static str) -> PartBody<'static> {
    stream::iter(vec![Ok(body(text))]).boxed()
}

/// The sorted names of the files in a directory.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The keys a backend lists under `prefix`.
async fn keys(backend: &DirBackend, prefix: &str) -> Vec<String> {
    let listed = backend.list(prefix).await.unwrap();
    listed.into_iter().map(|object| object.key).collect()
}

/// Asserts that an object whose key starts with `_entries/` is a file of the subdirectory `_entries`
/// of the root under the encoding of the rest of the key, none is made in the root, the subdirectory
/// holds the lock file besides, and the object reads back by head, get and a streamed read.
#[tokio::test]
async fn an_entry_key_is_a_file_of_the_entries_subdirectory() {
    let (dir, backend) = backend();
    backend.put("_entries/a b", body("one")).await.unwrap();

    assert_eq!(names(dir.path()), [".uploads", "_entries"]);
    let inside = dir.path().join(DIR);
    assert_eq!(names(&inside), [".lock", "a%20b"]);
    assert_eq!(backend.head("_entries/a b").await.unwrap().size, 3);
    assert_eq!(
        backend.get("_entries/a b", None).await.unwrap(),
        body("one")
    );
    let mut read = backend.get_stream("_entries/a b", None).await.unwrap();
    assert_eq!(read.next().await.unwrap().unwrap(), body("one"));
}

/// Asserts that opening a backend makes no subdirectory, that reading, listing and deleting an
/// entry key before any was written find nothing and make none, and that the first write makes it.
#[tokio::test]
async fn the_entries_subdirectory_is_made_by_the_first_write_of_an_entry() {
    let (dir, backend) = backend();
    assert_eq!(names(dir.path()), [".uploads"]);

    let missing = Error::NotFound("_entries/e".to_owned());
    assert_eq!(backend.head("_entries/e").await, Err(missing));
    assert!(keys(&backend, "_entries/").await.is_empty());
    assert!(keys(&backend, "").await.is_empty());
    assert_eq!(names(dir.path()), [".uploads"]);

    backend.put("_entries/e", body("x")).await.unwrap();
    assert_eq!(names(dir.path()), [".uploads", "_entries"]);
}

/// Asserts that a listing gives the entry keys for a prefix they can match and no other, from the
/// subdirectory, with the sizes of their bodies, and the other keys from the root, in one sorted
/// answer.
#[tokio::test]
async fn a_listing_reads_the_subdirectory_for_the_prefixes_that_can_match_an_entry() {
    let (_dir, backend) = backend();
    for key in ["plain", "_ex", "_entries/a", "_entries/b"] {
        backend.put(key, body("four")).await.unwrap();
    }
    let all = ["_entries/a", "_entries/b", "_ex", "plain"];
    assert_eq!(keys(&backend, "").await, all);
    assert_eq!(keys(&backend, "_e").await, all[..3]);
    assert_eq!(keys(&backend, "_entries").await, all[..2]);
    assert_eq!(keys(&backend, "_entries/").await, all[..2]);
    assert_eq!(keys(&backend, "_entries/b").await, ["_entries/b"]);
    assert!(keys(&backend, "_entries/c").await.is_empty());
    assert_eq!(keys(&backend, "p").await, ["plain"]);
    assert!(keys(&backend, "_x").await.is_empty());
    let listed = backend.list("_entries/a").await.unwrap();
    assert_eq!(listed[0].size, 4);
}

/// Asserts that a listing of the prefix `_entries/` reads the subdirectory and not the root: with
/// the root made unreadable (but still searchable) it lists the entries, and a listing of every
/// key fails.
#[tokio::test]
async fn a_listing_of_the_entries_prefix_does_not_read_the_root() {
    let (dir, backend) = backend();
    backend.put("_entries/e", body("x")).await.unwrap();
    std::fs::set_permissions(dir.path(), Permissions::from_mode(0o311)).unwrap();
    let inside = keys(&backend, "_entries/").await;
    let everything = backend.list("").await;
    std::fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();

    assert_eq!(inside, ["_entries/e"]);
    assert!(matches!(everything, Err(Error::Other(_))));
}

/// Asserts that a subdirectory that cannot be read fails a listing that needs it.
#[tokio::test]
async fn an_unreadable_entries_subdirectory_fails_the_listing() {
    let (dir, backend) = backend();
    backend.put("_entries/e", body("x")).await.unwrap();
    let inside = dir.path().join(DIR);
    std::fs::set_permissions(&inside, Permissions::from_mode(0o300)).unwrap();
    let listed = backend.list("_entries/").await;
    std::fs::set_permissions(&inside, Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(listed, Err(Error::Other(_))));
}

/// Asserts that a file in the root whose name stands for a key under `_entries/` (as a release that
/// did not know the subdirectory wrote it) is neither listed nor read as that key.
#[tokio::test]
async fn a_root_file_named_like_an_entry_key_is_not_that_key() {
    let (dir, backend) = backend();
    std::fs::write(dir.path().join("_entries~old"), [7_u8; 40]).unwrap();
    backend.put("kept", body("x")).await.unwrap();

    assert_eq!(keys(&backend, "").await, ["kept"]);
    let missing = Error::NotFound("_entries/old".to_owned());
    assert_eq!(backend.head("_entries/old").await, Err(missing));
}

/// Asserts that the rest of an entry key must make a file name: empty is refused, and 255 encoded
/// bytes are taken and 256 refused.
#[tokio::test]
async fn the_rest_of_an_entry_key_must_make_a_file_name() {
    let (_dir, backend) = backend();
    let refused = backend.put("_entries/", body("x")).await;
    assert!(matches!(refused, Err(Error::Other(_))));
    let long = format!("_entries/{}", "a".repeat(256));
    assert!(matches!(
        backend.put(&long, body("x")).await,
        Err(Error::Other(_))
    ));
    let longest = format!("_entries/{}", "a".repeat(255));
    backend.put(&longest, body("x")).await.unwrap();
    assert_eq!(keys(&backend, "_entries/").await, [longest]);
}

/// Asserts that every conditional write of an entry key keeps its meaning under the lock: a
/// create-only put of a key that exists is refused, a replacement by a tag that is not the current
/// one is refused, one by the current tag lands, one of a key that is not there fails, a delete
/// of a key that is not there succeeds, and a conditional delete by a stale tag is refused.
#[tokio::test]
async fn conditional_writes_of_entry_keys_keep_their_meaning() {
    let (_dir, backend) = backend();
    let key = "_entries/e";
    let missing = backend.put_if_match(key, body("n"), "tag").await;
    assert_eq!(missing, Err(Error::NotFound(key.to_owned())));

    let first = backend.put_tagged(key, body("one")).await.unwrap();
    let twice = backend.put(key, body("two")).await;
    assert_eq!(twice, Err(Error::AlreadyExists(key.to_owned())));
    let stale = backend.put_if_match(key, body("n"), "stale").await;
    assert_eq!(stale, Err(Error::PreconditionFailed(key.to_owned())));

    let second = backend
        .put_if_match(key, body("two"), &first)
        .await
        .unwrap();
    let streamed = backend.put_stream_if_match(key, None, part("three"), &second);
    let third = streamed.await.unwrap();
    assert_eq!(backend.get(key, None).await.unwrap(), body("three"));

    let refused = backend.delete_if_match(key, &first).await;
    assert_eq!(refused, Err(Error::PreconditionFailed(key.to_owned())));
    backend.delete_if_match(key, &third).await.unwrap();
    assert_eq!(
        backend.head(key).await,
        Err(Error::NotFound(key.to_owned()))
    );
    backend.delete(key).await.unwrap();
    backend.delete("_entries/never").await.unwrap();
}

/// Asserts that a streamed create-only put and a completed multipart upload of an entry key land in
/// the subdirectory.
#[tokio::test]
async fn streamed_and_multipart_puts_of_entry_keys_land_in_the_subdirectory() {
    let (dir, backend) = backend();
    backend
        .put_stream("_entries/s", None, part("streamed"))
        .await
        .unwrap();
    let id = backend.create_upload("_entries/m").await.unwrap();
    let tag = backend
        .upload_part("_entries/m", &id, 1, part("joined"))
        .await;
    let parts = [(1, tag.unwrap())];
    backend
        .complete_upload("_entries/m", &id, &parts)
        .await
        .unwrap();

    assert_eq!(names(&dir.path().join(DIR)), [".lock", "m", "s"]);
    assert_eq!(
        backend.get("_entries/m", None).await.unwrap(),
        body("joined")
    );
}

/// Asserts that a write of an entry fails, and does not panic, where the name `_entries` is a
/// regular file, so the lock file cannot be opened, and that it leaves no temporary file behind,
/// whether it is a put, a reservation or a streamed put.
#[tokio::test]
async fn an_entry_write_fails_where_a_file_is_named_entries() {
    let (dir, backend) = backend();
    std::fs::write(dir.path().join(DIR), b"in the way").unwrap();
    let blocked = backend.put("_entries/e", body("x")).await;
    assert!(matches!(blocked, Err(Error::Other(_))));
    let reserved = backend.put_reservation("_entries/e", body("x")).await;
    assert!(matches!(reserved, Err(Error::Other(_))));
    let streamed = backend.put_stream("_entries/e", None, part("x")).await;
    assert!(matches!(streamed, Err(Error::Other(_))));
    assert_eq!(names(dir.path()), [".uploads", "_entries"]);
}

/// Asserts that a delete of an entry fails where the root is gone, so the subdirectory cannot be
/// made for the lock.
#[tokio::test]
async fn an_entry_delete_fails_where_the_root_is_gone() {
    let (dir, backend) = backend();
    std::fs::remove_dir_all(dir.path()).unwrap();
    let lost = backend.delete("_entries/e").await;
    assert!(matches!(lost, Err(Error::Other(_))));
}

/// Asserts that the lock is exclusive between open files and released on drop: a second open of
/// the lock file gets WouldBlock while it is held and can take it after the drop, and a second
/// `take` of the subdirectory succeeds once it is released.
#[tokio::test]
async fn the_lock_is_exclusive_between_open_files_and_released_on_drop() {
    let (dir, _backend) = backend();
    let inside = dir.path().join(DIR);
    let held = entries::take(inside.clone(), || {}).await.unwrap();

    let other = File::options()
        .write(true)
        .open(inside.join(".lock"))
        .unwrap();
    assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));
    drop(held);
    other.try_lock().unwrap();
    drop(other);

    drop(entries::take(inside, || {}).await.unwrap());
}

/// Reports each wait for the lock on a channel, and syncs directories for real.
struct Reached(UnboundedSender<()>);

#[async_trait]
impl SyncDir for Reached {
    async fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        tokio::fs::File::open(dir).await?.sync_all().await
    }

    fn lock_reached(&self) {
        let _ = self.0.send(());
    }
}

/// A backend over `dir` that reports on the channel when a write of an entry reaches the lock.
fn reporting(dir: &TestDir) -> (DirBackend, UnboundedReceiver<()>) {
    let (sender, receiver) = unbounded_channel();
    let backend = DirBackend::open_with(dir.path().to_owned(), Arc::new(Reached(sender)));
    (backend.unwrap(), receiver)
}

/// Waits until a write has reached the lock, which another open file holds. A write that never
/// reaches it (the lock bypassed) ends the call, drops the backend and so the channel, and the
/// wait gets `None` instead of an event.
async fn reached(receiver: &mut UnboundedReceiver<()>) {
    assert_eq!(
        receiver.recv().await,
        Some(()),
        "the write skipped the lock"
    );
}

/// Asserts that a replacement of an entry waits for the lock held by another process, and so
/// cannot make again an entry that process deleted under the lock: the replacement is asked while
/// the lock is held, and has reached the wait for it; the entry is deleted behind the lock, the
/// lock is released, and the replacement fails with not-found and leaves no file.
#[tokio::test]
async fn a_replacement_of_an_entry_cannot_recreate_what_another_process_deleted() {
    let (dir, backend) = backend();
    let (other, mut receiver) = reporting(&dir);
    let key = "_entries/e";
    let tag = backend.put_tagged(key, body("one")).await.unwrap();
    let inside = dir.path().join(DIR);

    let held = entries::take(inside.clone(), || {}).await.unwrap();
    let asked = tokio::spawn(async move { other.put_if_match(key, body("two"), &tag).await });
    reached(&mut receiver).await;
    std::fs::remove_file(inside.join("e")).unwrap();
    drop(held);

    assert_eq!(asked.await.unwrap(), Err(Error::NotFound(key.to_owned())));
    assert_eq!(names(&inside), [".lock"]);
}

/// Asserts that a create-only put of an entry waits for the lock held by another process: a put
/// asked while the lock is held has reached the wait for it and has made nothing, and lands after
/// the lock is released.
#[tokio::test]
async fn a_create_only_put_of_an_entry_waits_for_the_lock() {
    let (dir, _backend) = backend();
    let (writer, mut receiver) = reporting(&dir);
    let inside = dir.path().join(DIR);
    let held = entries::take(inside.clone(), || {}).await.unwrap();

    let asked = tokio::spawn(async move { writer.put("_entries/e", body("one")).await });
    reached(&mut receiver).await;
    assert_eq!(names(&inside), [".lock"]);
    drop(held);

    asked.await.unwrap().unwrap();
    assert_eq!(names(&inside), [".lock", "e"]);
}

/// Asserts that a delete of an entry waits for the lock held by another process: the delete has
/// reached the wait for it, the file is still there, and it is gone after the lock is released.
#[tokio::test]
async fn a_delete_of_an_entry_waits_for_the_lock() {
    let (dir, backend) = backend();
    backend.put("_entries/e", body("one")).await.unwrap();
    let (writer, mut receiver) = reporting(&dir);
    let inside = dir.path().join(DIR);
    let held = entries::take(inside.clone(), || {}).await.unwrap();

    let asked = tokio::spawn(async move { writer.delete("_entries/e").await });
    reached(&mut receiver).await;
    assert_eq!(names(&inside), [".lock", "e"]);
    drop(held);

    asked.await.unwrap().unwrap();
    assert_eq!(names(&inside), [".lock"]);
}

/// Asserts that the keys that are not under `_entries/` take no lock: with the lock held, a put, a
/// replacement and a delete of a plain key all complete.
#[tokio::test]
async fn keys_outside_the_entries_prefix_take_no_lock() {
    let (dir, backend) = backend();
    let held = entries::take(dir.path().join(DIR), || {}).await.unwrap();

    let tag = backend.put_tagged("plain", body("one")).await.unwrap();
    backend
        .put_if_match("plain", body("two"), &tag)
        .await
        .unwrap();
    backend.delete("plain").await.unwrap();
    assert_eq!(names(dir.path()), [".uploads", "_entries"]);
    drop(held);
}
