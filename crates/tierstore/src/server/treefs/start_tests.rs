//! The neutral start: what it makes without a mount. It runs on every target.

use super::{Identity, SIGNALS, Setup, Signals, StartError, new_mount_id};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::core::Errno;
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Rig, file, sample};

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::UnixStream;

/// Settings over the scratch directories of a rig.
fn settings(rig: &Rig) -> FsSettings {
    FsSettings {
        mountpoint: rig.mountpoint.clone(),
        runtime_dir: rig.runtime.clone(),
        control_socket: rig.dir.path().join("control.sock"),
        client_group: std::fs::metadata(&rig.runtime).unwrap().gid(),
        owner: (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        ),
        fill_socket: rig.dir.path().join("fill.sock"),
        cache_path: rig.cache.clone(),
        fetch_timeout: std::time::Duration::from_secs(5),
        threads: 4,
        passthrough: true,
        max_backing_ids: 100,
        allow_other: false,
        nfs: crate::server::treefs::nfs::config::NfsSettings::default(),
    }
}

fn ask(path: &std::path::Path, request: &str) -> serde_json::Value {
    let stream = UnixStream::connect(path).unwrap();
    (&stream)
        .write_all(format!("{request}\n").as_bytes())
        .unwrap();
    let mut line = String::new();
    let _ = BufReader::new(&stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Asserts that a start makes attachments again from the file in the runtime directory (leaving out
/// a tree it cannot read), tells the cache which trees are attached, and serves the control socket:
/// a client attaches over it and the core has the attachment.
#[test]
fn setup_restores_saved_attachments_and_serves_the_control_socket() {
    let rig = Rig::new("tfs-start-restore", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let gone = "ab".repeat(32);
    std::fs::write(
        rig.runtime.join("attachments"),
        format!("{root} run-1\n{gone} run-2\n"),
    )
    .unwrap();
    let settings = settings(&rig);
    let setup = Setup::start(&settings, true, Identity::Fresh).unwrap();
    let kept: Vec<String> = setup
        .core
        .attachments()
        .into_iter()
        .map(|(_, run)| run)
        .collect();
    assert_eq!(kept, ["run-1"]);
    let pins = rig.upstream.asked("pins-live");
    assert_eq!(pins.last().unwrap().split(' ').next_back().unwrap(), root);
    let answer = ask(
        &settings.control_socket,
        &format!("attach {root} for run-3"),
    );
    assert_eq!(answer["ok"], serde_json::json!(true));
    assert_eq!(setup.core.attachments().len(), 2);
}

/// Asserts that a start that cannot do what it needs says which key it was doing it for: a runtime
/// directory or a mount point under a file, a control socket in a directory that is not there; and
/// that a backing table it cannot remove is said by path. None of them is a stacked-cache status.
#[test]
fn setup_refuses_an_unwritable_runtime_dir_with_its_key_name() {
    let rig = Rig::new("tfs-start-refused", sample());
    let base = settings(&rig);
    let file = rig.dir.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let cases: Vec<(FsSettings, &str)> = vec![
        (
            FsSettings {
                runtime_dir: file.join("run"),
                ..base.clone()
            },
            "fs.runtime_dir",
        ),
        (
            FsSettings {
                mountpoint: file.join("mnt"),
                ..base.clone()
            },
            "fs.mountpoint",
        ),
        (
            FsSettings {
                control_socket: rig.dir.path().join("no/dir/c.sock"),
                ..base.clone()
            },
            "fs.control_socket",
        ),
    ];
    for (settings, what) in cases {
        let error = Setup::start(&settings, true, Identity::Fresh)
            .err()
            .unwrap();
        assert!(error.to_string().contains(what), "{what}: {error}");
        assert_eq!(error.status(), 1);
    }
    std::fs::create_dir_all(rig.runtime.join("backing/inside")).unwrap();
    let error = Setup::start(&base, true, Identity::Fresh).err().unwrap();
    assert!(error.to_string().contains("removing"), "{error}");
    assert!(matches!(error, StartError::Io { .. }));
}

/// Asserts that every start makes the table of tree numbers afresh, as a new FUSE mount needs: the
/// second tree attached by the first start is number 2, and after a restart with nothing attached
/// the tree that was number 2 is attached first and is number 1, and number 2 is no inode.
#[test]
fn setup_creates_a_fresh_table_of_tree_numbers() {
    let rig = Rig::new("tfs-start-table", sample());
    let first_root = rig.upstream.built.lock().unwrap().root.clone();
    let second_root = rig
        .upstream
        .built
        .lock()
        .unwrap()
        .add_tree(vec![("only.txt", file(b"only"))]);
    let settings = settings(&rig);
    {
        let first = Setup::start(&settings, true, Identity::Fresh).unwrap();
        let _ = first.core.attach(&first_root, "run-1").unwrap();
        let _ = first.core.attach(&second_root, "run-2").unwrap();
        assert!(first.core.getattr(ino::of(2, 0)).is_ok());
        assert!(first.core.detach("run-1").is_some());
        assert!(first.core.detach("run-2").is_some());
    }
    let second = Setup::start(&settings, true, Identity::Fresh).unwrap();
    assert!(second.core.attachments().is_empty());
    assert_eq!(second.core.getattr(ino::of(1, 0)), Err(Errno::NoEnt));
    let _ = second.core.attach(&second_root, "run-3").unwrap();
    assert!(second.core.getattr(ino::of(1, 0)).is_ok());
    assert_eq!(second.core.getattr(ino::of(2, 0)), Err(Errno::NoEnt));
}

/// Asserts that a start chooses whether its core counts lookups: with the choice false a tree is
/// dropped at its detach.
#[test]
fn setup_passes_the_lookup_choice_to_the_core() {
    let rig = Rig::new("tfs-start-lookups", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let settings = settings(&rig);
    let setup = Setup::start(&settings, false, Identity::Fresh).unwrap();
    let _ = setup.core.attach(&root, "run-1").unwrap();
    let ino = ino::of(1, 0);
    setup.core.count(ino);
    assert!(setup.core.detach("run-1").is_some());
    assert_eq!(setup.core.trees_in_memory(), 0);
}

/// Asserts that a mount id is thirty-two hex digits and two are not alike.
#[test]
fn a_mount_id_is_random_hex() {
    let (a, b) = (new_mount_id().unwrap(), new_mount_id().unwrap());
    assert_eq!(a.len(), 32);
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b);
}

/// Asserts that SIGTERM and SIGINT, each, end the wait, and that one that came before the wait is
/// not lost.
#[test]
fn sigterm_and_sigint_end_the_wait_even_if_they_came_first() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut signals = Signals::install().unwrap();
        for signal in [rustix::process::Signal::TERM, rustix::process::Signal::INT] {
            rustix::process::kill_process(rustix::process::getpid(), signal).unwrap();
            signals.wait().await;
        }
    });
}
