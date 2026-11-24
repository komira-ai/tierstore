//! The binary started as the next image of a build node that handed over to it (`--handed 0`, as
//! a Mac's build node executes itself on SIGUSR2; `docs/design/one-service.md` section 3.4): the
//! offer of the listening socket and of the cache lock is on its standard input, written here by
//! hand as the image before would have left it. The exec itself is made only on macOS, by the
//! integration test of `tests/exec_handover_integration.rs`.

use std::io::{BufRead as _, BufReader, IoSlice};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

use rustix::fs::{FlockOperation, flock};
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
use rustix::process::{Pid, Signal, kill_process};
use tierstore_testkit::reaped::spawn;

use crate::binary::run;
use crate::files::Files;

const BIN: &str = env!("CARGO_BIN_EXE_tierstore");

/// The file of a build node over a memory upstream, its cache in `cache`, serving on loopback.
fn node_file(files: &Files, cache: &std::path::Path) -> String {
    let text = format!(
        "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\n\
         [server]\nproxy_id = \"bin-handed\"\nbucket = \"tier-bucket\"\naccess_key_file = \"{key}\"\n\n\
         [local_cache]\npath = \"{cache}\"\nbudget = \"1MiB\"\n\n\
         [cold_store]\nkind = \"memory\"\n\n[[policy]]\nprefix = \"cas/\"\ncache = true\n",
        key = files.key.display(),
        cache = cache.display(),
    );
    files.secret("node.toml", &text).display().to_string()
}

/// The end of a pair the frame of `lines` was written into, with `fds` on it, and the other end
/// closed: what an image leaves its next one on its standard input.
fn offer(lines: &[String], fds: &[BorrowedFd<'_>]) -> OwnedFd {
    let (near, far) = UnixStream::pair().unwrap();
    let mut bytes = lines.join("\n").into_bytes();
    bytes.extend_from_slice(b"\n\n");
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    assert!(control.push(SendAncillaryMessage::ScmRights(fds)));
    let sent = sendmsg(
        &near,
        &[IoSlice::new(&bytes)],
        &mut control,
        SendFlags::empty(),
    )
    .unwrap();
    assert_eq!(sent, bytes.len());
    far.into()
}

/// The answer to an unsigned GET of `/readyz` at `addr`.
async fn readyz(addr: &str) -> String {
    let reply = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
    reply.text().await.unwrap()
}

/// Asserts that a build node started with `--handed 0` serves on the listening socket of the offer
/// on its standard input (its start line names that socket's address, and a client that connected
/// to it before the start is answered), and becomes the cache's writer with the lock of the offer,
/// which no tick of its own could have taken while the offer held it: `/readyz` says
/// `ready read-write`. Then SIGTERM ends it with status 0.
#[tokio::test]
async fn a_build_node_handed_an_offer_serves_its_socket_and_takes_its_lock() {
    let files = Files::new("bin-handed");
    let cache = files.key.with_file_name("cache");
    std::fs::create_dir(&cache).unwrap();
    let lock = std::fs::File::create(cache.join(".lock")).unwrap();
    flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let lines = [
        "offer".to_owned(),
        format!("s3 {addr}"),
        format!("lock {}", cache.join(".lock").display()),
    ];
    let handed = offer(&lines, &[listener.as_fd(), lock.as_fd()]);
    // What the exec closes: the old image's own descriptors. The offer holds the socket and the lock.
    drop((listener, lock));
    let mut early = std::net::TcpStream::connect(addr).unwrap();

    let config = node_file(&files, &cache);
    let mut command = Command::new(BIN);
    command
        .args([
            "--config",
            &config,
            "--drain-lb-wait",
            "50ms",
            "--handed",
            "0",
        ])
        .stdin(Stdio::from(handed))
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = spawn(&mut command, 3).unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let listen = line
        .split_whitespace()
        .find_map(|word| word.strip_prefix("listen="));
    assert_eq!(listen, Some(addr.to_string().as_str()), "{line}");

    let early = tokio::task::spawn_blocking(move || {
        use std::io::{Read as _, Write as _};
        let ask = b"GET /readyz HTTP/1.1\r\nhost: tier.test\r\nconnection: close\r\n\r\n";
        early.write_all(ask).unwrap();
        let mut answer = String::new();
        let _ = early.read_to_string(&mut answer);
        answer
    });
    let early = early.await.unwrap();
    assert!(early.starts_with("HTTP/1.1 200 OK"), "{early}");
    let mut said = readyz(&addr.to_string()).await;
    for _ in 0..10_000 {
        if said == "ready read-write" {
            break;
        }
        tokio::task::yield_now().await;
        said = readyz(&addr.to_string()).await;
    }
    assert_eq!(said, "ready read-write");

    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(0));
}

/// Asserts that a build node handed an offer whose sockets are not the ones its file names (two
/// where it serves one address) refuses it: the start fails with status 1 and says why (the start
/// that launchd makes next binds the address itself).
#[test]
fn a_build_node_refuses_an_offer_of_other_sockets_with_status_1() {
    let files = Files::new("bin-handed-refused");
    let cache = files.key.with_file_name("cache");
    let one = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let two = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (a, b) = (one.local_addr().unwrap(), two.local_addr().unwrap());
    let lines = ["offer".to_owned(), format!("s3 {a}"), format!("s3 {b}")];
    let handed = offer(&lines, &[one.as_fd(), two.as_fd()]);
    drop((one, two));
    let config = node_file(&files, &cache);
    let ran = Command::new(BIN)
        .args(["--config", &config, "--handed", "0"])
        .stdin(Stdio::from(handed))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&ran.stderr);
    assert_eq!(ran.status.code(), Some(1), "{err}");
    assert!(err.contains("the running instance listens on"), "{err}");
}

/// Asserts that `--handed` names descriptor 0 alone, and that only a build node takes it: another
/// number is a usage error, and a storage node's file is refused; both with status 2 and nothing
/// started.
#[test]
fn handed_is_descriptor_0_of_a_build_node_or_refused_with_status_2() {
    let files = Files::new("bin-handed-usage");
    let cache = files.key.with_file_name("cache");
    let config = node_file(&files, &cache);
    let (code, out, err) = run(&["--config", &config, "--handed", "3"].map(str::to_owned));
    assert_eq!((code, out.as_str()), (2, ""), "{err}");
    assert!(err.contains("--handed"), "{err}");
    let (code, out, err) = run(&files.flags(&["--handed", "0"]));
    assert_eq!((code, out.as_str()), (2, ""), "{err}");
    assert!(
        err.contains("--handed: only a build node (a file with [local_cache] and [cold_store])"),
        "{err}"
    );
}
