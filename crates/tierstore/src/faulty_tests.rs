//! The call counts of the faulty backend: per kind and key, and in order.

use std::sync::Arc;

use bytes::Bytes;

use crate::faulty::{FaultyBackend, Op};
use crate::{Backend, Error, MemoryBackend};

fn counted() -> FaultyBackend {
    FaultyBackend::new(Arc::new(MemoryBackend::new()))
}

fn bytes(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

/// Each key has its own count, and a kind or key nobody called has none.
#[tokio::test]
async fn calls_on_counts_each_kind_and_key_separately() {
    let backend = counted();
    backend.put("a", bytes("1")).await.unwrap();
    assert!(matches!(
        backend.put("a", bytes("2")).await,
        Err(Error::AlreadyExists(_))
    ));
    backend.put("b", bytes("3")).await.unwrap();
    backend.get("a", None).await.unwrap();

    assert_eq!(backend.calls_on(Op::Put, "a"), 2);
    assert_eq!(backend.calls_on(Op::Put, "b"), 1);
    assert_eq!(backend.calls_on(Op::Get, "a"), 1);
    assert_eq!(backend.calls_on(Op::Get, "b"), 0);
    assert_eq!(backend.calls_on(Op::Put, "c"), 0);
    assert_eq!(backend.calls(Op::Put), 3);
}

/// A key is matched whole, not as a prefix: `a` does not count the calls on `ab`.
#[tokio::test]
async fn calls_on_matches_the_whole_key() {
    let backend = counted();
    backend.put("ab", bytes("1")).await.unwrap();
    assert_eq!(backend.calls_on(Op::Put, "a"), 0);
    assert_eq!(backend.calls_on(Op::Put, "ab"), 1);
}

/// A call that was made to fail is counted on its key too.
#[tokio::test]
async fn calls_on_counts_a_failed_call() {
    let backend = counted();
    backend.fail_next(Op::Head, 1);
    assert!(backend.head("a").await.is_err());
    assert!(matches!(backend.head("a").await, Err(Error::NotFound(_))));
    assert_eq!(backend.calls_on(Op::Head, "a"), 2);
}

/// A list call is counted on the prefix it listed.
#[tokio::test]
async fn calls_on_a_list_call_is_keyed_by_its_prefix() {
    let backend = counted();
    backend.list("p/").await.unwrap();
    backend.list("p/").await.unwrap();
    backend.list("q/").await.unwrap();
    assert_eq!(backend.calls_on(Op::List, "p/"), 2);
    assert_eq!(backend.calls_on(Op::List, "q/"), 1);
}

/// The log has every call, failed or not, with its kind and key, in the order of the calls.
#[tokio::test]
async fn call_log_lists_every_call_in_order() {
    let backend = counted();
    assert!(backend.call_log().is_empty());
    backend.put("x", bytes("1")).await.unwrap();
    backend.fail_next(Op::Head, 1);
    assert!(backend.head("x").await.is_err());
    backend.get("x", None).await.unwrap();
    backend.list("p/").await.unwrap();
    backend.delete("x").await.unwrap();

    let expected = vec![
        (Op::Put, "x".to_owned()),
        (Op::Head, "x".to_owned()),
        (Op::Get, "x".to_owned()),
        (Op::List, "p/".to_owned()),
        (Op::Delete, "x".to_owned()),
    ];
    assert_eq!(backend.call_log(), expected);
}
