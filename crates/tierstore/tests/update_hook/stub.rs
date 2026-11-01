//! The instances as the hook sees them: a stub HTTP service on the two private ports and, when a
//! test asks for it, on the public port 9300.
//!
//! A slot answers only while the fake `systemctl` has its unit active; otherwise the connection
//! is closed unanswered, as a port nothing listens on would leave the hook with no answer.
//! `/readyz` follows the cache lock: the active unit that started first holds it and says `ready
//! read-write`, any other says `ready read-only`. `/metricz` is the server's own rendering of the
//! build gauge (`build_info::gauge`) for the version of the release the unit runs and the slot's
//! port. The public port answers as the slot the fake `nft`'s redirect table names, which is what
//! the redirect would do. A test may script a slot: silent, or another `/readyz` or `/metricz`
//! body.
//!
//! The public port is fixed in the hook, so the tests that serve it take an exclusive lock on a
//! file first, and run one at a time.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tierstore::server::build_info::gauge;
use tierstore::server::update_hook::PUBLIC_PORT;
use tierstore_testkit::port::Reserved;

use crate::world;

/// The commit the stub's build gauge names.
pub const COMMIT: &str = "0123456789ab";
/// The file the tests that serve the public port lock, in the temporary directory.
const PUBLIC_LOCK: &str = "tierstore-update-hook-public-port.lock";

/// What a test makes one slot say instead of what it would.
#[derive(Clone, Debug, Default)]
pub struct SlotScript {
    /// Close every connection unanswered, as if the instance did not listen.
    pub silent: bool,
    /// The body of `/readyz`.
    pub readyz: Option<String>,
    /// The body of `/metricz`.
    pub metricz: Option<String>,
}

struct Shared {
    world: PathBuf,
    scripts: Mutex<BTreeMap<u16, SlotScript>>,
    stop: AtomicBool,
}

impl Shared {
    /// The answer of the instance on `port` to `GET path`; `None` closes the connection.
    fn answer(&self, port: u16, path: &str) -> Option<(u16, String)> {
        let units = world::units(&self.world);
        let unit = units.get(&port)?;
        let script = self
            .scripts
            .lock()
            .unwrap()
            .get(&port)
            .cloned()
            .unwrap_or_default();
        if script.silent {
            return None;
        }
        let holder = units
            .iter()
            .min_by_key(|(_, unit)| unit.seq)
            .map(|(port, _)| *port);
        match path {
            "/readyz" => {
                let mode = if holder == Some(port) {
                    "read-write"
                } else {
                    "read-only"
                };
                let body = script.readyz.unwrap_or_else(|| format!("ready {mode}\n"));
                Some((200, body))
            }
            "/metricz" => {
                let version = fs::read_to_string(unit.release.join("version")).unwrap_or_default();
                let body = script
                    .metricz
                    .unwrap_or_else(|| gauge(version.trim(), COMMIT, Some(port)));
                Some((200, body))
            }
            _ => Some((404, "not found\n".to_owned())),
        }
    }
}

/// Reads one request and answers it as the instance on `slot`, or, for the public port (`None`),
/// as the instance the redirect names.
fn handle(shared: &Shared, slot: Option<u16>, mut stream: TcpStream) {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&head);
    let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let port = slot.or_else(|| world::redirect(&shared.world));
    let Some((status, body)) = port.and_then(|port| shared.answer(port, &path)) else {
        return;
    };
    let reply = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Not Found" },
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes());
}

fn serve(shared: &Shared, slot: Option<u16>, listener: &TcpListener) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(stream) = stream {
            handle(shared, slot, stream);
        }
    }
}

/// The running stub: stopped, and the public port's lock released, when dropped.
pub struct Stub {
    shared: Arc<Shared>,
    addrs: Vec<SocketAddr>,
    threads: Vec<JoinHandle<()>>,
    _public: Option<File>,
}

impl Stub {
    /// Serves the two slots on the ports `slots` hold, reading the world of the fakes in
    /// `world`, and the public port too when `public` is set.
    pub fn start(world: PathBuf, slots: [Reserved; 2], public: bool) -> Stub {
        let shared = Arc::new(Shared {
            world,
            scripts: Mutex::new(BTreeMap::new()),
            stop: AtomicBool::new(false),
        });
        let mut listeners: Vec<(Option<u16>, TcpListener)> = slots
            .into_iter()
            .map(|held| (Some(held.port()), held.listen()))
            .collect();
        let lock = public.then(|| {
            let file = File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(std::env::temp_dir().join(PUBLIC_LOCK))
                .expect("the lock file of the public port");
            file.lock().expect("the lock of the public port");
            let listener = TcpListener::bind(("127.0.0.1", PUBLIC_PORT)).unwrap_or_else(|e| {
                panic!("the public port {PUBLIC_PORT} must be free for this test: {e}")
            });
            listeners.push((None, listener));
            file
        });
        let mut addrs = Vec::new();
        let mut threads = Vec::new();
        for (slot, listener) in listeners {
            addrs.push(listener.local_addr().expect("an address"));
            let shared = Arc::clone(&shared);
            threads.push(std::thread::spawn(move || {
                serve(&shared, slot, &listener);
            }));
        }
        Stub {
            shared,
            addrs,
            threads,
            _public: lock,
        }
    }

    /// Makes the instance on `port` answer as `script` says.
    pub fn script(&self, port: u16, script: SlotScript) {
        self.shared.scripts.lock().unwrap().insert(port, script);
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for addr in &self.addrs {
            let _ = TcpStream::connect(addr);
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
