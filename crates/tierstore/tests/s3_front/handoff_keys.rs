//! The serve entry point (`run_serving`, which the binary runs) with and without the two
//! hand-off keys of `[server]`: `handoff_socket` and `handoff_wait`.
//!
//! Without the keys the start binds as before and makes no control socket; these tests run on
//! every system. The tests with the keys are in `linux` below: they run the hand-off, which the
//! macOS job does not run (an update on a Mac is a restart, and the macOS package never sets the
//! key), so they are compiled for Linux only, which is where the coverage run measures them.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use tierstore::server::Drained;
use tierstore::server::cli::{Addresses, Cli, run_serving};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use crate::binary::run;
use crate::files::{Files, Tables};

/// What a client sends to ask `/healthz`, keeping the connection alive.
const HEALTHZ: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: tier.test\r\n\r\n";

/// The settings of a start with these tables, with a drain that waits no longer than it must.
fn settings(files: &Files, tables: &Tables) -> Cli {
    let mut cli = Cli::from_config_text(&files.text(tables)).expect("a file");
    cli.drain_lb_wait = Duration::from_millis(50);
    cli
}

/// Reads one answer to [`HEALTHZ`] from `connection`: its head and its body `ok`.
async fn answer_of(connection: &mut TcpStream) -> String {
    let mut answer = Vec::new();
    let mut piece = [0_u8; 1024];
    while !answer.ends_with(b"\r\n\r\nok") {
        let read = connection.read(&mut piece).await.expect("read");
        assert_ne!(read, 0, "{:?}", String::from_utf8_lossy(&answer));
        answer.extend_from_slice(&piece[..read]);
    }
    String::from_utf8_lossy(&answer).into_owned()
}

/// Asks `/healthz` at `at` on a connection of its own and gives the answer.
async fn healthz(at: std::net::SocketAddr) -> String {
    let mut connection = TcpStream::connect(at).await.expect("connected");
    connection.write_all(HEALTHZ).await.expect("written");
    answer_of(&mut connection).await
}

/// The names in `dir`, and whether any of them is a Unix socket.
fn listing(dir: &Path) -> (BTreeSet<String>, bool) {
    use std::os::unix::fs::FileTypeExt as _;
    let mut names = BTreeSet::new();
    let mut socket = false;
    for entry in std::fs::read_dir(dir).expect("a directory") {
        let entry = entry.expect("an entry");
        socket |= entry.file_type().expect("a type").is_socket();
        names.insert(entry.file_name().to_string_lossy().into_owned());
    }
    (names, socket)
}

/// Serves `cli` through `run_serving` until `work` (given the addresses) is over, then stops it,
/// and gives how the drain ended.
async fn serving<Fut>(cli: &Cli, work: impl FnOnce(Addresses) -> Fut) -> Drained
where
    Fut: std::future::Future<Output = ()>,
{
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let running = run_serving(
        cli,
        async move {
            let _ = stopped.await;
        },
        move |at| drop(told.send(at)),
    );
    let driving = async {
        work(bound.await.expect("served")).await;
        stop.send(()).expect("still serving");
    };
    let (drained, ()) = tokio::join!(running, driving);
    drained.expect("a start")
}

/// Asserts (a) that a file without the hand-off keys serves as before: the settings have no
/// hand-off, the start binds the address the `s3` interface names (127.0.0.1, a port the system
/// picks) and answers `/healthz` 200 on it, and a stop drains with nothing cut. Catches: a start
/// that asks for a hand-off the file does not ask for, or binds elsewhere. Runs on every system.
#[tokio::test]
async fn without_the_keys_the_start_binds_and_serves_as_before() {
    let files = Files::new("handoff-keys-absent");
    let cli = settings(&files, &Tables::default());
    assert_eq!(cli.handoff, None);
    let drained = serving(&cli, |at| async move {
        assert_eq!(at.s3.ip().to_string(), "127.0.0.1");
        assert_ne!(at.s3.port(), 0);
        assert_eq!(at.s3_all, vec![at.s3]);
        let answer = healthz(at.s3).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    })
    .await;
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts (f) that a start without the hand-off keys never reaches the hand-off: nothing is made
/// in the node's state directory (here the scratch directory that holds the key file and
/// `hot_store.lock_file`) but the lock file, while it serves and after its drain, and no Unix
/// socket is among them. Catches: a control socket made, or the hand-off asked, with the key left
/// out. The choice depends on the settings alone, so this runs on every system.
#[tokio::test]
async fn without_the_keys_the_start_makes_no_control_socket() {
    let files = Files::new("handoff-keys-untouched");
    let state = files.key.parent().expect("a directory").to_path_buf();
    let lock = state.join("instance.lock");
    let tables = Tables {
        hot: format!(
            "kind = \"memory\"\nbudget = 1048576\nlock_file = {:?}",
            lock.display().to_string()
        ),
        ..Tables::default()
    };
    let cli = settings(&files, &tables);
    assert_eq!(cli.handoff, None);
    let expected: BTreeSet<String> = ["instance.lock", "key"].map(str::to_owned).into();
    let drained = serving(&cli, |at| {
        let state = state.clone();
        let expected = expected.clone();
        async move {
            let answer = healthz(at.s3).await;
            assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
            assert_eq!(listing(&state), (expected, false));
        }
    })
    .await;
    assert_eq!(drained, Drained { abandoned: 0 });
    assert_eq!(listing(&state), (expected, false));
}

/// Asserts (e) that the binary refuses a hand-off key with a value it cannot use before it opens
/// anything: status 2, nothing on stdout, and the refusal naming the key. Catches: a bad value
/// that reaches the bind, after the proxy is open, or is read past.
#[test]
fn the_binary_refuses_a_handoff_key_it_cannot_use_with_status_2() {
    let files = Files::new("handoff-keys-refused");
    for (lines, want) in [
        (
            "handoff_socket = \"handoff.sock\"",
            "server.handoff_socket: \"handoff.sock\" must be an absolute path",
        ),
        (
            "handoff_socket = \"/run/x.sock\"\nhandoff_wait = \"0s\"",
            "server.handoff_wait: 0ms is not between 1ms and 3600000ms",
        ),
        (
            "handoff_wait = \"5s\"",
            "server.handoff_wait: it is the wait for the running instance of \
             server.handoff_socket, which is not given",
        ),
    ] {
        let tables = Tables {
            server: lines.to_owned(),
            ..Tables::default()
        };
        let (code, out, err) = run(&files.flags_of(&tables, &[]));
        assert_eq!((code, out.as_str()), (2, ""), "{lines}: {err}");
        assert!(err.contains(want), "{lines}: {err}");
    }
}

/// The tests that run the hand-off. Linux only: see the module's comment.
#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use tierstore::server::Drained;
    use tierstore::server::cli::{Addresses, Cli, HandingError, run_serving};
    use tierstore::server::handoff::{Handoff, HandoffError};
    use tierstore_testkit::dir::TestDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UnixListener, UnixStream};
    use tokio::sync::oneshot;

    use super::{HEALTHZ, answer_of, healthz, serving, settings};
    use crate::files::{Files, Tables};

    /// A scratch directory for the control socket, and the socket's path in it. The directory is
    /// under the system's temporary directory, whose path is short enough for a Unix socket.
    fn state() -> (TestDir, PathBuf) {
        let dir = TestDir::under(std::env::temp_dir(), "handoff-keys");
        let socket = dir.path().join("control.sock");
        (dir, socket)
    }

    /// The settings of a start that hands off over `socket`, waiting `wait` (a key's text).
    fn handing(files: &Files, socket: &Path, wait: &str) -> Cli {
        let tables = Tables {
            server: format!(
                "handoff_socket = {:?}\nhandoff_wait = \"{wait}\"",
                socket.display().to_string()
            ),
            ..Tables::default()
        };
        settings(files, &tables)
    }

    /// Asserts that `socket` is a Unix socket of mode 0600.
    fn assert_control_socket(socket: &Path) {
        let meta = std::fs::symlink_metadata(socket).expect("the control socket");
        assert!(meta.file_type().is_socket(), "{}", socket.display());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    /// The hand-off error `error` holds, if it is one.
    fn handoff_error(error: &tierstore::server::cli::Error) -> Option<&HandoffError> {
        match error.downcast_ref::<HandingError>()? {
            HandingError::Handoff(why) => Some(why),
            _ => None,
        }
    }

    /// Asserts (b) the switch from the keys alone: two starts of one file that sets
    /// `server.handoff_socket`, in this process. The second takes the first's listening socket
    /// (it says the first's address though its file names port 0) and answers the control socket
    /// in its place; the first drains with nothing cut. A client connected to the first before the
    /// switch, with a request half written when the second takes over, gets its answer; a client
    /// that connects after the switch, and one after the first has ended, get theirs from the
    /// same address. Catches: a serve path that binds beside the
    /// running instance instead of taking its socket, one that leaves the control socket unmade
    /// for the next update, and a gap in which a connection is refused or a request dropped.
    #[tokio::test]
    async fn a_second_start_on_the_socket_takes_the_listener_with_no_gap() {
        let files = Files::new("handoff-keys-switch");
        let (_dir, socket) = state();
        let cli = handing(&files, &socket, "1h");
        let (old_told, old_bound) = oneshot::channel::<Addresses>();
        let (new_told, new_bound) = oneshot::channel::<Addresses>();
        let (old_done, old_ended) = oneshot::channel::<()>();
        let (go, going) = oneshot::channel::<()>();
        let (stop_old, stopped_old) = oneshot::channel::<()>();
        let (stop_new, stopped_new) = oneshot::channel::<()>();
        let old = async {
            let stopped = async move {
                let _ = stopped_old.await;
            };
            let drained = run_serving(&cli, stopped, move |at| drop(old_told.send(at))).await;
            old_done.send(()).expect("the test waits");
            drained
        };
        let new = async {
            going.await.expect("told to start");
            let stopped = async move {
                let _ = stopped_new.await;
            };
            run_serving(&cli, stopped, move |at| drop(new_told.send(at))).await
        };
        let driving = async {
            let at = old_bound.await.expect("the first serves");
            assert_control_socket(&socket);
            let mut before = TcpStream::connect(at.s3).await.expect("connected");
            before.write_all(HEALTHZ).await.expect("written");
            let first = answer_of(&mut before).await;
            assert!(first.starts_with("HTTP/1.1 200"), "{first}");
            let (head, rest) = HEALTHZ.split_at(16);
            before.write_all(head).await.expect("written");
            go.send(()).expect("the second waits");

            let taken = new_bound.await.expect("the second serves");
            assert_eq!(taken, at, "the second serves the very socket");
            let after = healthz(at.s3).await;
            assert!(after.starts_with("HTTP/1.1 200"), "{after}");
            before.write_all(rest).await.expect("written");
            let late = answer_of(&mut before).await;
            assert!(late.starts_with("HTTP/1.1 200"), "{late}");
            // The first instance's drain holds a connection it closed until the client closes
            // its end.
            drop(before);

            old_ended.await.expect("the first ends");
            for _ in 0..3 {
                let alone = healthz(at.s3).await;
                assert!(alone.starts_with("HTTP/1.1 200"), "{alone}");
            }
            assert_control_socket(&socket);
            stop_new.send(()).expect("the second serves");
            drop(stop_old);
        };
        let (old, new, ()) = tokio::join!(old, new, driving);
        assert_eq!(old.expect("the first"), Drained { abandoned: 0 });
        assert_eq!(new.expect("the second"), Drained { abandoned: 0 });
        assert!(
            !socket.exists(),
            "a start that stops with no successor removes its control socket"
        );
    }

    /// Asserts (c) the cold start: with no file at `server.handoff_socket`, and with a file there
    /// that no process listens on (what a killed instance leaves), the start binds its own socket
    /// and serves, and makes the control socket at the path (mode 0600, replacing the stale
    /// file), and a stop removes it. Catches: a start that fails, or waits out
    /// `server.handoff_wait`, where nothing answers, and one that leaves the stale file in place.
    #[tokio::test]
    async fn with_no_instance_on_the_socket_the_start_binds_its_own() {
        let files = Files::new("handoff-keys-cold");
        for stale in [false, true] {
            let (_dir, socket) = state();
            if stale {
                drop(UnixListener::bind(&socket).expect("a listener"));
                assert!(socket.exists(), "a file no one listens on");
            }
            let cli = handing(&files, &socket, "1h");
            let path = socket.clone();
            let drained = serving(&cli, |at| async move {
                let answer = healthz(at.s3).await;
                assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
                assert_control_socket(&path);
            })
            .await;
            assert_eq!(drained, Drained { abandoned: 0 }, "stale: {stale}");
            assert!(!socket.exists(), "stale: {stale}");
        }
    }

    /// Asserts (c) that a running instance that accepts the connection and never answers fails
    /// the start with the named error, `HandingError::Handoff(HandoffError::Timeout)` of the
    /// socket's path, once `server.handoff_wait` is over, and that the start binds nothing (its
    /// `ready` is never called): a bind beside an instance that may be alive would fight it for
    /// the port. The clock is tokio's paused one: the wait of 30 seconds ends when every task
    /// waits, with no real time passing. Catches: a silent cold start where an instance is there,
    /// and a start that waits for ever.
    #[tokio::test(start_paused = true)]
    async fn an_instance_that_never_answers_fails_the_start_after_the_wait() {
        let files = Files::new("handoff-keys-silent");
        let (_dir, socket) = state();
        let silent = UnixListener::bind(&socket).expect("a listener");
        let cli = handing(&files, &socket, "30s");
        assert_eq!(
            cli.handoff,
            Some(Handoff {
                socket: socket.clone(),
                wait: Duration::from_secs(30),
            })
        );
        let error = run_serving(&cli, async {}, |at| panic!("bound {at:?}"))
            .await
            .expect_err("the start fails");
        match handoff_error(&error) {
            Some(HandoffError::Timeout(path)) => assert_eq!(path, &socket),
            other => panic!("{other:?}: {error}"),
        }
        assert_eq!(
            error.to_string(),
            format!(
                "hand-off: no end of the hand-off at {} in time",
                socket.display()
            )
        );
        drop(silent);
    }

    /// Asserts (d) that while one taker is at the running instance's control socket (it asked and
    /// holds the offer), a start on the same socket is refused with the reason in the exact words,
    /// binds nothing, and leaves the running instance serving; once that taker goes away, the
    /// running instance stops on its own stop with nothing cut. Its control socket is mode 0600.
    /// Catches: two takers handed the same sockets, and a refused start that binds anyway.
    #[tokio::test]
    async fn a_second_taker_is_refused_while_one_is_at_it() {
        let files = Files::new("handoff-keys-second");
        let (_dir, socket) = state();
        let cli = handing(&files, &socket, "1h");
        let (told, bound) = oneshot::channel::<Addresses>();
        let (stop, stopped) = oneshot::channel::<()>();
        let running = run_serving(
            &cli,
            async move {
                let _ = stopped.await;
            },
            move |at| drop(told.send(at)),
        );
        let driving = async {
            let at = bound.await.expect("serving");
            assert_control_socket(&socket);
            let mut first = UnixStream::connect(&socket).await.expect("connected");
            first.write_all(b"take 1\n\n").await.expect("asked");
            let mut offer = Vec::new();
            let mut piece = [0_u8; 1024];
            while !offer.ends_with(b"\n\n") {
                let read = first.read(&mut piece).await.expect("read");
                assert_ne!(read, 0, "{:?}", String::from_utf8_lossy(&offer));
                offer.extend_from_slice(&piece[..read]);
            }
            let offer = String::from_utf8_lossy(&offer).into_owned();
            assert_eq!(offer, format!("offer\ns3 {}\n\n", at.s3));

            let refused = run_serving(&cli, async {}, |at| panic!("bound {at:?}"))
                .await
                .expect_err("refused");
            assert!(
                matches!(handoff_error(&refused), Some(HandoffError::Refused(_))),
                "{refused}"
            );
            assert_eq!(
                refused.to_string(),
                "hand-off: the running instance refused: another taker is taking the sockets"
            );
            let answer = healthz(at.s3).await;
            assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
            drop(first);
            stop.send(()).expect("serving");
        };
        let (drained, ()) = tokio::join!(running, driving);
        assert_eq!(drained.expect("a start"), Drained { abandoned: 0 });
        assert!(!socket.exists());
    }

    /// The file of a build node over a memory upstream, with its cache in `cache_dir` and the
    /// control socket at `socket`.
    fn build_node_file(files: &Files, cache_dir: &Path, socket: &Path) -> String {
        let text = format!(
            r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[server]
proxy_id        = "bin-cache"
bucket          = "tier-bucket"
access_key_file = "{key}"
handoff_socket  = "{socket}"

[local_cache]
path   = "{cache}"
budget = "1MiB"

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#,
            key = files.key.display(),
            cache = cache_dir.display(),
            socket = socket.display(),
        );
        files.secret("node.toml", &text).display().to_string()
    }

    /// Asserts that the binary started from the file of a build node with `server.handoff_socket`
    /// starts (it is not refused with status 2), serves `/healthz`, makes the control socket
    /// while it runs, removes it on SIGTERM and exits with status 0. Catches a build node that
    /// refuses the key.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_build_node_binary_with_the_socket_key_makes_the_control_socket() {
        use rustix::process::Signal;
        let files = Files::new("handoff-keys-binary");
        let (dir, socket) = state();
        let cache_dir = dir.path().join("cache");
        let config = build_node_file(&files, &cache_dir, &socket);
        let args = ["--config", config.as_str(), "--drain-lb-wait", "50ms"]
            .map(str::to_owned)
            .to_vec();
        let mut running = crate::binary::start(&args);
        let answer = healthz(running.addr().parse().expect("an address")).await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert_control_socket(&socket);
        running.signal(Signal::TERM);
        assert_eq!(running.exit_code().await, Some(0));
        assert!(!socket.exists());
    }
}
