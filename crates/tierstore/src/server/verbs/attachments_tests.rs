//! The verb `tierstore attachments`: the socket it reads from the node's file, what it lists, and
//! each way it is refused or fails.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use tierstore_testkit::dir::TestDir;

use super::run;
use crate::server::treefs::testing::{Rig, sample};
use crate::server::verbs::testing::{args, status};

/// Runs `tierstore attachments <words>` and gives its status and what it wrote to stdout and
/// stderr.
fn attachments(words: &[&str]) -> (String, String, String) {
    let mut all = vec!["attachments"];
    all.extend_from_slice(words);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args(&all), &mut out, &mut err);
    (
        format!("{code:?}"),
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// The tables of a build node over the cache directory `cache`, with the entries `entries`.
fn node(entries: &str, cache: &std::path::Path) -> String {
    format!(
        "[[interface]]\ntype = \"s3\"\nparams = {{ port = 9300 }}\n\n{entries}\
         [server]\nproxy_id = \"c1\"\nbucket = \"tier-bucket\"\naccess_key_file = \"key\"\n\n\
         [local_cache]\npath = {cache:?}\nbudget = \"1GiB\"\n\n\
         [cold_store]\nkind = \"memory\"\n\n\
         [[policy]]\nprefix = \"cas/\"\ncache = true\n"
    )
}

/// The `fs` interface over the scratch directories of `rig`, with the control socket `socket`.
fn fs_interface(rig: &Rig, socket: &std::path::Path) -> String {
    let gid = std::fs::metadata(&rig.runtime).unwrap().gid();
    format!(
        "[[interface]]\ntype = \"fs\"\n[interface.params]\nmountpoint = {:?}\nruntime_dir = {:?}\n\
         control_socket = {socket:?}\nclient_group = \"{gid}\"\nowner = \"1:2\"\n\n",
        rig.mountpoint, rig.runtime,
    )
}

/// A node's file in the scratch directory of `rig` whose `fs` interface names `socket`.
fn node_file(rig: &Rig, socket: &std::path::Path) -> PathBuf {
    let path = rig.dir.path().join("config.toml");
    std::fs::write(&path, node(&fs_interface(rig, socket), &rig.cache)).unwrap();
    path
}

/// Asserts that the verb asks the control socket that the `fs` interface of the node's file names
/// (not the default one in the runtime directory): nothing is listed while nothing is attached,
/// then one line for each attachment, the digest of the tree and the run, in the order of the runs.
#[test]
fn attachments_reads_the_control_socket_from_the_fs_interface() {
    let rig = Rig::new("tfs-attach-list", sample());
    let socket = rig.dir.path().join("named.sock");
    let path = node_file(&rig, &socket);
    let gid = std::fs::metadata(&rig.runtime).unwrap().gid();
    let _control =
        crate::server::treefs::control::Control::serve(rig.core.clone(), &socket, gid).unwrap();
    let (code, out, err) = attachments(&["--config", path.to_str().unwrap()]);
    assert_eq!(code, status(0), "{err}");
    assert!(out.is_empty(), "{out}");
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
    let (code, out, err) = attachments(&["--config", path.to_str().unwrap()]);
    assert_eq!(code, status(0), "{err}");
    assert_eq!(out, format!("{root} run-a\n{root} run-b\n"));
    assert!(err.is_empty(), "{err}");
}

/// Asserts that a file that loads but has no `fs` interface, a build node without one and a
/// storage node, is status 2 with the one line that names `[[interface]] with type = "fs"`.
#[test]
fn attachments_refuses_a_file_with_no_fs_interface() {
    let rig = Rig::new("tfs-attach-nofs", sample());
    let message = "tierstore attachments: the file has no [[interface]] with type = \"fs\": \
                   there is no file system whose attachments could be listed\n";
    let build = rig.dir.path().join("build.toml");
    std::fs::write(&build, node("", &rig.cache)).unwrap();
    let (code, out, err) = attachments(&["--config", build.to_str().unwrap()]);
    assert_eq!(code, status(2));
    assert!(out.is_empty(), "{out}");
    assert_eq!(err, message);
    let storage = rig.dir.path().join("storage.toml");
    let text = "[[interface]]\ntype = \"s3\"\nparams = { port = 9300 }\n\n\
                [server]\nproxy_id = \"c1\"\nbucket = \"tier-bucket\"\naccess_key_file = \"key\"\n\n\
                [hot_store]\nkind = \"memory\"\nbudget = 1000\n\n[cold_store]\nkind = \"memory\"\n";
    std::fs::write(&storage, text).unwrap();
    let (code, out, err) = attachments(&["--config", storage.to_str().unwrap()]);
    assert_eq!(code, status(2));
    assert!(out.is_empty(), "{out}");
    assert_eq!(err, message);
}

/// Asserts that a control socket that is not there, one that answers what is not JSON and one that
/// hangs up are each status 1, and the line names the socket's path and what went wrong.
#[test]
fn attachments_of_a_socket_that_does_not_answer_are_an_error() {
    let rig = Rig::new("tfs-attach-nolist", sample());
    let socket = rig.dir.path().join("named.sock");
    let path = node_file(&rig, &socket);
    let (code, out, err) = attachments(&["--config", path.to_str().unwrap()]);
    assert_eq!(code, status(1));
    assert!(out.is_empty(), "{out}");
    assert_eq!(
        err,
        format!(
            "tierstore attachments: the control socket {}: No such file or directory (os error 2)\n",
            socket.display()
        )
    );
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        for (i, stream) in listener.incoming().flatten().take(2).enumerate() {
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line).unwrap();
            if i == 0 {
                (&stream).write_all(b"not json\n").unwrap();
            }
        }
    });
    let (code, _, err) = attachments(&["--config", path.to_str().unwrap()]);
    assert_eq!(code, status(1));
    assert!(
        err.starts_with(&format!(
            "tierstore attachments: the control socket {}: ",
            socket.display()
        )),
        "{err}"
    );
    assert!(err.contains("expected ident"), "{err}");
    let (code, _, err) = attachments(&["--config", path.to_str().unwrap()]);
    assert_eq!(code, status(1));
    assert!(err.contains("EOF while parsing"), "{err}");
    server.join().unwrap();
}

/// Asserts that an answer of `{"ok":false}` is status 1 with its error on stderr, and nothing on
/// stdout, not an empty list that is status 0.
#[test]
fn attachments_of_an_answer_that_is_an_error_is_status_one_with_its_error() {
    let rig = Rig::new("tfs-attach-refused", sample());
    let socket = rig.dir.path().join("named.sock");
    let path = node_file(&rig, &socket);
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut line).unwrap();
        assert_eq!(line, "attachments\n");
        (&stream)
            .write_all(b"{\"ok\":false,\"error\":\"unknown request\"}\n")
            .unwrap();
    });
    let (code, out, err) = attachments(&["--config", path.to_str().unwrap()]);
    assert_eq!(code, status(1));
    assert!(out.is_empty(), "{out}");
    assert_eq!(
        err,
        format!(
            "tierstore attachments: the control socket {}: unknown request\n",
            socket.display()
        )
    );
    server.join().unwrap();
}

/// Asserts that a missing `--config`, a flag the verb does not take, a file that is not there and
/// a file that is not a node's file are status 2, the last two with the reason of a start.
#[test]
fn attachments_of_arguments_or_a_file_that_is_wrong_is_status_two() {
    let (code, out, err) = attachments(&[]);
    assert_eq!(code, status(2));
    assert!(out.is_empty(), "{out}");
    assert_eq!(err, "tierstore attachments: --config is needed\n");
    let (code, _, err) = attachments(&["--bogus"]);
    assert_eq!(code, status(2));
    assert!(err.contains("--bogus"), "{err}");
    let dir = TestDir::under(std::env::temp_dir(), "tfs-attach-config");
    let missing = dir.path().join("none.toml");
    let (code, out, err) = attachments(&["--config", missing.to_str().unwrap()]);
    assert_eq!(code, status(2));
    assert!(out.is_empty(), "{out}");
    assert!(
        err.starts_with("tierstore attachments: ") && err.contains("none.toml"),
        "{err}"
    );
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "[fs]\nowner = \"1:2\"\n").unwrap();
    let (code, out, err) = attachments(&["--config", bad.to_str().unwrap()]);
    assert_eq!(code, status(2));
    assert!(out.is_empty(), "{out}");
    assert!(
        err.starts_with("tierstore attachments: ") && err.contains("bad.toml"),
        "{err}"
    );
}

/// Asserts that help is a success that prints the verb's usage, and that the verb is answered by
/// name before the flags of a start are read.
#[test]
fn the_verb_is_answered_by_name_and_help_names_its_flag() {
    let (code, out, _) = attachments(&["--help"]);
    assert_eq!(code, status(0));
    assert!(out.contains("--config"), "{out}");
    let answered = crate::server::verbs::answer(&args(&["attachments", "--help"])).unwrap();
    assert_eq!(format!("{answered:?}"), status(0));
}
