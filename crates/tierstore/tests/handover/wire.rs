//! A blocking HTTP/1.1 client over one TCP connection, which says precisely how a request failed:
//! the connect was refused, the connection broke before any byte of the answer came (on a new
//! connection, or on one that had carried answers before), the answer was cut, or nothing came
//! in time. That is what tells a gap a restart may have (a refused connect) from what no update
//! may cause (a reset of an established connection, a cut answer).

use std::fmt;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// How long a client waits for a connect or a read before it calls the request timed out. Only
/// a guard against a hang: nothing asserts how long anything takes.
const PATIENCE: Duration = Duration::from_secs(120);

/// How a request failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Broke {
    /// The connect was refused: nothing listened behind the address.
    Refused,
    /// The connect failed otherwise.
    Connect(String),
    /// The connection broke before any byte of the answer came. `fresh`: it was the
    /// connection's first request.
    BeforeAnswer { fresh: bool, what: String },
    /// Part of the answer came, then the connection broke.
    Cut(String),
    /// Nothing came within the guard.
    TimedOut { fresh: bool },
}

impl fmt::Display for Broke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Broke::Refused => f.write_str("connect refused"),
            Broke::Connect(what) => write!(f, "connect failed: {what}"),
            Broke::BeforeAnswer { fresh: true, what } => {
                write!(f, "new connection broke before any answer byte: {what}")
            }
            Broke::BeforeAnswer { fresh: false, what } => {
                write!(
                    f,
                    "kept-alive connection broke before any answer byte: {what}"
                )
            }
            Broke::Cut(what) => write!(f, "answer cut: {what}"),
            Broke::TimedOut { fresh } => write!(f, "timed out (new connection: {fresh})"),
        }
    }
}

/// An answer.
#[derive(Clone, Debug)]
pub struct Answer {
    pub status: u16,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Answer {
    /// The value of the header `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.as_str())
    }

    /// Whether the answer says `Connection: close`.
    pub fn closes(&self) -> bool {
        self.header("connection")
            .is_some_and(|value| value.eq_ignore_ascii_case("close"))
    }
}

/// One connection.
pub struct Conn {
    reader: BufReader<TcpStream>,
    /// The answers it has carried.
    pub answered: u64,
}

fn describe(error: &io::Error) -> String {
    format!("{:?}: {error}", error.kind())
}

fn timed_out(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

impl Conn {
    /// Connects to `addr`.
    pub fn open(addr: SocketAddr) -> Result<Conn, Broke> {
        let stream =
            TcpStream::connect_timeout(&addr, PATIENCE).map_err(|error| match error.kind() {
                ErrorKind::ConnectionRefused => Broke::Refused,
                _ if timed_out(&error) => Broke::TimedOut { fresh: true },
                _ => Broke::Connect(describe(&error)),
            })?;
        stream
            .set_read_timeout(Some(PATIENCE))
            .and_then(|()| stream.set_write_timeout(Some(PATIENCE)))
            .and_then(|()| stream.set_nodelay(true))
            .map_err(|error| Broke::Connect(describe(&error)))?;
        Ok(Conn {
            reader: BufReader::new(stream),
            answered: 0,
        })
    }

    fn before_answer(&self, error: &io::Error) -> Broke {
        let fresh = self.answered == 0;
        if timed_out(error) {
            return Broke::TimedOut { fresh };
        }
        Broke::BeforeAnswer {
            fresh,
            what: describe(error),
        }
    }

    /// Sends `method path` with `headers` and `body` (a `content-length` is added when there is
    /// a body) and reads the answer; `head` says the answer has no body.
    pub fn exchange(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
        head: bool,
    ) -> Result<Answer, Broke> {
        let mut request = format!("{method} {path} HTTP/1.1\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() || method == "PUT" {
            request.push_str(&format!("content-length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        let mut bytes = request.into_bytes();
        bytes.extend_from_slice(body);
        let stream = self.reader.get_mut();
        if let Err(error) = stream.write_all(&bytes).and_then(|()| stream.flush()) {
            return Err(self.before_answer(&error));
        }
        let answer = self.read_answer(head)?;
        self.answered += 1;
        Ok(answer)
    }

    /// One line of the head, without its CRLF; `first` says no byte of the answer came yet.
    fn line(&mut self, first: bool) -> Result<String, Broke> {
        let mut line = Vec::new();
        match self.reader.read_until(b'\n', &mut line) {
            Ok(0) if first => {
                Err(self.before_answer(&io::Error::new(ErrorKind::UnexpectedEof, "end of stream")))
            }
            Ok(0) => Err(Broke::Cut("end of stream in the head".to_owned())),
            Ok(_) => Ok(String::from_utf8_lossy(&line).trim_end().to_owned()),
            Err(error) if first => Err(self.before_answer(&error)),
            Err(error) if timed_out(&error) => Err(Broke::TimedOut { fresh: false }),
            Err(error) => Err(Broke::Cut(describe(&error))),
        }
    }

    fn read_answer(&mut self, head: bool) -> Result<Answer, Broke> {
        let status_line = self.line(true)?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| Broke::Cut(format!("not a status line: {status_line:?}")))?;
        let mut headers = Vec::new();
        loop {
            let line = self.line(false)?;
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
        let mut answer = Answer {
            status,
            headers,
            body: Vec::new(),
        };
        if head || status == 204 || status == 304 {
            return Ok(answer);
        }
        answer.body = if answer
            .header("transfer-encoding")
            .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
        {
            self.chunked()?
        } else if let Some(length) = answer.header("content-length") {
            let length: usize = length
                .parse()
                .map_err(|_| Broke::Cut(format!("content-length {length:?}")))?;
            self.exactly(length)?
        } else {
            let mut rest = Vec::new();
            self.reader
                .read_to_end(&mut rest)
                .map_err(|error| Broke::Cut(describe(&error)))?;
            rest
        };
        Ok(answer)
    }

    fn exactly(&mut self, length: usize) -> Result<Vec<u8>, Broke> {
        let mut body = vec![0; length];
        self.reader.read_exact(&mut body).map_err(|error| {
            if timed_out(&error) {
                Broke::TimedOut { fresh: false }
            } else {
                Broke::Cut(describe(&error))
            }
        })?;
        Ok(body)
    }

    fn chunked(&mut self) -> Result<Vec<u8>, Broke> {
        let mut body = Vec::new();
        loop {
            let size = self.line(false)?;
            let size = size.split(';').next().unwrap_or_default().trim();
            let size = usize::from_str_radix(size, 16)
                .map_err(|_| Broke::Cut(format!("chunk size {size:?}")))?;
            if size == 0 {
                while !self.line(false)?.is_empty() {}
                return Ok(body);
            }
            body.extend(self.exactly(size)?);
            self.exactly(2)?;
        }
    }
}
