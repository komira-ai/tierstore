//! The end of the settle (`door`): first over pipes on the paused clock, where a test is the
//! kernel and decides when a handshake ends; then the tables of `/proc` the door counts the
//! kernel's handshakes from, as text and as this namespace's own, and real listeners.
//!
//! The bounds on the paused clock are read from its own instant (the clock is injected, not the
//! wall's): a wait that ended late would show it there.

use std::time::Duration;

use tokio::io::AsyncWriteExt as _;
use tokio::time::Instant;

use super::{GRACE, LOOK};
use crate::server::pipes::{Kernel, Serving, drained, head, read_answer, status_of, until};
use crate::server::testing::BUCKET;
use crate::server::{ACCEPT_PAUSE, Drained, ServerConfig};

/// A probe that asks the server to close the connection after its answer.
const HEALTHZ: &str = "GET /healthz HTTP/1.1\r\nhost: tier.test\r\nconnection: close\r\n\r\n";

/// The settle of the tests: it ends at once, and the tests begin the drain and then wait.
const SETTLE: Duration = Duration::from_millis(5);

/// A settle that ends at once.
fn short_settle(config: &mut ServerConfig) {
    config.drain_lb_wait = SETTLE;
}

/// Asserts that `answer` is the 200 of `/healthz`, sent with `Connection: close`, as every answer
/// of the drain is.
fn assert_served(answer: &str) {
    assert_eq!(status_of(answer), 200, "{answer}");
    assert!(answer.contains("connection: close"), "{answer}");
}

/// Asserts that the paused clock reads no later than `bound` after `begun`.
#[track_caller]
fn no_later(begun: Instant, bound: Duration) {
    let passed = Instant::now() - begun;
    assert!(passed <= bound, "{passed:?} passed, more than {bound:?}");
}

/// Asserts that a connection whose handshake is in flight when the settle ends is taken once it
/// ends, and its request served: the door waits for it, rather than close the socket and have the
/// kernel reset a connection whose client saw its connect succeed. The door then closes, a new
/// connect is refused, and the drain ends with nothing cut. It would catch an end of the settle
/// that closes the socket at once, which is what reset the clients of a plain restart (T1, run
/// (c)).
#[tokio::test(start_paused = true)]
async fn a_handshake_in_flight_when_the_settle_ends_is_taken_and_served() {
    let serving = Serving::start(short_settle).await;
    let handshake = serving.handshake();
    serving.rig.server.begin_drain();
    serving.settle_over().await;

    let mut client = handshake
        .complete()
        .expect("the door is still open: the connection is not reset");
    client.write_all(HEALTHZ.as_bytes()).await.unwrap();
    assert_served(&read_answer(&mut client).await);
    serving.refusing().await;
    assert!(
        serving.connect().is_none(),
        "a new connect, once closed, is refused"
    );
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a burst of connections whose handshakes end together at the end of the settle,
/// all waiting in the accept queue at once, is taken and served whole, each with its answer, and
/// that the drain then ends with nothing cut. It would catch a door that takes one connection of
/// the queue and closes on the rest.
#[tokio::test(start_paused = true)]
async fn a_burst_queued_at_the_end_of_the_settle_is_all_served() {
    let serving = Serving::start(short_settle).await;
    let handshakes: Vec<_> = (0..4).map(|_| serving.handshake()).collect();
    serving.rig.server.begin_drain();
    serving.settle_over().await;
    let mut clients: Vec<_> = handshakes
        .into_iter()
        .map(|handshake| handshake.complete().expect("queued, not reset"))
        .collect();
    for client in &mut clients {
        client.write_all(HEALTHZ.as_bytes()).await.unwrap();
    }
    for client in &mut clients {
        assert_served(&read_answer(client).await);
    }
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a connection that arrives between the two looks that would close the door (the
/// first found nothing) is taken and served, and that the looks then begin again: a second one,
/// arriving a look later, is served too. It would catch a door that counts quiet looks across an
/// arrival, which closes it on a connection that came a moment after.
#[tokio::test(start_paused = true)]
async fn an_arrival_between_the_quiet_looks_begins_them_again() {
    let serving = Serving::start(short_settle).await;
    serving.rig.server.begin_drain();
    serving.settle_over().await;
    let mut clients = Vec::new();
    for _ in 0..2 {
        tokio::time::advance(LOOK / 2).await;
        let mut client = serving.connect().expect("the door is still open");
        client.write_all(HEALTHZ.as_bytes()).await.unwrap();
        clients.push(client);
        tokio::time::advance(LOOK).await;
        assert!(serving.door_open(), "one quiet look since the arrival");
    }
    for client in &mut clients {
        assert_served(&read_answer(client).await);
    }
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a connection the server took before the settle ended, and whose first request
/// comes after it, is served that request (with `Connection: close`), not closed as an idle
/// connection: its client opened it to send that request. It would catch an end of the settle
/// that closes a new connection before its first request, whose client then reads the end of the
/// stream for a request the server never answered (T1's "end of stream", and its late request).
#[tokio::test(start_paused = true)]
async fn a_first_request_that_comes_after_the_settle_is_served() {
    let serving = Serving::start(short_settle).await;
    let mut client = serving.connect().unwrap();
    until(&serving.rig.server, |server| server.open_connections() == 1).await;
    serving.rig.server.begin_drain();
    serving.refusing().await;

    client.write_all(HEALTHZ.as_bytes()).await.unwrap();
    assert_served(&read_answer(&mut client).await);
    assert_eq!(serving.rig.server.late_requests(), 0);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a first request whose head begins a moment before the grace ends, and whose rest
/// comes after it, is served whole: the grace is for the first bytes, and a request that has begun
/// is on the wire until it is answered. Nothing is counted late. It would catch a wait for the
/// first request that closes the connection on a head that is not whole when the grace ends.
#[tokio::test(start_paused = true)]
async fn a_first_request_whose_head_straddles_the_grace_is_served() {
    let serving = Serving::start(short_settle).await;
    let mut client = serving.connect().unwrap();
    until(&serving.rig.server, |server| server.open_connections() == 1).await;
    serving.rig.server.begin_drain();
    serving.refusing().await;

    let (first, rest) = HEALTHZ.split_at(20);
    tokio::time::sleep(GRACE - LOOK * 3).await;
    client.write_all(first.as_bytes()).await.unwrap();
    tokio::time::sleep(LOOK * 6).await;
    client.write_all(rest.as_bytes()).await.unwrap();
    assert_served(&read_answer(&mut client).await);
    assert_eq!(serving.rig.server.late_requests(), 0);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a first request that is a put, whose head comes before the grace ends and whose
/// body comes after it, is answered 200 and its object stored with every byte of the body, and
/// that nothing is counted late. It would catch a close that cuts a body the grace saw begin.
#[tokio::test(start_paused = true)]
async fn a_first_put_whose_body_straddles_the_grace_is_stored_whole() {
    let serving = Serving::start(short_settle).await;
    let mut client = serving.connect().unwrap();
    until(&serving.rig.server, |server| server.open_connections() == 1).await;
    serving.rig.server.begin_drain();
    serving.refusing().await;

    let body = vec![7_u8; 4096];
    let uri = format!("/{BUCKET}/straddled");
    let text = head(
        &serving.rig,
        "PUT",
        &uri,
        &[],
        &body,
        "connection: close\r\n",
    );
    tokio::time::sleep(GRACE - LOOK * 3).await;
    client.write_all(text.as_bytes()).await.unwrap();
    client.write_all(&body[..100]).await.unwrap();
    tokio::time::sleep(LOOK * 6).await;
    client.write_all(&body[100..]).await.unwrap();
    let answer = read_answer(&mut client).await;
    assert_eq!(status_of(&answer), 200, "{answer}");
    assert_eq!(serving.rig.server.late_requests(), 0);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
    let stored = serving.rig.get("straddled", None).await;
    assert_eq!(stored.status, 200);
    assert_eq!(stored.body.as_bytes(), body.as_slice());
}

/// Asserts that a new connection that sends nothing is given [`GRACE`] from the end of the settle
/// and no more: it is still open a moment before, on the paused clock, and then the server shuts
/// it (its client reads the end of the stream) no later than the grace, and the drain ends. It
/// would catch a wait for a first request that holds the drain until the header timeout, or for
/// ever.
#[tokio::test(start_paused = true)]
async fn a_new_connection_that_sends_nothing_is_closed_once_the_grace_is_over() {
    let serving = Serving::start(short_settle).await;
    let mut client = serving.connect().unwrap();
    until(&serving.rig.server, |server| server.open_connections() == 1).await;
    let begun = Instant::now();
    serving.rig.server.begin_drain();
    serving.refusing().await;

    tokio::time::advance(GRACE - LOOK * 3).await;
    assert_eq!(serving.rig.server.open_connections(), 1, "still waited for");
    assert_eq!(
        read_answer(&mut client).await,
        "",
        "shut, with nothing sent"
    );
    no_later(begun, SETTLE + GRACE);
    drop(client);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a header timeout shorter than [`GRACE`] wins: a new connection that sends nothing
/// is closed when its header timeout ends, before the grace would, and the drain ends with nothing
/// cut. It would catch a grace that holds a connection past the header timeout it was given.
#[tokio::test(start_paused = true)]
async fn a_header_timeout_shorter_than_the_grace_closes_first() {
    let header_timeout = Duration::from_secs(1);
    let serving = Serving::start(|config| {
        short_settle(config);
        config.header_timeout = header_timeout;
    })
    .await;
    let begun = Instant::now();
    let mut client = serving.connect().unwrap();
    until(&serving.rig.server, |server| server.open_connections() == 1).await;
    serving.rig.server.begin_drain();
    assert_eq!(read_answer(&mut client).await, "", "closed, nothing sent");
    no_later(begun, header_timeout);
    drop(client);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a drain deadline shorter than [`GRACE`], and one of zero, wins: the drain ends at
/// its deadline after the door closes, with a new connection that sent nothing still open, and
/// counts nothing cut, since that connection has no request. It would catch a grace that holds the
/// drain past its deadline.
#[tokio::test(start_paused = true)]
async fn a_drain_deadline_shorter_than_the_grace_ends_the_drain_first() {
    for deadline in [Duration::from_secs(1), Duration::ZERO] {
        let serving = Serving::start(|config| {
            short_settle(config);
            config.drain_deadline = deadline;
        })
        .await;
        let client = serving.connect().unwrap();
        until(&serving.rig.server, |server| server.open_connections() == 1).await;
        let begun = Instant::now();
        serving.rig.server.begin_drain();
        assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
        no_later(begun, SETTLE + LOOK * 3 + deadline);
        assert_eq!(serving.rig.server.open_connections(), 1, "{deadline:?}");
        drop(client);
    }
}

/// Asserts that a new connection whose client closes it before sending anything, during the
/// grace, ends at once: the server's count of open connections falls to zero and the drain ends
/// before the grace is over, with nothing cut or late. It would catch a wait for a first request
/// that outlives the connection it waits on.
#[tokio::test(start_paused = true)]
async fn a_new_connection_closed_by_its_client_in_the_grace_ends_the_wait() {
    let serving = Serving::start(short_settle).await;
    let client = serving.connect().unwrap();
    until(&serving.rig.server, |server| server.open_connections() == 1).await;
    let begun = Instant::now();
    serving.rig.server.begin_drain();
    serving.refusing().await;
    drop(client);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
    no_later(begun, SETTLE + LOOK * 3);
    assert_eq!(serving.rig.server.open_connections(), 0);
    assert_eq!(serving.rig.server.late_requests(), 0);
}

/// Asserts that a handshake that never ends holds the door open for [`GRACE`] and no more: the
/// socket is still open a moment before, on the paused clock, and is closed no later than the
/// grace, and the drain ends with nothing cut; the connection, ended too late, is reset. It would
/// catch a door that waits for ever for a client that has gone, or that does not wait at all.
#[tokio::test(start_paused = true)]
async fn a_handshake_that_never_ends_holds_the_door_for_the_grace_only() {
    let serving = Serving::start(short_settle).await;
    let handshake = serving.handshake();
    let begun = Instant::now();
    serving.rig.server.begin_drain();
    serving.settle_over().await;

    tokio::time::advance(GRACE - LOOK * 3).await;
    assert!(serving.door_open(), "the handshake is still waited for");
    serving.refusing().await;
    no_later(begun, SETTLE + GRACE);
    assert!(handshake.complete().is_none(), "too late: reset");
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a door whose handshakes cannot be counted takes them to be in flight and waits out
/// [`GRACE`] (still open a moment before), then closes no later than the grace, and the drain
/// ends. It would catch a failure to count that is read as "none in flight", which closes the
/// door on a handshake.
#[tokio::test(start_paused = true)]
async fn a_door_that_cannot_count_the_handshakes_waits_out_the_grace() {
    let serving = Serving::start_on(0, Kernel::count_failing(), short_settle).await;
    let begun = Instant::now();
    serving.rig.server.begin_drain();
    serving.settle_over().await;

    tokio::time::advance(GRACE - LOOK * 3).await;
    assert!(
        serving.door_open(),
        "the uncounted handshakes are waited for"
    );
    serving.refusing().await;
    no_later(begun, SETTLE + GRACE);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a handshake answered with a SYN cookie (one the count cannot see), whose last
/// segment comes after the looks that would have closed the door but within [`GRACE`], is taken
/// and served, since the count fails once a cookie has been sent. It would catch a door that trusts
/// a count of zero while the kernel answers handshakes it keeps no entry for.
#[tokio::test(start_paused = true)]
async fn a_cookie_handshake_that_ends_after_the_looks_is_served() {
    let serving = Serving::start_on(0, Kernel::count_failing(), short_settle).await;
    let handshake = serving.cookie_handshake();
    serving.rig.server.begin_drain();
    serving.settle_over().await;
    tokio::time::advance(LOOK * 5).await;

    let mut client = handshake.complete().expect("still waited for, not reset");
    client.write_all(HEALTHZ.as_bytes()).await.unwrap();
    assert_served(&read_answer(&mut client).await);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts what the count failing guards against: with a count that works and sees no
/// handshake, the door closes after its two looks, and a cookie handshake that ends after them is
/// reset. It would catch a pipe model in which the cookie test above passes for another reason.
#[tokio::test(start_paused = true)]
async fn a_cookie_handshake_the_count_trusts_is_reset_after_the_looks() {
    let serving = Serving::start(short_settle).await;
    let handshake = serving.cookie_handshake();
    serving.rig.server.begin_drain();
    serving.settle_over().await;
    serving.refusing().await;
    assert!(handshake.complete().is_none(), "the door closed on it");
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that while the count fails, a handshake in flight is waited for, taken when it ends
/// and served, and that once the count works again (and sees nothing) the door closes after its
/// two looks, before [`GRACE`] is over. It would catch a door that stops waiting for handshakes
/// when the count fails, or that waits out the grace after the count has recovered.
#[tokio::test(start_paused = true)]
async fn a_count_that_fails_then_recovers_serves_the_pending_and_closes() {
    let serving = Serving::start_on(0, Kernel::count_failing_for(3), short_settle).await;
    let handshake = serving.handshake();
    let begun = Instant::now();
    serving.rig.server.begin_drain();
    serving.settle_over().await;
    tokio::time::advance(LOOK * 5).await;
    assert!(serving.door_open());

    let mut client = handshake.complete().expect("waited for, not reset");
    client.write_all(HEALTHZ.as_bytes()).await.unwrap();
    assert_served(&read_answer(&mut client).await);
    serving.refusing().await;
    assert!(
        Instant::now() - begun < SETTLE + GRACE,
        "closed before the grace"
    );
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that accepts that keep failing while the door closes (each followed by the accept
/// pause) hold it open no longer than [`GRACE`] and the one pause it was in, and the drain then
/// ends with nothing cut. It would catch a door that looks at its bound only while it waits, which
/// a source that always answers, with an error, would keep open for ever.
#[tokio::test(start_paused = true)]
async fn accepts_that_keep_failing_hold_the_door_no_longer_than_the_grace() {
    let serving = Serving::start_after(usize::MAX, short_settle).await;
    let begun = Instant::now();
    serving.rig.server.begin_drain();
    serving.refusing().await;
    no_later(begun, SETTLE + ACCEPT_PAUSE + GRACE + ACCEPT_PAUSE);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that outside Linux the count is an error, which the door takes for handshakes in
/// flight, and waits out the grace: the system keeps no table it can read, and a count of zero
/// would close the door on a handshake it cannot see.
#[cfg(not(target_os = "linux"))]
#[test]
fn outside_linux_the_count_is_an_error() {
    assert_eq!(super::syn_cookies(), None);
    let error = super::handshakes(&[9301], None).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

/// The kernel's side, on Linux: the tables of `/proc`, and real listeners.
#[cfg(target_os = "linux")]
mod kernel {
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::net::TcpStream;
    use std::sync::Arc;

    use tierstore_testkit::dir::TestDir;

    use super::super::{handshakes, handshakes_in, handshakes_under, syn_cookies, syn_cookies_in};
    use super::{HEALTHZ, assert_served, short_settle};
    use crate::server::Incoming as _;
    use crate::server::listen::{Listeners, bind};
    use crate::server::testing::Rig;
    use crate::server::{Drained, Server};

    /// A bound on a client's read, so that a server that never answers fails the test rather than
    /// hang it. No test passes by the end of it.
    const SAFETY: std::time::Duration = std::time::Duration::from_secs(120);

    /// A heading and lines in the format of `/proc/net/tcp`: two handshakes in flight on 9301 (hex
    /// `2455`), one IPv4 and one IPv6, one open connection on it (state `01`), a handshake on
    /// another port, and lines that are not whole.
    const TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
       0: 0100007F:2455 0100007F:D431 03 00000000:00000000 01:00000064 00000000     0        0 0 2 0000000000000000
       1: 0100007F:2455 0100007F:D432 01 00000000:00000000 00:00000000 00000000     0        0 77 1 0000000000000000
       2: 0100007F:2456 0100007F:D433 03 00000000:00000000 01:00000064 00000000     0        0 0 2 0000000000000000
       3: 00000000000000000000000001000000:2455 00000000000000000000000001000000:D434 03 00000000:00000000 01:00000064 00000000 0 0 0 2 0
       4: 0100007F:ZZZZ 0100007F:D435 03
       5: 0100007F
       6: 0100007F:2455
    ";

    /// The format of `/proc/net/netstat`: `TcpExt` names and values, with 7 SYN cookies sent, and
    /// another table's lines before them.
    const NETSTAT: &str = "IpExt: InNoRoutes InTruncatedPkts
IpExt: 0 0
TcpExt: SyncookiesSent SyncookiesRecv SyncookiesFailed
TcpExt: 7 3 0
";

    /// Asserts that the count of a table takes the lines in state `SYN_RECV` on the given ports, of
    /// either family, and nothing else: not an open connection, not another port, and not a line it
    /// cannot read (which counts for nothing).
    #[test]
    fn the_count_takes_the_handshakes_on_the_ports_and_nothing_else() {
        assert_eq!(handshakes_in(TABLE, &[9301]), 2);
        assert_eq!(handshakes_in(TABLE, &[9301, 9302]), 3);
        assert_eq!(handshakes_in(TABLE, &[9400]), 0);
        assert_eq!(handshakes_in("", &[9301]), 0);
    }

    /// Asserts that the count of SYN cookies sent is read from the `TcpExt` lines, and that a
    /// table without them, without a value line, or with a value that is not a number has none.
    #[test]
    fn the_syn_cookies_sent_are_read_from_the_tcpext_lines() {
        assert_eq!(syn_cookies_in(NETSTAT), Some(7));
        assert_eq!(syn_cookies_in("IpExt: A\nIpExt: 1\n"), None);
        assert_eq!(syn_cookies_in("TcpExt: SyncookiesSent\n"), None);
        assert_eq!(syn_cookies_in("TcpExt: Other\nTcpExt: 1\n"), None);
        assert_eq!(syn_cookies_in("TcpExt: SyncookiesSent\nTcpExt: x\n"), None);
    }

    /// Asserts that the count reads both tables, IPv4 and IPv6, and adds them up while the count of
    /// SYN cookies sent is the one the listeners were taken with, and that it is an error (which
    /// the door takes for handshakes in flight, and waits out the grace) when a table is missing,
    /// when `netstat` is missing or has no count of cookies, when that count could not be read when
    /// the listeners were taken, and when a cookie has been sent since: such a handshake has no
    /// entry in the tables. It would catch a count of zero that the door trusts while the kernel
    /// answers handshakes it does not list.
    #[test]
    fn the_count_reads_both_tables_and_fails_once_a_cookie_is_sent() {
        let dir = TestDir::under(std::env::temp_dir(), "door-tables");
        std::fs::write(dir.path().join("tcp"), TABLE).unwrap();
        std::fs::write(dir.path().join("tcp6"), TABLE).unwrap();
        let no_netstat = handshakes_under(dir.path(), &[9301], Some(7)).unwrap_err();
        assert_eq!(no_netstat.kind(), ErrorKind::NotFound);
        std::fs::write(dir.path().join("netstat"), "TcpExt: Other\nTcpExt: 1\n").unwrap();
        let no_count = handshakes_under(dir.path(), &[9301], Some(7)).unwrap_err();
        assert_eq!(no_count.kind(), ErrorKind::InvalidData);
        std::fs::write(dir.path().join("netstat"), NETSTAT).unwrap();
        assert_eq!(handshakes_under(dir.path(), &[9301], Some(7)).unwrap(), 4);
        let sent_since = handshakes_under(dir.path(), &[9301], Some(6)).unwrap_err();
        assert_eq!(
            sent_since.to_string(),
            "SYN cookies have been sent since the listeners were taken, and their handshakes have no entry to count"
        );
        assert!(handshakes_under(dir.path(), &[9301], None).is_err());
        std::fs::remove_file(dir.path().join("tcp6")).unwrap();
        let missing = handshakes_under(dir.path(), &[9301], Some(7)).unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::NotFound);
    }

    /// Asserts that the count reads this network namespace's own tables: on a real loopback
    /// listener whose connection is set up and waits in the accept queue, it is `Ok(0)`, since a
    /// connection whose handshake is over is not in flight, and the listeners' count over their
    /// own ports is the same. It would catch a door that looks for the tables where they are not
    /// (it would then wait out the grace at every stop) and a count that takes a set-up connection
    /// for a handshake.
    #[tokio::test]
    async fn the_count_reads_this_namespaces_tables_and_skips_a_queued_connection() {
        let listeners = Listeners::new(vec![bind("127.0.0.1:0".parse().unwrap()).unwrap()]);
        let addr = listeners.local_addrs().unwrap()[0];
        let client = TcpStream::connect(addr).unwrap();
        assert_eq!(handshakes(&[addr.port()], syn_cookies()).unwrap(), 0);
        assert_eq!(listeners.handshakes().unwrap(), 0);
        drop(client);
    }

    /// Asserts that connections set up on two listeners of one front, waiting in their accept
    /// queues when the drain begins, are all served, each its whole answer with `Connection: close`,
    /// before the sockets close, and that the drain then ends with nothing cut. It would catch an end of the settle that takes from one socket of a front and closes
    /// the other on its queue.
    #[tokio::test]
    async fn connections_queued_on_two_listeners_are_all_served() {
        let mut sockets = vec![bind("127.0.0.1:0".parse().unwrap()).unwrap()];
        // A second family where the host has one; the two sockets of one family test the same.
        let second = if std::net::TcpListener::bind("[::1]:0").is_ok() {
            "[::1]:0"
        } else {
            "127.0.0.1:0"
        };
        sockets.push(bind(second.parse().unwrap()).unwrap());
        let addrs: Vec<_> = sockets
            .iter()
            .map(|socket| socket.local_addr().unwrap())
            .collect();
        let clients: Vec<_> = addrs
            .iter()
            .flat_map(|addr| [*addr, *addr])
            .map(|addr| {
                std::thread::spawn(move || {
                    let mut stream = TcpStream::connect(addr).expect("the connect succeeds");
                    // A bound on a server that never answers, so that the test fails, not hangs.
                    stream.set_read_timeout(Some(SAFETY)).unwrap();
                    stream.write_all(HEALTHZ.as_bytes()).unwrap();
                    let mut answer = String::new();
                    stream.read_to_string(&mut answer).map(|_| answer)
                })
            })
            .collect();
        let rig = Rig::with(1 << 20, short_settle).await;
        rig.server.begin_drain();
        let server: Arc<Server> = Arc::clone(&rig.server);
        let served = tokio::spawn(async move { server.serve_all(sockets).await });
        for client in clients {
            let answer = tokio::task::spawn_blocking(move || client.join().unwrap())
                .await
                .unwrap()
                .expect("an answer, not a reset");
            assert_served(&answer);
        }
        assert_eq!(served.await.unwrap(), Drained { abandoned: 0 });
    }
}
