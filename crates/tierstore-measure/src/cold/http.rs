//! The little HTTP/1.1 the prototype speaks: a `GET /<drive>/<file>` with one `Range` and an
//! `x-ts-class`, answered `206` with `x-ts-queue-wait-ms` (or `503`, or `404`), keep-alive.

use std::io::{BufRead, Read};

use crate::Error;
use crate::cold::sched::Class;

/// A parsed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The drive's name.
    pub drive: String,
    /// The file's name.
    pub file: String,
    /// First byte.
    pub off: u64,
    /// Bytes.
    pub len: u64,
    /// Its class.
    pub class: Class,
}

/// Reads one request head from `r`; `Ok(None)` at a clean end of the connection.
///
/// # Errors
/// When the head is not a request this server serves.
pub fn read_request(r: &mut impl BufRead) -> Result<Option<Request>, Error> {
    let mut line = String::new();
    if r.read_line(&mut line).map_err(Error::net)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some("GET"), Some(path)) = (parts.next(), parts.next()) else {
        return Err(Error::Net(format!("not a GET: {:?}", line.trim())));
    };
    let (drive, file) = path
        .trim_start_matches('/')
        .split_once('/')
        .ok_or_else(|| Error::Net(format!("path {path}: want /<drive>/<file>")))?;
    let (mut range, mut class) = (None, Class::Prepare);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).map_err(Error::net)? == 0 {
            return Err(Error::Net("the head ended early".into()));
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let Some((k, v)) = h.split_once(':') else {
            continue;
        };
        match k.trim().to_ascii_lowercase().as_str() {
            "range" => range = Some(parse_range(v.trim())?),
            "x-ts-class" => {
                class = Class::parse(v.trim())
                    .ok_or_else(|| Error::Net(format!("class {}", v.trim())))?;
            }
            _ => {}
        }
    }
    let (off, len) = range.ok_or_else(|| Error::Net("no Range".into()))?;
    Ok(Some(Request {
        drive: drive.to_string(),
        file: file.to_string(),
        off,
        len,
        class,
    }))
}

/// Parses `bytes=a-b` into offset and length.
///
/// # Errors
/// When it is another form or empty.
pub fn parse_range(v: &str) -> Result<(u64, u64), Error> {
    let bad = || Error::Net(format!("range {v}: want bytes=a-b"));
    let (a, b) = v
        .strip_prefix("bytes=")
        .and_then(|r| r.split_once('-'))
        .ok_or_else(bad)?;
    let (a, b): (u64, u64) = (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?);
    if b < a {
        return Err(bad());
    }
    Ok((a, b - a + 1))
}

/// The request text for a range of `drive/file`.
#[must_use]
pub fn request(drive: &str, file: &str, off: u64, len: u64, class: Class) -> String {
    format!(
        "GET /{drive}/{file} HTTP/1.1\r\nHost: cold\r\nRange: bytes={off}-{}\r\nx-ts-class: {}\r\n\r\n",
        off + len - 1,
        class.name()
    )
}

/// The head of a `206` of `len` bytes after `wait_ms` in the queue.
#[must_use]
pub fn ok_head(len: u64, wait_ms: f64) -> String {
    format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {len}\r\nx-ts-queue-wait-ms: {wait_ms:.1}\r\n\r\n"
    )
}

/// The head of an empty answer with `status` (`404 Not Found`, `503 Service Unavailable`).
#[must_use]
pub fn empty_head(status: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nRetry-After: 1\r\n\r\n")
}

/// An answer's status and its headers that matter here.
#[derive(Debug, Clone, PartialEq)]
pub struct Head {
    /// The status code.
    pub status: u16,
    /// Its body's length.
    pub len: u64,
    /// The server's queue wait.
    pub wait_ms: f64,
}

/// Reads an answer head.
///
/// # Errors
/// When it is not one.
pub fn read_head(r: &mut impl BufRead) -> Result<Head, Error> {
    let mut line = String::new();
    r.read_line(&mut line).map_err(Error::net)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Net(format!("not an answer: {:?}", line.trim())))?;
    let mut head = Head {
        status,
        len: 0,
        wait_ms: 0.0,
    };
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).map_err(Error::net)? == 0 {
            return Err(Error::Net("the answer ended early".into()));
        }
        let h = h.trim_end();
        if h.is_empty() {
            return Ok(head);
        }
        let Some((k, v)) = h.split_once(':') else {
            continue;
        };
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => head.len = v.trim().parse().unwrap_or(0),
            "x-ts-queue-wait-ms" => head.wait_ms = v.trim().parse().unwrap_or(0.0),
            _ => {}
        }
    }
}

/// Reads and drops `len` bytes, calling `first` once when the first arrives; returns the count.
///
/// # Errors
/// When the connection fails or ends early.
pub fn drain(
    r: &mut impl Read,
    len: u64,
    buf: &mut [u8],
    mut first: impl FnMut(),
) -> Result<u64, Error> {
    let mut got = 0u64;
    while got < len {
        let want = usize::try_from((len - got).min(buf.len() as u64)).unwrap_or(buf.len());
        let n = r.read(&mut buf[..want]).map_err(Error::net)?;
        if n == 0 {
            return Err(Error::Net(format!("the body ended at {got} of {len}")));
        }
        if got == 0 {
            first();
        }
        got += n as u64;
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn a_request_round_trips() {
        let text = request("hdd1", "p0.pack", 4096, 1 << 20, Class::Demand);
        let r = read_request(&mut Cursor::new(text.as_bytes()))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            Request {
                drive: "hdd1".into(),
                file: "p0.pack".into(),
                off: 4096,
                len: 1 << 20,
                class: Class::Demand
            }
        );
        assert_eq!(read_request(&mut Cursor::new(b"")).unwrap(), None);
        let plain = "GET /d/f HTTP/1.1\r\nweird\r\nRange: bytes=0-9\r\n\r\n";
        assert_eq!(
            read_request(&mut Cursor::new(plain))
                .unwrap()
                .unwrap()
                .class,
            Class::Prepare
        );
    }

    #[test]
    fn bad_requests_are_refused() {
        for text in [
            "PUT /d/f HTTP/1.1\r\n\r\n",
            "GET /nofile HTTP/1.1\r\n\r\n",
            "GET /d/f HTTP/1.1\r\nRange: bytes=0-9\r\n",
            "GET /d/f HTTP/1.1\r\n\r\n",
            "GET /d/f HTTP/1.1\r\nRange: bytes=9-0\r\n\r\n",
            "GET /d/f HTTP/1.1\r\nRange: items=0-9\r\n\r\n",
            "GET /d/f HTTP/1.1\r\nRange: bytes=a-9\r\n\r\n",
            "GET /d/f HTTP/1.1\r\nRange: bytes=0-b\r\n\r\n",
            "GET /d/f HTTP/1.1\r\nRange: bytes=0-9\r\nx-ts-class: urgent\r\n\r\n",
        ] {
            assert!(read_request(&mut Cursor::new(text)).is_err(), "{text:?}");
        }
    }

    #[test]
    fn answers_round_trip() {
        let h = read_head(&mut Cursor::new(ok_head(42, 3.26))).unwrap();
        assert_eq!(
            h,
            Head {
                status: 206,
                len: 42,
                wait_ms: 3.3
            }
        );
        let e = read_head(&mut Cursor::new(empty_head("503 Service Unavailable"))).unwrap();
        assert_eq!((e.status, e.len), (503, 0));
        let odd = "HTTP/1.1 200 OK\r\nno-colon\r\nContent-Length: x\r\nServer: y\r\n\r\n";
        assert_eq!(read_head(&mut Cursor::new(odd)).unwrap().len, 0);
        assert!(read_head(&mut Cursor::new("garbage\r\n\r\n")).is_err());
        assert!(read_head(&mut Cursor::new("HTTP/1.1 206 X\r\n")).is_err());
    }

    #[test]
    fn drain_reads_the_body_and_reports_the_first_byte_once() {
        let mut firsts = 0;
        let mut buf = [0u8; 3];
        let n = drain(&mut Cursor::new(vec![1u8; 10]), 10, &mut buf, || {
            firsts += 1
        })
        .unwrap();
        assert_eq!((n, firsts), (10, 1));
        let e = drain(&mut Cursor::new(vec![1u8; 4]), 10, &mut buf, || {}).unwrap_err();
        assert_eq!(e.to_string(), "the body ended at 4 of 10");
    }
}
