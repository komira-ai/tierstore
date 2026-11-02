//! The end of the settle (`door`): first over pipes on the paused clock, where a test is the
//! kernel and decides when a handshake ends; then the tables of `/proc` the door counts the
//! kernel's handshakes from, as text and as this namespace's own.

use std::time::Duration;

use tokio::io::AsyncWriteExt as _;

use super::{GRACE, LOOK};
use crate::server::pipes::{Kernel, Serving, drained, read_answer, status_of, until};
use crate::server::{Drained, ServerConfig};

/// A probe that asks the server to close the connection after its answer.
const HEALTHZ: &str = "GET /healthz HTTP/1.1\r\nhost: tier.test\r\nconnection: close\r\n\r\n";

/// A settle that ends at once: the tests begin the drain and then wait for its end.
fn short_settle(config: &mut ServerConfig) {
    config.drain_lb_wait = Duration::from_millis(5);
}

/// Asserts that `answer` is the 200 of `/healthz`, sent with `Connection: close`, as every answer
/// of the drain is.
fn assert_served(answer: &str) {
    assert_eq!(status_of(answer), 200, "{answer}");
    assert!(answer.contains("connection: close"), "{answer}");
}

/// Asserts that a connection whose handshake is in flight when the settle ends is taken once it
/// ends, and its request served: the door waits for it, rather than close the socket and have the
/// kernel reset a connection whose client saw its connect succeed. The door then closes, a new
/// connect is refused, and the drain ends with nothing cut. It would catch an end of the settle that closes the socket
/// at once, which is what reset the clients of a plain restart (T1, run (c)).
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

/// Asserts that a new connection that sends nothing is given [`GRACE`] from the end of the settle
/// and no more: it is still open a moment before, on the paused clock, and then the server shuts
/// it (its client reads the end of the stream) and the drain ends. It would catch a wait for a
/// first request that holds the drain until the header timeout, or for ever.
#[tokio::test(start_paused = true)]
async fn a_new_connection_that_sends_nothing_is_closed_once_the_grace_is_over() {
    let serving = Serving::start(short_settle).await;
    let mut client = serving.connect().unwrap();
    until(&serving.rig.server, |server| server.open_connections() == 1).await;
    serving.rig.server.begin_drain();
    serving.refusing().await;

    tokio::time::advance(GRACE - LOOK * 3).await;
    assert_eq!(serving.rig.server.open_connections(), 1, "still waited for");
    assert_eq!(
        read_answer(&mut client).await,
        "",
        "shut, with nothing sent"
    );
    drop(client);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a handshake that never ends holds the door open for [`GRACE`] and no more: the
/// socket is still open a moment before, on the paused clock, and is then closed, and the drain
/// ends with nothing cut; the connection, ended too late, is reset. It would catch a door that
/// waits for ever for a client that has gone, or that does not wait at all.
#[tokio::test(start_paused = true)]
async fn a_handshake_that_never_ends_holds_the_door_for_the_grace_only() {
    let serving = Serving::start(short_settle).await;
    let handshake = serving.handshake();
    serving.rig.server.begin_drain();
    serving.settle_over().await;

    tokio::time::advance(GRACE - LOOK * 3).await;
    assert!(serving.door_open(), "the handshake is still waited for");
    serving.refusing().await;
    assert!(handshake.complete().is_none(), "too late: reset");
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a door whose handshakes cannot be counted takes them to be in flight and waits out
/// [`GRACE`] (still open a moment before), then closes, and the drain ends. It would catch a
/// failure to count that is read as "none in flight", which closes the door on a handshake.
#[tokio::test(start_paused = true)]
async fn a_door_that_cannot_count_the_handshakes_waits_out_the_grace() {
    let serving = Serving::start_on(0, Kernel::count_failing(), short_settle).await;
    serving.rig.server.begin_drain();
    serving.settle_over().await;

    tokio::time::advance(GRACE - LOOK * 3).await;
    assert!(
        serving.door_open(),
        "the uncounted handshakes are waited for"
    );
    serving.refusing().await;
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that accepts that keep failing while the door closes (each followed by the accept
/// pause) hold it open no longer than [`GRACE`], and the drain then ends with nothing cut. It would
/// catch a door that looks at its bound only while it waits, which a source that always answers,
/// with an error, would keep open for ever.
#[tokio::test(start_paused = true)]
async fn accepts_that_keep_failing_hold_the_door_no_longer_than_the_grace() {
    let serving = Serving::start_after(usize::MAX, short_settle).await;
    serving.rig.server.begin_drain();
    serving.refusing().await;
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// The kernel's side, on Linux: the tables of `/proc`.
#[cfg(target_os = "linux")]
mod kernel {
    use std::io::ErrorKind;
    use std::net::TcpStream;

    use tierstore_testkit::dir::TestDir;

    use super::super::{handshakes, handshakes_in, handshakes_under};
    use crate::server::Incoming as _;
    use crate::server::listen::{Listeners, bind};

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

    /// Asserts that the count reads both tables, IPv4 and IPv6, and adds them up, and that a table it
    /// cannot read is an error (which the door takes for handshakes in flight), not a count of 0.
    #[test]
    fn the_count_reads_both_tables_and_a_missing_one_is_an_error() {
        let dir = TestDir::under(std::env::temp_dir(), "door-tables");
        std::fs::write(dir.path().join("tcp"), TABLE).unwrap();
        let missing = handshakes_under(dir.path(), &[9301]).unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::NotFound);
        std::fs::write(dir.path().join("tcp6"), TABLE).unwrap();
        assert_eq!(handshakes_under(dir.path(), &[9301]).unwrap(), 4);
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
        assert_eq!(handshakes(&[addr.port()]).unwrap(), 0);
        assert_eq!(listeners.handshakes().unwrap(), 0);
        drop(client);
    }
}
