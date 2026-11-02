//! The loopback client against a listener the test runs itself: it answers with the bytes the
//! test gives it, or holds the connection and says nothing.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::super::ports::{Instances, PortError, PortKind, Reply};
use super::LoopbackInstances;
use tierstore_testkit::port::dead_addr;

const SHORT: Duration = Duration::from_millis(300);

/// A server for one connection: it reads the request, answers `response`, closes, and gives
/// the request back.
fn serve(response: Vec<u8>) -> (u16, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        let _ = stream.write_all(&response);
        String::from_utf8(request).unwrap()
    });
    (port, thread)
}

fn client() -> LoopbackInstances {
    LoopbackInstances::new(Duration::from_secs(5), Duration::from_secs(5))
}

fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Instances,
            message,
        } => message,
        other => panic!("not an instances failure: {other:?}"),
    }
}

fn get_failure(response: &[u8]) -> String {
    let (port, server) = serve(response.to_vec());
    let error = client().get(port, "/readyz").unwrap_err();
    server.join().unwrap();
    message(error)
}

/// Asserts the request on the wire (HTTP/1.1, the path, `Host`, `Connection: close`) and that a
/// 200 with a body of a given length is a reply with its status and body.
#[test]
fn a_200_is_a_reply_with_its_body_and_the_request_is_as_documented() {
    let (port, server) =
        serve(b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\nready read-only".to_vec());
    let reply = client().get(port, "/readyz").unwrap();
    assert_eq!(
        reply,
        Reply {
            status: 200,
            body: "ready read-only".to_owned()
        }
    );
    let request = server.join().unwrap();
    assert_eq!(
        request,
        format!("GET /readyz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
    );
}

/// Asserts that a status other than 200 is a reply, not an error: a 503 from an instance that
/// is not ready is an answer the steps read.
#[test]
fn a_non_200_is_a_reply_not_an_error() {
    let (port, server) =
        serve(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\n\r\nwait".to_vec());
    let reply = client().get(port, "/readyz").unwrap();
    assert_eq!(
        reply,
        Reply {
            status: 503,
            body: "wait".to_owned()
        }
    );
    server.join().unwrap();
}

/// Asserts that an answer with no length is read to the close, that header names are matched
/// whatever their case, and that a header line with no colon is ignored.
#[test]
fn an_answer_without_a_length_is_read_to_the_close() {
    let (port, server) =
        serve(b"HTTP/1.0 404 Not Found\r\nnonsense\r\nX-A: b\r\n\r\nno such path".to_vec());
    let reply = client().get(port, "/x").unwrap();
    assert_eq!(
        reply,
        Reply {
            status: 404,
            body: "no such path".to_owned()
        }
    );
    server.join().unwrap();
}

/// Asserts that a port nothing listens on is an error that names the request.
#[test]
fn a_refused_connection_is_an_error() {
    let port = dead_addr().port();
    let text = message(client().get(port, "/readyz").unwrap_err());
    assert!(
        text.starts_with(&format!("GET 127.0.0.1:{port}/readyz: ")),
        "{text}"
    );
}

/// Asserts that a server that accepts and then says nothing makes the read time out, and that
/// the call returns an error at the read timeout instead of waiting for the server. The server
/// holds the connection until the test has its answer, so nothing here depends on timing but
/// the bound.
#[test]
fn a_server_that_never_answers_is_a_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (release, wait) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let _ = wait.recv();
        drop(stream);
    });
    let silent = LoopbackInstances::new(SHORT, SHORT);
    let error = silent.get(port, "/readyz").unwrap_err();
    release.send(()).unwrap();
    holder.join().unwrap();
    let text = message(error);
    assert!(
        text.starts_with(&format!("GET 127.0.0.1:{port}/readyz: ")),
        "{text}"
    );
}

/// Asserts that an answer that is not an HTTP answer, one with no end of headers and a status
/// that is not a number are errors.
#[test]
fn an_answer_that_is_not_http_is_an_error() {
    assert!(get_failure(b"").contains("no end of headers"));
    assert!(get_failure(b"hello\r\n\r\n").contains("is not a status line"));
    assert!(get_failure(b"HTTP/1.1 abc OK\r\n\r\n").contains("is not a status line"));
}

/// Asserts that an answer cut short of its Content-Length is an error and not a shorter body,
/// and that a chunked answer is refused by name.
#[test]
fn a_short_body_and_a_chunked_body_are_errors() {
    let short = get_failure(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc");
    assert!(short.contains("cut short"), "{short}");
    let chunked =
        get_failure(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n");
    assert!(chunked.contains("chunked"), "{chunked}");
}

/// Asserts that an answer over 1 MiB is an error and not read without bound.
#[test]
fn an_answer_over_a_mebibyte_is_an_error() {
    let mut huge = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
    huge.resize(huge.len() + (1 << 20), b'x');
    assert!(get_failure(&huge).contains("larger than 1 MiB"));
}

/// Asserts that a path that is not a path (it does not start with a slash, or it holds a space
/// or a line break) is refused before anything is sent, so a request cannot be forged through
/// it; no listener is needed, so a connection could not have been made.
#[test]
fn a_path_that_could_forge_a_request_is_refused() {
    let port = dead_addr().port();
    for path in ["readyz", "/a b", "/a\r\nHost: x", "/a\u{7f}"] {
        let text = message(client().get(port, path).unwrap_err());
        assert!(text.contains("is not a path"), "{text}");
    }
}

/// A server for one connection whose answer comes in `pieces`: before each piece it moves the
/// shared clock on by that piece's seconds, then writes it; after the last it holds the
/// connection open until `release` says so, or closes it at once when `hold` is `None`. Time
/// moves only here, however the client's reads split the bytes. A piece the client no longer
/// takes (it gave up and closed) ends the server quietly: whether that write lands depends on
/// when the client closed, so the tests assert the client's outcome, never the write's.
fn serve_in_pieces(
    pieces: Vec<(u64, &'static [u8])>,
    clock: std::sync::Arc<std::sync::atomic::AtomicU64>,
    hold: Option<mpsc::Receiver<()>>,
) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        for (seconds, bytes) in pieces {
            clock.fetch_add(seconds, std::sync::atomic::Ordering::SeqCst);
            // A client that has already given up may have closed its end: a piece it no longer
            // reads is not the server's failure, and the test asserts what the client says.
            if stream
                .write_all(bytes)
                .and_then(|()| stream.flush())
                .is_err()
            {
                break;
            }
        }
        if let Some(hold) = hold {
            let _ = hold.recv();
        }
    });
    (port, thread)
}

/// Asserts that one deadline of 5 s bounds the whole request, the write and every read of the
/// answer together, on a clock that moves only when the server says (before each piece it
/// sends), so that how the client's reads split the bytes does not matter: an answer sent in
/// four pieces over 4 s is read whole; an answer whose head comes and whose next byte comes after
/// the clock has passed the deadline fails at the next look with an error that names the
/// deadline, though the server still holds the connection and a read could wait; nothing is
/// timed. It would catch a client whose reads each wait their own timeout, so that an answer
/// that comes a piece at a time keeps a look going past its bound.
#[test]
fn one_deadline_bounds_the_whole_request() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;
    let clocked = |seconds: &Arc<AtomicU64>| {
        let base = Instant::now();
        let seconds = Arc::clone(seconds);
        LoopbackInstances::new(Duration::from_secs(5), Duration::from_secs(5))
            .with_clock(move || base + Duration::from_secs(seconds.load(Ordering::SeqCst)))
    };
    let seconds = Arc::new(AtomicU64::new(0));
    let (port, server) = serve_in_pieces(
        vec![
            (1, b"HTTP/1.1 200 OK\r\n"),
            (1, b"Content-Length: 5\r\n\r\n"),
            (1, b"rea"),
            (1, b"dy"),
        ],
        Arc::clone(&seconds),
        None,
    );
    let reply = clocked(&seconds).get(port, "/readyz").unwrap();
    server.join().unwrap();
    assert_eq!((reply.status, reply.body.as_str()), (200, "ready"));
    let seconds = Arc::new(AtomicU64::new(0));
    let (release, hold) = mpsc::channel::<()>();
    let (port, server) = serve_in_pieces(
        vec![
            (0, b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"),
            (6, b"r"),
        ],
        Arc::clone(&seconds),
        Some(hold),
    );
    let error = clocked(&seconds).get(port, "/readyz").unwrap_err();
    // The server may have ended already, its last piece refused by the closed connection.
    let _ = release.send(());
    server.join().unwrap();
    assert_eq!(
        message(error),
        format!("GET 127.0.0.1:{port}/readyz: no whole answer within 5s")
    );
}
