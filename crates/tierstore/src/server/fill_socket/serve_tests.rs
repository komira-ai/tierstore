//! The fill socket over real Unix sockets: who may connect, what is answered, when the socket is
//! bound and when it is gone.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::{Listening, SocketFile, serve, serve_as};
use crate::Op;
use crate::server::local_cache::pack_testing::{Pack, blob_hex, fills, recency};
use crate::server::local_cache::testing::{Rig, digest, pattern, until, wait_advancing};
use crate::server::local_cache::{LocalCache, Tick, blob_path};

/// The directory of a socket, and the path of the socket in it.
struct Place {
    _dir: TestDir,
    path: PathBuf,
}

impl Place {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let path = dir.path().join("fill.sock");
        Self { _dir: dir, path }
    }
}

/// A socket served for `cache` in a task, and the means to stop it.
struct Served {
    halt: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Served {
    /// Serves `cache` at `path` for the user of this process.
    fn start(cache: &LocalCache, path: &Path) -> Self {
        Self::spawn(cache, path, None)
    }

    fn spawn(cache: &LocalCache, path: &Path, uid: Option<u32>) -> Self {
        let halt = Arc::new(Notify::new());
        let (cache, path, notify) = (cache.clone(), path.to_owned(), Arc::clone(&halt));
        let task = tokio::spawn(async move {
            match uid {
                Some(uid) => serve_as(cache, &path, notify, uid).await,
                None => serve(cache, &path, notify).await,
            }
        });
        Self { halt, task }
    }

    /// Stops it and waits until it has.
    async fn stop(self) {
        self.halt.notify_one();
        self.task.await.unwrap();
    }
}

/// A connection to the socket.
struct Client {
    read: BufReader<tokio::net::unix::OwnedReadHalf>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl Client {
    async fn connect(path: &Path) -> Self {
        let mut found = None;
        until("the socket accepts", || {
            found = std::os::unix::net::UnixStream::connect(path).ok();
            found.is_some()
        })
        .await;
        let connected = found.unwrap();
        connected.set_nonblocking(true).unwrap();
        let stream = UnixStream::from_std(connected).unwrap();
        let (read, write) = stream.into_split();
        Self {
            read: BufReader::new(read),
            write,
        }
    }

    async fn send(&mut self, text: &str) {
        self.write.write_all(text.as_bytes()).await.unwrap();
    }

    /// The next line, without its newline; none at the end of the stream.
    async fn answer(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.read.read_line(&mut line).await.unwrap() {
            0 => None,
            _ => Some(line.trim_end_matches('\n').to_owned()),
        }
    }

    /// Asks one request and gives its answer.
    async fn ask(&mut self, request: &str) -> String {
        self.send(&format!("{request}\n")).await;
        self.answer().await.expect("an answer")
    }
}

/// The request that fills blob `i` of `pack`.
fn fill_line(pack: &Pack, i: usize) -> String {
    let (sha, offset, size) = &pack.blobs[i];
    format!("fill {sha} {size} from {} @ {offset}", pack.key)
}

/// Asserts that a fill over the socket puts the blob in the cache: the answer is `ok`, the file
/// holds the blob at the path the layout gives its reserved key, and the socket is mode 0600.
#[tokio::test]
async fn a_fill_over_the_socket_puts_the_blob_in_the_cache() {
    let rig = Rig::new("tfs-fill");
    let place = Place::new("tfs-fill");
    let pack = Pack::put(&rig, "packs/one", &[2000, 3000]).await;
    let served = Served::start(&rig.cache, &place.path);
    let mut client = Client::connect(&place.path).await;
    assert_eq!(client.ask(&fill_line(&pack, 1)).await, "ok");
    let file = blob_path(&rig.config.path, &pack.blobs[1].0);
    assert_eq!(std::fs::read(file).unwrap(), pack.content(1));
    let mode = std::fs::metadata(&place.path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    served.stop().await;
}

/// Asserts that the answers of one connection come in the order of its requests, each the words
/// of what happened: a mismatch, a missing source, a blob too large for the budget, a line that is
/// not a request, and then a fill that works.
#[tokio::test]
async fn the_answers_of_a_connection_come_in_order_with_the_words_of_the_failure() {
    let rig = Rig::new("tfs-words");
    let place = Place::new("tfs-words");
    let pack = Pack::put(&rig, "packs/two", &[500, 600]).await;
    let served = Served::start(&rig.cache, &place.path);
    let mut client = Client::connect(&place.path).await;
    let (sha, offset, size) = &pack.blobs[0];
    let wrong = format!("fill {sha} {size} from {} @ {}", pack.key, offset + 1);
    assert_eq!(client.ask(&wrong).await, "mismatch");
    let absent = format!("fill {sha} {size} from packs/none @ 0");
    assert_eq!(client.ask(&absent).await, "upstream 404");
    let huge = format!("fill {sha} {} from {} @ 0", 2 << 20, pack.key);
    assert_eq!(client.ask(&huge).await, "no space");
    assert_eq!(client.ask("fill").await, "refused not a sha256 digest");
    assert_eq!(client.ask("pin a b c").await, "refused unknown request");
    assert_eq!(client.ask(&fill_line(&pack, 0)).await, "ok");
    let body = pattern(800);
    let named = digest(&body);
    rig.put(&format!("cas/l/{named}"), &body).await;
    assert_eq!(client.ask(&format!("fill {named}")).await, "ok");
    served.stop().await;
}

/// Asserts that a `touch` is not answered: the answer after a touch and a fill is the fill's, and
/// the touched blob is the most recently used.
#[tokio::test]
async fn a_touch_is_not_answered_and_counts_as_a_use() {
    let rig = Rig::new("tfs-touch");
    let place = Place::new("tfs-touch");
    let pack = Pack::put(&rig, "packs/three", &[400, 400, 400]).await;
    let served = Served::start(&rig.cache, &place.path);
    let mut client = Client::connect(&place.path).await;
    assert_eq!(client.ask(&fill_line(&pack, 0)).await, "ok");
    assert_eq!(client.ask(&fill_line(&pack, 1)).await, "ok");
    client
        .send(&format!(
            "touch {}\nnot a request at all\n",
            pack.blobs[0].0
        ))
        .await;
    assert_eq!(client.answer().await.unwrap(), "refused unknown request");
    assert_eq!(client.ask(&fill_line(&pack, 2)).await, "ok");
    let first = blob_hex(&pack.blobs[0].0);
    let order = recency(&rig.cache);
    assert_eq!(order[1], first);
    served.stop().await;
}

/// Asserts that a peer that is not the user of the instance is told so, in one line, and
/// disconnected, without being asked for a request.
#[tokio::test]
async fn a_peer_of_another_user_is_refused_and_disconnected() {
    let rig = Rig::new("tfs-user");
    let place = Place::new("tfs-user");
    let other = rustix::process::geteuid().as_raw() + 1;
    let served = Served::spawn(&rig.cache, &place.path, Some(other));
    let mut client = Client::connect(&place.path).await;
    assert_eq!(
        client.answer().await.unwrap(),
        "refused not the user of this instance"
    );
    assert_eq!(client.answer().await, None);
    assert_eq!(rig.calls(Op::Get), 0);
    served.stop().await;
}

/// Asserts the limits of a line: one longer than a request may be is refused and the connection
/// is closed; a line that is not text is refused and the connection goes on; and a stream that
/// ends inside a line is closed without an answer.
#[tokio::test]
async fn a_line_longer_than_a_request_or_not_text_is_refused() {
    let rig = Rig::new("tfs-lines");
    let place = Place::new("tfs-lines");
    let pack = Pack::put(&rig, "packs/five", &[300]).await;
    let served = Served::start(&rig.cache, &place.path);

    let mut client = Client::connect(&place.path).await;
    client.write.write_all(b"fill \xff\n").await.unwrap();
    assert_eq!(
        client.answer().await.unwrap(),
        "refused the request is not text"
    );
    assert_eq!(client.ask(&fill_line(&pack, 0)).await, "ok");

    let mut long = Client::connect(&place.path).await;
    long.write.write_all(&vec![b'a'; 70_000]).await.unwrap();
    assert_eq!(
        long.answer().await.unwrap(),
        "refused the request is too long"
    );
    assert_eq!(long.answer().await, None);

    let mut partial = Client::connect(&place.path).await;
    partial.send("fill 01").await;
    partial.write.shutdown().await.unwrap();
    assert_eq!(partial.answer().await, None);

    let mut empty = Client::connect(&place.path).await;
    empty.write.shutdown().await.unwrap();
    assert_eq!(empty.answer().await, None);
    served.stop().await;
}

/// Asserts that a line of exactly the longest length of a request is read: a touch of digests
/// that fills the limit is taken (it is not answered, so the answer that follows is a fill's).
#[tokio::test]
async fn a_line_of_exactly_the_longest_length_is_read() {
    let rig = Rig::new("tfs-edge");
    let place = Place::new("tfs-edge");
    let pack = Pack::put(&rig, "packs/six", &[300]).await;
    let served = Served::start(&rig.cache, &place.path);
    let mut client = Client::connect(&place.path).await;
    let mut touch = String::from("touch");
    while touch.len() + 65 <= super::MAX_LINE {
        touch.push(' ');
        touch.push_str(&pack.blobs[0].0);
    }
    touch.push_str(&" ".repeat(super::MAX_LINE - touch.len()));
    assert_eq!(touch.len(), super::MAX_LINE);
    client.send(&format!("{touch}\n")).await;
    assert_eq!(client.ask(&fill_line(&pack, 0)).await, "ok");
    served.stop().await;
}

/// Asserts that an instance that does not hold the lock of the cache directory binds no socket,
/// and binds one in the tick in which it takes the lock, answering fills from then on.
#[tokio::test]
async fn a_read_only_instance_binds_nothing_until_it_takes_the_lock() {
    let rig = Rig::new("tfs-wait");
    let place = Place::new("tfs-wait");
    let pack = Pack::put(&rig, "packs/seven", &[300]).await;
    let holder = rig.locked();
    let waiting = rig.locked();
    let served = Served::start(&waiting, &place.path);
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(!place.path.exists());
    drop(holder);
    assert_eq!(waiting.tick().unwrap(), Tick::Took);
    until("the socket is bound", || place.path.exists()).await;
    let mut client = Client::connect(&place.path).await;
    assert_eq!(client.ask(&fill_line(&pack, 0)).await, "ok");
    served.stop().await;
    assert!(!place.path.exists());
}

/// Asserts the design's second scenario: a fill during an update of the S3 instance is answered by
/// the old instance, which holds the lock through its drain; the new instance, read-only, binds
/// nothing, and binds the socket when the old one has gone and it takes the lock, replacing the
/// file the old one left; the old instance's end removes no file of the new one's.
#[tokio::test]
async fn a_fill_during_an_update_is_answered_by_the_old_instance_and_then_by_the_new() {
    let rig = Rig::new("tfs-update");
    let place = Place::new("tfs-update");
    let pack = Pack::put(&rig, "packs/eight", &[300, 400]).await;
    let old = rig.locked();
    let new = rig.locked();
    let old_served = Served::start(&old, &place.path);
    let new_served = Served::start(&new, &place.path);
    let mut client = Client::connect(&place.path).await;
    assert_eq!(client.ask(&fill_line(&pack, 0)).await, "ok");
    assert_eq!(fills(&old), 1);
    assert_eq!(fills(&new), 0);
    // The old instance drains and exits: its socket goes with it, and its lock is freed.
    old_served.stop().await;
    assert!(!place.path.exists());
    drop(old);
    assert_eq!(new.tick().unwrap(), Tick::Took);
    until("the new instance binds", || place.path.exists()).await;
    let mut again = Client::connect(&place.path).await;
    assert_eq!(again.ask(&fill_line(&pack, 1)).await, "ok");
    assert_eq!(fills(&new), 1);
    new_served.stop().await;
}

/// Asserts that an instance that finds the lock file replaced closes its socket, its file and its
/// connections, and is read-only until it takes a lock again.
#[tokio::test]
async fn an_instance_that_loses_the_lock_closes_its_socket() {
    let rig = Rig::new("tfs-lost");
    let place = Place::new("tfs-lost");
    let pack = Pack::put(&rig, "packs/nine", &[300]).await;
    let cache = rig.locked();
    let served = Served::start(&cache, &place.path);
    let mut client = Client::connect(&place.path).await;
    assert_eq!(client.ask(&fill_line(&pack, 0)).await, "ok");
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    assert_eq!(cache.tick().unwrap(), Tick::Lost);
    until("the socket is gone", || !place.path.exists()).await;
    assert_eq!(client.answer().await, None);
    served.stop().await;
}

/// Asserts that a file left at the path by an instance that is gone is replaced by the socket, and
/// that the file of a socket another instance made at the path since is not removed by the one
/// that made the first.
#[tokio::test]
async fn a_stale_file_is_replaced_and_a_newer_one_is_not_removed() {
    let rig = Rig::new("tfs-stale");
    let place = Place::new("tfs-stale");
    std::fs::write(&place.path, b"left over").unwrap();
    let served = Served::start(&rig.cache, &place.path);
    let mut client = Client::connect(&place.path).await;
    drop(client.write.shutdown().await);
    served.stop().await;
    assert!(!place.path.exists());

    let first = place.path.with_file_name("a.sock");
    let listener = std::os::unix::net::UnixListener::bind(&first).unwrap();
    let ours = SocketFile {
        path: first.clone(),
        identity: SocketFile::identity_of(&first).unwrap(),
    };
    std::fs::remove_file(&first).unwrap();
    let _newer = std::os::unix::net::UnixListener::bind(&first).unwrap();
    drop(ours);
    assert!(first.exists());
    drop(listener);
}

/// Asserts that a socket that cannot be bound (its directory is not there) is tried again after a
/// pause, and is bound when the directory is made. The clock is paused and moves when every task
/// waits.
#[tokio::test(start_paused = true)]
async fn a_socket_that_cannot_be_bound_is_tried_again() {
    let rig = Rig::new("tfs-rebind");
    let place = Place::new("tfs-rebind");
    let dir = place.path.with_file_name("later");
    let path = dir.join("fill.sock");
    let served = Served::start(&rig.cache, &path);
    // The first try fails: the directory is not there.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    std::fs::create_dir(&dir).unwrap();
    wait_advancing(
        "the socket was tried again and bound",
        Duration::from_secs(1),
        || path.exists(),
    )
    .await;
    served.stop().await;
}

/// Asserts that a client that goes away does not stop its fill: the blob lands in the cache, and
/// the answer that cannot be written is dropped without harm to the socket.
#[tokio::test]
async fn a_client_that_goes_away_leaves_its_fill_to_finish() {
    let rig = Rig::new("tfs-gone");
    let place = Place::new("tfs-gone");
    let pack = Pack::put(&rig, "packs/ten", &[300, 300]).await;
    let served = Served::start(&rig.cache, &place.path);
    let gate = rig.tamper.hold();
    let mut client = Client::connect(&place.path).await;
    client.send(&format!("{}\n", fill_line(&pack, 0))).await;
    until("the read is at the upstream", || rig.calls(Op::Get) == 1).await;
    drop(client);
    gate.add_permits(8);
    let file = blob_path(&rig.config.path, &pack.blobs[0].0);
    until("the blob landed", || {
        file.exists() && rig.cache.held().1 == 1
    })
    .await;
    let mut next = Client::connect(&place.path).await;
    assert_eq!(next.ask(&fill_line(&pack, 1)).await, "ok");
    served.stop().await;
}

/// Asserts that a halt ends the socket with connections open, and that the file is gone.
#[tokio::test]
async fn a_halt_ends_the_socket_and_its_connections() {
    let rig = Rig::new("tfs-halt");
    let place = Place::new("tfs-halt");
    let served = Served::start(&rig.cache, &place.path);
    let mut client = Client::connect(&place.path).await;
    let mut raw = vec![0_u8; 1];
    served.stop().await;
    assert!(matches!(client.read.read(&mut raw).await, Ok(0) | Err(_)));
    assert!(!place.path.exists());
}

/// Asserts that a path that is a directory cannot be bound: the file in the way cannot be removed,
/// and the socket is not made.
#[test]
fn a_directory_in_the_way_is_not_replaced() {
    let place = Place::new("tfs-dir");
    std::fs::create_dir(&place.path).unwrap();
    assert!(Listening::bind(&place.path).is_err());
    assert!(place.path.is_dir());
}
