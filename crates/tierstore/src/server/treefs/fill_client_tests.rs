//! The client of the fill socket against sockets that answer, close, replace themselves and stay
//! silent.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{AskError, FillClient};

/// A client that pauses `tries` times at most, counting the pauses instead of taking them.
fn client(path: &Path, tries: u32, wait: Duration) -> (FillClient, Arc<AtomicUsize>) {
    let pauses = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&pauses);
    let pause = Arc::new(move |_: Duration| {
        let _ = counted.fetch_add(1, Ordering::AcqRel);
    });
    (
        FillClient::pausing(path.to_owned(), tries, wait, pause),
        pauses,
    )
}

/// A socket path in a scratch directory.
fn place(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().join("fill.sock");
    (dir, path)
}

/// Serves each connection with `answer` for each line, until the line is the last the connection
/// is to get (`quit`), when it closes the connection. The lines are recorded.
fn serve(
    listener: UnixListener,
    lines: Arc<Mutex<Vec<String>>>,
    answer: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) {
    let answer = Arc::new(answer);
    let _ = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (lines, answer) = (Arc::clone(&lines), Arc::clone(&answer));
            let _ = std::thread::spawn(move || {
                let mut reader = BufReader::new(&stream);
                let mut writer = &stream;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let text = line.trim_end_matches('\n').to_owned();
                    lines.lock().unwrap().push(text.clone());
                    if text == "quit" {
                        return;
                    }
                    if let Some(reply) = answer(&text) {
                        writer.write_all(format!("{reply}\n").as_bytes()).unwrap();
                    }
                    line.clear();
                }
            });
        }
    });
}

/// Asserts that a fill is one line out and one back, that `ok` is success and any other answer is
/// the failure in the socket's words, and that the connection is kept for the next request.
#[test]
fn a_fill_is_one_line_out_and_one_back_over_a_kept_connection() {
    let (_dir, path) = place("tfs-fc-fill");
    let lines = Arc::new(Mutex::new(Vec::new()));
    serve(
        UnixListener::bind(&path).unwrap(),
        Arc::clone(&lines),
        |line| {
            Some(
                if line.contains("good") {
                    "ok"
                } else {
                    "mismatch"
                }
                .to_owned(),
            )
        },
    );
    let (client, pauses) = client(&path, 3, Duration::from_secs(30));
    assert_eq!(client.fill_manifest("good1"), Ok(()));
    assert_eq!(client.fill_blob("good2", 7, "packs/a b", 9), Ok(()));
    assert_eq!(
        client.fill_blob("bad", 7, "k", 0),
        Err("mismatch".to_owned())
    );
    assert_eq!(
        *lines.lock().unwrap(),
        [
            "fill good1",
            "fill good2 7 from packs/a b @ 9",
            "fill bad 7 from k @ 0"
        ]
    );
    assert_eq!(pauses.load(Ordering::Acquire), 0);
    assert_eq!(client.idle.lock().unwrap().len(), 1);
}

/// Asserts that a touch is sent without waiting for an answer and the connection stays usable, and
/// that the pin requests carry the mount, the tree and the blobs.
#[test]
fn a_touch_is_not_waited_for_and_the_pin_requests_carry_their_words() {
    let (_dir, path) = place("tfs-fc-pin");
    let lines = Arc::new(Mutex::new(Vec::new()));
    serve(
        UnixListener::bind(&path).unwrap(),
        Arc::clone(&lines),
        |line| (!line.starts_with("touch")).then(|| "ok 1 0 0".to_owned()),
    );
    let (client, _) = client(&path, 3, Duration::from_secs(30));
    client.touch(&["aa", "bb"]);
    assert_eq!(client.pin("m", "t", &["aa", "bb"]).unwrap(), "ok 1 0 0");
    assert_eq!(client.unpin("m", "t").unwrap(), "ok 1 0 0");
    assert_eq!(
        client
            .pins_live("m", &["t1".to_owned(), "t2".to_owned()])
            .unwrap(),
        "ok 1 0 0"
    );
    assert_eq!(client.pins_live("m", &[]).unwrap(), "ok 1 0 0");
    assert_eq!(
        *lines.lock().unwrap(),
        [
            "touch aa bb",
            "pin m t aa bb",
            "unpin m t",
            "pins-live m t1 t2",
            "pins-live m "
        ]
    );
}

/// Asserts that a socket that is not there is waited for in pauses, up to the number allowed, then
/// the request fails as unavailable; and that a socket that appears in time is used.
#[test]
fn a_socket_that_is_not_there_is_waited_for_in_pauses_up_to_a_limit() {
    let (_dir, path) = place("tfs-fc-gone");
    let (client, pauses) = client(&path, 3, Duration::from_secs(30));
    assert_eq!(
        client.fill_manifest("x"),
        Err("the fill socket is not answering".to_owned())
    );
    assert_eq!(pauses.load(Ordering::Acquire), 3);
    assert_eq!(client.pin("m", "t", &["a"]), Err(AskError::Unavailable));
    client.touch(&["a"]);
    assert_eq!(client.unpin("m", "t"), Err(AskError::Unavailable));
    // A socket that comes up during the pauses is found by the next try.
    let listener = UnixListener::bind(&path).unwrap();
    serve(listener, Arc::new(Mutex::new(Vec::new())), |_| {
        Some("ok".to_owned())
    });
    assert_eq!(client.fill_manifest("x"), Ok(()));
}

/// Asserts that a connection kept from before the socket was replaced fails once, is dropped, and
/// the request is sent again over a new connection without a pause.
#[test]
fn a_kept_connection_to_a_replaced_socket_is_replaced_without_a_pause() {
    let (_dir, path) = place("tfs-fc-replaced");
    let lines = Arc::new(Mutex::new(Vec::new()));
    serve(
        UnixListener::bind(&path).unwrap(),
        Arc::clone(&lines),
        |line| {
            Some(
                if line == "fill first" {
                    "ok"
                } else {
                    "mismatch"
                }
                .to_owned(),
            )
        },
    );
    let (client, pauses) = client(&path, 3, Duration::from_secs(30));
    assert_eq!(client.fill_manifest("first"), Ok(()));
    // The instance that answered is replaced: the connection the client kept is one whose
    // other end is closed.
    client.idle.lock().unwrap().clear();
    let (stale, closed) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(closed);
    client.keep(super::Connection {
        reader: BufReader::new(stale),
    });
    assert_eq!(client.fill_manifest("second"), Err("mismatch".to_owned()));
    assert_eq!(pauses.load(Ordering::Acquire), 0);
    assert_eq!(lines.lock().unwrap().last().unwrap(), "fill second");
}

/// Asserts that an answer that does not come is waited for as long as the client was told and is
/// then a failure that is not asked again, and that a connection that closes without an answer is
/// asked again over a new one.
#[test]
fn a_silent_socket_times_out_and_a_closed_connection_is_asked_again() {
    let (_dir, path) = place("tfs-fc-silent");
    serve(
        UnixListener::bind(&path).unwrap(),
        Arc::new(Mutex::new(Vec::new())),
        |_| None,
    );
    let (client, pauses) = client(&path, 3, Duration::from_millis(50));
    assert_eq!(
        client.fill_manifest("x"),
        Err("the fill socket gave no answer in time".to_owned())
    );
    assert_eq!(pauses.load(Ordering::Acquire), 0);
    assert_eq!(client.unpin("m", "t"), Err(AskError::TimedOut));
    // The server hangs up on a `quit`; the client gets no answer and goes on to ask again, which
    // is hung up on again, until the pauses are used.
    let (client, pauses) = client_for_quit(&path);
    assert_eq!(client.ask_line("quit", true), Err(AskError::Unavailable));
    assert_eq!(pauses.load(Ordering::Acquire), 2);
}

fn client_for_quit(path: &Path) -> (FillClient, Arc<AtomicUsize>) {
    client(path, 2, Duration::from_secs(30))
}

/// Asserts that an answer that ends before its line does is a connection that is gone, not an
/// answer: the request is asked again and, the socket going on so, fails as unavailable.
#[test]
fn an_answer_without_its_end_of_line_is_not_an_answer() {
    let (_dir, path) = place("tfs-fc-partial");
    let listener = UnixListener::bind(&path).unwrap();
    let _ = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line).unwrap();
            (&stream).write_all(b"ok").unwrap();
        }
    });
    let (client, pauses) = client(&path, 2, Duration::from_secs(30));
    assert_eq!(
        client.fill_manifest("x"),
        Err("the fill socket is not answering".to_owned())
    );
    assert_eq!(pauses.load(Ordering::Acquire), 2);
}

/// Asserts that no more than the most connections are kept.
#[test]
fn only_so_many_connections_are_kept() {
    let (_dir, path) = place("tfs-fc-kept");
    serve(
        UnixListener::bind(&path).unwrap(),
        Arc::new(Mutex::new(Vec::new())),
        |_| Some("ok".to_owned()),
    );
    let (client, _) = client(&path, 3, Duration::from_secs(30));
    for _ in 0..super::KEPT + 3 {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        client.keep(super::Connection {
            reader: BufReader::new(a),
        });
    }
    assert_eq!(client.idle.lock().unwrap().len(), super::KEPT);
}

/// Asserts that `new` pauses for a second at a time, as many as whole seconds of the timeout.
#[test]
fn the_pauses_of_a_client_are_the_whole_seconds_of_its_timeout() {
    let (_dir, path) = place("tfs-fc-new");
    let client = FillClient::new(path, Duration::from_millis(2500));
    assert_eq!(client.tries, 2);
    assert_eq!(client.answer_timeout, Duration::from_millis(2500));
}
