//! What the client has done, counted: the progress events tests wait for (no test waits on time,
//! section 14) and the numbers an operator reads.

/// Counts since the client started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Connections the connector made.
    pub connects: u64,
    /// Connections the connector failed to make.
    pub connect_failures: u64,
    /// Connections that ended (the server closed it, a frame was not one, or a request was
    /// refused for a handle or holder the server no longer knows).
    pub disconnects: u64,
    /// Frames that were not frames, or replies that do not answer their request: each ends the
    /// connection.
    pub protocol_errors: u64,
    /// Handshakes the server accepted.
    pub handshakes: u64,
    /// Handshakes answered under another epoch than the one before (a new server).
    pub epoch_changes: u64,
    /// `open-pack` requests sent, re-opens included.
    pub opens_sent: u64,
    /// Packs opened for the first time.
    pub packs_opened: u64,
    /// Packs re-opened after a reconnect (their view swapped for the new one).
    pub reopens: u64,
    /// Opens answered from the table of open packs, with no request.
    pub opens_shared: u64,
    /// `wait` requests sent, re-issues included.
    pub waits_sent: u64,
    /// `wait` requests sent again after a reconnect.
    pub waits_reissued: u64,
    /// Parts of reads that joined an outstanding `wait`.
    pub wait_parts_joined: u64,
    /// Parts of reads queued behind an outstanding `wait` of their window.
    pub wait_parts_queued: u64,
    /// Reads and parts found ready in the state page, with no request.
    pub waits_ready_local: u64,
    /// `waited` replies that ended a window's wait, counted once the reads it answers are
    /// answered.
    pub waits_answered: u64,
    /// Windows with a `wait` outstanding now, across every pack.
    pub windows_outstanding: u64,
    /// Replies stamped with an epoch other than the connection's, dropped.
    pub stale_replies: u64,
    /// Replies to no request in flight on the connection, dropped.
    pub unknown_replies: u64,
    /// `ensure` requests sent.
    pub ensures_sent: u64,
    /// `ensure` requests dropped: no connection, or the pack not open on it.
    pub ensures_dropped: u64,
}
