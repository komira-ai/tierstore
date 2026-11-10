//! A hand-off between servers of one process, over real sockets and a control socket in a
//! scratch directory. No test measures time: each waits for an event (a request in flight, a
//! drain that ends) and asserts what happened.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::Backend;
use crate::server::handoff::testing::{handoff, scratch, within};
use crate::server::handoff::{Front, Offer, Socket, Want, take};
use crate::server::listen::{bind, duplicates};
use crate::server::pipes::{head, status_of, until};
use crate::server::testing::{BUCKET, Rig};
use crate::server::{Drained, ServerConfig};

/// The header line that has the server close the connection after the answer.
const CLOSE: &str = "connection: close\r\n";

/// A header that makes a put create the object, not replace it.
const CREATE: [(&str, &str); 1] = [("if-none-match", "*")];

/// A server with no wait before its drain closes its door.
async fn rig() -> Rig {
    let quick = |config: &mut ServerConfig| config.drain_lb_wait = Duration::ZERO;
    Rig::with(1000, quick).await
}

/// Serves `listeners` from `rig` as an instance of a live update: it answers the control socket
/// at `path`, and begins its drain when a successor has taken the sockets. The task ends with the
/// drain.
fn instance(rig: &Rig, listeners: Vec<TcpListener>, path: &Path) -> JoinHandle<Drained> {
    let copies = duplicates(&listeners).expect("copies");
    let sockets = copies
        .into_iter()
        .map(|copy| Socket::new(Front::S3, copy))
        .collect();
    let offer = Offer::bind(path, sockets).expect("a control socket");
    let (serving, handing) = (Arc::clone(&rig.server), Arc::clone(&rig.server));
    drop(tokio::spawn(async move {
        offer.serve().await;
        handing.begin_drain();
    }));
    tokio::spawn(async move { serving.serve_all(listeners).await })
}

/// The sockets the instance at `path` hands over, taken for settings that name `at`.
async fn taken(path: &Path, at: SocketAddr) -> Option<Vec<TcpListener>> {
    let want = Want {
        s3: &[at],
        oci: &[],
        nfs: &[],
    };
    let setup = handoff(path.to_owned());
    let taken = within("taking the sockets", take(&setup, &want))
        .await
        .expect("a hand-off");
    taken.map(|inherited| inherited.s3)
}

/// A put of `key` to `at`, signed for `rig`, on a connection of its own: the whole answer.
async fn put(rig: &Rig, at: SocketAddr, key: &str) -> String {
    within(format!("the put of {key}"), put_unbounded(rig, at, key)).await
}

/// [`put`], with no bound on its waits.
async fn put_unbounded(rig: &Rig, at: SocketAddr, key: &str) -> String {
    let mut connection = TcpStream::connect(at).await.expect("connected");
    let (uri, body) = (format!("/{BUCKET}/{key}"), b"body");
    let text = head(rig, "PUT", &uri, &CREATE, body, CLOSE);
    connection
        .write_all(text.as_bytes())
        .await
        .expect("written");
    connection.write_all(body).await.expect("written");
    let mut answer = Vec::new();
    connection.read_to_end(&mut answer).await.expect("read");
    String::from_utf8_lossy(&answer).into_owned()
}

/// Puts `keys` and asserts that every one was answered 200 and is in the hot tier of one of
/// the `rigs`.
async fn put_all(rigs: [&Rig; 2], at: SocketAddr, keys: &[String]) {
    for key in keys {
        let answer = put(rigs[0], at, key).await;
        assert_eq!(status_of(&answer), 200, "{key}: {answer}");
        let stored = rigs[0].hot_mem.get(key, None).await.is_ok()
            || rigs[1].hot_mem.get(key, None).await.is_ok();
        assert!(stored, "{key} is in neither server");
    }
}

/// The keys `prefix-0` to `prefix-4`.
fn keys(prefix: &str) -> Vec<String> {
    (0..5).map(|n| format!("{prefix}-{n}")).collect()
}

/// Asserts the live update: a put whose body is half sent when the new instance takes the sockets
/// is answered 200 by the old one, which waits for it; every put before, during and after the
/// hand-off is answered 200 and stored by one of the two; the old instance's drain ends with
/// nothing cut; and once it has ended the new instance alone serves the same address. It would
/// catch a hand-off that resets the connection in flight, one that leaves a request unserved
/// between the two, and a successor that does not hold the very socket.
#[tokio::test]
async fn a_request_in_flight_finishes_and_none_fails_while_the_sockets_change_hands() {
    let (_dir, path) = scratch();
    let (old, new) = (rig().await, rig().await);
    let listener = bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let at = listener.local_addr().unwrap();
    let old_served = instance(&old, vec![listener], &path);
    put_all([&old, &new], at, &keys("before")).await;

    let mut slow = within("the connect of slow", TcpStream::connect(at))
        .await
        .unwrap();
    let (uri, body) = (format!("/{BUCKET}/slow"), b"0123456789");
    let slow_head = head(&old, "PUT", &uri, &CREATE, body, CLOSE);
    within("the head of slow", slow.write_all(slow_head.as_bytes()))
        .await
        .unwrap();
    within("half the body of slow", slow.write_all(&body[..4]))
        .await
        .unwrap();
    let begun = until(&old.server, |server| server.in_flight() == 1);
    within("slow in flight", begun).await;

    let sockets = taken(&path, at).await.expect("the old instance answers");
    let new_served = instance(&new, sockets, &path);
    put_all([&old, &new], at, &keys("during")).await;
    assert!(
        !old_served.is_finished(),
        "the request in flight holds the old drain open"
    );

    within("the rest of the body of slow", slow.write_all(&body[4..]))
        .await
        .unwrap();
    let mut answer = Vec::new();
    within("the answer to slow", slow.read_to_end(&mut answer))
        .await
        .unwrap();
    let answer = String::from_utf8_lossy(&answer).into_owned();
    assert_eq!(status_of(&answer), 200, "{answer}");
    assert!(old.hot_mem.get("slow", None).await.is_ok());
    let old_drained = within("the old drain", old_served).await.unwrap();
    assert_eq!(old_drained, Drained { abandoned: 0 });

    let after = keys("after");
    put_all([&new, &old], at, &after).await;
    for key in &after {
        assert!(
            new.hot_mem.get(key, None).await.is_ok(),
            "{key} is not in the new server"
        );
        assert!(
            old.hot_mem.get(key, None).await.is_err(),
            "{key} is in the old server"
        );
    }
    new.server.begin_drain();
    let new_drained = within("the new drain", new_served).await.unwrap();
    assert_eq!(new_drained, Drained { abandoned: 0 });
}

/// Asserts that with no instance running the take finds none (so the first instance binds its own
/// socket and serves), and that the instances then follow one another: the first serves, the
/// second takes from it and serves, the third takes from the second, which shows that each
/// successor answers the control socket in its turn; each earlier one drains with nothing cut and
/// every put is answered 200.
#[tokio::test]
async fn an_instance_starts_cold_when_none_answers_and_updates_follow_one_another() {
    let (_dir, path) = scratch();
    let (first, second, third) = (rig().await, rig().await, rig().await);
    let wanted: SocketAddr = "127.0.0.1:0".parse().unwrap();
    assert!(taken(&path, wanted).await.is_none());
    let listener = bind(wanted).unwrap();
    let at = listener.local_addr().unwrap();
    let first_served = instance(&first, vec![listener], &path);
    put_all([&first, &second], at, &keys("one")).await;

    let sockets = taken(&path, at).await.expect("the first answers");
    let second_served = instance(&second, sockets, &path);
    assert_eq!(first_served.await.unwrap(), Drained { abandoned: 0 });
    put_all([&second, &first], at, &keys("two")).await;

    let sockets = taken(&path, at).await.expect("the second answers");
    let third_served = instance(&third, sockets, &path);
    assert_eq!(second_served.await.unwrap(), Drained { abandoned: 0 });
    put_all([&third, &second], at, &keys("three")).await;
    assert!(
        path.exists(),
        "the third instance answers the control socket"
    );
    third.server.begin_drain();
    assert_eq!(third_served.await.unwrap(), Drained { abandoned: 0 });
}
