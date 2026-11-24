//! The steps of `release macos-assets` and how they are run: in order, from the checkout, the
//! first failure ending the run. That the steps are the hosted macOS job's is the workflow
//! lint's (tests/workflows/macos_farm_tests.rs).

use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

use super::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-macos-assets-{name}"))
}

fn programs() -> Programs {
    Programs {
        cargo: PathBuf::from("/c/cargo"),
        xtask: PathBuf::from("/x/xtask"),
    }
}

/// The program, arguments and standard output of each process step `exec` was given.
type Ran = Vec<(PathBuf, Vec<String>, Option<PathBuf>)>;

/// Catches a step that names another binary, version, commit or platform than the run's, the
/// package version not `<version>+<12 digits of the commit>`, the code signature checked on
/// another binary than the one hostkeeper unpacked, and the installer read back from another
/// place than the one it was built to.
#[test]
fn the_steps_name_the_runs_version_commit_and_scratch() {
    let shown: Vec<String> = steps("1.2.3", COMMIT, "s")
        .iter()
        .map(Step::shown)
        .collect();
    assert_eq!(shown.len(), 21);
    assert_eq!(shown[0], "cargo cyclonedx --version");
    assert_eq!(shown[1], "cargo xtask release set-version --version 1.2.3");
    assert_eq!(shown[2], "cp Cargo.lock s/Cargo.lock.release");
    assert_eq!(
        shown[7],
        "cargo metadata --locked --format-version 1 --manifest-path crates/tierstore/Cargo.toml --features server --filter-platform aarch64-apple-darwin > s/metadata.json"
    );
    assert_eq!(
        shown[11],
        format!("tar -xzf dist/tierstore-{COMMIT}-macos-arm64.tar.gz -C s/unpacked tierstore")
    );
    assert_eq!(
        shown[16],
        "codesign --verify --strict s/hostkeeper/packages/tierstore/1.2.3+0123456789ab/bin/tierstore"
    );
    assert_eq!(
        shown[19],
        "pkgutil --expand pkg/tierstore-1.2.3-macos-arm64.pkg s/pkg-expanded"
    );
}

/// Catches a step run out of order or twice, a process step started from another directory
/// than the checkout or with its output not where it says, a shell step of the hosted job (the
/// copy of the lock file, its comparison, the directories) not done, and the last line not
/// saying where the files are.
#[test]
fn every_step_runs_in_order_from_the_checkout() {
    let dir = scratch("in-order");
    let root = dir.path();
    std::fs::write(root.join("Cargo.lock"), "lock").expect("write");
    std::fs::create_dir(root.join("s")).expect("mkdir");
    let steps = steps("1.2.3", COMMIT, "s");
    let mut ran: Ran = Vec::new();
    let mut exec = |program: &Path, args: &[String], stdout: Option<&Path>, at: &Path| {
        assert_eq!(at, root);
        ran.push((
            program.to_owned(),
            args.to_vec(),
            stdout.map(Path::to_owned),
        ));
        Ok(true)
    };
    let mut log = Vec::new();
    let lines = run_steps(&steps, root, &programs(), &mut exec, &mut log).expect("every step");
    assert_eq!(
        lines,
        ["ok: the macos-arm64 release files are in dist/ and pkg/, every step passed"]
    );
    let processes = steps
        .iter()
        .filter(|s| matches!(s, Step::Run { .. }))
        .count();
    assert_eq!(ran.len(), processes);
    assert_eq!(ran[0].0, Path::new("/c/cargo"));
    assert_eq!(ran[1].0, Path::new("/x/xtask"));
    assert_eq!(ran[1].1, ["release", "set-version", "--version", "1.2.3"]);
    let metadata = ran
        .iter()
        .find(|(_, args, _)| args.first().map(String::as_str) == Some("metadata"))
        .expect("the metadata step");
    assert_eq!(
        metadata.2.as_deref(),
        Some(root.join("s/metadata.json").as_path())
    );
    assert_eq!(ran[8].0, Path::new("tar"));
    assert_eq!(
        std::fs::read(root.join("s/Cargo.lock.release")).expect("the copy"),
        b"lock"
    );
    assert!(root.join("s/unpacked").is_dir());
    assert!(root.join("pkg").is_dir());
    let log = String::from_utf8(log).expect("utf-8");
    assert!(
        log.starts_with("macos-assets: step 1 of 21: cargo cyclonedx --version\n"),
        "{log}"
    );
    assert!(
        log.ends_with(
            "macos-assets: step 21 of 21: cargo xtask release check-macos-pkg s/pkg-expanded --version 1.2.3 --binary target/aarch64-apple-darwin/release/tierstore\n"
        ),
        "{log}"
    );
}

/// Catches a step that failed passed over (the release files of a failed build uploaded), the
/// failure not naming the step, and a step after it run.
#[test]
fn the_first_step_that_fails_ends_the_run() {
    let dir = scratch("fails");
    let root = dir.path();
    let steps = steps("1.2.3", COMMIT, "s");
    let mut count = 0;
    let mut exec = |_: &Path, _: &[String], _: Option<&Path>, _: &Path| {
        count += 1;
        Ok(false)
    };
    let failed = run_steps(&steps, root, &programs(), &mut exec, &mut Vec::new())
        .expect_err("the first step fails");
    assert_eq!(
        failed.to_string(),
        "`cargo cyclonedx --version` failed: it exited non-zero"
    );
    assert_eq!(count, 1);
    let mut refused = |_: &Path, _: &[String], _: Option<&Path>, _: &Path| {
        Err(Error::Failed("cannot start".to_owned()))
    };
    let failed = run_steps(&steps, root, &programs(), &mut refused, &mut Vec::new())
        .expect_err("not started");
    assert_eq!(failed.to_string(), "cannot start");
}

/// Catches the SBOM step's change to the lock file passed over (the hosted job's `cmp`), a copy
/// of a file that is not there, and a directory that is there already reused (a scratch left
/// by another run).
#[test]
fn a_shell_step_that_fails_ends_the_run() {
    let dir = scratch("shell");
    let root = dir.path();
    std::fs::write(root.join("a"), "1").expect("write");
    std::fs::write(root.join("b"), "2").expect("write");
    std::fs::create_dir(root.join("there")).expect("mkdir");
    let mut never = |_: &Path, _: &[String], _: Option<&Path>, _: &Path| -> Result<bool, Error> {
        panic!("no process step here")
    };
    let compare = [Step::Compare {
        a: "a".to_owned(),
        b: "b".to_owned(),
    }];
    let failed =
        run_steps(&compare, root, &programs(), &mut never, &mut Vec::new()).expect_err("differ");
    assert_eq!(failed.to_string(), "`cmp a b` failed: a and b differ");
    let missing = [Step::Compare {
        a: "a".to_owned(),
        b: "gone".to_owned(),
    }];
    let failed =
        run_steps(&missing, root, &programs(), &mut never, &mut Vec::new()).expect_err("gone");
    assert!(
        failed.to_string().starts_with("`cmp a gone` failed: "),
        "{failed}"
    );
    let copy = [Step::Copy {
        from: "gone".to_owned(),
        to: "c".to_owned(),
    }];
    let failed =
        run_steps(&copy, root, &programs(), &mut never, &mut Vec::new()).expect_err("no file");
    assert!(
        failed.to_string().starts_with("`cp gone c` failed: "),
        "{failed}"
    );
    let mkdir = [Step::Mkdir {
        path: "there".to_owned(),
    }];
    let failed =
        run_steps(&mkdir, root, &programs(), &mut never, &mut Vec::new()).expect_err("there");
    assert!(
        failed.to_string().starts_with("`mkdir there` failed: "),
        "{failed}"
    );
    let same = [Step::Compare {
        a: "a".to_owned(),
        b: "a".to_owned(),
    }];
    assert!(run_steps(&same, root, &programs(), &mut never, &mut Vec::new()).is_ok());
}

/// Catches a version or commit that would name the files wrongly taken (they go into every file
/// name), and the scratch directory not made.
#[test]
fn a_wrong_version_or_commit_is_refused_and_the_scratch_is_made() {
    let dir = scratch("checked");
    let args = |version: &str, commit: &str| Args {
        version: version.to_owned(),
        commit: commit.to_owned(),
        scratch: PathBuf::from("t/s"),
    };
    for version in ["1.2", "1.2.3.4", "v1.2.3", "1..3", "1.2.x", ""] {
        assert_eq!(
            checked_steps(&args(version, COMMIT), dir.path())
                .expect_err("not X.Y.Z")
                .to_string(),
            format!("--version {version:?} is not <major>.<minor>.<patch>")
        );
    }
    for commit in [
        &COMMIT[..12],
        &COMMIT.to_uppercase(),
        &format!("{}g", &COMMIT[..39]),
    ] {
        assert_eq!(
            checked_steps(&args("1.2.3", commit), dir.path()).expect_err("not a SHA"),
            Error::NotACommit(commit.to_owned())
        );
    }
    let steps = checked_steps(&args("1.2.3", COMMIT), dir.path()).expect("steps");
    assert_eq!(steps, super::steps("1.2.3", COMMIT, "t/s"));
    assert!(dir.path().join("t/s").is_dir());
    std::fs::write(dir.path().join("file"), "").expect("write");
    let blocked = Args {
        scratch: PathBuf::from("file/s"),
        ..args("1.2.3", COMMIT)
    };
    assert!(
        checked_steps(&blocked, dir.path())
            .expect_err("not a directory")
            .to_string()
            .starts_with("create "),
    );
}

/// Catches a program's exit status not read (a failed step taken as a pass), its standard output
/// not sent to the file the step names, it started elsewhere than the checkout, and a program
/// that cannot start or a file that cannot be made taken as a pass. The program is this test
/// binary, by a link in the checkout named by a relative path, which starts only from there.
#[test]
fn exec_runs_a_program_from_the_checkout_and_reads_its_status() {
    let dir = scratch("exec");
    let root = dir.path();
    let this = std::env::current_exe().expect("the test binary");
    std::os::unix::fs::symlink(&this, root.join("prog")).expect("link");
    let prog = Path::new("./prog");
    let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let out = root.join("out.txt");
    assert!(
        exec(
            prog,
            &args(&["--list", "--exact", "no-such-test"]),
            Some(&out),
            root
        )
        .expect("ran")
    );
    let listed = std::fs::read_to_string(&out).expect("the output");
    assert!(listed.contains("0 tests"), "{listed}");
    assert!(!exec(prog, &args(&["--no-such-option"]), None, root).expect("ran"));
    let elsewhere = exec(prog, &[], None, &root.join("..")).expect_err("not from the checkout");
    assert!(
        elsewhere.to_string().starts_with("run ./prog: "),
        "{elsewhere}"
    );
    let unmade = exec(prog, &[], Some(&root.join("gone/out.txt")), root).expect_err("no directory");
    assert!(unmade.to_string().starts_with("create "), "{unmade}");
}

/// Catches the verb run with its flags unchecked, or with a Cargo other than the one it is
/// given: a Cargo that cannot start ends the run at its first step.
#[test]
fn the_verb_checks_its_flags_and_runs_the_cargo_it_is_given() {
    let dir = scratch("verb");
    let wrong = Args {
        version: "1".to_owned(),
        commit: COMMIT.to_owned(),
        scratch: PathBuf::from("s"),
    };
    let cargo = dir.path().join("no-cargo");
    assert!(macos_assets(&wrong, dir.path(), &cargo).is_err());
    let args = Args {
        version: "1.2.3".to_owned(),
        ..wrong
    };
    let failed = macos_assets(&args, dir.path(), &cargo).expect_err("no cargo");
    assert!(
        failed
            .to_string()
            .starts_with(&format!("run {}: ", cargo.display())),
        "{failed}"
    );
}
