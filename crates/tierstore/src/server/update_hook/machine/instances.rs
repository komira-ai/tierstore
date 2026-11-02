//! The instances' HTTP answers: a blocking `GET` on `127.0.0.1`, over `std::net`.
//!
//! The probes ask for small, known paths (`/readyz`, `/metricz`) of a server of this very
//! program, so the client is the little that needs: HTTP/1.1, `Connection: close`, the answer
//! read to its end. A chunked answer is refused, not decoded: the servers send a length.
//!
//! A connect is bounded by its own timeout; then the whole request, the write and every read of
//! the answer together, is bounded by one deadline, so that an answer that comes a byte at a
//! time cannot keep a look going past it. Each write and read waits at most what is left.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use super::super::ports::{Instances, PortError, PortKind, Reply};
use super::Fault;

const KIND: PortKind = PortKind::Instances;
/// The most of an answer that is read; a larger one is an error.
const MAX_ANSWER: usize = 1 << 20;
const HEAD_END: &[u8] = b"\r\n\r\n";

/// What tells the time to the deadline of a request.
type Now = Box<dyn Fn() -> Instant + Send + Sync>;

/// A client of the instances' loopback ports.
pub struct LoopbackInstances {
    connect_timeout: Duration,
    read_timeout: Duration,
    now: Now,
}

impl LoopbackInstances {
    /// A connect that takes longer than `connect_timeout` fails, and so does a request whose
    /// write and answer together take longer than `read_timeout`.
    #[must_use]
    pub fn new(connect_timeout: Duration, read_timeout: Duration) -> LoopbackInstances {
        LoopbackInstances {
            connect_timeout,
            read_timeout,
            now: Box::new(Instant::now),
        }
    }

    /// The same, telling the time with `now` instead of the monotonic clock: a test moves it on.
    #[must_use]
    pub fn with_clock(mut self, now: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    /// What is left of the request's time until `deadline`; an error once there is none.
    fn left(&self, deadline: Instant) -> Result<Duration, Fault> {
        deadline
            .checked_duration_since((self.now)())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| Fault(format!("no whole answer within {:?}", self.read_timeout)))
    }

    fn fetch(&self, port: u16, path: &str) -> Result<Reply, Fault> {
        if !path.starts_with('/') || path.bytes().any(|byte| byte <= b' ' || byte == 0x7f) {
            return Err(format!("{path:?} is not a path").into());
        }
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let mut stream = TcpStream::connect_timeout(&addr, self.connect_timeout)?;
        let deadline = (self.now)() + self.read_timeout;
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
        stream.set_write_timeout(Some(self.left(deadline)?))?;
        stream.write_all(request.as_bytes())?;
        let mut raw = Vec::new();
        let mut piece = [0u8; 16 * 1024];
        loop {
            stream.set_read_timeout(Some(self.left(deadline)?))?;
            let read = stream.read(&mut piece)?;
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&piece[..read]);
            if raw.len() > MAX_ANSWER {
                return Err("the answer is larger than 1 MiB".into());
            }
        }
        parse(&raw)
    }
}

/// The status and body of the answer `raw`.
fn parse(raw: &[u8]) -> Result<Reply, Fault> {
    let end = raw
        .windows(HEAD_END.len())
        .position(|window| window == HEAD_END)
        .ok_or("the answer has no end of headers")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let body = &raw[end + HEAD_END.len()..];
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|word| word.parse::<u16>().ok())
        .ok_or_else(|| format!("{status_line:?} is not a status line"))?;
    let mut length = None;
    for line in lines {
        let (name, value) = line.split_once(':').unwrap_or_default();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("the answer is chunked, which this client does not read".into());
        }
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    if length.is_some_and(|length| body.len() < length) {
        return Err("the answer was cut short".into());
    }
    Ok(Reply {
        status,
        body: String::from_utf8_lossy(body).into_owned(),
    })
}

impl Instances for LoopbackInstances {
    fn get(&self, port: u16, path: &str) -> Result<Reply, PortError> {
        self.fetch(port, path).map_err(|fault| {
            PortError::failed(KIND, format!("GET 127.0.0.1:{port}{path}: {fault}"))
        })
    }
}
