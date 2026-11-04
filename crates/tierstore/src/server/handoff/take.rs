//! The new instance's side: asking a running instance for its listening sockets.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Duration;

use rustix::io::{FdFlags, fcntl_setfd};
use tokio::net::{TcpListener, UnixStream};

use super::wire::{self, Frame};
use super::{Front, Handoff, HandoffError, line};

/// The addresses a new instance's settings name, front by front, in the order of its interfaces.
pub(crate) struct Want<'a> {
    pub(crate) s3: &'a [SocketAddr],
    pub(crate) oci: &'a [SocketAddr],
}

/// The sockets taken from a running instance, in the order it serves them.
pub(crate) struct Inherited {
    pub(crate) s3: Vec<TcpListener>,
    pub(crate) oci: Vec<TcpListener>,
}

/// Takes the listening sockets of the instance that answers at `handoff.socket`.
///
/// `Ok(None)` when no instance answers there: there is no file, or a file that no one listens on
/// (an instance that was killed leaves it). The caller then binds its own sockets.
///
/// # Errors
/// The running instance refused (another taker is at it), listens on other addresses than `want`
/// names, did not end the hand-off within `handoff.wait`, or did not keep to the protocol.
pub(crate) async fn take(
    handoff: &Handoff,
    want: &Want<'_>,
) -> Result<Option<Inherited>, HandoffError> {
    match tokio::time::timeout(handoff.wait, exchange(&handoff.socket, want)).await {
        Ok(taken) => taken,
        Err(_) => Err(HandoffError::Timeout(handoff.socket.clone())),
    }
}

/// Whether a socket at `offered` serves what `wanted` asks for: the same address, or, for a port
/// of 0 (the system picks one), the same host.
fn covers(wanted: SocketAddr, offered: SocketAddr) -> bool {
    wanted == offered || (wanted.port() == 0 && wanted.ip() == offered.ip())
}

/// Whether `offered` are the sockets `wanted` asks for, in the same order.
fn all_cover(wanted: &[SocketAddr], offered: &[SocketAddr]) -> bool {
    wanted.len() == offered.len() && wanted.iter().zip(offered).all(|(w, o)| covers(*w, *o))
}

/// The whole exchange with the running instance.
async fn exchange(path: &Path, want: &Want<'_>) -> Result<Option<Inherited>, HandoffError> {
    let stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(why)
            if matches!(
                why.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(why) => return Err(HandoffError::io("connecting to the control socket")(why)),
    };
    let io = HandoffError::io;
    wire::send(&stream, &["take 1".to_owned()], &[])
        .await
        .map_err(io("asking for the sockets"))?;
    let offer = wire::recv(&stream).await.map_err(io("reading the offer"))?;
    let Frame { lines, fds } = offer;
    let head = lines.first().map_or("", String::as_str);
    let rest = lines.get(1..).unwrap_or_default();
    if head == "refused" {
        return Err(HandoffError::Refused(rest.join(" ")));
    }
    if head != "offer" {
        return Err(HandoffError::Protocol("a frame that is not an offer"));
    }
    if rest.len() != fds.len() {
        return Err(HandoffError::Protocol(
            "another number of sockets than lines",
        ));
    }
    let mut offered = Vec::new();
    for entry in rest {
        let parsed = entry
            .split_once(' ')
            .and_then(|(name, addr)| Some((Front::named(name)?, addr.parse::<SocketAddr>().ok()?)));
        offered.push(parsed.ok_or(HandoffError::Protocol(
            "a socket line that is not <front> <address>",
        ))?);
    }
    let of = |front: Front| -> Vec<SocketAddr> {
        offered
            .iter()
            .filter(|(f, _)| *f == front)
            .map(|(_, a)| *a)
            .collect()
    };
    if !(all_cover(want.s3, &of(Front::S3)) && all_cover(want.oci, &of(Front::Oci))) {
        wire::send(&stream, &["nak".to_owned()], &[])
            .await
            .map_err(io("refusing the offer"))?;
        let named = |front: Front, addrs: &[SocketAddr]| -> Vec<String> {
            addrs.iter().map(|addr| line(front, *addr)).collect()
        };
        return Err(HandoffError::Mismatch {
            offered: rest.to_vec(),
            wanted: [named(Front::S3, want.s3), named(Front::Oci, want.oci)].concat(),
        });
    }
    let mut inherited = Inherited {
        s3: Vec::new(),
        oci: Vec::new(),
    };
    for ((front, _), fd) in offered.iter().zip(fds) {
        let listener = listener_of(fd).map_err(io("taking over a socket"))?;
        match front {
            Front::S3 => inherited.s3.push(listener),
            Front::Oci => inherited.oci.push(listener),
        }
    }
    wire::send(&stream, &["ack".to_owned()], &[])
        .await
        .map_err(io("accepting the offer"))?;
    let done = wire::recv(&stream)
        .await
        .map_err(io("waiting for the end of the hand-off"))?;
    if done.lines != ["done"] {
        return Err(HandoffError::Protocol(
            "something other than done after ack",
        ));
    }
    Ok(Some(inherited))
}

/// The listener of a descriptor that came on the control socket. The kernel hands it over open on
/// exec on macOS, and on Linux unless asked, so close-on-exec is set here, as `listen::bind` does.
fn listener_of(fd: OwnedFd) -> std::io::Result<TcpListener> {
    fcntl_setfd(&fd, FdFlags::CLOEXEC)?;
    let listener = std::net::TcpListener::from(fd);
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

#[cfg(test)]
#[path = "take_tests.rs"]
mod tests;
