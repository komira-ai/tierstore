//! The offer written into the pair and read by the next image, its arguments, the refusals of an
//! offer that is not the image's, and what an exec of the image carries over: a child process
//! started with the command the exec runs stands for the next image, as a test cannot replace its
//! own.

use std::ffi::OsString;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::fs::FileTypeExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use rustix::io::{FdFlags, fcntl_getfd};

use super::{Handing, Image, Own, handed_input, next_args, read};
use crate::server::handoff::testing::{identity, listening, scratch, socket, within};
use crate::server::handoff::wire;
use crate::server::handoff::{Front, HandoffError, Want};
use crate::server::lock::try_take;

/// The test the child process of [`only_the_handed_descriptor_survives_the_exec`] runs.
const IMAGE_SIDE: &str = "server::handoff::exec::tests::the_image_side_of_the_exec";

/// The words `args` stand for.
fn words(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// The end the next image reads, after `lines` and `fds` were written into the other.
async fn raw(lines: &[&str], fds: &[BorrowedFd<'_>]) -> OwnedFd {
    let (near, far) = UnixStream::pair().unwrap();
    near.set_nonblocking(true).unwrap();
    let near = tokio::net::UnixStream::from_std(near).unwrap();
    let lines: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    wire::send(&near, &lines, fds).await.unwrap();
    far.into()
}

/// Yields until `done` says so, and fails the test (naming `what`) if it never does. A process
/// that another test of this binary starts holds a copy of every descriptor of this process between
/// its fork and its exec, so that a socket or a lock closed here may stay open that long; nothing
/// of this test is judged by how long it waited.
async fn once_no_fork_holds(what: &str, mut done: impl FnMut() -> bool) {
    within(what, async {
        while !done() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// Whether `fd` is close-on-exec.
fn closed_on_exec(fd: BorrowedFd<'_>) -> bool {
    fcntl_getfd(fd).unwrap().contains(FdFlags::CLOEXEC)
}

/// Asserts that what the old image offers reaches the next image through the end of the pair: the
/// image is the program it was given with the arguments of before less their name and with
/// `--handed 0`; the next image takes the very listening sockets (by front, with their addresses),
/// the very control socket and the cache lock, all made close-on-exec; the lock stays locked while
/// only the offer holds it (the old image's descriptors are gone, as at an exec), and is free once
/// the next image's descriptor closes.
#[tokio::test]
async fn the_next_image_reads_the_offer_from_the_handed_descriptor() {
    let (dir, control_path) = scratch();
    let (s3, s3_addr) = listening();
    let (nfs, nfs_addr) = listening();
    let control = UnixListener::bind(&control_path).unwrap();
    let lock_path = dir.path().join(".lock");
    let mut held = try_take(&lock_path).unwrap().unwrap();
    let ids = [s3.as_fd(), nfs.as_fd(), control.as_fd()].map(identity);
    let handing = Handing {
        sockets: vec![socket(Front::S3, &s3), socket(Front::Nfs, &nfs)],
        control: Some((
            control_path.clone(),
            control.as_fd().try_clone_to_owned().unwrap(),
        )),
        lock: Some((lock_path.clone(), held.share().unwrap())),
    };
    let args = words(&["/old/tierstore", "--config", "/etc/tierstore.toml"]);
    let image = Image::prepare(&handing, PathBuf::from("/new/tierstore"), args)
        .await
        .unwrap();
    assert_eq!(image.program(), Path::new("/new/tierstore"));
    assert_eq!(
        image.args(),
        words(&["--config", "/etc/tierstore.toml", "--handed", "0"])
    );
    // What the exec closes: every descriptor of the old image. Its lock was disarmed at `ready`.
    held.disarm();
    drop((handing, held, s3, nfs, control));
    assert!(
        try_take(&lock_path).unwrap().is_none(),
        "the offer holds the lock"
    );

    let want = Want {
        s3: &[s3_addr],
        oci: &[],
        nfs: &[nfs_addr],
    };
    let own = Own {
        control: Some(&control_path),
        lock: Some(&lock_path),
    };
    let taken = within("the offer", read(image.into_handed(), &want, &own))
        .await
        .unwrap();
    let inherited = &taken.inherited;
    assert_eq!(
        (inherited.s3_addrs.clone(), inherited.nfs_addrs.clone()),
        (vec![s3_addr], vec![nfs_addr])
    );
    let control = taken.control.as_ref().unwrap();
    let got = [
        inherited.s3[0].as_fd(),
        inherited.nfs[0].as_fd(),
        control.as_fd(),
    ];
    assert_eq!(got.map(identity), ids);
    assert!(got.into_iter().all(closed_on_exec));
    assert!(taken.inherited.lock.as_ref().unwrap().is_current());
    assert!(
        try_take(&lock_path).unwrap().is_none(),
        "the next image holds the lock"
    );
    drop(taken);
    once_no_fork_holds("a dropped descriptor only closes", || {
        try_take(&lock_path).unwrap().is_some()
    })
    .await;
}

/// Asserts that the next image is given the arguments of the one before less a `--handed` that
/// one was given (in either spelling, with its value), then `--handed 0`: a second hand-over hands
/// over on descriptor 0 again, and the flag never piles up.
#[test]
fn the_next_image_is_given_the_arguments_less_a_handed_of_before() {
    let handed = words(&["--config", "/f", "--handed", "0"]);
    for args in [
        &["tierstore", "--config", "/f"][..],
        &["tierstore", "--config", "/f", "--handed", "0"],
        &["tierstore", "--handed=0", "--config", "/f"],
        &["tierstore", "--handed", "0", "--config", "/f", "--handed=0"],
    ] {
        assert_eq!(next_args(words(args)), handed, "{args:?}");
    }
    assert_eq!(next_args(words(&["tierstore"])), words(&["--handed", "0"]));
}

/// What the next image says of an offer of `lines` and `fds`, given what it wants.
async fn read_raw(
    lines: &[&str],
    fds: &[BorrowedFd<'_>],
    want: &Want<'_>,
    own: &Own<'_>,
) -> Result<super::Handed, HandoffError> {
    let end = raw(lines, fds).await;
    within("the offer", read(end, want, own)).await
}

/// Asserts each refusal of an offer that is not what the next image serves, with the reason it
/// gives, and that a refusal closes what the offer brought: the listening socket it came with is
/// closed (the next start binds the address itself), and so is the lock (which is then free).
#[tokio::test]
async fn an_offer_that_is_not_the_images_is_refused_and_closed_whole() {
    let (dir, control_path) = scratch();
    let lock_path = dir.path().join(".lock");
    let other = dir.path().join("other");
    let (_, elsewhere) = listening();
    let cases: Vec<(&str, Vec<String>, bool, Own<'_>, &str)> = vec![
        (
            "another address",
            vec![],
            true,
            Own {
                control: None,
                lock: None,
            },
            "Mismatch",
        ),
        (
            "another control socket",
            vec![format!("control {}", other.display())],
            false,
            Own {
                control: Some(&control_path),
                lock: None,
            },
            "Mismatch",
        ),
        (
            "a control socket to an image with none",
            vec![format!("control {}", control_path.display())],
            false,
            Own {
                control: None,
                lock: None,
            },
            "Mismatch",
        ),
        (
            "another lock file",
            vec![format!("lock {}", other.display())],
            false,
            Own {
                control: None,
                lock: Some(&lock_path),
            },
            "Mismatch",
        ),
        (
            "a FUSE device",
            vec!["fuse /mnt".to_owned()],
            false,
            Own {
                control: None,
                lock: None,
            },
            "Protocol",
        ),
    ];
    for (case, extra, other_address, own, kind) in cases {
        let (s3, s3_addr) = listening();
        let mut held = try_take(&lock_path).unwrap().unwrap();
        let shared = held.share().unwrap();
        let mut lines = vec!["offer".to_owned(), format!("s3 {s3_addr}")];
        lines.extend(extra);
        let texts: Vec<&str> = lines.iter().map(String::as_str).collect();
        let extra_fd = if texts.len() > 2 {
            vec![shared.as_fd()]
        } else {
            vec![]
        };
        let fds: Vec<BorrowedFd<'_>> = [s3.as_fd()].into_iter().chain(extra_fd).collect();
        let wanted = if other_address { elsewhere } else { s3_addr };
        let want = Want {
            s3: &[wanted],
            oci: &[],
            nfs: &[],
        };
        let end = raw(&texts, &fds).await;
        held.disarm();
        drop((s3, held, shared));
        let refused = within(case, read(end, &want, &own)).await.err().unwrap();
        assert!(
            format!("{refused:?}").starts_with(kind),
            "{case}: {refused:?}"
        );
        let closed = || std::net::TcpStream::connect(s3_addr).is_err();
        once_no_fork_holds(&format!("{case}: the socket is closed"), closed).await;
        let free = || try_take(&lock_path).unwrap().is_some();
        once_no_fork_holds(&format!("{case}: the lock is free"), free).await;
    }
}

/// Asserts the refusals of what is no offer at all: a frame that is not one, an offer whose lines
/// and descriptors do not match, a control line whose descriptor is not a listening Unix socket, a
/// pair whose other end was closed with nothing in it, and a descriptor that is not a socket.
#[tokio::test]
async fn what_is_no_offer_is_refused() {
    let (dir, control_path) = scratch();
    let none = Want {
        s3: &[],
        oci: &[],
        nfs: &[],
    };
    let own = Own {
        control: Some(&control_path),
        lock: None,
    };
    let refused = read_raw(&["refused", "busy"], &[], &none, &own).await.err();
    assert!(
        matches!(refused, Some(HandoffError::Protocol(_))),
        "{refused:?}"
    );
    let (s3, _) = listening();
    let lines = ["offer", "s3 127.0.0.1:1", "s3 127.0.0.1:2"];
    let refused = read_raw(&lines, &[s3.as_fd()], &none, &own).await.err();
    assert!(
        matches!(refused, Some(HandoffError::Protocol(_))),
        "{refused:?}"
    );
    let control = format!("control {}", control_path.display());
    let refused = read_raw(&["offer", &control], &[s3.as_fd()], &none, &own)
        .await
        .err();
    assert!(
        matches!(refused, Some(HandoffError::Io { .. })),
        "{refused:?}"
    );

    let (near, far) = UnixStream::pair().unwrap();
    drop(near);
    let refused = within("an empty pair", read(far.into(), &none, &own))
        .await
        .err();
    assert!(
        matches!(refused, Some(HandoffError::Io { .. })),
        "{refused:?}"
    );
    // An empty file: macOS would let the read wait for it, and it is never ready.
    let file = std::fs::File::create(dir.path().join("not-a-socket")).unwrap();
    let refused = within("a file", read(file.into(), &none, &own)).await.err();
    assert_eq!(
        refused.map(|error| error.to_string()).as_deref(),
        Some("hand-off: reading the handed offer: the handed descriptor is not a stream socket")
    );
}

/// Asserts the boundaries of what an offer may leave out or hold more of: an offer of the sockets
/// alone is taken with no control socket and no lock; a lock offered to an image without a cache is
/// closed (and so free), not taken.
#[tokio::test]
async fn an_offer_may_leave_out_the_control_socket_and_the_lock() {
    let (dir, control_path) = scratch();
    let (s3, s3_addr) = listening();
    let want = Want {
        s3: &[s3_addr],
        oci: &[],
        nfs: &[],
    };
    let own = Own {
        control: Some(&control_path),
        lock: None,
    };
    let line = format!("s3 {s3_addr}");
    let taken = read_raw(&["offer", &line], &[s3.as_fd()], &want, &own)
        .await
        .unwrap();
    assert!(taken.control.is_none() && taken.inherited.lock.is_none());
    assert_eq!(taken.inherited.s3_addrs, [s3_addr]);

    let lock_path = dir.path().join(".lock");
    let mut held = try_take(&lock_path).unwrap().unwrap();
    let shared = held.share().unwrap();
    let lock = format!("lock {}", lock_path.display());
    let end = raw(&["offer", &line, &lock], &[s3.as_fd(), shared.as_fd()]).await;
    held.disarm();
    drop((held, shared));
    let taken = within("the offer", read(end, &want, &own)).await.unwrap();
    assert!(taken.inherited.lock.is_none());
    once_no_fork_holds("the lock is closed, and free", || {
        try_take(&lock_path).unwrap().is_some()
    })
    .await;
}

/// Whether descriptor `n` of this process is open (a look that opens nothing).
fn is_open(n: i32) -> bool {
    std::fs::metadata(format!("/dev/fd/{n}")).is_ok()
}

/// Asserts that across the exec of the next image (here: the start of a process with the very
/// command the exec runs, which carries the same descriptors) none of the old image's descriptors
/// of the hand-over survives (the listening socket, its copy in the offer, the lock's copy, the
/// image's end of the pair under its own number) but the one at descriptor 0, the handed one: the
/// new image finds it a socket, and reads the offer from it with the sockets and the lock in it;
/// and that after the offer comes the frame `keep` with one descriptor, descriptor 0's own socket,
/// which keeps the offer's sockets reachable for macOS's collector of descriptors in flight while
/// nothing else holds them (without it a Mac's next image serves sockets whose accepted
/// connections cannot receive).
#[tokio::test]
async fn only_the_handed_descriptor_survives_the_exec() {
    let (dir, _) = scratch();
    let (s3, s3_addr) = listening();
    let lock_path = dir.path().join(".lock");
    let mut held = try_take(&lock_path).unwrap().unwrap();
    let handing = Handing {
        sockets: vec![socket(Front::S3, &s3)],
        control: None,
        lock: Some((lock_path.clone(), held.share().unwrap())),
    };
    let program = std::env::current_exe().unwrap();
    // The new image is this test binary running only the test of the image's side: the words
    // after `--` (`--handed`, `0`, the descriptors to look for) are names of no test.
    let args = [program.as_os_str(), "--exact".as_ref(), IMAGE_SIDE.as_ref()]
        .into_iter()
        .chain(["--ignored", "--nocapture", "--test-threads=1", "--"].map(AsRef::as_ref))
        .map(OsString::from);
    let image = Image::prepare(&handing, program.clone(), args)
        .await
        .unwrap();
    let ours = [
        s3.as_raw_fd(),
        handing.sockets[0].listener.as_raw_fd(),
        handing.lock.as_ref().unwrap().1.as_raw_fd(),
        image.handed_number(),
    ];
    assert!(ours.iter().all(|n| is_open(*n)), "{ours:?}");
    let looked: Vec<String> = ours.iter().map(ToString::to_string).collect();
    let mut command = image.command();
    command.arg(format!("descriptors={}", looked.join(",")));
    let ran = tokio::task::spawn_blocking(move || command.output());
    let ran = within("the next image", ran).await.unwrap().unwrap();
    let said = String::from_utf8_lossy(&ran.stdout);
    assert!(
        ran.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let lines = [
        "open after the exec: ".to_owned(),
        "standard input is a socket: true".to_owned(),
        format!("offer: offer | s3 {s3_addr} | lock {}", lock_path.display()),
        "descriptors in it: 2".to_owned(),
        "after it: keep, with its own socket: true".to_owned(),
    ];
    for line in lines {
        // The first line follows the name of the test, on the same line.
        assert!(said.contains(&format!("{line}\n")), "{line:?} in {said}");
    }
    held.disarm();
}

/// The next image's side of [`only_the_handed_descriptor_survives_the_exec`], in a process of its
/// own: it says which of the descriptors it is told to look for are open, what descriptor 0 is,
/// and what the offer on it holds.
#[test]
#[ignore = "the next image of only_the_handed_descriptor_survives_the_exec, which runs it in a process of its own"]
#[expect(
    clippy::disallowed_macros,
    reason = "the next image reports to the test that runs it on standard output, and a skip says why (AGENTS.md T12)"
)]
fn the_image_side_of_the_exec() {
    let Some(looked) =
        std::env::args().find_map(|arg| arg.strip_prefix("descriptors=").map(str::to_owned))
    else {
        eprintln!(
            "skipped: only only_the_handed_descriptor_survives_the_exec runs it, naming the descriptors"
        );
        return;
    };
    let open: Vec<&str> = looked
        .split(',')
        .filter(|n| n.parse().is_ok_and(is_open))
        .collect();
    println!("open after the exec: {}", open.join(","));
    let input = std::fs::metadata("/dev/fd/0").unwrap().file_type();
    println!("standard input is a socket: {}", input.is_socket());
    let handed = handed_input().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (frame, after) = runtime.block_on(async {
        let stream = UnixStream::from(handed);
        stream.set_nonblocking(true).unwrap();
        let stream = tokio::net::UnixStream::from_std(stream).unwrap();
        (wire::recv(&stream).await, wire::recv(&stream).await)
    });
    let frame = frame.unwrap();
    println!("offer: {}", frame.lines.join(" | "));
    println!("descriptors in it: {}", frame.fds.len());
    let after = after.unwrap();
    let input = identity(std::io::stdin().as_fd());
    let own: Vec<bool> = after
        .fds
        .iter()
        .map(|fd| identity(fd.as_fd()) == input)
        .collect();
    println!(
        "after it: {}, with its own socket: {}",
        after.lines.join(" | "),
        own == [true]
    );
}

/// Asserts that the frame grows as the conversation does (B8): a line whose name this release does
/// not know, as a later release may add, is skipped and its descriptor closed, and the rest of the
/// offer is taken.
#[tokio::test]
async fn a_line_this_release_does_not_know_is_skipped_and_its_descriptor_closed() {
    let (s3, s3_addr) = listening();
    let (later, later_addr) = listening();
    let want = Want {
        s3: &[s3_addr],
        oci: &[],
        nfs: &[],
    };
    let own = Own {
        control: None,
        lock: None,
    };
    let s3_line = format!("s3 {s3_addr}");
    let later_line = format!("quic {later_addr}");
    let end = raw(
        &["offer", &later_line, &s3_line],
        &[later.as_fd(), s3.as_fd()],
    )
    .await;
    drop(later);
    let taken = within("the offer", read(end, &want, &own)).await.unwrap();
    assert_eq!(taken.inherited.s3_addrs, [s3_addr]);
    let closed = || std::net::TcpStream::connect(later_addr).is_err();
    once_no_fork_holds("the unknown line's socket is closed", closed).await;
}

/// The test the child process of [`an_exec_that_fails_closes_what_the_offer_held`] runs.
const FAILED_EXEC: &str = "server::handoff::exec::tests::the_process_whose_exec_fails";

/// Asserts that an exec that fails (the program is not there) returns why, and leaves nothing in
/// the end of the pair: the offer and the frame `keep` were taken out and what they carried closed,
/// so the lock is free at once and the end is at its end. Without that, the end, which `keep` holds
/// in a message of its own, would keep the offer and its lock until the collector of descriptors in
/// flight next runs, while the process exits and launchd starts the job again to take that lock.
/// The exec runs in a process of its own: `Command::exec` restores SIGPIPE's default before it
/// tries, which this test binary, which writes to closed sockets, would not survive.
#[tokio::test]
async fn an_exec_that_fails_closes_what_the_offer_held() {
    let program = std::env::current_exe().unwrap();
    let mut command = std::process::Command::new(program);
    command.args([
        "--exact",
        FAILED_EXEC,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    let ran = tokio::task::spawn_blocking(move || command.output());
    let ran = within("the process", ran).await.unwrap().unwrap();
    let said = String::from_utf8_lossy(&ran.stdout);
    assert!(
        ran.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    for line in [
        "the exec failed: NotFound",
        "left in the end: 0 bytes",
        "the lock is free: true",
    ] {
        assert!(said.contains(&format!("{line}\n")), "{line:?} in {said}");
    }
}

/// The process of [`an_exec_that_fails_closes_what_the_offer_held`]: it prepares an image of a
/// program that is not there with an offer of a socket and the lock, closes its own copies (as the
/// exec would), keeps a copy of the image's end, executes the image, and says why the exec failed,
/// what is left in the end, and whether the lock can be taken.
#[test]
#[ignore = "the process of an_exec_that_fails_closes_what_the_offer_held, which runs it in a process of its own"]
#[expect(
    clippy::disallowed_macros,
    reason = "the process reports to the test that runs it on standard output"
)]
fn the_process_whose_exec_fails() {
    let (dir, _) = scratch();
    let (s3, _) = listening();
    let lock_path = dir.path().join(".lock");
    let mut held = try_take(&lock_path).unwrap().unwrap();
    let handing = Handing {
        sockets: vec![socket(Front::S3, &s3)],
        control: None,
        lock: Some((lock_path.clone(), held.share().unwrap())),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let missing = dir.path().join("not-there");
    let prepared = Image::prepare(&handing, missing, words(&["tierstore"]));
    let image = runtime.block_on(prepared).unwrap();
    held.disarm();
    drop((handing, held, s3));
    let looked = UnixStream::from(image.handed_copy());
    let why = image.exec();
    println!("the exec failed: {:?}", why.kind());
    looked.set_nonblocking(true).unwrap();
    let mut byte = [0_u8; 1];
    let left = std::io::Read::read(&mut &looked, &mut byte).map_err(|error| error.kind());
    println!("left in the end: {} bytes", left.unwrap());
    println!(
        "the lock is free: {}",
        try_take(&lock_path).unwrap().is_some()
    );
}

/// Asserts that an offer of more descriptors than one message carries is not written: the image
/// is refused with the count, rather than handed a frame whose descriptors the kernel would drop.
#[tokio::test]
async fn an_offer_of_more_descriptors_than_a_message_carries_is_not_made() {
    let (listener, _) = listening();
    let sockets = (0..=crate::server::handoff::MAX_SOCKETS)
        .map(|_| socket(Front::S3, &listener))
        .collect();
    let handing = Handing {
        sockets,
        control: None,
        lock: None,
    };
    let refused = Image::prepare(&handing, PathBuf::from("tierstore"), words(&["tierstore"])).await;
    let count = crate::server::handoff::MAX_SOCKETS + 1;
    assert!(
        matches!(refused, Err(HandoffError::TooMany(n)) if n == count),
        "{refused:?}"
    );
}
