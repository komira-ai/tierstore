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

use crate::{Backend, ByteRange, DirBackend, Error, PartBody};

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

/// Asserts that an object whose key starts with `_entries/` is a file of the subdirectory `.entries`
/// of the root under the encoding of the rest of the key, none is made in the root, the subdirectory
/// holds the lock file besides, and the object reads back by head, get and a streamed read.
#[tokio::test]
async fn an_entry_key_is_a_file_of_the_entries_subdirectory() {
    let (dir, backend) = backend();
    backend.put("_entries/a b", body("one")).await.unwrap();

    assert_eq!(names(dir.path()), [".entries", ".uploads"]);
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
    assert_eq!(names(dir.path()), [".entries", ".uploads"]);
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

/// Asserts that a listing of the prefix `_entries/` reads the root as well as the subdirectory,
/// since the files of the previous release for those keys are in the root: with the root made
/// unreadable (but still searchable) it fails, and with the subdirectory unreadable it fails too.
#[tokio::test]
async fn a_listing_of_the_entries_prefix_reads_the_root() {
    let (dir, backend) = backend();
    backend.put("_entries/e", body("x")).await.unwrap();
    std::fs::set_permissions(dir.path(), Permissions::from_mode(0o311)).unwrap();
    let inside = backend.list("_entries/").await;
    std::fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();

    assert!(matches!(inside, Err(Error::Other(_))));
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

/// The tag of the file the previous release made for `key` with the body `text`: the file in the
/// root, named as any key, holding the SHA-256 of the body and then the body. Gives the tag.
fn old_release(dir: &TestDir, key: &str, text: &str) -> String {
    let tag = super::sha256(text.as_bytes());
    let mut file = tag.to_vec();
    file.extend_from_slice(text.as_bytes());
    std::fs::write(dir.path().join(super::encode(key)), file).unwrap();
    hex::encode(tag)
}

/// Asserts that a key under `_entries/` that the previous release put as a file of the root is
/// still that key: head gives its size and tag, get gives the body and a range of it, a streamed
/// read and an open read give it, and a listing of every prefix that matches it has it once with
/// its size, with an ordinary key beside it; no file is made by the reads.
#[tokio::test]
async fn a_root_file_of_the_previous_release_is_still_that_entry_key() {
    let (dir, backend) = backend();
    let tag = old_release(&dir, "_entries/old", "shipped");
    backend.put("kept", body("x")).await.unwrap();

    let meta = backend.head("_entries/old").await.unwrap();
    assert_eq!((meta.size, meta.tag.as_str()), (7, tag.as_str()));
    let whole = backend.get("_entries/old", None).await.unwrap();
    assert_eq!(whole, body("shipped"));
    let range = ByteRange::new(2, 3);
    let part = backend.get("_entries/old", Some(range)).await.unwrap();
    assert_eq!(part, body("ipp"));
    let streamed = backend.get_stream("_entries/old", None).await.unwrap();
    let chunks: Vec<_> = streamed.collect().await;
    assert_eq!(chunks, [Ok(body("shipped"))]);
    let opened = backend.open_read("_entries/old", None).await.unwrap();
    assert_eq!(opened.meta.tag, tag);

    for prefix in ["", "_e", "_entries", "_entries/", "_entries/o"] {
        let listed = backend.list(prefix).await.unwrap();
        let old = listed.iter().filter(|object| object.key == "_entries/old");
        assert_eq!(old.map(|object| object.size).collect::<Vec<_>>(), [7]);
    }
    assert_eq!(keys(&backend, "").await, ["_entries/old", "kept"]);
    assert!(keys(&backend, "_entries/x").await.is_empty());
    assert_eq!(names(dir.path()), [".uploads", "_entries~old", "kept"]);
}

/// Asserts that a key present both as a file of the previous release and as a file of the
/// subdirectory reads from the file of the root, which is the key's, and is listed once, with the
/// size of that one.
#[tokio::test]
async fn a_key_in_both_places_is_read_and_listed_from_the_root_file() {
    let (dir, backend) = backend();
    old_release(&dir, "_entries/x", "older");
    backend.put("_entries/y", body("other")).await.unwrap();
    let inside = dir.path().join(DIR);
    let tag = super::sha256(b"newer!");
    let mut file = tag.to_vec();
    file.extend_from_slice(b"newer!");
    std::fs::write(inside.join("x"), file).unwrap();

    assert_eq!(
        backend.get("_entries/x", None).await.unwrap(),
        body("older")
    );
    assert_eq!(keys(&backend, "").await, ["_entries/x", "_entries/y"]);
    let listed = backend.list("_entries/x").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].size, 5);
}

/// Asserts that a create-only put of a key the previous release put as a file of the root is
/// refused as taken, whether it is a put, a reservation, a streamed put or a completed multipart
/// upload, and that none of them makes a file for the key or leaves a temporary file.
#[tokio::test]
async fn a_create_only_put_of_a_key_the_previous_release_made_is_refused() {
    let (dir, backend) = backend();
    old_release(&dir, "_entries/old", "shipped");
    let key = "_entries/old";
    let taken = Error::AlreadyExists(key.to_owned());

    assert_eq!(backend.put(key, body("new")).await, Err(taken.clone()));
    let reserved = backend.put_reservation(key, body("new")).await;
    assert_eq!(reserved, Err(taken.clone()));
    let streamed = backend.put_stream(key, None, part("new")).await;
    assert_eq!(streamed, Err(taken.clone()));
    let id = backend.create_upload(key).await.unwrap();
    let tag = backend.upload_part(key, &id, 1, part("new")).await;
    let joined = backend
        .complete_upload(key, &id, &[(1, tag.unwrap())])
        .await;
    assert_eq!(joined, Err(taken));

    assert_eq!(backend.get(key, None).await.unwrap(), body("shipped"));
    assert_eq!(names(&dir.path().join(DIR)), [".lock"]);
    assert_eq!(names(dir.path()), [".entries", ".uploads", "_entries~old"]);
}

/// Asserts that a replacement by the tag of a key the previous release put as a file of the root
/// writes that file in place, for a replacement, a streamed replacement and a reservation, so that
/// no file is made in the subdirectory for the key; that a stale tag changes nothing; and that the
/// key is listed once with the new body.
#[tokio::test]
async fn a_replacement_of_a_key_the_previous_release_made_stays_in_the_root_file() {
    let (dir, backend) = backend();
    let inside = dir.path().join(DIR);
    let key = "_entries/old";
    let tag = old_release(&dir, key, "shipped");
    let root_file = dir.path().join("_entries~old");
    let in_file = |text: &str| {
        let bytes = std::fs::read(&root_file).unwrap();
        assert_eq!(bytes[super::TAG_LEN..], *text.as_bytes());
    };

    let stale = backend.put_if_match(key, body("n"), "stale").await;
    assert_eq!(stale, Err(Error::PreconditionFailed(key.to_owned())));
    in_file("shipped");

    let second = backend.put_if_match(key, body("two"), &tag).await.unwrap();
    in_file("two");
    assert_eq!(backend.get(key, None).await.unwrap(), body("two"));
    assert_eq!(keys(&backend, "").await, [key]);

    let streamed = backend.put_stream_if_match(key, None, part("three"), &second);
    let third = streamed.await.unwrap();
    in_file("three");
    assert_eq!(backend.get(key, None).await.unwrap(), body("three"));

    let marked = backend.put_reservation_if_match(key, body("four"), &third);
    marked.await.unwrap();
    in_file("four");
    assert!(backend.head(key).await.unwrap().reservation);
    assert_eq!(names(dir.path()), [".entries", ".uploads", "_entries~old"]);
    assert_eq!(names(&inside), [".lock"]);
}

/// Asserts that a delete of a key removes the file of the previous release and the file of the
/// subdirectory, one or both; that a conditional delete by the tag of the file read removes both
/// and by a stale one removes neither; and that a conditional delete, while both files exist, goes
/// by the tag of the file of the root, which is the key's, and not by that of the subdirectory.
#[tokio::test]
async fn a_delete_of_an_entry_key_removes_the_file_of_the_previous_release_too() {
    let (dir, backend) = backend();
    let inside = dir.path().join(DIR);
    let key = "_entries/old";
    old_release(&dir, key, "shipped");
    backend.delete(key).await.unwrap();
    assert_eq!(names(dir.path()), [".entries", ".uploads"]);
    assert_eq!(
        backend.head(key).await,
        Err(Error::NotFound(key.to_owned()))
    );

    let tag = old_release(&dir, key, "shipped");
    let stale = backend.delete_if_match(key, "stale").await;
    assert_eq!(stale, Err(Error::PreconditionFailed(key.to_owned())));
    assert_eq!(backend.head(key).await.unwrap().tag, tag);
    backend.delete_if_match(key, &tag).await.unwrap();
    assert_eq!(names(dir.path()), [".entries", ".uploads"]);

    let old = old_release(&dir, key, "shipped");
    let new = backend.put_tagged(key, body("n")).await;
    assert_eq!(new, Err(Error::AlreadyExists(key.to_owned())));
    std::fs::remove_file(dir.path().join("_entries~old")).unwrap();
    let new = backend.put_tagged(key, body("n")).await.unwrap();
    old_release(&dir, key, "shipped");
    let refused = backend.delete_if_match(key, &new).await;
    assert_eq!(refused, Err(Error::PreconditionFailed(key.to_owned())));
    backend.delete_if_match(key, &old).await.unwrap();
    assert_eq!(names(&inside), [".lock"]);
    assert_eq!(names(dir.path()), [".entries", ".uploads"]);

    old_release(&dir, key, "shipped");
    backend.delete(key).await.unwrap();
    assert!(keys(&backend, "").await.is_empty());
}

/// Asserts that a delete fails, and does not panic, where the file of the subdirectory cannot be
/// removed, and that a read fails where the subdirectory is a regular file.
#[tokio::test]
async fn a_delete_fails_where_the_subdirectory_is_read_only_and_a_read_where_it_is_a_file() {
    let (dir, backend) = backend();
    backend.put("_entries/e", body("x")).await.unwrap();
    let inside = dir.path().join(DIR);
    std::fs::set_permissions(&inside, Permissions::from_mode(0o500)).unwrap();
    let refused = backend.delete("_entries/e").await;
    let conditional = backend.delete_if_match("_entries/e", "tag").await;
    std::fs::set_permissions(&inside, Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(refused, Err(Error::Other(_))));
    assert!(matches!(conditional, Err(Error::PreconditionFailed(_))));

    let tag = backend.head("_entries/e").await.unwrap().tag;
    std::fs::set_permissions(&inside, Permissions::from_mode(0o500)).unwrap();
    let stuck = backend.delete_if_match("_entries/e", &tag).await;
    std::fs::set_permissions(&inside, Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(stuck, Err(Error::Other(_))));

    std::fs::remove_dir_all(&inside).unwrap();
    std::fs::write(&inside, b"in the way").unwrap();
    assert!(matches!(
        backend.head("_entries/e").await,
        Err(Error::Other(_))
    ));
}

/// Asserts that a file of the root that has the name of a key under `_entries/` but not the name
/// the key is written to (`%2F` for the `/`) is not listed, and that the bare key `_entries` and
/// the keys `_entries.x` and `_entriesx` are ordinary keys beside a file of the previous release.
#[tokio::test]
async fn only_the_name_a_key_is_written_to_is_that_entry_key_in_the_root() {
    let (dir, backend) = backend();
    std::fs::write(dir.path().join("_entries%2Fodd"), [7_u8; 40]).unwrap();
    old_release(&dir, "_entries/old", "shipped");
    for key in ["_entries", "_entries.x", "_entriesx"] {
        backend.put(key, body("plain")).await.unwrap();
    }

    assert_eq!(
        keys(&backend, "").await,
        ["_entries", "_entries.x", "_entries/old", "_entriesx"]
    );
    assert_eq!(keys(&backend, "_entries/").await, ["_entries/old"]);
    let missing = backend.head("_entries/odd").await;
    assert_eq!(missing, Err(Error::NotFound("_entries/odd".to_owned())));
}

/// Asserts the keys of the previous release that this branch cannot write: a key under `_entries/`
/// with an empty rest, which the previous release accepted, is still listed, read, replaced in
/// place and deleted; and a key too long to be a file name is refused by a read, for a key under
/// `_entries/` and for any other.
#[tokio::test]
async fn keys_that_make_no_file_name_are_replaced_in_place_where_the_previous_release_made_them() {
    let (dir, backend) = backend();
    let tag = old_release(&dir, "_entries/", "bare");

    assert_eq!(keys(&backend, "").await, ["_entries/"]);
    assert_eq!(backend.head("_entries/").await.unwrap().tag, tag);
    backend
        .put_if_match("_entries/", body("n"), &tag)
        .await
        .unwrap();
    assert_eq!(backend.get("_entries/", None).await.unwrap(), body("n"));
    assert_eq!(keys(&backend, "").await, ["_entries/"]);
    backend.delete("_entries/").await.unwrap();
    assert!(keys(&backend, "").await.is_empty());

    for long in [format!("_entries/{}", "a".repeat(256)), "a".repeat(256)] {
        assert!(matches!(backend.head(&long).await, Err(Error::Other(_))));
    }
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

/// Asserts that a write of an entry fails, and does not panic, where the name `.entries` is a
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
    assert_eq!(names(dir.path()), [".entries", ".uploads"]);
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
    assert_eq!(names(dir.path()), [".entries", ".uploads"]);
    drop(held);
}

/// Writes the plain key `_entries` and the entry key `_entries/x` to a fresh directory, the
/// plain one first when `plain_first` is true, and asserts that each reads back its own body by
/// get and head, that a conditional replacement and a conditional delete of the entry leave the
/// plain object alone, and that the listings of `""`, `"_entries"` and `"_entries/"` are exact.
async fn coexist(plain_first: bool) {
    let (dir, backend) = backend();
    if plain_first {
        backend.put("_entries", body("plain")).await.unwrap();
    }
    let tag = backend
        .put_tagged("_entries/x", body("entry"))
        .await
        .unwrap();
    if !plain_first {
        backend.put("_entries", body("plain")).await.unwrap();
    }

    assert_eq!(backend.get("_entries", None).await.unwrap(), body("plain"));
    assert_eq!(backend.head("_entries").await.unwrap().size, 5);
    assert_eq!(
        backend.get("_entries/x", None).await.unwrap(),
        body("entry")
    );
    assert_eq!(keys(&backend, "").await, ["_entries", "_entries/x"]);
    assert_eq!(keys(&backend, "_entries").await, ["_entries", "_entries/x"]);
    assert_eq!(keys(&backend, "_entries/").await, ["_entries/x"]);
    assert!(dir.path().join(DIR).is_dir());
    assert!(dir.path().join("_entries").is_file());

    let tag = backend
        .put_if_match("_entries/x", body("newer"), &tag)
        .await
        .unwrap();
    assert_eq!(backend.get("_entries", None).await.unwrap(), body("plain"));
    backend.delete_if_match("_entries/x", &tag).await.unwrap();
    assert_eq!(keys(&backend, "").await, ["_entries"]);
    assert_eq!(backend.get("_entries", None).await.unwrap(), body("plain"));
    backend.delete("_entries").await.unwrap();
    assert!(keys(&backend, "").await.is_empty());
}

/// Asserts that an ordinary object at the bare key `_entries`, which is outside the reserved
/// prefix, and an object under `_entries/` are both readable and both listed once, whichever of
/// them is written first, because the subdirectory of the entries is not the file of that key.
#[tokio::test]
async fn a_plain_key_named_entries_coexists_with_entry_keys_in_either_write_order() {
    coexist(true).await;
    coexist(false).await;
}

/// Asserts that no client key has the name of the subdirectory: keys that begin with a dot, among
/// them `.entries` and `.entries/.lock` (the lock file's own name), are ordinary objects with
/// their own files, entry writes work beside them, and the listings are exact.
#[tokio::test]
async fn keys_that_look_like_the_entries_subdirectory_are_ordinary_objects() {
    let (dir, backend) = backend();
    backend.put(".entries", body("dot")).await.unwrap();
    backend.put(".entries/.lock", body("lock")).await.unwrap();
    backend.put("_entries/.lock", body("rest")).await.unwrap();
    backend.put("_entries/y", body("entry")).await.unwrap();

    assert_eq!(backend.get(".entries", None).await.unwrap(), body("dot"));
    assert_eq!(
        backend.get(".entries/.lock", None).await.unwrap(),
        body("lock")
    );
    assert_eq!(
        backend.get("_entries/.lock", None).await.unwrap(),
        body("rest")
    );
    assert_eq!(
        keys(&backend, "").await,
        [".entries", ".entries/.lock", "_entries/.lock", "_entries/y"]
    );
    assert!(dir.path().join(DIR).is_dir());
    assert_eq!(names(&dir.path().join(DIR)), ["%2Elock", ".lock", "y"]);
}
