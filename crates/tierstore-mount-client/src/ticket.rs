//! [`Ticket`]: how an answer of the client's runtime reaches a reader.
//!
//! The runtimes are separate (5.4): a reader runs on a thread of its own and may block in the
//! kernel (a `pread`, a splice, an image page fault), and the client runs on its own runtime. A
//! request of a reader returns a ticket at once; the client completes it from its runtime with a
//! send that never blocks, so a blocked reader never stalls the completions other readers wait
//! for. The reader receives on its own thread ([`Ticket::wait`]), polls ([`Ticket::try_take`]),
//! or awaits it on a runtime of its own (it is a future).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::channel::oneshot;

use crate::error::ClientError;

/// The sending half: what the client completes.
pub(crate) type Answer<T> = oneshot::Sender<Result<T, ClientError>>;

/// An answer the client gives later.
#[derive(Debug)]
#[must_use = "a ticket is the only way to the answer"]
pub struct Ticket<T> {
    rx: oneshot::Receiver<Result<T, ClientError>>,
}

impl<T> Ticket<T> {
    /// A ticket and the sender that completes it.
    pub(crate) fn new() -> (Answer<T>, Self) {
        let (tx, rx) = oneshot::channel();
        (tx, Self { rx })
    }

    /// A ticket already answered.
    pub(crate) fn ready(answer: Result<T, ClientError>) -> Self {
        let (tx, ticket) = Self::new();
        // The receiver is alive: it is in `ticket`.
        let _ = tx.send(answer);
        ticket
    }

    /// Blocks the calling thread until the answer. Call it from a reader's own thread, never from
    /// a task of an async runtime. [`ClientError::Stopped`] when the client stopped first.
    pub fn wait(self) -> Result<T, ClientError> {
        futures::executor::block_on(self)
    }

    /// The answer if it has come, without waiting; `Some(Err(Stopped))` when the client stopped
    /// without one. After it returns `Some`, the ticket answers [`ClientError::Stopped`].
    pub fn try_take(&mut self) -> Option<Result<T, ClientError>> {
        match self.rx.try_recv() {
            Ok(None) => None,
            Ok(Some(answer)) => Some(answer),
            Err(oneshot::Canceled) => Some(Err(ClientError::Stopped)),
        }
    }
}

impl<T> Future for Ticket<T> {
    type Output = Result<T, ClientError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx)
            .poll(cx)
            .map(|answer| answer.unwrap_or(Err(ClientError::Stopped)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_answers_once_or_says_the_client_stopped() {
        let mut t = Ticket::ready(Ok(3));
        assert_eq!(t.try_take(), Some(Ok(3)));
        assert_eq!(t.try_take(), Some(Err(ClientError::Stopped)));

        let (tx, mut t) = Ticket::<u8>::new();
        assert_eq!(t.try_take(), None);
        drop(tx);
        assert_eq!(t.wait(), Err(ClientError::Stopped));

        let (tx, t) = Ticket::<u8>::new();
        tx.send(Err(ClientError::Released)).unwrap();
        assert_eq!(t.wait(), Err(ClientError::Released));
    }
}
