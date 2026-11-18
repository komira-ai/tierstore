//! The prototype's cold service: packs on each drive served as byte ranges over HTTP/1.1, every
//! request a stream admitted by its drive's [`Sched`]. A stream reads `chunk`-byte pieces either
//! with `O_DIRECT` at `depth` pieces in flight (copied to the socket), or with buffered `sendfile`
//! followed by `POSIX_FADV_DONTNEED` of what it sent. With `release_after`, a stream whose socket
//! accepts nothing for that long gives its slot back until the socket drains (the slow-consumer
//! fix the design's review asks to measure). Writes (packs as the cold tier writes them) are
//! streams of the `write` kind of the same scheduler.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use serde::Serialize;

use crate::Error;
use crate::clock::Clock;
use crate::cold::http::{Request, empty_head, ok_head, read_request};
use crate::cold::sched::{Class, Params, Sched, Ticket};
use crate::io::{
    ALIGN, AlignedBuf, Mode, create, drop_pages, fill, open_read, read_full_at, write_full_at,
};

/// How a stream reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReadPath {
    /// `O_DIRECT`, this many chunks in flight.
    Direct(usize),
    /// Buffered `sendfile`, then `DONTNEED`.
    Sendfile,
}

/// The server's settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Drive name to its pack directory.
    pub drives: BTreeMap<String, PathBuf>,
    /// The scheduler of each drive.
    pub params: Params,
    /// Bytes per chunk.
    pub chunk: usize,
    /// How streams read.
    pub path: ReadPath,
    /// Give the slot back when the socket takes nothing for this long.
    pub release_after: Option<Duration>,
}

/// One served request, as the server saw it.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Served {
    /// Its class.
    pub class: Class,
    /// Time waiting to be admitted, and re-admitted after yields, ms.
    pub wait_ms: f64,
    /// Time from admission to the last byte written, ms.
    pub service_ms: f64,
    /// Bytes.
    pub bytes: u64,
}

/// The server.
pub struct Server<'c> {
    cfg: Config,
    scheds: BTreeMap<String, Sched<'c>>,
    clock: &'c dyn Clock,
    served: std::sync::Mutex<Vec<Served>>,
}

fn stopped<T>(_: T) -> Error {
    Error::Net("a reader stopped".into())
}

fn errno(e: rustix::io::Errno) -> Error {
    net(e.into())
}

fn net(e: std::io::Error) -> Error {
    Error::Net(e.to_string())
}

impl<'c> Server<'c> {
    /// A server of `cfg`'s drives.
    #[must_use]
    pub fn new(cfg: Config, clock: &'c dyn Clock) -> Self {
        let scheds = cfg
            .drives
            .keys()
            .map(|d| (d.clone(), Sched::new(cfg.params, clock)))
            .collect();
        Self {
            cfg,
            scheds,
            clock,
            served: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The scheduler of `drive`.
    #[must_use]
    pub fn sched(&self, drive: &str) -> Option<&Sched<'c>> {
        self.scheds.get(drive)
    }

    /// The counters of every drive's scheduler, summed.
    #[must_use]
    pub fn counts(&self) -> crate::cold::sched::Counts {
        let mut t = crate::cold::sched::Counts::default();
        for s in self.scheds.values() {
            let c = s.counts();
            t.admitted += c.admitted;
            t.yields += c.yields;
            t.preemptions += c.preemptions;
            t.refused += c.refused;
            t.parks += c.parks;
        }
        t
    }

    /// What it served so far (and forgets it).
    pub fn take_served(&self) -> Vec<Served> {
        std::mem::take(
            &mut *self
                .served
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Accepts connections on `listener` until `stop`, each on a thread of its own.
    ///
    /// # Errors
    /// When the listener fails.
    pub fn serve(&self, listener: &TcpListener, stop: &AtomicBool) -> Result<(), Error> {
        listener.set_nonblocking(true).map_err(net)?;
        std::thread::scope(|scope| {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((sock, _)) => {
                        scope.spawn(move || {
                            let _ = self.connection(sock, stop);
                        });
                    }
                    // No connection waiting (or a transient accept error): look again shortly.
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            Ok(())
        })
    }

    fn connection(&self, sock: TcpStream, stop: &AtomicBool) -> Result<(), Error> {
        sock.set_nonblocking(false).map_err(net)?;
        sock.set_nodelay(true).map_err(net)?;
        let mut r = BufReader::new(sock.try_clone().map_err(net)?);
        let mut w = sock;
        loop {
            w.set_read_timeout(Some(Duration::from_millis(100)))
                .map_err(net)?;
            let mut probe = [0u8; 1];
            // Wait for the next request, looking at the stop flag every 100 ms; a closed or
            // failed connection ends.
            match w.peek(&mut probe) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(_) if !stop.load(Ordering::SeqCst) => continue,
                Err(_) => return Ok(()),
            }
            w.set_read_timeout(None).map_err(net)?;
            let req = read_request(&mut r)?.ok_or(Error::Net("the client left".into()))?;
            self.handle(&mut w, &req)?;
        }
    }

    fn handle(&self, w: &mut TcpStream, req: &Request) -> Result<(), Error> {
        let (Some(dir), Some(sched)) =
            (self.cfg.drives.get(&req.drive), self.scheds.get(&req.drive))
        else {
            return w
                .write_all(empty_head("404 Not Found").as_bytes())
                .map_err(net);
        };
        let path = dir.join(&req.file);
        let mode = match self.cfg.path {
            ReadPath::Direct(_) => Mode::Direct,
            ReadPath::Sendfile => Mode::Buffered,
        };
        let Ok(file) = open_read(&path, mode) else {
            return w
                .write_all(empty_head("404 Not Found").as_bytes())
                .map_err(net);
        };
        let Some((ticket, waited)) = sched.admit(req.class, false) else {
            return w
                .write_all(empty_head("503 Service Unavailable").as_bytes())
                .map_err(net);
        };
        let t0 = self.clock.now();
        w.write_all(ok_head(req.len, waited.as_secs_f64() * 1000.0).as_bytes())
            .map_err(net)?;
        let more = match self.cfg.path {
            ReadPath::Direct(depth) => self.send_direct(w, &file, req, sched, &ticket, depth),
            ReadPath::Sendfile => self.send_file(w, &file, req, sched, &ticket),
        };
        sched.release(ticket);
        let more = more?;
        self.served
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Served {
                class: req.class,
                wait_ms: (waited + more).as_secs_f64() * 1000.0,
                service_ms: (self.clock.now() - t0).saturating_sub(more).as_secs_f64() * 1000.0,
                bytes: req.len,
            });
        Ok(())
    }

    /// Writes `buf` to the socket; if it takes nothing for `release_after`, gives the slot back
    /// until it is written. Returns the time spent waiting for the slot again.
    fn write_out(
        &self,
        w: &mut TcpStream,
        buf: &[u8],
        sched: &Sched<'c>,
        t: &Ticket,
    ) -> Result<Duration, Error> {
        let Some(after) = self.cfg.release_after else {
            w.write_all(buf).map_err(net)?;
            return Ok(Duration::ZERO);
        };
        w.set_write_timeout(Some(after)).map_err(net)?;
        let (mut sent, mut parked) = (0, false);
        while sent < buf.len() {
            match w.write(&buf[sent..]) {
                Ok(n) => sent += n,
                // Only the first time out can come: after it the socket has none.
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    sched.park(t);
                    parked = true;
                    w.set_write_timeout(None).map_err(net)?;
                }
                Err(e) => return Err(net(e)),
            }
        }
        Ok(if parked {
            sched.unpark(t)
        } else {
            Duration::ZERO
        })
    }

    fn send_direct(
        &self,
        w: &mut TcpStream,
        file: &File,
        req: &Request,
        sched: &Sched<'c>,
        t: &Ticket,
        depth: usize,
    ) -> Result<Duration, Error> {
        let chunk = self.cfg.chunk as u64;
        let start = req.off / ALIGN as u64 * ALIGN as u64;
        let end = req.off + req.len;
        let chunks = (end - start).div_ceil(chunk);
        let depth = depth.max(1);
        let mut waited = Duration::ZERO;
        std::thread::scope(|scope| -> Result<(), Error> {
            let mut to: Vec<mpsc::SyncSender<u64>> = Vec::new();
            let mut from: Vec<mpsc::Receiver<std::io::Result<(AlignedBuf, usize)>>> = Vec::new();
            for _ in 0..depth {
                let (qtx, qrx) = mpsc::sync_channel::<u64>(1);
                let (dtx, drx) = mpsc::sync_channel(1);
                to.push(qtx);
                from.push(drx);
                let size = self.cfg.chunk;
                scope.spawn(move || {
                    for i in qrx {
                        let mut b = AlignedBuf::new(size);
                        let r =
                            read_full_at(file, b.as_mut_slice(), start + i * chunk).map(|n| (b, n));
                        // A sender that is gone stopped the stream; the next receive ends.
                        let _ = dtx.send(r);
                    }
                });
            }
            let mut issued = 0u64;
            let issue = |n: u64, waited: &mut Duration| -> Result<(), Error> {
                if n > 0 {
                    *waited += sched.boundary(t, chunk);
                }
                to[usize::try_from(n % depth as u64).unwrap_or(0)]
                    .send(n)
                    .map_err(stopped)
            };
            while issued < chunks.min(depth as u64) {
                issue(issued, &mut waited)?;
                issued += 1;
            }
            for i in 0..chunks {
                let (b, n) = from[usize::try_from(i % depth as u64).unwrap_or(0)]
                    .recv()
                    .map_err(stopped)?
                    .map_err(net)?;
                let lo = i * chunk + start;
                let a = usize::try_from(req.off.saturating_sub(lo)).unwrap_or(0);
                let z = usize::try_from(end.saturating_sub(lo).min(chunk)).unwrap_or(0);
                if n < z {
                    // The range runs past the end of the file: the answer cannot be whole, so
                    // the connection ends (the client sees a short body).
                    return Err(Error::Net(format!(
                        "{} ends at {}",
                        req.file,
                        lo + n as u64
                    )));
                }
                waited += self.write_out(w, &b.as_slice()[a..z], sched, t)?;
                if issued < chunks {
                    issue(issued, &mut waited)?;
                    issued += 1;
                }
            }
            drop(to);
            Ok(())
        })?;
        Ok(waited)
    }

    fn send_file(
        &self,
        w: &mut TcpStream,
        file: &File,
        req: &Request,
        sched: &Sched<'c>,
        t: &Ticket,
    ) -> Result<Duration, Error> {
        let mut pos = req.off;
        let end = req.off + req.len;
        let mut waited = Duration::ZERO;
        while pos < end {
            if pos > req.off {
                waited += sched.boundary(t, self.cfg.chunk as u64);
            }
            let want =
                usize::try_from((end - pos).min(self.cfg.chunk as u64)).unwrap_or(self.cfg.chunk);
            let from = pos;
            let mut left = want;
            while left > 0 {
                let n = rustix::fs::sendfile(&*w, file, Some(&mut pos), left).map_err(errno)?;
                if n == 0 {
                    return Err(Error::Net(format!("the file ended at {pos}")));
                }
                left -= n;
            }
            drop_pages(file, from, want as u64, std::path::Path::new(&req.file))?;
        }
        Ok(waited)
    }

    /// Writes a pack of `size` bytes as `name` on `drive` as a write stream: `O_DIRECT`
    /// `chunk`-byte writes two at a time, a scheduler boundary per pair. Returns its seconds.
    ///
    /// # Errors
    /// When the drive is unknown, the queue refuses, or a write fails.
    pub fn write_pack(&self, drive: &str, name: &str, size: u64) -> Result<f64, Error> {
        let (Some(dir), Some(sched)) = (self.cfg.drives.get(drive), self.scheds.get(drive)) else {
            return Err(Error::Invalid(format!("no drive {drive}")));
        };
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        let f = create(&p, Mode::Direct)?;
        let (t, _) = sched
            .admit(Class::Prepare, true)
            .ok_or_else(|| Error::Invalid("the write queue is full".into()))?;
        let t0 = self.clock.now();
        let chunk = self.cfg.chunk as u64;
        let mut off = 0;
        let r = (|| -> Result<(), Error> {
            while off < size {
                std::thread::scope(|scope| -> Result<(), Error> {
                    let hs: Vec<_> = (0..2u64)
                        .map(|k| off + k * chunk)
                        .filter(|o| *o < size)
                        .map(|o| {
                            let (f, p) = (&f, &p);
                            scope.spawn(move || -> Result<(), Error> {
                                let n = usize::try_from((size - o).min(chunk)).unwrap_or(0);
                                let mut b = AlignedBuf::new(n);
                                fill(b.as_mut_slice(), o);
                                write_full_at(f, b.as_slice(), o).map_err(Error::io("write", p))
                            })
                        })
                        .collect();
                    hs.into_iter()
                        .try_for_each(|h| crate::run::joined(h.join()))
                })?;
                off += 2 * chunk;
                sched.boundary(&t, 2 * chunk);
            }
            f.sync_all().map_err(Error::io("fsync", &p))
        })();
        sched.release(t);
        r?;
        Ok((self.clock.now() - t0).as_secs_f64())
    }
}

#[cfg(test)]
mod tests;
