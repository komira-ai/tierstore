//! The client of the fill socket against sockets that answer, close, replace themselves and stay
//! silent.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{AskError, FillClient, Place, PlacedTree, REPORTS_QUEUED, Until, within};
use crate::server::treefs::testing::until;
use crate::{Clock, Mono, TreeDigest};

/// A clock that stays at zero.
struct Still;

impl Clock for Still {
    fn now(&self) -> u64 {
        0
    }
}

/// An end that never comes: a request given it is asked as one given none.
const OPEN: Until<'static> = Until {
    clock: &Still,
    end_ms: u64::MAX,
};

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
    assert_eq!(client.fill_manifest("good1", OPEN), Ok(()));
    assert_eq!(client.fill_blob(("good2", 7), ("packs/a b", 9), None), Ok(()));
    assert_eq!(
        client.fill_blob(("bad", 7), ("k", 0), None),
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
        client.fill_manifest("x", OPEN),
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
    assert_eq!(client.fill_manifest("x", OPEN), Ok(()));
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
    assert_eq!(client.fill_manifest("first", OPEN), Ok(()));
    // The instance that answered is replaced: the connection the client kept is one whose
    // other end is closed.
    client.idle.lock().unwrap().clear();
    let (stale, closed) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(closed);
    client.keep(super::Connection {
        reader: BufReader::new(stale),
    });
    assert_eq!(
        client.fill_manifest("second", OPEN),
        Err("mismatch".to_owned())
    );
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
        client.fill_manifest("x", OPEN),
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
        client.fill_manifest("x", OPEN),
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

/// A client over a socket that answers every line with `reply`, and the lines it was sent.
fn replying(name: &str, reply: &'static str) -> (FillClient, Arc<Mutex<Vec<String>>>, TestDir) {
    let (dir, path) = place(name);
    let lines = Arc::new(Mutex::new(Vec::new()));
    serve(
        UnixListener::bind(&path).unwrap(),
        Arc::clone(&lines),
        move |_| Some(reply.to_owned()),
    );
    let (client, _) = client(&path, 3, Duration::from_secs(30));
    (client, lines, dir)
}

/// Asserts that a place is one line of the mount and the path, and that its answers read as a
/// placement, a directory above locations, nothing (also from a node that does not know the
/// request), and any other answer as the error it is.
#[test]
fn a_place_reads_a_placement_a_directory_nothing_and_refuses_other_answers() {
    for (reply, wanted) in [
        ("tree 0123abcd", Ok(Place::Tree("0123abcd".to_owned()))),
        ("dir", Ok(Place::Dir)),
        ("none", Ok(Place::Nothing)),
        ("refused unknown request", Ok(Place::Nothing)),
        ("read-only", Err("read-only".to_owned())),
        ("ok 0", Err("ok 0".to_owned())),
        ("tree", Err("tree".to_owned())),
    ] {
        let (client, lines, _dir) = replying("tfs-fc-place", reply);
        assert_eq!(client.place("m-1", "jobs/1f3a"), wanted, "{reply}");
        assert_eq!(*lines.lock().unwrap(), ["place m-1 jobs/1f3a"]);
    }
}

/// Asserts that mounted is one line, reads the JSON list of paths, reads the refusal of a node
/// that does not know it as no paths, and refuses anything else as the answer.
#[test]
fn mounted_reads_a_list_of_paths_and_an_unknown_request_as_none() {
    let (client, lines, _dir) = replying("tfs-fc-mounted", r#"["jobs/1","a b/c"]"#);
    assert_eq!(
        client.mounted(),
        Ok(vec!["jobs/1".to_owned(), "a b/c".to_owned()])
    );
    assert_eq!(*lines.lock().unwrap(), ["mounted"]);
    let (client, _, _dir) = replying("tfs-fc-mounted-old", "refused unknown request");
    assert_eq!(client.mounted(), Ok(Vec::new()));
    let (client, _, _dir) = replying("tfs-fc-mounted-junk", "junk");
    assert_eq!(client.mounted(), Err("junk".to_owned()));
}

/// Asserts that flat sends the placement id, reads the JSON of the tree, and gives the answer as
/// the error when it is not a tree, such as `none` for a placement that is gone.
#[test]
fn flat_reads_the_tree_of_a_placement_and_gives_other_answers_as_errors() {
    let json = r#"{"paths":["jobs/1"],"packs":["p"],"entries":[{"name":"","mode":493,"kind":"dir"},{"parent":0,"name":"f","mode":420,"kind":"file","size":3,"sha256":"aa","pack":0,"offset":9}]}"#;
    let (client, lines, _dir) = replying("tfs-fc-flat", json);
    let flat = client.flat("0123abcd").unwrap();
    assert_eq!(*lines.lock().unwrap(), ["flat 0123abcd"]);
    assert_eq!(flat.paths, ["jobs/1"]);
    assert_eq!(flat.packs, ["p"]);
    assert_eq!(flat.entries.len(), 2);
    assert_eq!(flat.entries[1].name, "f");
    assert_eq!(flat.entries[1].parent, Some(0));
    let (client, _, _dir) = replying("tfs-fc-flat-none", "none");
    assert_eq!(client.flat("0123abcd").err(), Some("none".to_owned()));
}

/// Asserts that span sends the sha256, the size, the pack and the offset, reads the offset and the
/// path of the file (spaces kept), and refuses a failure, an offset that is not a number and an
/// answer with no path, as the answer.
#[test]
fn a_span_reads_the_offset_and_path_and_refuses_other_answers() {
    let (client, lines, _dir) = replying("tfs-fc-span", "ok 4096 /cache/a dir/pack 1");
    assert_eq!(
        client.span(("ab", 12), ("cas/pack/one", 100), None),
        Ok((4096, PathBuf::from("/cache/a dir/pack 1")))
    );
    assert_eq!(
        *lines.lock().unwrap(),
        ["span ab 12 from cas/pack/one @ 100"]
    );
    for reply in ["fill: mismatch", "ok x /p", "ok 5", "ok"] {
        let (client, _, _dir) = replying("tfs-fc-span-bad", reply);
        assert_eq!(
            client.span(("ab", 12), ("k", 0), None),
            Err(reply.to_owned()),
            "{reply}"
        );
    }
}

/// Asserts that an unplace sends the mount and the placement, and that nothing is raised when the
/// socket is not there.
#[test]
fn an_unplace_sends_the_mount_and_the_placement_and_survives_a_missing_socket() {
    let (client, lines, _dir) = replying("tfs-fc-unplace", "ok");
    client.unplace("m-1", "0123abcd");
    assert_eq!(*lines.lock().unwrap(), ["unplace m-1 0123abcd"]);
    let (_dir, path) = place("tfs-fc-unplace-gone");
    let (client, _) = client_for_quit(&path);
    client.unplace("m-1", "0123abcd");
}

const FLAT: &str =
    r#"{"paths":["jobs/1"],"packs":["p"],"entries":[{"name":"","mode":493,"kind":"dir"}]}"#;
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Asserts that image sends the placement id and reads `image <tree hex> [paths]` as the image of
/// that digest with those paths, with the paths' quotes, spaces and escapes kept, and none of them
/// as an empty list.
#[test]
fn image_reads_the_digest_and_the_paths_of_an_image_answer() {
    let reply: &'static str =
        Box::leak(format!(r#"image {DIGEST} ["jobs/1", "a b/\"c\""]"#).into_boxed_str());
    let (client, lines, _dir) = replying("tfs-fc-image", reply);
    assert_eq!(
        client.image("0123abcd"),
        Ok(PlacedTree::Image {
            tree: TreeDigest::from_hex(DIGEST).unwrap(),
            paths: vec!["jobs/1".to_owned(), "a b/\"c\"".to_owned()],
        })
    );
    assert_eq!(*lines.lock().unwrap(), ["image 0123abcd"]);
    let empty: &'static str = Box::leak(format!("image {DIGEST} []").into_boxed_str());
    let (client, _, _dir) = replying("tfs-fc-image-empty", empty);
    assert_eq!(
        client.image("0123abcd"),
        Ok(PlacedTree::Image {
            tree: TreeDigest::from_hex(DIGEST).unwrap(),
            paths: Vec::new(),
        })
    );
}

/// Asserts that an answer of the flat JSON to an image request, from a records tree, is the tree
/// flattened, and that the request was sent once.
#[test]
fn image_reads_the_flat_json_of_a_records_tree() {
    let (client, lines, _dir) = replying("tfs-fc-image-flat", FLAT);
    let Ok(PlacedTree::Flat(flat)) = client.image("0123abcd") else {
        panic!("not flat");
    };
    assert_eq!(flat.paths, ["jobs/1"]);
    assert_eq!(flat.packs, ["p"]);
    assert_eq!(flat.entries.len(), 1);
    assert_eq!(*lines.lock().unwrap(), ["image 0123abcd"]);
}

/// Asserts that a node that does not know `image` is asked `flat` next and its tree is the answer,
/// and that when it gives no tree to `flat` either, the answer to `flat` is the error.
#[test]
fn image_falls_back_to_flat_when_the_node_does_not_know_the_request() {
    let (dir, path) = place("tfs-fc-image-old");
    let lines = Arc::new(Mutex::new(Vec::new()));
    serve(
        UnixListener::bind(&path).unwrap(),
        Arc::clone(&lines),
        |line| {
            Some(match line.split(' ').next() {
                Some("image") => "refused unknown request".to_owned(),
                _ if line.ends_with("gone") => "none".to_owned(),
                _ => FLAT.to_owned(),
            })
        },
    );
    let (client, _) = client(&path, 3, Duration::from_secs(30));
    let Ok(PlacedTree::Flat(flat)) = client.image("0123abcd") else {
        panic!("not flat");
    };
    assert_eq!(flat.paths, ["jobs/1"]);
    assert_eq!(client.image("gone"), Err("none".to_owned()));
    assert_eq!(
        *lines.lock().unwrap(),
        ["image 0123abcd", "flat 0123abcd", "image gone", "flat gone"]
    );
    drop(dir);
}

/// Asserts that an image answer whose digest is not sixty-four lower-case hex digits, that has no
/// paths, or whose paths are not a JSON list of strings, is the error, as the answer; and that
/// `none`, `damaged` and `too large` are the errors as the answers.
#[test]
fn image_gives_an_answer_it_cannot_read_as_the_error() {
    for (at, reply) in [
        "image zz [\"a\"]",
        "image aaaa [\"a\"]",
        "image",
        "image ",
        "image  []",
        "image {DIGEST}",
        "image {DIGEST} not-json",
        "image {DIGEST} [1, 2]",
        "image {DIGEST} {\"a\": 1}",
        "none",
        "damaged",
        "too large",
        "ok 0",
    ]
    .into_iter()
    .enumerate()
    {
        let reply: &'static str = Box::leak(reply.replace("{DIGEST}", DIGEST).into_boxed_str());
        let (client, _, _dir) = replying(&format!("tfs-fc-image-bad-{at}"), reply);
        assert_eq!(client.image("0123abcd"), Err(reply.to_owned()), "{reply}");
    }
}

/// Asserts that when nobody answers, image gives the words of that as the error, and when the
/// node does not know the request and nobody answers `flat` either, the same.
#[test]
fn image_gives_the_error_of_a_socket_that_does_not_answer() {
    let (_dir, path) = place("tfs-fc-image-away");
    let (client, pauses) = client(&path, 2, Duration::from_secs(30));
    assert_eq!(
        client.image("0123abcd"),
        Err("the fill socket is not answering".to_owned())
    );
    assert_eq!(pauses.load(Ordering::Acquire), 2);
}

/// Asserts that a `started` report is sent by a thread of its own and is never waited for: with
/// that thread stuck on a socket that has not answered its first report, [`REPORTS_QUEUED`] more
/// are queued and the next ones are dropped, all without the caller waiting. Once the socket
/// answers, every queued report is sent, in order, over the one connection, although the socket
/// answers each as a node of the release before does (`refused unknown request`): an answer is
/// read for each, so none is left on the connection for a later request.
#[test]
fn started_reports_leave_on_a_thread_of_their_own_and_past_the_queue_are_dropped() {
    let (_dir, path) = place("tfs-fc-started");
    let lines = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let held = Arc::clone(&gate);
    serve(
        UnixListener::bind(&path).unwrap(),
        Arc::clone(&lines),
        move |_| {
            let (open, opened) = &*held;
            let open = opened.wait_while(open.lock().unwrap(), |open| !*open);
            drop(open);
            Some("refused unknown request".to_owned())
        },
    );
    let (client, _) = client(&path, 3, Duration::from_secs(30));
    let received = |text: &str| lines.lock().unwrap().iter().any(|line| line == text);
    client.report_started("p/0");
    until(|| received("started p/0"));
    for n in 1..=REPORTS_QUEUED + 3 {
        client.report_started(&format!("p/{n}"));
    }
    {
        let (open, opened) = &*gate;
        *open.lock().unwrap() = true;
        opened.notify_all();
    }
    let last = REPORTS_QUEUED;
    until(|| received(&format!("started p/{last}")));
    client.report_started("p/last");
    until(|| received("started p/last"));
    let mut want: Vec<String> = (0..=REPORTS_QUEUED)
        .map(|n| format!("started p/{n}"))
        .collect();
    want.push("started p/last".to_owned());
    assert_eq!(*lines.lock().unwrap(), want);
    // The caller's own requests use connections of their own: none was made for the reports.
    assert!(client.idle.lock().unwrap().is_empty());
}

/// A client over `path` that pauses 60 times at most (an `fs.fetch_timeout` of 60 s), each pause
/// moving `clock` on by the pause and counted.
fn clocked(path: &Path, clock: &Arc<Mono>) -> (FillClient, Arc<AtomicUsize>) {
    let pauses = Arc::new(AtomicUsize::new(0));
    let (counted, moved) = (Arc::clone(&pauses), Arc::clone(clock));
    let pause = Arc::new(move |pause: Duration| {
        moved.advance(pause);
        let _ = counted.fetch_add(1, Ordering::AcqRel);
    });
    let client = FillClient::pausing(path.to_owned(), 60, Duration::from_secs(60), pause);
    (client, pauses)
}

/// The words of a request whose end came before its answer.
const SPENT: &str = "the fill socket did not answer within the time the request was given";

/// Asserts that a request given an end, with the socket away, takes no pause that would pass the
/// end and fails as spent, long before the client's own 60 pauses are used; that a second request
/// given the same end takes no pause at all; and that an end that has passed fails a request at
/// once.
#[test]
fn a_request_given_an_end_takes_no_pause_past_it() {
    let (_dir, path) = place("tfs-fc-end");
    let clock = Arc::new(Mono::new());
    let (client, pauses) = clocked(&path, &clock);
    let until = Until {
        clock: &*clock,
        end_ms: 2_500,
    };
    assert_eq!(client.fill_manifest("x", until), Err(SPENT.to_owned()));
    assert_eq!((pauses.load(Ordering::Acquire), clock.now()), (2, 2_000));
    assert_eq!(client.fill_manifest("y", until), Err(SPENT.to_owned()));
    assert_eq!((pauses.load(Ordering::Acquire), clock.now()), (2, 2_000));
    clock.advance(Duration::from_millis(500));
    assert_eq!(client.fill_manifest("z", until), Err(SPENT.to_owned()));
    assert_eq!(pauses.load(Ordering::Acquire), 2);
    // With no end the same client retries for all of its pauses.
    assert_eq!(
        client.fill_blob(("b", 1), ("k", 0), None),
        Err("the fill socket is not answering".to_owned())
    );
    assert_eq!(pauses.load(Ordering::Acquire), 62);
}

/// Asserts that a request whose end has passed is not sent, and that one whose end is still to
/// come is sent and answered as any other.
#[test]
fn a_request_whose_end_has_passed_is_not_sent() {
    let (_dir, path) = place("tfs-fc-passed");
    let lines = Arc::new(Mutex::new(Vec::new()));
    serve(
        UnixListener::bind(&path).unwrap(),
        Arc::clone(&lines),
        |_| Some("ok".to_owned()),
    );
    let clock = Arc::new(Mono::new());
    let (client, pauses) = clocked(&path, &clock);
    let until = Until {
        clock: &*clock,
        end_ms: 60_000,
    };
    assert_eq!(client.fill_manifest("early", until), Ok(()));
    clock.advance(Duration::from_secs(60));
    assert_eq!(client.fill_manifest("late", until), Err(SPENT.to_owned()));
    assert_eq!(*lines.lock().unwrap(), ["fill early"]);
    assert_eq!(pauses.load(Ordering::Acquire), 0);
}

/// Asserts that an answer is waited for the client's own time, or the time left before the
/// request's end where that is shorter.
#[test]
fn an_answer_is_waited_for_no_longer_than_the_time_left() {
    let (minute, second) = (Duration::from_secs(60), Duration::from_secs(1));
    assert_eq!(within(minute, None), minute);
    assert_eq!(within(minute, Some(second)), second);
    assert_eq!(within(second, Some(minute)), second);
    assert_eq!(within(second, Some(second)), second);
}

/// Asserts that every exchange sets how long its answer is waited for, so a kept connection waits
/// the client's own time for the next request: not the longer wait of a `put` before it, nor the
/// time left to a request given an end.
#[test]
fn a_kept_connection_waits_the_clients_own_time_for_the_next_request() {
    let (dir, path) = place("tfs-fc-wait");
    serve(
        UnixListener::bind(&path).unwrap(),
        Arc::new(Mutex::new(Vec::new())),
        |_| Some("ok".to_owned()),
    );
    let (client, _) = client(&path, 3, Duration::from_secs(120));
    let waits = |client: &FillClient| {
        let idle = client.idle.lock().unwrap();
        assert_eq!(idle.len(), 1);
        idle[0].reader.get_ref().read_timeout().unwrap()
    };
    let staged = dir.path().join("staged");
    let put = client.put(("k", "s", 1), &staged, Duration::from_secs(600));
    assert_eq!(put, Ok("ok".to_owned()));
    assert_eq!(waits(&client), Some(Duration::from_secs(600)));
    assert_eq!(client.fill_blob(("b", 1), ("k", 0), None), Ok(()));
    assert_eq!(waits(&client), Some(Duration::from_secs(120)));
    let clock = Mono::new();
    let until = Until {
        clock: &clock,
        end_ms: 50_000,
    };
    assert_eq!(client.fill_manifest("m", until), Ok(()));
    assert_eq!(waits(&client), Some(Duration::from_secs(50)));
    assert_eq!(client.fill_blob(("b", 1), ("k", 0), None), Ok(()));
    assert_eq!(waits(&client), Some(Duration::from_secs(120)));
}
