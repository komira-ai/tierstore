//! What the server has done, counted: the progress events tests wait for (no test waits on time,
//! section 14) and the numbers an operator reads.

/// Counts since the server started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Connections accepted or handed to [`crate::MountServer::serve_connection`].
    pub connections: u64,
    /// Connections that ended (the peer closed, or a protocol error ended them).
    pub connections_closed: u64,
    /// Connections ended because a frame was not one, or came with the wrong descriptors.
    pub protocol_errors: u64,
    /// Handshakes accepted, a reconnect's included.
    pub handshakes: u64,
    /// Holders registered (a holder that reconnects is registered once).
    pub holders_registered: u64,
    /// Lifelines that hung up, a released holder's included.
    pub lifeline_hangups: u64,
    /// Holders ended by their lifeline's hangup.
    pub holders_hung_up: u64,
    /// Holders ended by `release`.
    pub holders_released: u64,
    /// Requests answered (`HandshakeOk`, `PackOpened`, `Waited`, `Ensured`, `Released` or
    /// `Refused`).
    pub replies: u64,
    /// Replies that could not be written because the mount stopped reading; the connection's
    /// writer stops at the first.
    pub replies_lost: u64,
    /// Requests answered `Refused`.
    pub refusals: u64,
}

impl Stats {
    /// Holders registered and not yet ended.
    #[must_use]
    pub fn holders_live(&self) -> u64 {
        self.holders_registered - self.holders_hung_up - self.holders_released
    }
}
