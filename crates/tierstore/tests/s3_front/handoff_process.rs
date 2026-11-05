//! Two `tierstore` processes handing their sockets over under load.
//!
//! The one test here runs the built binary twice over one file of the combined shape (an S3 hot
//! tier, a directory cold tier, a cache directory, an S3 and an OCI interface), the second while
//! the first serves, with requests counted all through and two transfers held open across the
//! hand-off. What the processes do is judged by what they answer and how they end; nothing is
//! asserted about how long anything takes.

#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use bytes::Bytes;
    use objstore_s3::s3::fake::FakeS3;
    use objstore_s3::{ByteRange, Capabilities, ObjectKey, ObjectStore};
    use reqwest::Client;
    use rustix::process::{Pid, Signal, getuid, kill_process};
    use sha2::{Digest as _, Sha256};
    use tierstore::server::build_info::VERSION;
    use tierstore::{Clock as _, SystemClock};
    use tierstore_testkit::dir::TestDir;
    use tierstore_testkit::reaped::{Reaped, first_line, spawn};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpStream;

    use crate::files::{BIN_KEY_ID, BIN_SECRET, oci_interface};
    use crate::signer::{Request, sign};

    const BIN: &str = env!("CARGO_BIN_EXE_tierstore");

    /// Bytes of the transfers that are held open across the hand-off.
    const BLOB_BYTES: usize = 32 << 20;
    const PUT_BYTES: usize = 4 << 20;
    /// How much of the held PUT the client sends before the hand-off.
    const PUT_SENT: usize = 1 << 20;
    /// How much of the blob the client reads before the hand-off.
    const BLOB_READ: usize = 64 << 10;

    /// The most polls of the second process's `/readyz` after the first has gone; the polls are
    /// spaced, and a test that never sees 200 fails.
    const READY_POLLS: usize = 240;

    /// A running binary and what it printed on its start line.
    struct Node {
        child: Reaped,
        line: String,
        _stdout: std::io::BufReader<std::process::ChildStdout>,
    }

    impl Node {
        fn start(config: &str) -> Self {
            let mut command = Command::new(BIN);
            command
                .args(["--config", config])
                .args(["--drain-lb-wait", "50ms"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut child = spawn(&mut command, 3).unwrap();
            let stdout = child.stdout.take().unwrap();
            let (line, reader) = first_line(stdout, Duration::from_secs(30)).unwrap();
            assert!(
                line.starts_with(&format!("tierstore {VERSION} listen=")),
                "{line}"
            );
            Self {
                child,
                line,
                _stdout: reader,
            }
        }

        /// The value of the start line's `key=` word.
        fn word(&self, key: &str) -> &str {
            let prefix = format!("{key}=");
            self.line
                .split_whitespace()
                .find_map(|word| word.strip_prefix(prefix.as_str()))
                .unwrap_or_else(|| panic!("{key} in {}", self.line))
        }

        fn running(&mut self) -> bool {
            self.child.try_wait().unwrap().is_none()
        }

        fn signal(&self, signal: Signal) {
            let pid = Pid::from_raw(i32::try_from(self.child.id()).unwrap()).unwrap();
            kill_process(pid, signal).unwrap();
        }

        /// The exit code, waiting for the process to end on a thread of the blocking pool.
        async fn exit_code(mut self) -> Option<i32> {
            let waiting = tokio::task::spawn_blocking(move || self.child.wait());
            waiting.await.unwrap().unwrap().code()
        }
    }

    /// The head of a put of `body` to `key`, signed for the clock of the machine, with its
    /// content length.
    fn put_head(addr: &str, key: &str, body: &[u8]) -> String {
        let path = format!("/tier-bucket/{key}");
        let stamp = stamp_now();
        let request = Request {
            method: "PUT",
            host: addr,
            path: &path,
            query: &[],
            headers: &[],
            body,
            stamp: &stamp,
        };
        let mut text = format!("PUT {path} HTTP/1.1\r\n");
        for (name, value) in sign(&request, BIN_KEY_ID, BIN_SECRET) {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        text.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
        text
    }

    /// The date header of a request made now: `YYYYMMDDTHHMMSSZ`.
    fn stamp_now() -> String {
        let seconds = SystemClock.now() / 1000;
        let (days, rest) = (seconds / 86_400, seconds % 86_400);
        // The civil date of a day count since 1970-01-01.
        let z = i64::try_from(days).unwrap() + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        format!(
            "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
            rest / 3600,
            rest % 3600 / 60,
            rest % 60
        )
    }

    /// Bytes that differ from place to place, so that a body cut short or shifted is seen.
    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        (0..len)
            .map(|n| {
                let n = u32::try_from(n % (1 << 24)).unwrap() ^ seed;
                (n.wrapping_mul(2_654_435_761) >> 24) as u8
            })
            .collect()
    }

    /// The file of the combined shape: an S3 hot tier at `hot`, a directory cold tier, a cache
    /// directory, an S3 and an OCI interface on loopback port 0, and a control socket.
    fn node_file(root: &Path, hot: &str, socket: &Path, key: &Path, hot_key: &Path) -> String {
        let oci = oci_interface(&["127.0.0.1:0"], "");
        let text = format!(
            r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}
{oci}
[server]
proxy_id          = "handoff-process"
bucket            = "tier-bucket"
access_key_file   = "{key}"
handoff_socket    = "{socket}"
body_idle_timeout = "120s"
put_rate_grace    = "120s"

[hot_store]
kind             = "s3"
bucket           = "tierstore-fake"
endpoint         = "{hot}"
credentials_file = "{hot_key}"
budget           = "64MiB"
lease_ttl        = "5s"
lock_file        = "{lock}"

[cold_store]
kind = "dir"
path = "{cold}"

[local_cache]
path   = "{cache}"
budget = "64MiB"

[[policy]]
prefix = "cas/"
cache  = true

[[policy]]
prefix = "oci/"
cache  = true
"#,
            key = key.display(),
            socket = socket.display(),
            hot_key = hot_key.display(),
            lock = root.join("hot.lock").display(),
            cold = root.join("cold").display(),
            cache = root.join("cache").display(),
        );
        let file = root.join("node.toml");
        std::fs::write(&file, text).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        file.display().to_string()
    }

    /// An `S3Store` for the S3 front at `addr`, with the key of the file.
    fn store_at(addr: &str) -> objstore_s3::s3::S3Store {
        use objstore_s3::s3::{Credentials, S3Config, S3Store};
        let config = S3Config {
            endpoint: format!("http://{addr}"),
            region: "us-east-1".to_owned(),
            bucket: "tier-bucket".to_owned(),
            credentials: Credentials::new(BIN_KEY_ID, BIN_SECRET),
            capabilities: Capabilities {
                conditional_put: true,
                object_lock: false,
            },
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            min_body_rate: None,
        };
        S3Store::new(config).unwrap()
    }

    /// The requests of the counted load, and the errors among them.
    #[derive(Default)]
    struct Counts {
        ok: AtomicUsize,
        errors: Mutex<Vec<String>>,
    }

    /// One keep-alive client that puts an object and reads it back until `stop`; every call
    /// counts, and a call that fails is recorded with its error.
    async fn load(addr: String, name: usize, counts: Arc<Counts>, stop: Arc<AtomicBool>) {
        let store = store_at(&addr);
        let mut round = 0_usize;
        while !stop.load(Ordering::SeqCst) {
            round += 1;
            let key = ObjectKey::new(format!("cas/load/{name}/{round}")).unwrap();
            let body = Bytes::from(pattern(2000 + round, u32::try_from(name).unwrap()));
            let put = store.put_new(&key, body.clone(), None).await;
            let all = ByteRange::new(0, body.len() as u64).unwrap();
            let got = match put {
                Ok(_) => store.get_range(&key, all).await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            match got {
                Ok(read) if read == body => {
                    counts.ok.fetch_add(2, Ordering::SeqCst);
                }
                Ok(_) => counts
                    .errors
                    .lock()
                    .unwrap()
                    .push(format!("{key}: wrong bytes")),
                Err(e) => counts.errors.lock().unwrap().push(format!("{key}: {e}")),
            }
            tokio::task::yield_now().await;
        }
    }

    /// Reads the head of an answer on `stream`, and gives its status line; the rest of the
    /// answer is read to its end.
    async fn status_of(stream: &mut TcpStream) -> String {
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await.unwrap();
        let text = String::from_utf8_lossy(&answer).into_owned();
        text.lines().next().unwrap_or_default().to_owned()
    }

    /// Asks `/readyz` of `addr` on a connection of its own: the status, or None for a refusal.
    async fn readyz(client: &Client, addr: &str) -> Option<u16> {
        let sent = client.get(format!("http://{addr}/readyz")).send().await;
        sent.ok().map(|answer| answer.status().as_u16())
    }

    /// Asserts (T10) that a second `tierstore` process, started from the same file while the
    /// first serves, takes the first's sockets over its control socket and the first drains:
    /// every request of a counted keep-alive load (S3 puts and gets, from before the second
    /// start to its being ready) is answered with the bytes put, a put whose body is half sent
    /// and a blob download half read when the second starts both finish on the first with the
    /// full bytes (compared by hash), the first exits 0 on SIGTERM, the second's start line
    /// names the very addresses of the first and its `/readyz` answers 200 once the first is
    /// gone, the control socket is a socket of mode 0600 owned by this user while the second
    /// runs, and SIGTERM ends the second with status 0 and removes the socket. The test fails
    /// if either transfer has sent or received nothing when the second starts. Catches: a
    /// hand-off that cuts the transfers or the keep-alive connections of the first process, a
    /// start that names other addresses, and a socket left behind or made with other rights.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_second_process_takes_the_sockets_of_the_first_under_load() {
        let fake = FakeS3::start().await;
        let caps = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        fake.store(caps)
            .put_new(
                &ObjectKey::new("_tiering/format").unwrap(),
                Bytes::from_static(br#"{"epoch":2,"phase":"active"}"#),
                None,
            )
            .await
            .unwrap();
        let dir = TestDir::under(std::env::temp_dir(), "ts-hp");
        let root = dir.path().to_path_buf();
        let key = root.join("key");
        std::fs::write(&key, format!("{BIN_KEY_ID}\n{BIN_SECRET}\n")).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let hot_key = root.join("hot.key");
        std::fs::write(&hot_key, "HOTKEY\nhot-secret\n").unwrap();
        std::fs::set_permissions(&hot_key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let socket = root.join("control.sock");
        let config = node_file(&root, fake.endpoint(), &socket, &key, &hot_key);

        let mut first = Node::start(&config);
        let s3_at = first.word("listen").to_owned();
        let oci_at = first.word("oci-listen").to_owned();
        assert!(first.line.contains("mode=storage-cache"), "{}", first.line);
        assert!(
            socket.exists(),
            "the first process makes the control socket"
        );

        // The blob goes in through the S3 front, under the key the registry reads.
        let blob = pattern(BLOB_BYTES, 7);
        let digest = hex::encode(Sha256::digest(&blob));
        let store = store_at(&s3_at);
        let blob_key = ObjectKey::new(format!("oci/blobs/sha256/{digest}")).unwrap();
        store
            .put_new(&blob_key, Bytes::from(blob.clone()), None)
            .await
            .unwrap();

        // The counted load: some calls before anything else, then two clients to the end.
        let counts = Arc::new(Counts::default());
        let stop = Arc::new(AtomicBool::new(false));
        let loads: Vec<_> = (0..2)
            .map(|name| tokio::spawn(load(s3_at.clone(), name, counts.clone(), stop.clone())))
            .collect();

        // The put: its head and a part of its body are sent, and the rest is held back.
        let put_body = pattern(PUT_BYTES, 3);
        let mut put = TcpStream::connect(&s3_at).await.unwrap();
        put.write_all(put_head(&s3_at, "cas/held-put", &put_body).as_bytes())
            .await
            .unwrap();
        put.write_all(&put_body[..PUT_SENT]).await.unwrap();
        put.flush().await.unwrap();
        let put_sent = PUT_SENT;

        // The download: a part of the blob is read, and the rest is left in the socket.
        let downloads = Client::builder().pool_max_idle_per_host(0).build().unwrap();
        let mut download = downloads
            .get(format!("http://{oci_at}/v2/app/blobs/sha256:{digest}"))
            .send()
            .await
            .unwrap();
        assert_eq!(download.status(), 200);
        let mut seen = Sha256::new();
        let mut blob_read = 0_usize;
        while blob_read < BLOB_READ {
            let piece = download
                .chunk()
                .await
                .unwrap()
                .expect("a piece of the blob");
            seen.update(&piece);
            blob_read += piece.len();
        }

        // The gate: a transfer with nothing moved is no proof of anything.
        assert!(put_sent > 0, "no bytes of the put were sent");
        assert!(blob_read > 0, "no bytes of the blob were received");
        assert!(put_sent < PUT_BYTES && blob_read < BLOB_BYTES);

        // The second process takes the sockets, and the first is asked to stop too.
        let second = Node::start(&config);
        assert_eq!(second.word("listen"), s3_at);
        assert_eq!(second.word("oci-listen"), oci_at);
        assert!(first.running(), "the transfers keep the first draining");
        first.signal(Signal::TERM);

        // The overlap: the first answers `draining` and the second serves what comes in.
        let overlap = counts.ok.load(Ordering::SeqCst);
        let mut polls = 0;
        while counts.ok.load(Ordering::SeqCst) < overlap + 40 {
            assert!(
                first.running(),
                "the first process ended with work held open"
            );
            polls += 1;
            let errors = counts.errors.lock().unwrap().clone();
            assert!(polls < 600 && errors.is_empty(), "{overlap} {errors:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // The held transfers finish on the first process.
        put.write_all(&put_body[PUT_SENT..]).await.unwrap();
        let status = status_of(&mut put).await;
        assert!(status.contains(" 200"), "{status}");
        drop(put);
        while let Some(piece) = download.chunk().await.unwrap() {
            seen.update(&piece);
            blob_read += piece.len();
        }
        drop(download);
        assert_eq!(blob_read, BLOB_BYTES);
        assert_eq!(hex::encode(seen.finalize()), digest);

        assert_eq!(first.exit_code().await, Some(0));

        // The second is the only process now: ready once its cache has the lock.
        let client = Client::builder().pool_max_idle_per_host(0).build().unwrap();
        let mut ready = None;
        for _ in 0..READY_POLLS {
            ready = readyz(&client, &s3_at).await;
            if ready == Some(200) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert_eq!(ready, Some(200), "the second process never became ready");

        stop.store(true, Ordering::SeqCst);
        for task in loads {
            task.await.unwrap();
        }
        let errors = counts.errors.lock().unwrap().clone();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(counts.ok.load(Ordering::SeqCst) > 0);

        // What the first took in is all there, through the second.
        let held = store_at(&s3_at);
        let held_key = ObjectKey::new("cas/held-put").unwrap();
        let all = ByteRange::new(0, PUT_BYTES as u64).unwrap();
        let got = held.get_range(&held_key, all).await.unwrap();
        assert_eq!(Sha256::digest(&got), Sha256::digest(&put_body));

        // The control socket is the second's: a socket, for this user alone.
        let meta = std::fs::symlink_metadata(&socket).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.uid(), getuid().as_raw());

        second.signal(Signal::TERM);
        assert_eq!(second.exit_code().await, Some(0));
        assert!(!socket.exists(), "a process that stops removes its socket");
    }
}
