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
    server.join().unwrap();
    assert_eq!(
        reply,
        Reply {
            status: 503,
            body: "wait".to_owned()
        }
    );
}

/// Asserts that an answer with no length is read to the close, that header names are matched
/// whatever their case, and that a header line with no colon is ignored.
#[test]
fn an_answer_without_a_length_is_read_to_the_close() {
    let (port, server) =
        serve(b"HTTP/1.0 404 Not Found\r\nnonsense\r\nX-A: b\r\n\r\nno such path".to_vec());
    let reply = client().get(port, "/x").unwrap();
    server.join().unwrap();
    assert_eq!(
        reply,
        Reply {
            status: 404,
            body: "no such path".to_owned()
        }
    );
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
