//! The end of the settle (`door`): first over pipes on the paused clock, where a test is the
//! kernel and decides when a handshake ends; then over loopback sockets, where the kernel's own
//! handshakes are held back by a socket filter, and the tables of `/proc` are read.

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
/// kernel reset a connection whose client saw its connect succeed. A new connect then is refused,
/// and the drain ends with nothing cut. It would catch an end of the settle that closes the socket
/// at once, which is what reset the clients of a plain restart (T1, run (c)).
#[tokio::test(start_paused = true)]
async fn a_handshake_in_flight_when_the_settle_ends_is_taken_and_served() {
    let serving = Serving::start(short_settle).await;
    let handshake = serving.handshake();
    serving.rig.server.begin_drain();
    serving.settle_over().await;
    assert!(serving.connect().is_none(), "a new handshake is not taken");

    let mut client = handshake
        .complete()
        .expect("the door is still open: the connection is not reset");
    client.write_all(HEALTHZ.as_bytes()).await.unwrap();
    assert_served(&read_answer(&mut client).await);
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

/// Asserts that a door whose new handshakes cannot be stopped still takes the connection whose
/// handshake was in flight and serves it, then closes once nothing is waiting, without waiting out
/// [`GRACE`] (on the paused clock, it closes before it), and the drain ends. It would catch a
/// failed filter that drops the connections the kernel holds, or that holds the drain.
#[tokio::test(start_paused = true)]
async fn a_door_whose_handshakes_cannot_be_stopped_closes_once_nothing_waits() {
    let serving = Serving::start_on(0, Kernel::stop_failing(), short_settle).await;
    let handshake = serving.handshake();
    serving.rig.server.begin_drain();
    serving.settle_over().await;
    let mut client = handshake.complete().expect("taken, not reset");
    client.write_all(HEALTHZ.as_bytes()).await.unwrap();
    assert_served(&read_answer(&mut client).await);
    let mut closed = Box::pin(serving.refusing());
    tokio::select! {
        () = &mut closed => {}
        () = tokio::time::sleep(GRACE - LOOK) => panic!("the door waited out the grace"),
    }
    drop(closed);
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

/// The kernel's side, on Linux: the tables of `/proc`, and loopback sockets whose handshakes a
/// socket filter holds back.
#[cfg(target_os = "linux")]
mod kernel {
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::net::{SocketAddr, TcpStream};
    use std::sync::Arc;
    use std::time::Duration;

    use tierstore_testkit::dir::TestDir;
    use tokio::net::{TcpListener, TcpSocket};

    use super::super::{handshakes, handshakes_in, handshakes_under, stop_handshakes};
    use super::{HEALTHZ, assert_served, short_settle};
    use crate::server::Drained;
    use crate::server::testing::Rig;

    /// The longest a test waits for the kernel to do what it must. It only stops a test that
    /// would hang; no test passes by the end of it.
    const SAFETY: Duration = Duration::from_secs(120);

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

    /// A socket filter, for the tests only, that drops every segment without SYN: the client's last
    /// segment of the handshake, and its request, never reach the listening socket, whose handshake
    /// stays in flight until another filter lets them through.
    const HOLD_ACK: [socket2::SockFilter; 4] = [
        socket2::SockFilter::new(0x30, 0, 0, 13),
        socket2::SockFilter::new(0x45, 0, 1, 0x02),
        socket2::SockFilter::new(0x06, 0, 0, u32::MAX),
        socket2::SockFilter::new(0x06, 0, 0, 0),
    ];

    /// A listening socket on a free loopback port of `family`'s loopback address.
    async fn loopback(address: &str) -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind(address).await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    /// Asserts, on a loopback socket, the whole end of the settle: a connection whose handshake the
    /// kernel holds in flight (its last segment held back by a filter) is counted from the table of
    /// `/proc`, and once the settle ends the door's filter lets its handshake end, the server takes
    /// it and serves its request with `Connection: close`. It would catch a filter that drops more
    /// than SYN, a count that does not see the kernel's handshakes, and a door that closes the socket
    /// on them: the client, which saw its connect succeed, would read a reset.
    #[tokio::test]
    async fn a_handshake_the_kernel_holds_when_the_settle_ends_is_served() {
        let (listener, addr) = loopback("127.0.0.1:0").await;
        socket2::SockRef::from(&listener)
            .attach_filter(&HOLD_ACK)
            .unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).expect("the connect succeeds");
            stream.write_all(HEALTHZ.as_bytes()).unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).map(|_| answer)
        });
        let held = async {
            while handshakes(&[addr.port()]).unwrap() == 0 {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(SAFETY, held)
            .await
            .expect("the kernel holds the handshake");

        let rig = Rig::with(1 << 20, short_settle).await;
        let server = Arc::clone(&rig.server);
        let served = tokio::spawn(async move { server.serve_all(vec![listener]).await });
        rig.server.begin_drain();
        let answer = tokio::task::spawn_blocking(move || client.join().unwrap())
            .await
            .unwrap()
            .expect("an answer, not a reset");
        assert_served(&answer);
        assert_eq!(served.await.unwrap(), Drained { abandoned: 0 });
        let refused = TcpStream::connect(addr).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::ConnectionRefused);
    }

    /// Whether the table of `/proc` shows a socket of this namespace that connects to `port` (state
    /// `SYN_SENT`, `02`) and has sent its SYN again: its first SYN went unanswered.
    fn syn_sent_again(port: u16) -> bool {
        ["tcp", "tcp6"].iter().any(|table| {
            let text = std::fs::read_to_string(format!("/proc/self/net/{table}")).unwrap();
            text.lines().skip(1).any(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let remote = fields[2].rsplit_once(':').map(|(_, port)| port);
                remote == Some(&format!("{port:04X}"))
                    && fields[3] == "02"
                    && fields[6] != "00000000"
            })
        })
    }

    /// Asserts, on a loopback socket of each family, that the door's filter drops a new SYN: the
    /// client's connect is not answered (the kernel sends its SYN again), and once the socket is
    /// closed the connect is refused. It would catch a filter that lets a SYN through, whose
    /// handshake the kernel would end and then reset when the socket closes.
    #[tokio::test]
    async fn the_doors_filter_drops_a_new_syn_and_the_connect_is_refused() {
        for address in ["127.0.0.1:0", "[::1]:0"] {
            let (listener, addr) = loopback(address).await;
            stop_handshakes(&listener).unwrap();
            let client = std::thread::spawn(move || {
                TcpStream::connect_timeout(&addr, Duration::from_secs(120)).map(drop)
            });
            let unanswered = async {
                while !syn_sent_again(addr.port()) {
                    tokio::task::yield_now().await;
                }
            };
            tokio::time::timeout(SAFETY, unanswered)
                .await
                .expect("the SYN is sent again, unanswered");
            // The port stays bound, with no listening socket, so that no other test takes it before
            // the client sends its SYN again: that SYN finds no listener and is refused.
            drop(listener);
            let held = if addr.is_ipv4() {
                TcpSocket::new_v4()
            } else {
                TcpSocket::new_v6()
            }
            .unwrap();
            held.bind(addr).unwrap();
            let refused = tokio::task::spawn_blocking(move || client.join().unwrap())
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(refused.kind(), ErrorKind::ConnectionRefused, "{address}");
            drop(held);
        }
    }
}
