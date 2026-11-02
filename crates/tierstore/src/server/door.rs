//! The end of a front's settle: how it stops taking connections without resetting one whose
//! client saw its connect succeed.
//!
//! The kernel completes a TCP handshake on behalf of a listening socket before the front accepts
//! the connection, and when the socket is closed it resets every connection it still holds: those
//! whose handshake is over and that wait in the accept queue, and those whose handshake is in
//! flight (the SYN answered, the client's ACK not yet come), whose client then reads a reset on a
//! connection it saw open. Whoever stops the instance moves new connections off it first (the
//! update hook points the redirect elsewhere before it stops a slot, in both modes), so by the
//! end of the settle the kernel holds at most the few that came just before; [`Closing`] takes
//! them rather than close the sockets on them:
//!
//! 1. Every connection the kernel holds is taken and served as any other: those in the accept
//!    queue, and those whose handshake was in flight, once it ends ([`Incoming::handshakes`]).
//! 2. The sockets are closed once two looks [`LOOK`] apart have found no handshake in flight and
//!    the accept queue empty, or once [`GRACE`] has passed.
//!
//! The looks can end the wait early only when the count sees every handshake. A handshake the
//! kernel answered with a SYN cookie (it does so when the listen queue is full) has no entry to
//! count, and the kernel keeps no record of which listener a cookie was for or when, so once this
//! network namespace has sent any SYN cookie, and on a system whose handshakes cannot be counted
//! at all (any but Linux), the count is an error and the door waits out [`GRACE`].
//!
//! The drain closes the idle connections when the door begins (see
//! [`time_to_close`](super::drain::time_to_close)), so that a client waiting for the end of its
//! stream frees what it holds (the OCI front's slots) while the door is open. Each connection the
//! door takes is a new one, whose first request the drain waits for, for [`GRACE`] too.
//!
//! What this does not close: a handshake still in flight when [`GRACE`] has passed (its client
//! lost the server's SYN-ACK and its retransmits), and a connection that begins as the sockets
//! close, which only a port that still sends new connections to the instance can bring. The
//! client of either is reset.

use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::task::Poll;
use std::time::Duration;

use tokio::time::Instant;

use super::Incoming;

/// How long a new connection has, from the end of the settle, to finish its handshake and to send
/// its first request: the first retransmit of the server's SYN-ACK (one second), for a handshake
/// whose last segment was lost, and some round trips.
pub(super) const GRACE: Duration = Duration::from_secs(3);

/// How far apart the two looks are that must both find nothing in flight before the door closes:
/// far longer than the moment between the end of a handshake and its connection's place in the
/// accept queue, and a round trip on a local network.
const LOOK: Duration = Duration::from_millis(10);

/// The door of a front, closing: its listening sockets, from the end of the settle.
pub(super) struct Closing<I> {
    incoming: I,
    /// When the door closes on the handshakes still in flight.
    deadline: Instant,
    /// The looks in a row that found nothing in flight and nothing queued.
    quiet: u8,
    /// Whether a failure to count the handshakes has been said on stderr.
    told: bool,
}

impl<I: Incoming> Closing<I> {
    /// The door of `incoming`, from the end of its settle.
    pub(super) fn begin(incoming: I) -> Self {
        Self {
            incoming,
            deadline: Instant::now() + GRACE,
            quiet: 0,
            told: false,
        }
    }

    /// The next connection that the kernel holds for the front, or an accept that failed; `None`
    /// once two looks [`LOOK`] apart have found no handshake in flight and the accept queue empty,
    /// or once [`GRACE`] has passed.
    ///
    /// Each look counts the handshakes before it looks at the queue: a handshake that ends after
    /// the count is in the queue at the next look.
    pub(super) async fn next(&mut self) -> Option<io::Result<(I::Stream, SocketAddr)>> {
        loop {
            if Instant::now() >= self.deadline {
                return None;
            }
            let in_flight = self.in_flight();
            let queued = poll_fn(|cx| Poll::Ready(self.incoming.poll_incoming(cx))).await;
            if let Poll::Ready(accepted) = queued {
                self.quiet = 0;
                return Some(accepted);
            }
            if in_flight {
                self.quiet = 0;
            } else {
                self.quiet += 1;
                if self.quiet == 2 {
                    return None;
                }
            }
            tokio::select! {
                accepted = poll_fn(|cx| self.incoming.poll_incoming(cx)) => {
                    self.quiet = 0;
                    return Some(accepted);
                }
                () = tokio::time::sleep(LOOK) => {}
                () = tokio::time::sleep_until(self.deadline) => return None,
            }
        }
    }

    /// Resolves once [`GRACE`] has passed: a front that waits for something else before it takes
    /// the next connection (the OCI front, for a free slot) waits no longer.
    pub(super) async fn out_of_time(&self) {
        tokio::time::sleep_until(self.deadline).await;
    }

    /// Whether a handshake is in flight. One that cannot be counted is taken to be, so that the
    /// door waits out [`GRACE`]; the failure is said on stderr once.
    fn in_flight(&mut self) -> bool {
        match self.incoming.handshakes() {
            Ok(count) => count > 0,
            Err(error) => {
                if !self.told {
                    self.told = true;
                    eprintln!(
                        "tierstore: the end of the settle: cannot count the handshakes in flight, so the listener waits {GRACE:?}: {error}"
                    );
                }
                true
            }
        }
    }
}

/// Where the tables of this network namespace's TCP sockets are.
#[cfg(target_os = "linux")]
const TABLES: &str = "/proc/self/net";

/// The handshakes in flight on the listening sockets of `ports`, in this network namespace.
///
/// # Errors
/// A table cannot be read, or the namespace has sent a SYN cookie (its handshake has no entry).
#[cfg(target_os = "linux")]
pub(super) fn handshakes(ports: &[u16]) -> io::Result<usize> {
    handshakes_under(Path::new(TABLES), ports)
}

/// No table of handshakes outside Linux: the count is an error, and the door waits out [`GRACE`].
///
/// # Errors
/// Always.
#[cfg(not(target_os = "linux"))]
pub(super) fn handshakes(_ports: &[u16]) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this system's handshakes in flight cannot be counted",
    ))
}

/// The handshakes in flight on the listening sockets of `ports`: the entries in state `SYN_RECV`
/// of the tables `tcp` and `tcp6` in `dir` whose local port is one of them, if `dir`'s `netstat`
/// says that no SYN cookie has been sent.
///
/// # Errors
/// A table cannot be read, or a SYN cookie has been sent: its handshake, if it is still under
/// way, has no entry, and nothing tells which listener it was for or when it was sent.
#[cfg(target_os = "linux")]
fn handshakes_under(dir: &Path, ports: &[u16]) -> io::Result<usize> {
    let sent = syn_cookies_under(dir)?;
    if sent != 0 {
        return Err(io::Error::other(format!(
            "this network namespace has sent {sent} SYN cookies, and their handshakes have no entry to count"
        )));
    }
    let mut count = 0;
    for table in ["tcp", "tcp6"] {
        count += handshakes_in(&std::fs::read_to_string(dir.join(table))?, ports);
    }
    Ok(count)
}

/// The SYN cookies sent, from the `netstat` table in `dir`.
///
/// # Errors
/// The table cannot be read, or has no count of them.
#[cfg(target_os = "linux")]
fn syn_cookies_under(dir: &Path) -> io::Result<u64> {
    let table = std::fs::read_to_string(dir.join("netstat"))?;
    syn_cookies_in(&table).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "no count of the SYN cookies sent (TcpExt SyncookiesSent)",
        )
    })
}

/// `SyncookiesSent` of a table in the format of `/proc/net/netstat`: a line of names and a line of
/// values, each beginning with `TcpExt:`.
#[cfg(target_os = "linux")]
fn syn_cookies_in(table: &str) -> Option<u64> {
    let mut lines = table.lines().filter(|line| line.starts_with("TcpExt:"));
    let (names, values) = (lines.next()?, lines.next()?);
    let at = names
        .split_whitespace()
        .position(|name| name == "SyncookiesSent")?;
    values.split_whitespace().nth(at)?.parse().ok()
}

/// The entries of a table in the format of `/proc/net/tcp` that are in state `SYN_RECV` (`03`)
/// with a local port of `ports`. The first line is the heading; each other has the slot, the local
/// address (`address:port`, both hex), the remote one, then the state (hex).
#[cfg(target_os = "linux")]
fn handshakes_in(table: &str, ports: &[u16]) -> usize {
    table
        .lines()
        .skip(1)
        .filter(|line| {
            let mut fields = line.split_whitespace().skip(1);
            let port = fields
                .next()
                .and_then(|local| local.rsplit_once(':'))
                .and_then(|(_, port)| u16::from_str_radix(port, 16).ok());
            let state = fields.nth(1);
            state == Some("03") && port.is_some_and(|port| ports.contains(&port))
        })
        .count()
}

#[cfg(test)]
#[path = "door_tests.rs"]
mod tests;
