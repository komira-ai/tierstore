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
    /// The connect was reset in its handshake: the listener closed while it was being made.
    ConnectReset,
    /// The connect failed otherwise.
    Connect(String),
    /// The connection ended before any byte of the answer came. `fresh`: it was the
    /// connection's first request; `eof`: a clean end of stream (the peer's FIN), else an error,
    /// `reset` when the peer reset it.
    BeforeAnswer {
        fresh: bool,
        eof: bool,
        reset: bool,
        what: String,
    },
    /// Part of the answer came, then the connection broke.
    Cut(String),
    /// Nothing came within the guard.
    TimedOut { fresh: bool },
}

impl fmt::Display for Broke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Broke::Refused => f.write_str("connect refused"),
            Broke::ConnectReset => f.write_str("connect reset"),
            Broke::Connect(what) => write!(f, "connect failed: {what}"),
            Broke::BeforeAnswer {
                fresh: true, what, ..
            } => write!(f, "new connection broke before any answer byte: {what}"),
            Broke::BeforeAnswer {
                fresh: false, what, ..
            } => write!(
                f,
                "kept-alive connection broke before any answer byte: {what}"
            ),
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

    /// The length the head announces.
    pub fn length(&self) -> Option<usize> {
        self.header("content-length")?.parse().ok()
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

/// What a read that failed in the middle of an answer means.
fn cut(error: &io::Error) -> Broke {
    if timed_out(error) {
        Broke::TimedOut { fresh: false }
    } else {
        Broke::Cut(describe(error))
    }
}

impl Conn {
    /// Connects to `addr`.
    pub fn open(addr: SocketAddr) -> Result<Conn, Broke> {
        let stream =
            TcpStream::connect_timeout(&addr, PATIENCE).map_err(|error| match error.kind() {
                ErrorKind::ConnectionRefused => Broke::Refused,
                ErrorKind::ConnectionReset => Broke::ConnectReset,
                _ if timed_out(&error) => Broke::TimedOut { fresh: true },
                _ => Broke::Connect(describe(&error)),
            })?;
        Conn::over(stream).map_err(|error| Broke::Connect(describe(&error)))
    }

    /// Connects to `addr` with a receive buffer of `bytes` (set before the connect, so that the
    /// window the peer sees is small): a client that stops reading stops the peer's sending soon.
    pub fn open_small(addr: SocketAddr, bytes: usize) -> io::Result<Conn> {
        use rustix::net::{AddressFamily, SocketFlags, SocketType, connect, socket_with, sockopt};
        let family = if addr.is_ipv4() {
            AddressFamily::INET
        } else {
            AddressFamily::INET6
        };
        // Close-on-exec, as std's sockets are: a step the run starts next must not inherit it
        // and hold the connection open after this side has closed it.
        let fd = socket_with(family, SocketType::STREAM, SocketFlags::CLOEXEC, None)?;
        sockopt::set_socket_recv_buffer_size(&fd, bytes)?;
        connect(&fd, &addr)?;
        Conn::over(TcpStream::from(fd))
    }

    /// The bytes that have come and are not yet read: in this connection's buffer, and in the
    /// socket's receive queue.
    pub fn pending(&self) -> usize {
        let queued = rustix::io::ioctl_fionread(self.reader.get_ref()).expect("FIONREAD");
        self.reader.buffer().len() + usize::try_from(queued).expect("a size")
    }

    /// A connection over `stream`, which is connected.
    pub fn over(stream: TcpStream) -> io::Result<Conn> {
        stream.set_read_timeout(Some(PATIENCE))?;
        stream.set_write_timeout(Some(PATIENCE))?;
        stream.set_nodelay(true)?;
        Ok(Conn {
            reader: BufReader::new(stream),
            answered: 0,
        })
    }

    /// The failure of a connection that gave no byte of an answer: `error`, or a clean end of
    /// stream for `None`.
    fn before_answer(&self, error: Option<&io::Error>) -> Broke {
        let fresh = self.answered == 0;
        match error {
            Some(error) if timed_out(error) => Broke::TimedOut { fresh },
            Some(error) => Broke::BeforeAnswer {
                fresh,
                eof: false,
                reset: error.kind() == ErrorKind::ConnectionReset,
                what: describe(error),
            },
            None => Broke::BeforeAnswer {
                fresh,
                eof: true,
                reset: false,
                what: "end of stream".to_owned(),
            },
        }
    }

    /// Sends the head of `method path` with `headers`, announcing a body of `length` bytes,
    /// and `part`, the first bytes of that body.
    pub fn begin(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        length: usize,
        part: &[u8],
    ) -> Result<(), Broke> {
        let mut request = format!("{method} {path} HTTP/1.1\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        if length > 0 || method == "PUT" {
            request.push_str(&format!("content-length: {length}\r\n"));
        }
        request.push_str("\r\n");
        let mut bytes = request.into_bytes();
        bytes.extend_from_slice(part);
        self.send(&bytes)
    }

    /// Sends more of the body begun with [`Conn::begin`].
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), Broke> {
        let stream = self.reader.get_mut();
        match stream.write_all(bytes).and_then(|()| stream.flush()) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.before_answer(Some(&error))),
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
        self.begin(method, path, headers, body.len(), body)?;
        let mut answer = self.answer_head()?;
        if !(head || answer.status == 204 || answer.status == 304) {
            answer.body = self.body_of(&answer)?;
        }
        self.answered += 1;
        Ok(answer)
    }

    /// One line of the head, without its CRLF; `first` says no byte of the answer came yet. A
    /// line that broke after some of its bytes came is a cut answer, whatever line it is.
    fn line(&mut self, first: bool) -> Result<String, Broke> {
        let mut line = Vec::new();
        match self.reader.read_until(b'\n', &mut line) {
            Ok(0) if first => Err(self.before_answer(None)),
            Ok(0) => Err(Broke::Cut("end of stream in the head".to_owned())),
            Ok(_) if !line.ends_with(b"\n") => Err(Broke::Cut(format!(
                "end of stream after {:?}",
                String::from_utf8_lossy(&line)
            ))),
            Ok(_) => Ok(String::from_utf8_lossy(&line).trim_end().to_owned()),
            Err(error) if first && line.is_empty() => Err(self.before_answer(Some(&error))),
            Err(error) => Err(cut(&error)),
        }
    }

    /// Reads the status line and the headers of the next answer, and no byte of its body.
    pub fn answer_head(&mut self) -> Result<Answer, Broke> {
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
        Ok(Answer {
            status,
            headers,
            body: Vec::new(),
        })
    }

    /// The whole body of the answer whose head is `answer`.
    fn body_of(&mut self, answer: &Answer) -> Result<Vec<u8>, Broke> {
        if answer
            .header("transfer-encoding")
            .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
        {
            return self.chunked();
        }
        if let Some(length) = answer.header("content-length") {
            let length: usize = length
                .parse()
                .map_err(|_| Broke::Cut(format!("content-length {length:?}")))?;
            return self.exactly(length);
        }
        let mut rest = Vec::new();
        self.reader.read_to_end(&mut rest).map_err(|e| cut(&e))?;
        Ok(rest)
    }

    /// The next `length` bytes of a body.
    pub fn exactly(&mut self, length: usize) -> Result<Vec<u8>, Broke> {
        let mut body = vec![0; length];
        self.reader.read_exact(&mut body).map_err(|e| cut(&e))?;
        Ok(body)
    }

    /// The stream, with what was read ahead of the caller: none after a whole answer.
    pub fn into_stream(self) -> TcpStream {
        assert!(self.reader.buffer().is_empty(), "bytes after the answer");
        self.reader.into_inner()
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

#[cfg(test)]
mod tests {
    //! The classification, over real loopback sockets: a peer that says nothing, part of a status
    //! line, or a whole answer, and then closes or resets. These need no privilege, and run in
    //! the workspace's test run.

    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::thread;

    use super::{Broke, Conn};

    /// A peer that reads one request head, writes `answer`, then closes, with a reset when
    /// `reset` (SO_LINGER 0).
    fn peer(answer: &'static [u8], reset: bool) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a listener");
        let addr = listener.local_addr().expect("an address");
        let serving = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("a connection");
            let mut head = Vec::new();
            let mut byte = [0];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).expect("the request");
                head.push(byte[0]);
            }
            stream.write_all(answer).expect("the answer");
            if reset {
                rustix::net::sockopt::set_socket_linger(&stream, Some(std::time::Duration::ZERO))
                    .expect("linger 0");
            } else {
                stream.shutdown(Shutdown::Write).expect("a FIN");
                let mut rest = Vec::new();
                let _ = stream.read_to_end(&mut rest);
            }
        });
        (addr, serving)
    }

    fn ask(addr: SocketAddr) -> Result<super::Answer, Broke> {
        let mut conn = Conn::over(TcpStream::connect(addr).expect("connect")).expect("a conn");
        let asked = conn.exchange("GET", "/", &[], b"", false);
        drop(conn);
        asked
    }

    /// Asserts that a status line cut by a reset, or by an end of stream, is a cut answer and
    /// never "no answer byte": the restart's bound admits only the latter. It would catch a
    /// classifier that looks only at whether it was reading the first line.
    #[test]
    fn a_status_line_cut_short_is_a_cut_answer() {
        for reset in [true, false] {
            let (addr, serving) = peer(b"HTTP/1.1 2", reset);
            let broke = ask(addr).expect_err("no whole answer");
            assert!(matches!(broke, Broke::Cut(_)), "{reset}: {broke:?}");
            serving.join().expect("the peer");
        }
    }

    /// Asserts that a peer that answers nothing and resets is a reset before any answer byte,
    /// and one that closes cleanly is a clean end of stream, both on a new connection.
    #[test]
    fn nothing_then_a_reset_or_a_close_is_before_any_answer_byte() {
        let (addr, serving) = peer(b"", true);
        let broke = ask(addr).expect_err("no answer");
        assert!(
            matches!(
                broke,
                Broke::BeforeAnswer {
                    fresh: true,
                    eof: false,
                    reset: true,
                    ..
                }
            ),
            "{broke:?}"
        );
        serving.join().expect("the peer");
        let (addr, serving) = peer(b"", false);
        let broke = ask(addr).expect_err("no answer");
        assert!(
            matches!(
                broke,
                Broke::BeforeAnswer {
                    fresh: true,
                    eof: true,
                    reset: false,
                    ..
                }
            ),
            "{broke:?}"
        );
        serving.join().expect("the peer");
    }

    /// Asserts that a whole answer is read, with its body, and one cut in its body is cut.
    #[test]
    fn a_whole_answer_is_read_and_a_cut_body_is_cut() {
        let (addr, serving) = peer(b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\nabc", false);
        let answer = ask(addr).expect("an answer");
        assert_eq!((answer.status, answer.body.as_slice()), (200, &b"abc"[..]));
        serving.join().expect("the peer");
        let (addr, serving) = peer(b"HTTP/1.1 200 OK\r\ncontent-length: 9\r\n\r\nabc", true);
        assert!(matches!(ask(addr), Err(Broke::Cut(_))));
        serving.join().expect("the peer");
    }
}
