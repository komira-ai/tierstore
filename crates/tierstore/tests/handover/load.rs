//! The clients of a run, each a thread that sends its next request only once it has the answer
//! to the one before, and never sends a request again: a request that failed is recorded and the
//! client goes on with its next one, which is another request (another key for a put).
//!
//! - An S3 client calls the public port: a create-only put (`If-None-Match: *`) of a new key, a
//!   get of a key it was told it wrote (the bytes must be the ones it wrote), and every fourth
//!   request `/metricz`, whose build gauge names the slot that answered. Over one kept-alive
//!   connection (which it leaves when an answer says `Connection: close`) or a new connection for
//!   each request.
//! - An OCI client calls the OCI front of the slot that serves when the run starts (the hook
//!   redirects only the public S3 port, so nothing moves this traffic): `GET /v2/`, and `GET` and
//!   `HEAD` of the manifest and of each blob by digest, checking every digest. Its sequence ends,
//!   cleanly, when an answer says `Connection: close` or its kept-alive connection reads a clean
//!   end of stream: the drain of that slot.
//!
//! Some clients are in the host namespace, others in the remote one, on IPv4 and IPv6. The run
//! waits on the count of the S3 clients' finished requests ([`Load::wait_more`]), never on a
//! clock.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use sha2::{Digest, Sha256};
use tierstore::server::update_hook::build_of;

use crate::netns::{self, LOCAL_V4, LOCAL_V6, Names};
use crate::site::{BUCKET, PUBLIC, oci_port, signed};
use crate::wire::{Answer, Broke, Conn};

/// What a client calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    S3,
    Oci,
}

/// A client.
#[derive(Clone, Debug)]
pub struct Spec {
    pub name: String,
    pub kind: Kind,
    /// Keeps its connection alive; else a new connection for each request.
    pub keep: bool,
    pub addr: SocketAddr,
    /// The namespace it runs in, when not the host's.
    pub remote: Option<String>,
}

/// The registry content the OCI clients read: the manifest and the blobs, as hex digests and
/// sizes.
#[derive(Clone, Debug)]
pub struct Registry {
    pub manifest: (String, usize),
    pub blobs: Vec<(String, usize)>,
}

/// What went wrong with a request.
#[derive(Clone, Debug)]
pub enum Fault {
    /// The connection failed.
    Broke(Broke),
    /// An answer with an unexpected status.
    Status(u16, String),
    /// An answer whose content is wrong.
    Wrong(String),
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::Broke(broke) => write!(f, "{broke}"),
            Fault::Status(status, body) => write!(f, "status {status}: {body}"),
            Fault::Wrong(what) => write!(f, "wrong answer: {what}"),
        }
    }
}

/// A failed request.
#[derive(Clone, Debug)]
pub struct Failure {
    pub client: String,
    pub kind: Kind,
    pub op: String,
    pub fault: Fault,
    /// The run said a restart's gap was open ([`Load::open_gap`]) when the request failed.
    pub in_gap: bool,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.client, self.op, self.fault)
    }
}

/// An object a client was told it wrote.
#[derive(Clone, Debug)]
pub struct Acked {
    pub key: String,
    pub sha256: String,
}

/// What a client did.
#[derive(Debug)]
pub struct Report {
    pub name: String,
    pub kind: Kind,
    /// The client keeps its connection alive.
    pub keep: bool,
    pub requests: u64,
    pub answered: u64,
    pub failures: Vec<Failure>,
    pub acked: Vec<Acked>,
    /// The answers of `/metricz`, by the slot the build gauge names.
    pub slots: BTreeMap<u16, u64>,
    /// Why an OCI client's sequence ended, when it did.
    pub ended: Option<String>,
}

/// What one client has done so far, for [`Load::wait_more`].
#[derive(Clone, Copy, Default)]
struct Progress {
    finished: u64,
    /// An OCI client's sequence has ended.
    ended: bool,
}

struct Shared {
    stop: AtomicBool,
    /// Set by the run while a restart's gap is open: from just before the step that stops the
    /// old slot until that step has returned.
    gap: AtomicBool,
    /// Set by the run just before the step that drains the slot the OCI clients call: an OCI
    /// sequence may end only after it.
    draining: AtomicBool,
    /// Each client's progress, in the order of the specs.
    progress: Mutex<Vec<Progress>>,
    changed: Condvar,
}

impl Shared {
    fn record(&self, index: usize, ended: bool) {
        let mut progress = self.progress.lock().expect("the progress");
        progress[index].finished += 1;
        progress[index].ended |= ended;
        self.changed.notify_all();
    }
}

/// The running clients.
pub struct Load {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<Report>>,
}

/// The clients of a run whose OCI front is that of `slot`: eight S3 clients and four OCI clients,
/// half kept-alive and half on new connections, in both namespaces, on IPv4 and IPv6.
pub fn clients(names: &Names, slot: u16) -> Vec<Spec> {
    let at = |ip: &str, port: u16| -> SocketAddr {
        format!(
            "{}:{port}",
            if ip.contains(':') {
                format!("[{ip}]")
            } else {
                ip.to_owned()
            }
        )
        .parse()
        .expect("an address")
    };
    let oci = oci_port(slot);
    let remote = Some(names.remote.clone());
    let spec = |name: &str, kind, keep, addr, remote: &Option<String>| Spec {
        name: name.to_owned(),
        kind,
        keep,
        addr,
        remote: remote.clone(),
    };
    vec![
        spec(
            "s3-local-keep-v4",
            Kind::S3,
            true,
            at("127.0.0.1", PUBLIC),
            &None,
        ),
        spec(
            "s3-local-keep-veth",
            Kind::S3,
            true,
            at(LOCAL_V4, PUBLIC),
            &None,
        ),
        spec(
            "s3-local-new-v4",
            Kind::S3,
            false,
            at("127.0.0.1", PUBLIC),
            &None,
        ),
        spec("s3-local-new-v6", Kind::S3, false, at("::1", PUBLIC), &None),
        spec(
            "s3-remote-keep-v4",
            Kind::S3,
            true,
            at(LOCAL_V4, PUBLIC),
            &remote,
        ),
        spec(
            "s3-remote-keep-v6",
            Kind::S3,
            true,
            at(LOCAL_V6, PUBLIC),
            &remote,
        ),
        spec(
            "s3-remote-new-v4",
            Kind::S3,
            false,
            at(LOCAL_V4, PUBLIC),
            &remote,
        ),
        spec(
            "s3-remote-new-v6",
            Kind::S3,
            false,
            at(LOCAL_V6, PUBLIC),
            &remote,
        ),
        spec(
            "oci-local-keep",
            Kind::Oci,
            true,
            at("127.0.0.1", oci),
            &None,
        ),
        spec(
            "oci-local-new",
            Kind::Oci,
            false,
            at("127.0.0.1", oci),
            &None,
        ),
        spec(
            "oci-remote-keep-v6",
            Kind::Oci,
            true,
            at(LOCAL_V6, oci),
            &remote,
        ),
        spec(
            "oci-remote-new-v4",
            Kind::Oci,
            false,
            at(LOCAL_V4, oci),
            &remote,
        ),
    ]
}

/// The bytes of the `seq`-th object of the client `name`: empty for every sixteenth number, else
/// a size from 1 byte to 32 KiB, the content a digest of the name and the number, repeated.
pub fn object(name: &str, seq: u64) -> Vec<u8> {
    if seq % 16 == 1 {
        return Vec::new();
    }
    let seed = Sha256::digest(format!("{name}/{seq}").as_bytes());
    let size = 1 + usize::from(u16::from_be_bytes([seed[0], seed[1]])) % (32 << 10);
    seed.iter().copied().cycle().take(size).collect()
}

/// The hex SHA-256 of `bytes`.
pub fn hex_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A client's connection: the one it keeps, or a new one.
struct Client {
    spec: Spec,
    kept: Option<Conn>,
    report: Report,
    shared: Arc<Shared>,
}

impl Client {
    fn fail(&mut self, op: &str, fault: Fault) {
        self.report.failures.push(Failure {
            client: self.spec.name.clone(),
            kind: self.spec.kind,
            op: op.to_owned(),
            fault,
            in_gap: self.shared.gap.load(Ordering::SeqCst),
        });
    }

    /// One request: on the kept connection, or on a new one. The connection is kept after the
    /// answer when the client keeps connections and the answer does not say `Connection: close`.
    fn exchange(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Answer, Broke> {
        self.report.requests += 1;
        let mut conn = match self.kept.take() {
            Some(conn) => conn,
            None => Conn::open(self.spec.addr)?,
        };
        let answer = conn.exchange(method, path, headers, body, method == "HEAD")?;
        self.report.answered += 1;
        if self.spec.keep && !answer.closes() {
            self.kept = Some(conn);
        }
        Ok(answer)
    }

    fn host(&self) -> Vec<(String, String)> {
        vec![("host".to_owned(), self.spec.addr.to_string())]
    }

    /// The S3 client's `seq`-th request.
    fn s3(&mut self, seq: u64) {
        let host = self.spec.addr.to_string();
        let read = (seq % 4 == 3)
            .then_some(self.report.acked.len())
            .filter(|&n| n > 0);
        if seq.is_multiple_of(4) {
            let op = "GET /metricz";
            match self.exchange("GET", "/metricz", &self.host(), b"") {
                Ok(answer) if answer.status == 200 => {
                    let text = String::from_utf8_lossy(&answer.body);
                    match build_of(&text).and_then(|build| build.slot) {
                        Some(slot) => *self.report.slots.entry(slot).or_default() += 1,
                        None => self.fail(op, Fault::Wrong("no slot in the build gauge".into())),
                    }
                }
                Ok(answer) => self.fail(op, status(&answer)),
                Err(broke) => self.fail(op, Fault::Broke(broke)),
            }
        } else if let Some(count) = read {
            let acked = self.report.acked[usize::try_from(seq / 4).expect("small") % count].clone();
            let op = format!("GET {}", acked.key);
            let headers = signed("GET", &host, &acked.key, b"");
            match self.exchange("GET", &format!("/{BUCKET}/{}", acked.key), &headers, b"") {
                Ok(answer) if answer.status == 200 => {
                    if hex_sha256(&answer.body) != acked.sha256 {
                        self.fail(&op, Fault::Wrong("not the bytes written".into()));
                    }
                }
                Ok(answer) => self.fail(&op, status(&answer)),
                Err(broke) => self.fail(&op, Fault::Broke(broke)),
            }
        } else {
            let key = format!("t1/{}/{seq}", self.spec.name);
            let body = object(&self.spec.name, seq);
            let op = format!("PUT {key}");
            let headers = signed("PUT", &host, &key, &body);
            match self.exchange("PUT", &format!("/{BUCKET}/{key}"), &headers, &body) {
                Ok(answer) if answer.status == 200 => self.report.acked.push(Acked {
                    key,
                    sha256: hex_sha256(&body),
                }),
                Ok(answer) => self.fail(&op, status(&answer)),
                Err(broke) => self.fail(&op, Fault::Broke(broke)),
            }
        }
    }

    /// The OCI client's `seq`-th request; false once its sequence has ended. Its sequence may
    /// end only on the drain of its slot: an end before the run said the drain begins is a
    /// failure.
    fn oci(&mut self, seq: u64, registry: &Registry) -> bool {
        let mut reads = vec![
            ("GET", String::from("/v2/"), None),
            ("HEAD", String::from("/v2/"), None),
        ];
        let (hex, size) = &registry.manifest;
        for method in ["GET", "HEAD"] {
            let path = format!("/v2/t1/app/manifests/sha256:{hex}");
            reads.push((method, path, Some((hex.clone(), *size))));
        }
        for (hex, size) in &registry.blobs {
            for method in ["GET", "HEAD"] {
                let path = format!("/v2/t1/app/blobs/sha256:{hex}");
                reads.push((method, path, Some((hex.clone(), *size))));
            }
        }
        let n = u64::try_from(reads.len()).expect("a few");
        let (method, path, expected) = &reads[usize::try_from(seq % n).expect("small")];
        let op = format!("{method} {path}");
        let answer = match self.exchange(method, path, &self.host(), b"") {
            Ok(answer) => answer,
            Err(Broke::BeforeAnswer {
                fresh: false,
                eof: true,
                ..
            }) => return !self.end(&op, "a clean end of stream"),
            Err(broke) => {
                self.fail(&op, Fault::Broke(broke));
                return true;
            }
        };
        if answer.status != 200 {
            self.fail(&op, status(&answer));
        } else if let Some((hex, size)) = expected {
            if answer.length() != Some(*size) {
                self.fail(
                    &op,
                    Fault::Wrong(format!("content-length {:?}", answer.length())),
                );
            }
            let digest = format!("sha256:{hex}");
            if answer.header("docker-content-digest") != Some(digest.as_str()) {
                let said = answer.header("docker-content-digest").map(str::to_owned);
                self.fail(&op, Fault::Wrong(format!("docker-content-digest {said:?}")));
            }
            if *method == "GET" && hex_sha256(&answer.body) != *hex {
                self.fail(&op, Fault::Wrong("not the digest asked for".into()));
            }
        }
        if answer.closes() {
            return !self.end(&op, "Connection: close");
        }
        true
    }

    /// Ends an OCI client's sequence by `why`; a failure if the drain of its slot has not begun.
    /// Always true: the sequence is over either way.
    fn end(&mut self, op: &str, why: &str) -> bool {
        if !self.shared.draining.load(Ordering::SeqCst) {
            let fault = Fault::Wrong(format!("{why} before the drain of its slot began"));
            self.fail(op, fault);
        }
        self.report.ended = Some(why.to_owned());
        true
    }
}

/// The fault of an answer with an unexpected status: the status and the start of the body.
fn status(answer: &Answer) -> Fault {
    let body = String::from_utf8_lossy(&answer.body);
    Fault::Status(answer.status, body.chars().take(200).collect())
}

fn run(index: usize, spec: Spec, registry: &Registry, shared: Arc<Shared>) -> Report {
    if let Some(remote) = &spec.remote {
        netns::enter(remote);
    }
    let report = Report {
        name: spec.name.clone(),
        kind: spec.kind,
        keep: spec.keep,
        requests: 0,
        answered: 0,
        failures: Vec::new(),
        acked: Vec::new(),
        slots: BTreeMap::new(),
        ended: None,
    };
    let mut client = Client {
        spec,
        kept: None,
        report,
        shared: Arc::clone(&shared),
    };
    let mut seq = 0;
    while !shared.stop.load(Ordering::SeqCst) {
        seq += 1;
        let going = match client.spec.kind {
            Kind::S3 => {
                client.s3(seq);
                true
            }
            Kind::Oci => client.oci(seq, registry),
        };
        shared.record(index, !going);
        if !going {
            break;
        }
    }
    client.report
}

impl Load {
    /// Starts the clients `specs`; the OCI clients read `registry`.
    pub fn start(specs: Vec<Spec>, registry: &Registry) -> Load {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            gap: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            progress: Mutex::new(vec![Progress::default(); specs.len()]),
            changed: Condvar::new(),
        });
        let threads = specs
            .into_iter()
            .enumerate()
            .map(|(index, spec)| {
                let (shared, registry) = (Arc::clone(&shared), registry.clone());
                thread::spawn(move || run(index, spec, &registry, shared))
            })
            .collect();
        Load { shared, threads }
    }

    /// Returns once every client has finished `n` more requests, answered or failed, or (an OCI
    /// client) has ended its sequence: each client takes part in each phase, not only the
    /// fastest.
    pub fn wait_more(&self, n: u64) {
        let mut progress = self.shared.progress.lock().expect("the progress");
        let targets: Vec<u64> = progress.iter().map(|p| p.finished + n).collect();
        while !progress
            .iter()
            .zip(&targets)
            .all(|(p, &target)| p.ended || p.finished >= target)
        {
            progress = self.shared.changed.wait(progress).expect("the progress");
        }
    }

    /// Opens (`true`) or closes a restart's gap: the failures in between are marked
    /// [`Failure::in_gap`].
    pub fn gap(&self, open: bool) {
        self.shared.gap.store(open, Ordering::SeqCst);
    }

    /// Says the slot the OCI clients call is about to drain: their sequences may end from now.
    pub fn draining(&self) {
        self.shared.draining.store(true, Ordering::SeqCst);
    }

    /// Stops the clients after the request each is sending, and returns what they did.
    pub fn finish(mut self) -> Vec<Report> {
        self.shared.stop.store(true, Ordering::SeqCst);
        std::mem::take(&mut self.threads)
            .into_iter()
            .map(|thread| thread.join().expect("a client"))
            .collect()
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for thread in std::mem::take(&mut self.threads) {
            let _ = thread.join();
        }
    }
}
