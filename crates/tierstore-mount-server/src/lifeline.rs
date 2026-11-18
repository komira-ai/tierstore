//! The lifeline (5.1): the server's end of a socketpair whose other end the mount holds for its
//! whole life. Only `POLLHUP` on it (the mount exited or closed its end) ends the holder.
//!
//! Each lifeline is watched by its own thread, blocked in `poll(2)` with no event asked for: the
//! kernel then returns only for `POLLHUP`, `POLLERR` or `POLLNVAL`, never for bytes the mount wrote
//! or for a writing side it shut down (`POLLRDHUP`). So a mount that writes to its end, or shuts
//! its writing side, stays a live holder, and the watcher never spins. The thread is not on the
//! runtime's blocking pool, so the lifelines of many mounts never starve the entry store's calls.

use std::io;
use std::os::fd::OwnedFd;

use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno;
use rustix::pipe::{PipeFlags, pipe_with};
use tokio::sync::oneshot;

/// A lifeline being watched: the receiver gets `Ok(())` when the lifeline hangs up, or the error
/// that stopped the watch. Dropping `stop` ends the watch (its thread exits and closes the
/// lifeline) without a hangup: the server does that for a handshake it refuses after the watch
/// began.
pub(crate) struct Watch {
    pub(crate) hangup: oneshot::Receiver<io::Result<()>>,
    pub(crate) stop: OwnedFd,
}

/// Watches `fd` on a thread of its own.
pub(crate) fn watch(fd: OwnedFd, name: String) -> io::Result<Watch> {
    let (tx, rx) = oneshot::channel();
    let (stopped, stop) = pipe_with(PipeFlags::CLOEXEC)?;
    std::thread::Builder::new().name(name).spawn(move || {
        // Nobody waits any more when the server dropped the holder's watcher.
        let _ = match wait_hangup(&fd, &stopped) {
            Ok(false) => return,
            done => tx.send(done.map(drop)),
        };
    })?;
    Ok(Watch { hangup: rx, stop })
}

/// Blocks until `fd` hangs up (`true`) or the write end of `stopped` is closed (`false`). With no
/// event asked for, `poll` returns only for `POLLHUP`, `POLLERR` or `POLLNVAL`: the descriptors are
/// the server's own, so never `POLLNVAL`, and a unix stream socket reports `POLLERR` only as its
/// peer closes (`ECONNRESET`), with `POLLHUP`. A signal that interrupts the wait is not a hangup.
fn wait_hangup(fd: &OwnedFd, stopped: &OwnedFd) -> io::Result<bool> {
    loop {
        let mut fds = [
            PollFd::new(fd, PollFlags::empty()),
            PollFd::new(stopped, PollFlags::empty()),
        ];
        match poll(&mut fds, None) {
            Err(Errno::INTR) => {}
            done => {
                return done
                    .map(|_| !fds[0].revents().is_empty())
                    .map_err(io::Error::from);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    #[test]
    fn a_closed_peer_is_a_hangup() {
        let (mine, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let watch = watch(mine, "lifeline-test".into()).unwrap();
        drop(theirs);
        assert!(watch.hangup.blocking_recv().unwrap().is_ok());
        drop(watch.stop);
    }

    #[test]
    fn a_stopped_watch_ends_without_a_hangup() {
        let (mine, _theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let watch = watch(mine, "lifeline-test".into()).unwrap();
        drop(watch.stop);
        // The thread ends without sending: the receiver sees the sender dropped.
        assert!(watch.hangup.blocking_recv().is_err());
    }
}
