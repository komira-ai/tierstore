//! Transfers held part-way across the boundaries of an update, and idle connections that probe the
//! end of a drain, all on v-current. The run moves each of them on at an event it observes, never
//! at a time.
//!
//! - [`Held`]: an S3 put whose body is half sent, and an S3 get and an OCI blob get whose answers
//!   are half read, begun before `cutover`. They are let go once v-current's settle is over (its
//!   S3 and OCI listeners refuse connections), or, in a run where v-current never drains, at the
//!   end. Each must then complete: the put acknowledged and its object read back, each get whole
//!   and with its digest. Catches a swap or a drain that cuts a request in flight.
//! - [`Idle`]: an S3 and an OCI connection that are used once and then left idle. Each waits for
//!   v-current to end it, which only the end of the settle does (no answer of its carries
//!   `Connection: close`), then writes a request and closes. With the lingering close (P2) the
//!   server reads that request and counts it: v-current's exit note then names exactly one late
//!   connection on each front. Without it the request is never read and nothing is counted. So
//!   the note is held to the real counters, and P2 is tested apart from P1.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread::{self, JoinHandle};

use crate::load::{Acked, hex_sha256, object};
use crate::site::{BUCKET, PUBLIC, oci_port, signed};
use crate::wire::{Answer, Conn};

/// The size of the objects of the held transfers: larger than the socket buffers, so that the
/// server is still sending when the client stops reading.
pub const BIG: usize = 4 << 20;
/// The key of the object the held S3 get reads.
pub const BIG_KEY: &str = "t1/held/big";
/// The key the held put writes.
pub const PUT_KEY: &str = "t1/held/put";

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// The bytes of the held transfers' objects.
pub fn big(name: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(BIG);
    let mut seq = 0;
    while bytes.len() < BIG {
        seq += 1;
        let mut part = object(name, seq);
        if part.is_empty() {
            continue;
        }
        part.truncate(BIG - bytes.len());
        bytes.extend(part);
    }
    bytes
}

/// A get whose answer has been read in part.
struct HalfRead {
    conn: Conn,
    answer: Answer,
    got: Vec<u8>,
    hex: String,
}

impl HalfRead {
    fn begin(addr: SocketAddr, path: &str, headers: &[(String, String)], hex: String) -> HalfRead {
        let mut conn = Conn::open(addr).expect("connect");
        conn.begin("GET", path, headers, 0, b"").expect("ask");
        let answer = conn.answer_head().expect("a head");
        assert_eq!(answer.status, 200, "{path}");
        let length = answer.length().expect("a length");
        let got = conn.exactly(length / 2).expect("half the body");
        HalfRead {
            conn,
            answer,
            got,
            hex,
        }
    }

    fn finish(mut self, what: &str, failures: &mut Vec<String>) {
        let length = self.answer.length().expect("a length");
        match self.conn.exactly(length - self.got.len()) {
            Ok(rest) => {
                self.got.extend(rest);
                if hex_sha256(&self.got) != self.hex {
                    failures.push(format!("{what}: not the bytes asked for"));
                }
            }
            Err(broke) => failures.push(format!("{what}: {broke}")),
        }
    }
}

/// The held transfers.
pub struct Held {
    put: Conn,
    body: Vec<u8>,
    s3_get: HalfRead,
    oci_get: HalfRead,
}

/// What the held transfers came to.
pub struct Released {
    pub failures: Vec<String>,
    /// The put, when it was acknowledged.
    pub acked: Option<Acked>,
}

impl Held {
    /// Begins the three transfers on the slot `slot`, which serves the public port: `big_hex` is
    /// the digest of the object at [`BIG_KEY`] and of the blob of the same bytes.
    pub fn begin(slot: u16, big_hex: &str) -> Held {
        let host = local(PUBLIC).to_string();
        let body = big("held-put");
        let mut put = Conn::open(local(PUBLIC)).expect("connect");
        let headers = signed("PUT", &host, PUT_KEY, &body);
        let path = format!("/{BUCKET}/{PUT_KEY}");
        put.begin("PUT", &path, &headers, body.len(), &body[..body.len() / 2])
            .expect("half a put");
        let headers = signed("GET", &host, BIG_KEY, b"");
        let path = format!("/{BUCKET}/{BIG_KEY}");
        let s3_get = HalfRead::begin(local(PUBLIC), &path, &headers, big_hex.to_owned());
        let oci = local(oci_port(slot));
        let path = format!("/v2/t1/app/blobs/sha256:{big_hex}");
        let host = vec![("host".to_owned(), oci.to_string())];
        let oci_get = HalfRead::begin(oci, &path, &host, big_hex.to_owned());
        Held {
            put,
            body,
            s3_get,
            oci_get,
        }
    }

    /// Lets the transfers go on, and says how they ended.
    pub fn release(mut self) -> Released {
        let mut failures = Vec::new();
        let mut acked = None;
        let rest = &self.body[self.body.len() / 2..];
        match self.put.send(rest).and_then(|()| self.put.answer_head()) {
            Ok(answer) if answer.status == 200 => {
                acked = Some(Acked {
                    key: PUT_KEY.to_owned(),
                    sha256: hex_sha256(&self.body),
                });
            }
            Ok(answer) => failures.push(format!("the held put: status {}", answer.status)),
            Err(broke) => failures.push(format!("the held put: {broke}")),
        }
        self.s3_get.finish("the held S3 get", &mut failures);
        self.oci_get.finish("the held OCI get", &mut failures);
        Released { failures, acked }
    }
}

/// Idle connections to v-current, each on a thread that waits for the end of its stream.
pub struct Idle {
    probes: Vec<JoinHandle<Result<(), String>>>,
}

/// One idle connection: used once, then it waits for its server's end of stream, writes a
/// request, and closes.
fn idle(addr: SocketAddr, path: &'static str) -> JoinHandle<Result<(), String>> {
    let mut conn = Conn::open(addr).expect("connect");
    let host = vec![("host".to_owned(), addr.to_string())];
    let answer = conn
        .exchange("GET", path, &host, b"", false)
        .expect("an answer");
    assert_eq!(answer.status, 200, "{addr}{path}");
    assert!(!answer.closes(), "{addr}{path} is kept alive");
    let mut stream: TcpStream = conn.into_stream();
    thread::spawn(move || {
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => {}
            Ok(_) => return Err(format!("{addr}: bytes on an idle connection")),
            Err(e) => return Err(format!("{addr}: {e}")),
        }
        let late = format!("GET {path} HTTP/1.1\r\nhost: {addr}\r\n\r\n");
        // The server may have closed already (no lingering close): then this write is answered
        // by a reset, which is what the counters, not this probe, tell apart.
        let _ = stream.write_all(late.as_bytes());
        drop(stream);
        Ok(())
    })
}

impl Idle {
    /// Opens the idle connections to v-current, `slot`: one to its S3 port, over loopback (which
    /// the guard lets through), and one to its OCI port.
    pub fn open(slot: u16) -> Idle {
        Idle {
            probes: vec![
                idle(local(slot), "/healthz"),
                idle(local(oci_port(slot)), "/v2/"),
            ],
        }
    }

    /// Waits for each idle connection to have been ended by its server and to have sent its late
    /// request.
    pub fn join(self) -> Vec<String> {
        self.probes
            .into_iter()
            .filter_map(|probe| probe.join().expect("a probe").err())
            .collect()
    }
}
