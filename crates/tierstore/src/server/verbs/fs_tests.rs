//! The verb: its arguments, what a bad file does, and the list of attachments.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::UnixListener;

use tierstore_testkit::dir::TestDir;

use super::run;
use crate::server::treefs::testing::{Rig, sample};
use crate::server::verbs::testing::{args, status};

/// Runs `tierstore fs <words>` and gives its status and what it wrote to stdout and stderr.
fn fs(words: &[&str]) -> (String, String, String) {
    let mut all = vec!["fs"];
    all.extend_from_slice(words);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args(&all), &mut out, &mut err);
    (
        format!("{code:?}"),
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// A file with the `[fs]` table over the scratch directories of `rig`.
fn config(rig: &Rig, extra: &str) -> std::path::PathBuf {
    let gid = std::fs::metadata(&rig.runtime).unwrap().gid();
    let text = format!(
        "[fs]\nmountpoint = {:?}\nruntime_dir = {:?}\ncontrol_socket = {:?}\nclient_group = \"{gid}\"\n\
         owner = \"1:2\"\nfill_socket = {:?}\ncache_path = {:?}\n{extra}",
        rig.mountpoint,
        rig.runtime,
        rig.dir.path().join("control.sock"),
        rig.dir.path().join("fill.sock"),
        rig.cache,
    );
    let path = rig.dir.path().join("fs.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// Asserts that help is a success and prints the verb's usage, and that flags the verb does not
/// take, and a missing `--config`, are status 2.
#[test]
fn the_arguments_of_the_verb_are_parsed() {
    let (code, out, _) = fs(&["--help"]);
    assert_eq!(code, status(0));
    assert!(out.contains("--config"), "{out}");
    assert!(out.contains("attachments"), "{out}");
    let (code, _, err) = fs(&["--bogus"]);
    assert_eq!(code, status(2));
    assert!(err.contains("--bogus"), "{err}");
    let (code, _, err) = fs(&[]);
    assert_eq!(code, status(2));
    assert!(err.contains("--config"), "{err}");
}

/// Asserts that a file that is missing, or has no usable `[fs]` table, is status 2 and the message
/// names the key or the file.
#[test]
fn a_file_that_is_missing_or_wrong_is_status_two() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-verb-config");
    let missing = dir.path().join("none.toml");
    let (code, _, err) = fs(&["--config", missing.to_str().unwrap()]);
    assert_eq!(code, status(2));
    assert!(err.contains("none.toml"), "{err}");
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "[fs]\nowner = \"1:2\"\n").unwrap();
    let (code, _, err) = fs(&["attachments", "--config", bad.to_str().unwrap()]);
    assert_eq!(code, status(2));
    assert!(err.contains("fs.client_group"), "{err}");
}

/// Asserts that a start that fails is its status and its words on stderr: the cache directory is
/// not there, status 1.
#[test]
fn a_start_that_fails_says_why_with_its_status() {
    let rig = Rig::new("tfs-verb-start", sample());
    let path = config(&rig, "");
    std::fs::remove_dir_all(&rig.cache).unwrap();
    let (code, out, err) = fs(&["--config", path.to_str().unwrap()]);
    assert_eq!(code, status(1));
    assert!(out.is_empty());
    assert!(err.starts_with("tierstore fs: fs.cache_path "), "{err}");
}

/// Asserts that `attachments` lists one line for each attachment of the running process, the digest
/// of the tree and the run, and nothing when there are none.
#[test]
fn the_attachments_are_listed_from_the_control_socket() {
    let rig = Rig::new("tfs-verb-list", sample());
    let path = config(&rig, "");
    let gid = std::fs::metadata(&rig.runtime).unwrap().gid();
    let socket = rig.dir.path().join("control.sock");
    let _control =
        crate::server::treefs::control::Control::serve(rig.core.clone(), &socket, gid).unwrap();
    let (code, out, _) = fs(&["attachments", "--config", path.to_str().unwrap()]);
    assert_eq!(code, status(0));
    assert!(out.is_empty());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    for run in ["run-b", "run-a"] {
        let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        (&stream)
            .write_all(format!("attach {root} for {run}\n").as_bytes())
            .unwrap();
        let mut line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut line).unwrap();
        assert!(line.contains("\"ok\":true"), "{line}");
    }
    let (code, out, _) = fs(&["--config", path.to_str().unwrap(), "attachments"]);
    assert_eq!(code, status(0));
    assert_eq!(out, format!("{root} run-a\n{root} run-b\n"));
}

/// Asserts that `attachments` against a socket that is not there, or one that answers what is not
/// JSON, or hangs up, is status 1 and says so.
#[test]
fn the_attachments_of_a_socket_that_does_not_answer_are_an_error() {
    let rig = Rig::new("tfs-verb-nolist", sample());
    let path = config(&rig, "");
    let (code, _, err) = fs(&["attachments", "--config", path.to_str().unwrap()]);
    assert_eq!(code, status(1));
    assert!(err.contains("the control socket"), "{err}");
    let listener = UnixListener::bind(rig.dir.path().join("control.sock")).unwrap();
    let server = std::thread::spawn(move || {
        for (i, stream) in listener.incoming().flatten().take(2).enumerate() {
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line).unwrap();
            if i == 0 {
                (&stream).write_all(b"not json\n").unwrap();
            }
        }
    });
    let (code, _, err) = fs(&["attachments", "--config", path.to_str().unwrap()]);
    assert_eq!(code, status(1));
    assert!(err.contains("expected ident"), "{err}");
    let (code, _, err) = fs(&["attachments", "--config", path.to_str().unwrap()]);
    assert_eq!(code, status(1));
    assert!(err.contains("EOF while parsing"), "{err}");
    server.join().unwrap();
}

/// Asserts that the verb is answered by name before the flags of a start are read.
#[test]
fn the_verb_is_answered_by_name() {
    let answered = crate::server::verbs::answer(&args(&["fs", "--help"])).unwrap();
    assert_eq!(format!("{answered:?}"), status(0));
}

/// Asserts the status of a serve: success for a mount that ended well, and for one that did not the
/// status of its error (3 for a stacked cache, 1 for the rest) with its words on stderr.
#[cfg(target_os = "linux")]
#[test]
fn the_status_of_a_serve_is_the_status_of_its_error() {
    use crate::server::treefs::StartError;
    let mut err = Vec::new();
    assert_eq!(
        format!("{:?}", super::finished(Ok(()), &mut err)),
        status(0)
    );
    assert!(err.is_empty());
    let stacked = StartError::Stacked {
        path: "/c".to_owned(),
        name: "overlayfs",
    };
    assert_eq!(
        format!("{:?}", super::finished(Err(stacked), &mut err)),
        status(3)
    );
    let io = StartError::Io {
        what: "mounting".to_owned(),
        source: std::io::Error::other("busy"),
    };
    assert_eq!(
        format!("{:?}", super::finished(Err(io), &mut err)),
        status(1)
    );
    let text = String::from_utf8(err).unwrap();
    assert!(
        text.contains("tierstore fs: fs.cache_path /c") && text.contains("mounting: busy"),
        "{text}"
    );
}
