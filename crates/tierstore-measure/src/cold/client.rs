//! The prototype's client: one keep-alive connection, ranged GETs timed to the first and the
//! last byte, optionally read no faster than a rate (a slow consumer).

use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde::Serialize;

use crate::Error;
use crate::clock::Clock;
use crate::cold::http::{drain, read_head, request};
use crate::cold::sched::Class;

/// One GET as the client saw it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Got {
    /// The answer's status.
    pub status: u16,
    /// Time to the first body byte (to the head for an empty answer), ms.
    pub first_ms: f64,
    /// Time to the last byte, ms.
    pub total_ms: f64,
    /// Bytes of body.
    pub bytes: u64,
    /// The server's queue wait, ms.
    pub wait_ms: f64,
}

/// A connection to a cold service.
#[derive(Debug)]
pub struct Conn {
    r: BufReader<TcpStream>,
    w: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    /// Connects to `addr`.
    ///
    /// # Errors
    /// When it cannot.
    pub fn open(addr: SocketAddr) -> Result<Self, Error> {
        let w = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(|e| Error::Net(format!("connect {addr}: {e}")))?;
        w.set_nodelay(true).map_err(Error::net)?;
        let r = BufReader::with_capacity(1 << 20, w.try_clone().map_err(Error::net)?);
        Ok(Self {
            r,
            w,
            buf: vec![0u8; 256 << 10],
        })
    }

    /// GETs `len` bytes at `off` of `drive/file`; with `rate` (MB/s), reads no faster.
    ///
    /// # Errors
    /// When the exchange fails.
    #[expect(clippy::too_many_arguments, reason = "the coordinates of one GET")]
    pub fn get(
        &mut self,
        drive: &str,
        file: &str,
        off: u64,
        len: u64,
        class: Class,
        clock: &dyn Clock,
        rate: Option<f64>,
    ) -> Result<Got, Error> {
        let t0 = clock.now();
        self.w
            .write_all(request(drive, file, off, len, class).as_bytes())
            .map_err(Error::net)?;
        let head = read_head(&mut self.r)?;
        let mut first = None;
        let body = if head.status == 206 {
            let mut got = 0u64;
            while got < head.len {
                let step = (head.len - got).min(self.buf.len() as u64);
                got += drain(&mut self.r, step, &mut self.buf, || {
                    first.get_or_insert_with(|| clock.now());
                })?;
                if let Some(mb_s) = rate {
                    let due = Duration::from_secs_f64(got as f64 / (mb_s * 1e6));
                    while clock.now() - t0 < due {
                        clock.sleep(Duration::from_millis(2));
                    }
                }
            }
            got
        } else {
            drain(&mut self.r, head.len, &mut self.buf, || {})?
        };
        let t1 = clock.now();
        let first = first.unwrap_or(t1);
        Ok(Got {
            status: head.status,
            first_ms: (first - t0).as_secs_f64() * 1000.0,
            total_ms: (t1 - t0).as_secs_f64() * 1000.0,
            bytes: body,
            wait_ms: head.wait_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::StepClock;
    use std::io::BufRead as _;
    use std::net::TcpListener;

    /// A server that answers each request on one connection with the next canned answer.
    fn canned(answers: Vec<Vec<u8>>) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            for a in answers {
                loop {
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    // The blank line ends a head (an ended connection reads as blank too).
                    if line.trim_end().is_empty() {
                        break;
                    }
                }
                w.write_all(&a).unwrap();
            }
        });
        addr
    }

    #[test]
    fn gets_a_range_and_an_empty_answer_on_one_connection() {
        let mut ok = b"HTTP/1.1 206 Partial Content\r\nContent-Length: 600000\r\nx-ts-queue-wait-ms: 2.5\r\n\r\n".to_vec();
        ok.extend(vec![9u8; 600_000]);
        let busy = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".to_vec();
        let addr = canned(vec![ok.clone(), busy, ok]);
        let mut c = Conn::open(addr).unwrap();
        let clock = StepClock::new(Duration::from_millis(1), 0);
        let g = c
            .get("d", "f", 0, 600_000, Class::Demand, &clock, None)
            .unwrap();
        assert_eq!((g.status, g.bytes, g.wait_ms), (206, 600_000, 2.5));
        assert!(g.first_ms > 0.0);
        assert!(g.total_ms >= g.first_ms);
        let b = c
            .get("d", "f", 0, 10, Class::Background, &clock, None)
            .unwrap();
        assert_eq!((b.status, b.bytes), (503, 0));
        assert!((b.first_ms - b.total_ms).abs() < 1e-9);
        // At 1 MB/s, 600 kB take at least 600 ms of the clock.
        let slow = c
            .get("d", "f", 0, 600_000, Class::Prepare, &clock, Some(1.0))
            .unwrap();
        assert!(slow.total_ms >= 600.0, "{}", slow.total_ms);
        // The server is gone: the next exchange fails.
        assert!(c.get("d", "f", 0, 1, Class::Demand, &clock, None).is_err());
    }

    #[test]
    fn a_refused_connection_and_a_cut_body_fail() {
        let dead = tierstore_testkit::port::dead_addr();
        assert!(
            Conn::open(dead)
                .unwrap_err()
                .to_string()
                .starts_with("connect ")
        );
        let cut = b"HTTP/1.1 206 Partial Content\r\nContent-Length: 100\r\n\r\nshort".to_vec();
        let mut c = Conn::open(canned(vec![cut])).unwrap();
        let clock = StepClock::new(Duration::from_millis(1), 0);
        assert!(
            c.get("d", "f", 0, 100, Class::Demand, &clock, None)
                .is_err()
        );
    }
}
