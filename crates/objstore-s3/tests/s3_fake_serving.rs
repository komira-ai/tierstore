//! The loopback S3 service of `s3::fake` made to serve otherwise than it should
//! ([`FakeS3::serve_as`]): a stalled key prefix, whose requests are held unanswered until the
//! service serves again, and a failing one, whose requests are answered with an error. Every
//! request is plain HTTP on a socket of the test, so what the service answers is seen as it is
//! written.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use objstore_s3::s3::fake::{FakeS3, Serving};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// A connection to the service, and what it has read past the last answer.
struct Connection {
    sock: TcpStream,
    host: String,
}

impl Connection {
    async fn to(s3: &FakeS3) -> Self {
        let host = s3.endpoint().strip_prefix("http://").unwrap().to_owned();
        let sock = TcpStream::connect(&host).await.unwrap();
        Self { sock, host }
    }

    /// Sends `method` of the key `key` of a bucket, with `body`.
    async fn send(&mut self, method: &str, key: &str, body: &[u8]) {
        let head = format!(
            "{method} /bucket/{key} HTTP/1.1\r\nhost: {}\r\ncontent-length: {}\r\n\r\n",
            self.host,
            body.len()
        );
        self.sock.write_all(head.as_bytes()).await.unwrap();
        self.sock.write_all(body).await.unwrap();
    }

    /// Reads one answer: its status line and its body.
    async fn answer(&mut self) -> (String, Vec<u8>) {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            assert_eq!(
                self.sock.read(&mut byte).await.unwrap(),
                1,
                "the answer ended"
            );
            buf.push(byte[0]);
        }
        let head = String::from_utf8(buf).unwrap();
        let status = head.lines().next().unwrap().to_owned();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .unwrap()
            .parse()
            .unwrap();
        let mut body = vec![0u8; length];
        self.sock.read_exact(&mut body).await.unwrap();
        (status, body)
    }

    async fn ask(&mut self, method: &str, key: &str, body: &[u8]) -> (String, Vec<u8>) {
        self.send(method, key, body).await;
        self.answer().await
    }
}

/// One request on a connection of its own.
async fn ask(s3: &FakeS3, method: &str, key: &str, body: &[u8]) -> (String, Vec<u8>) {
    Connection::to(s3).await.ask(method, key, body).await
}

fn ok(body: &[u8]) -> (String, Vec<u8>) {
    ("HTTP/1.1 200 OK".to_owned(), body.to_vec())
}

/// Asserts that a stall of `cas/p/` holds a GET of a key under it, read and unanswered, while a
/// key outside it is served at once; that the service counts the held request once, however
/// often it looks at it again; and that the request is answered with its object when the
/// service serves all again.
#[tokio::test]
async fn a_stalled_prefix_holds_its_requests_until_the_service_serves_again() {
    let s3 = FakeS3::start().await;
    assert_eq!(s3.serving(), Serving::All);
    assert_eq!(ask(&s3, "PUT", "cas/l/list", b"list").await, ok(b""));
    assert_eq!(ask(&s3, "PUT", "cas/p/pack", b"pack").await, ok(b""));
    let stalled = Serving::Stalled {
        prefix: "cas/p/".to_owned(),
    };
    s3.serve_as(stalled.clone());
    assert_eq!(s3.serving(), stalled);

    let mut held = Connection::to(&s3).await;
    held.send("GET", "cas/p/pack", b"").await;
    s3.wait_for(|s3| s3.stalled_requests() == 1).await;
    assert_eq!(ask(&s3, "GET", "cas/l/list", b"").await, ok(b"list"));
    // The served request woke the held one, which looked again and was not counted again.
    assert_eq!(ask(&s3, "GET", "cas/l/list", b"").await, ok(b"list"));
    assert_eq!(s3.stalled_requests(), 1);
    assert!(
        !s3.requests()
            .iter()
            .any(|line| line.contains("cas/p/pack?") && line.starts_with("GET")),
        "the held GET reached the store: {:?}",
        s3.requests()
    );

    s3.serve_as(Serving::All);
    assert_eq!(held.answer().await, ok(b"pack"));
    assert_eq!(s3.stalled_requests(), 1);
}

/// Asserts that a request held by a stall of every key, whose client closed its connection, is
/// let go into a closed socket without harm: the service serves the next request.
#[tokio::test]
async fn a_held_request_whose_client_left_is_answered_to_no_one() {
    let s3 = FakeS3::start().await;
    assert_eq!(ask(&s3, "PUT", "k", b"bytes").await, ok(b""));
    s3.serve_as(Serving::Stalled {
        prefix: String::new(),
    });
    let mut gone = Connection::to(&s3).await;
    gone.send("GET", "k", b"").await;
    s3.wait_for(|s3| s3.stalled_requests() == 1).await;
    drop(gone);
    s3.serve_as(Serving::All);
    assert_eq!(ask(&s3, "GET", "k", b"").await, ok(b"bytes"));
}

/// Asserts that a failing prefix answers each request under it with its status and S3's error
/// body (503 `SlowDown`, 500 `InternalError`, any other status with its number), that such a PUT
/// stores nothing, and that a key outside the prefix is served as usual.
#[tokio::test]
async fn a_failing_prefix_is_answered_with_its_status_and_changes_nothing() {
    let s3 = FakeS3::start().await;
    let failing = |status| Serving::Failing {
        prefix: "cas/".to_owned(),
        status,
    };
    let body = |code: &str| format!("<Error><Code>{code}</Code><Message>{code}</Message></Error>");

    s3.serve_as(failing(503));
    let (status, text) = ask(&s3, "PUT", "cas/x", b"x").await;
    assert_eq!(status, "HTTP/1.1 503 Slow Down");
    assert_eq!(String::from_utf8(text).unwrap(), body("SlowDown"));
    assert_eq!(ask(&s3, "PUT", "other/y", b"y").await, ok(b""));

    s3.serve_as(failing(500));
    let (status, text) = ask(&s3, "GET", "cas/x", b"").await;
    assert_eq!(status, "HTTP/1.1 500 Internal Server Error");
    assert_eq!(String::from_utf8(text).unwrap(), body("InternalError"));

    s3.serve_as(failing(502));
    let (status, text) = ask(&s3, "GET", "cas/x", b"").await;
    assert_eq!(status, "HTTP/1.1 502 Error");
    assert_eq!(String::from_utf8(text).unwrap(), body("InternalError"));

    s3.serve_as(Serving::All);
    let (status, _) = ask(&s3, "GET", "cas/x", b"").await;
    assert_eq!(status, "HTTP/1.1 404 Not Found");
    assert_eq!(ask(&s3, "GET", "other/y", b"").await, ok(b"y"));
    assert_eq!(s3.stalled_requests(), 0);
}
