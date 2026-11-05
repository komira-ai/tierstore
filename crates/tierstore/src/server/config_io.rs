//! The keys of `[server]` that tune the sockets, the closing door, the streamed bodies and the
//! chunks of the tiers, and that no other module of the loader owns: how long a client has to send
//! the head of a request (`header_timeout`) or to finish the TLS handshake of the OCI front
//! (`oci_handshake_timeout`), the listen backlog and the pauses of the accept loops and of the
//! closing door, the pieces a streamed put holds, the S3 multipart sizes and queue depth, and the
//! sizes in which a `dir` tier copies and reads.
//!
//! Each has the value that was a constant of the code before it was a key as its default, a range
//! that a value outside is refused with (naming the key), and, where two keys depend on each
//! other, a check of the pair. They are in `[server]`, a table that the release before this one
//! reads and warns of the keys it does not know (see `config_unknown`), so a file with these keys
//! still starts there.

use std::time::Duration;

use crate::dir::{COPY_CHUNK, READ_CHUNK};
use crate::s3::CHUNKS_WAITING;

use super::config::{ConfigError, ServerTable, invalid, within};
use super::config_tuning::{S3Client, span};
use super::config_units::Size;
use super::handover::PIECES_QUEUED;
use super::listen::ListenTuning;
use super::{OCI_HANDSHAKE, ServerConfig};

/// The settings of the S3 front that `[server]` gives besides its limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoTuning {
    /// `server.header_timeout`: how long a client has to send the head of a request, and so how
    /// long a connection may wait, idle, for its next request.
    pub header_timeout: Duration,
    /// `server.listen_backlog`, `server.accept_pause`, `server.door_grace` and
    /// `server.door_look`.
    pub listen: ListenTuning,
    /// `server.put_pieces_queued`: the pieces of a streamed put's body that wait for the backend.
    pub put_pieces_queued: usize,
    /// `server.oci_handshake_timeout`: how long a client has to finish the TLS handshake with
    /// the OCI front.
    pub oci_handshake_timeout: Duration,
}

impl Default for IoTuning {
    fn default() -> Self {
        Self {
            header_timeout: Duration::from_secs(60),
            listen: ListenTuning::default(),
            put_pieces_queued: PIECES_QUEUED,
            oci_handshake_timeout: OCI_HANDSHAKE,
        }
    }
}

impl IoTuning {
    /// Sets the keys that are the server's own on `server`.
    pub(super) fn apply(&self, server: &mut ServerConfig) {
        server.header_timeout = self.header_timeout;
        server.listen = self.listen;
        server.put_pieces_queued = self.put_pieces_queued;
        server.oci_handshake_timeout = self.oci_handshake_timeout;
    }
}

const MS: Duration = Duration::from_millis(1);
const HOUR: Duration = Duration::from_secs(3600);
/// The most bytes of a chunk or a part buffer a key may ask for: 1 GiB.
const MOST_BUFFER: u64 = 1 << 30;

/// The size of `server.key`, or `default`, from `low` to `high`, both included.
fn bytes(
    key: &str,
    value: Option<Size>,
    default: u64,
    limits: (u64, u64),
) -> Result<u64, ConfigError> {
    within(
        "server",
        key,
        value.map_or(default, |size| size.0),
        limits.0,
        limits.1,
    )
}

/// The count of `server.key`, or `default`, from 1 to `high`.
fn count(key: &str, value: Option<u32>, default: usize, high: u32) -> Result<usize, ConfigError> {
    let default = u32::try_from(default).unwrap_or(u32::MAX);
    let got = within("server", key, value.unwrap_or(default), 1, high)?;
    Ok(got as usize)
}

/// The server's settings of `[server]` that this module owns, or the defaults of those it leaves
/// out, and puts the ones that are a tier's on `client`.
pub(super) fn io_tuning(
    server: &ServerTable,
    client: &mut S3Client,
) -> Result<IoTuning, ConfigError> {
    let old = IoTuning::default();
    let header_timeout = span(
        ("server", "header_timeout"),
        server.header_timeout,
        old.header_timeout,
        (MS, HOUR),
    )?;
    let backlog = within(
        "server",
        "listen_backlog",
        server.listen_backlog.unwrap_or(1024),
        1,
        65_535,
    )?;
    let accept_pause = span(
        ("server", "accept_pause"),
        server.accept_pause,
        old.listen.accept_pause,
        (MS, Duration::from_secs(60)),
    )?;
    let door_grace = span(
        ("server", "door_grace"),
        server.door_grace,
        old.listen.door_grace,
        (MS, Duration::from_secs(60)),
    )?;
    let door_look = span(
        ("server", "door_look"),
        server.door_look,
        // Left out, the look is the default or the grace if that is shorter, so that a file that
        // shortens the grace alone loads.
        old.listen.door_look.min(door_grace),
        (MS, Duration::from_secs(10)),
    )?;
    if door_look > door_grace {
        let why = "it must not be longer than server.door_grace: the door would close at the grace before it looked twice";
        return Err(invalid("server.door_look", why));
    }
    let oci_handshake_timeout = span(
        ("server", "oci_handshake_timeout"),
        server.oci_handshake_timeout,
        old.oci_handshake_timeout,
        (MS, HOUR),
    )?;
    let copy = bytes(
        "dir_copy_chunk",
        server.dir_copy_chunk,
        COPY_CHUNK as u64,
        (1, MOST_BUFFER),
    )?;
    let read = bytes(
        "dir_read_chunk",
        server.dir_read_chunk,
        READ_CHUNK,
        (1, MOST_BUFFER),
    )?;
    client.chunks_waiting = count(
        "s3_chunks_waiting",
        server.s3_chunks_waiting,
        CHUNKS_WAITING,
        1024,
    )?;
    client.dir_copy_chunk = usize::try_from(copy).unwrap_or(usize::MAX);
    client.dir_read_chunk = read;
    Ok(IoTuning {
        header_timeout,
        listen: ListenTuning {
            backlog,
            accept_pause,
            door_grace,
            door_look,
        },
        put_pieces_queued: count(
            "put_pieces_queued",
            server.put_pieces_queued,
            old.put_pieces_queued,
            4096,
        )?,
        oci_handshake_timeout,
    })
}

#[cfg(test)]
#[path = "config_io_tests.rs"]
mod tests;
