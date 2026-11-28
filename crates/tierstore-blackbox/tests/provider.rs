//! The client of the mount provider's socket, against a provider served on a socket of the test,
//! and against servers whose answers no provider gives.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]
#![cfg(target_os = "linux")]

mod support;

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use support::provider::{FakeProvider, serve_raw, serve_silent};
use tierstore_blackbox::provider::{Bounds, Code, Provider, Refusal, source};
use tierstore_testkit::dir::TestDir;
use tonic::Status;

const TREE: &str = "cas/l/0000000000000000000000000000000000000000000000000000000000000000";

/// Asserts the calls of a job's input in the agent's order: a Prepare with its source and ref,
/// a Mount that answers the provider's directory, a Release; the three on one connection, and a
/// source named as the agent on main names it (no attempt).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_input_is_prepared_mounted_and_released() {
    let dir = TestDir::under(support::SCRATCH, "provider-cycle");
    let socket = dir.path().join("p.sock");
    let fake = FakeProvider::serve(&socket, &dir.path().join("trees"));
    let mut provider = Provider::new(&socket, Bounds::default());
    let one = source("agent-1", "js.job-1", 0);
    assert_eq!(one.attempt, 0);

    provider.prepare(&one, TREE).await.unwrap();
    let path = provider.mount(&one).await.unwrap();
    assert_eq!(path, dir.path().join("trees/js.job-1/0"));
    assert!(path.is_dir());
    provider.release(&one).await.unwrap();
    assert_eq!(
        fake.calls(),
        [
            format!("Prepare js.job-1 0 {TREE}"),
            "Mount js.job-1 0".to_owned(),
            "Release js.job-1 0".to_owned(),
        ]
    );
    assert!(fake.prepared().is_empty());
}

/// Asserts that each refusal keeps the provider's code and reason: `UNAVAILABLE`,
/// `FAILED_PRECONDITION` and `INVALID_ARGUMENT` by name, any other by its number; that only the
/// two the agent fails a start on say so; and how a refusal reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_keeps_the_code_and_the_reason() {
    let dir = TestDir::under(support::SCRATCH, "provider-refused");
    let socket = dir.path().join("p.sock");
    let fake = FakeProvider::serve(&socket, &dir.path().join("trees"));
    let mut provider = Provider::new(&socket, Bounds::default());
    let one = source("agent-1", "js.job-2", 3);

    let unprepared = provider.mount(&one).await.unwrap_err();
    assert_eq!(
        unprepared,
        Refusal {
            code: Code::Unavailable,
            message: "the source is not prepared here".to_owned(),
        }
    );
    assert!(!unprepared.fails_the_start());
    assert_eq!(
        unprepared.to_string(),
        "Unavailable: the source is not prepared here"
    );

    fake.answer_next(Status::failed_precondition("the source was released"));
    fake.answer_next(Status::invalid_argument("the agent is 1 to 120 bytes"));
    fake.answer_next(Status::internal("a fault"));
    let precondition = provider.prepare(&one, TREE).await.unwrap_err();
    assert_eq!(precondition.code, Code::FailedPrecondition);
    assert_eq!(precondition.message, "the source was released");
    assert!(precondition.fails_the_start());
    let invalid = provider.release(&one).await.unwrap_err();
    assert_eq!(invalid.code, Code::InvalidArgument);
    assert!(invalid.fails_the_start());
    let other = provider.mount(&one).await.unwrap_err();
    assert_eq!(other.code, Code::Other(13));
    assert!(!other.fails_the_start());
}

/// Asserts that a socket nobody serves is no answer naming it, and that the client connects at
/// its next call once a provider serves there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_provider_is_no_answer_and_a_later_one_is_reached() {
    let dir = TestDir::under(support::SCRATCH, "provider-none");
    let socket = dir.path().join("p.sock");
    let mut provider = Provider::new(&socket, Bounds::default());
    let one = source("agent-1", "js.job-3", 0);
    let none = provider.prepare(&one, TREE).await.unwrap_err();
    assert_eq!(none.code, Code::NoAnswer);
    assert!(
        none.message.starts_with(&format!("{}: ", socket.display())),
        "{}",
        none.message
    );
    let _fake = FakeProvider::serve(&socket, &dir.path().join("trees"));
    provider.prepare(&one, TREE).await.unwrap();
}

/// Asserts that a connection that ends is dropped, and the next call makes a new one to the
/// provider that serves there then.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_ends_is_made_again() {
    let dir = TestDir::under(support::SCRATCH, "provider-again");
    let socket = dir.path().join("p.sock");
    let mut provider = Provider::new(&socket, Bounds::default());
    let one = source("agent-1", "js.job-4", 0);
    let first = FakeProvider::serve(&socket, &dir.path().join("trees"));
    provider.prepare(&one, TREE).await.unwrap();
    drop(first);
    std::fs::remove_file(&socket).unwrap();
    // The server's task is gone, and with it the connection: the call finds it closed.
    let gone = provider.mount(&one).await.unwrap_err();
    assert_eq!(gone.code, Code::NoAnswer, "{gone}");
    let second = FakeProvider::serve(&socket, &dir.path().join("trees"));
    let unprepared = provider.mount(&one).await.unwrap_err();
    assert_eq!(unprepared.code, Code::Unavailable);
    assert_eq!(second.calls(), ["Mount js.job-4 0"]);
}

/// A fixed answer of a raw server.
type Answer = fn() -> hyper::Response<Full<Bytes>>;

fn grpc(status: &str, body: &'static [u8]) -> hyper::Response<Full<Bytes>> {
    hyper::Response::builder()
        .header("content-type", "application/grpc")
        .header("grpc-status", status)
        .body(Full::new(Bytes::from_static(body)))
        .unwrap()
}

/// Asserts that an answer no provider gives is no answer, saying what was wrong: one with no
/// grpc-status, a success shorter than its frame, and a success whose message does not decode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_that_is_no_answer_says_what_is_wrong() {
    let dir = TestDir::under(support::SCRATCH, "provider-raw");
    let answers: [(&str, Answer, &str); 3] = [
        (
            "none.sock",
            || hyper::Response::new(Full::new(Bytes::new())),
            "an answer with no grpc-status",
        ),
        (
            "short.sock",
            || grpc("0", b"\0\0"),
            "an answer shorter than its frame",
        ),
        ("bad.sock", || grpc("0", b"\0\0\0\0\x02\x0a\xff"), ""),
    ];
    for (name, answer, said) in answers {
        let socket = dir.path().join(name);
        serve_raw(&socket, answer);
        let mut provider = Provider::new(Path::new(&socket), Bounds::default());
        let refused = provider.mount(&source("a", "r", 0)).await.unwrap_err();
        assert_eq!(refused.code, Code::NoAnswer, "{name}");
        assert!(refused.message.starts_with(said), "{name}: {refused}");
    }
}

/// Asserts that a provider that takes the connection and never answers is no answer once the
/// call's bound is spent, for each call with its own bound, as the agent takes it; and that the
/// bounds are the agent's unless set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_that_never_answers_is_no_answer_within_the_bound() {
    assert_eq!(
        Bounds::default(),
        Bounds {
            prepare: Duration::from_secs(10),
            mount: Duration::from_secs(30),
            release: Duration::from_secs(5),
        }
    );
    let dir = TestDir::under(support::SCRATCH, "provider-silent");
    let socket = dir.path().join("p.sock");
    serve_silent(&socket);
    let bounds = Bounds {
        prepare: Duration::from_millis(1),
        mount: Duration::from_millis(2),
        release: Duration::from_millis(3),
    };
    let mut provider = Provider::new(&socket, bounds);
    let one = source("a", "r", 0);
    let prepare = provider.prepare(&one, TREE).await.unwrap_err();
    let mount = provider.mount(&one).await.unwrap_err();
    let release = provider.release(&one).await.unwrap_err();
    for (refusal, bound) in [(prepare, "1ms"), (mount, "2ms"), (release, "3ms")] {
        assert_eq!(
            refusal,
            Refusal {
                code: Code::NoAnswer,
                message: format!("no answer within {bound}"),
            }
        );
    }
}
