//! The outputs `mac-farm` writes below the checkout from the fake farm's answer, and the ones it
//! refuses to write: what the Command declared, at its paths and modes, each file held to its
//! digest, nothing written over and nothing left of a file that is not its digest.

use super::*;

/// A `dist/` on the fake: a file, an executable and an empty subdirectory, its tree and their
/// blobs stored; the output directory that names it.
fn put_dist(rig: &Rig) -> OutputDirectory {
    let tarball = rig.put(b"the tarball");
    let tool = rig.put(b"#!run");
    let empty = Directory::default();
    let root = Directory {
        files: vec![
            FileNode {
                name: "a.tar.gz".to_owned(),
                digest: Some(tarball),
                is_executable: false,
            },
            FileNode {
                name: "run".to_owned(),
                digest: Some(tool),
                is_executable: true,
            },
        ],
        directories: vec![DirectoryNode {
            name: "empty".to_owned(),
            digest: Some(digest_of(&empty.encode_to_vec())),
        }],
        symlinks: Vec::new(),
    };
    let tree = RawTree {
        root: root.encode_to_vec(),
        children: vec![empty.encode_to_vec()],
    };
    OutputDirectory {
        path: "dist".to_owned(),
        tree_digest: Some(rig.put(&tree.encode_to_vec())),
    }
}

/// The answer of a run that exited 0 and made `directories` and `files`.
fn made(directories: Vec<OutputDirectory>, files: Vec<OutputFile>) -> ExecuteResponse {
    ExecuteResponse {
        result: Some(ActionResult {
            output_directories: directories,
            output_files: files,
            ..ActionResult::default()
        }),
        ..ExecuteResponse::default()
    }
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

/// Catches the outputs not declared in the Command (the farm keeps only what is declared), not
/// sorted, not written below the checkout at their paths, written with other bytes, an
/// executable bit lost or one added, an empty directory of an output dropped, and the lines not
/// saying what was written.
#[test]
fn the_declared_outputs_are_written_below_the_checkout() {
    let rig = Rig::new();
    let dist = put_dist(&rig);
    let pkg = rig.put(b"the pkg");
    rig.state.queue(Answer::Updates(vec![answered(
        "op/20",
        &made(
            vec![dist],
            vec![OutputFile {
                path: "pkg/t.pkg".to_owned(),
                digest: Some(pkg),
                is_executable: false,
                contents: Vec::new(),
            }],
        ),
    )]));
    let ran = rig.run_with_outputs("outputs", &["pkg/t.pkg", "dist"], |_| {});
    assert_eq!(
        ran.result.expect("ok"),
        [
            "ok: `cargo test -p tierstore` passed on a farm Mac",
            "ok: dist written from the farm Mac (2 files)",
            "ok: pkg/t.pkg written from the farm Mac (1 file)",
        ]
    );
    let (_, command) = rig.executed(0);
    assert_eq!(command.output_paths, ["dist", "pkg/t.pkg"]);
    let root = &ran.root;
    assert_eq!(
        std::fs::read(root.join("dist/a.tar.gz")).expect("read"),
        b"the tarball"
    );
    assert_eq!(mode(&root.join("dist/a.tar.gz")), 0o644);
    assert_eq!(
        std::fs::read(root.join("dist/run")).expect("read"),
        b"#!run"
    );
    assert_eq!(mode(&root.join("dist/run")), 0o755);
    assert!(root.join("dist/empty").is_dir());
    assert_eq!(
        std::fs::read(root.join("pkg/t.pkg")).expect("read"),
        b"the pkg"
    );
}

/// Catches a file the farm serves with other bytes than its digest written and left for the
/// upload (the files of `dist/` are signed and released): the run fails naming the file, and
/// the file is not there.
#[test]
fn an_output_the_farm_serves_corrupt_is_refused_and_removed() {
    let rig = Rig::new();
    let dist = put_dist(&rig);
    *rig.state.corrupt.lock().expect("corrupt") = Some(digest_of(b"the tarball").hash);
    rig.state.queue(Answer::Updates(vec![answered(
        "op/21",
        &made(vec![dist], Vec::new()),
    )]));
    let ran = rig.run_with_outputs("corrupt", &["dist"], |_| {});
    let failed = failure(ran.result);
    let hash = digest_of(b"the tarball").hash;
    assert!(
        failed.starts_with(&format!(
            "dist/a.tar.gz: the farm sent {} for the blob {hash}/11",
            {
                let mut bad = b"the tarball".to_vec();
                *bad.last_mut().expect("last") ^= 1;
                format!("{}/11", digest_of(&bad).hash)
            }
        )),
        "{failed}"
    );
    assert!(!ran.root.join("dist/a.tar.gz").exists());
}

/// Catches a file the farm sent inline with other bytes than its digest written: the inline
/// bytes are held to the digest as a read from the CAS is, and the file is removed.
#[test]
fn an_inline_output_that_is_not_its_digest_is_refused() {
    let rig = Rig::new();
    rig.state.queue(Answer::Updates(vec![answered(
        "op/22",
        &made(
            Vec::new(),
            vec![OutputFile {
                path: "out.txt".to_owned(),
                digest: Some(digest_of(b"right")),
                is_executable: false,
                contents: b"wrong".to_vec(),
            }],
        ),
    )]));
    let ran = rig.run_with_outputs("inline-wrong", &["out.txt"], |_| {});
    assert_eq!(
        failure(ran.result),
        format!(
            "the farm sent out.txt inline with bytes that are not its digest {}/5",
            digest_of(b"right").hash
        )
    );
    assert!(!ran.root.join("out.txt").exists());
    rig.state.queue(Answer::Updates(vec![answered(
        "op/23",
        &made(
            Vec::new(),
            vec![OutputFile {
                path: "out.txt".to_owned(),
                digest: Some(digest_of(b"right")),
                is_executable: true,
                contents: b"right".to_vec(),
            }],
        ),
    )]));
    let ran = rig.run_with_outputs("inline-right", &["out.txt"], |_| {});
    assert!(ran.result.is_ok());
    assert_eq!(
        std::fs::read(ran.root.join("out.txt")).expect("read"),
        b"right"
    );
    assert_eq!(mode(&ran.root.join("out.txt")), 0o755);
}

/// Catches an output written over what the checkout already has there (a stale `dist/` of an
/// earlier run, published as this one's), a declared output the command did not make taken as
/// done, and a tree the farm cannot serve taken as an empty directory.
#[test]
fn an_output_that_is_there_already_missing_or_unreadable_fails_the_run() {
    let rig = Rig::new();
    let dist = put_dist(&rig);
    rig.state.queue(Answer::Updates(vec![answered(
        "op/24",
        &made(vec![dist.clone()], Vec::new()),
    )]));
    let ran = rig.run_with_outputs("there", &["dist"], |root| {
        std::fs::create_dir(root.join("dist")).expect("mkdir");
    });
    assert_eq!(
        failure(ran.result),
        "the output dist is there already; a run writes its outputs where nothing is"
    );
    rig.state.queue(Answer::Updates(vec![answered(
        "op/25",
        &made(Vec::new(), Vec::new()),
    )]));
    let ran = rig.run_with_outputs("missing", &["dist"], |_| {});
    assert_eq!(
        failure(ran.result),
        "the command made no \"dist\", a declared output"
    );
    let lost = OutputDirectory {
        path: "dist".to_owned(),
        tree_digest: Some(digest_of(b"a tree the farm lost")),
    };
    rig.state.queue(Answer::Updates(vec![answered(
        "op/26",
        &made(vec![lost], Vec::new()),
    )]));
    let ran = rig.run_with_outputs("lost-tree", &["dist"], |_| {});
    assert!(
        failure(ran.result).starts_with("ByteStream.Read: NotFound: no blobs/"),
        "the tree's read"
    );
    let no_tree = OutputDirectory {
        path: "dist".to_owned(),
        tree_digest: None,
    };
    rig.state.queue(Answer::Updates(vec![answered(
        "op/27",
        &made(vec![no_tree], Vec::new()),
    )]));
    let ran = rig.run_with_outputs("no-tree", &["dist"], |_| {});
    assert_eq!(failure(ran.result), "the output \"dist\" has no digest");
}

/// Catches the outputs of a command that failed written (they are half made), and a command's
/// failure hidden by an output error: the exit code is the failure and nothing is written.
#[test]
fn a_command_that_fails_writes_no_outputs() {
    let rig = Rig::new();
    let dist = put_dist(&rig);
    let mut response = made(vec![dist], Vec::new());
    if let Some(result) = response.result.as_mut() {
        result.exit_code = 1;
    }
    rig.state
        .queue(Answer::Updates(vec![answered("op/28", &response)]));
    let ran = rig.run_with_outputs("failed-outputs", &["dist"], |_| {});
    assert_eq!(
        failure(ran.result),
        "`cargo test -p tierstore` exited 1 on a farm Mac"
    );
    assert!(!ran.root.join("dist").exists());
}
