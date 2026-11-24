//! `release macos-assets --version <X.Y.Z> --commit <sha> --scratch <dir>`: the macOS release
//! files of a commit, built, packaged and checked on the Mac this runs on, as the steps of
//! artifacts.yml's hosted `macos-arm64` job make them, in their order, each with the same
//! arguments ([`steps`]; the workflow lint holds the two equal, crates/xtask/tests/workflows).
//! It is the one command the farm job of artifacts.yml runs on a farm Mac
//! (`cargo xtask mac-farm ... -- xtask release macos-assets ...`, docs/mac-farm.md), where a
//! workflow's steps cannot run: an action is one command.
//!
//! The steps write `dist/` (the tarball, the SBOM, the hostkeeper package, the tap tarball and
//! `SHA256SUMS.macos-arm64`) and `pkg/` (the unsigned installer), the two outputs the farm job
//! brings back and uploads as the hosted job does; `--scratch` takes what the hosted job writes
//! to `$RUNNER_TEMP`. A step that fails ends the run, naming the step; nothing after it runs.
//!
//! A step is a program (Cargo, this xtask, or a tool of the Mac: `tar`, `codesign`, `pkgutil`)
//! with its arguments, started from the checkout's root, or one of the hosted job's shell
//! commands (`cp`, `cmp`, `mkdir`), done here.

#[cfg(test)]
mod tests;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Args as ClapArgs;

use crate::Error;
use crate::package::Platform;

/// The Rust target of the macOS build.
pub const TARGET: &str = "aarch64-apple-darwin";

/// The platform the files are named by.
pub const PLATFORM: Platform = Platform::MacosArm64;

/// The hostkeeper version the package needs: the first whose loader reads `platform`.
pub const MIN_HOSTKEEPER: &str = "0.1.0";

/// The flags of the verb.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// `<major>.<minor>.<patch>`, the version of the release the files are built under.
    #[arg(long)]
    pub version: String,
    /// The commit they are built from, a full SHA (on Actions, `GITHUB_SHA`).
    #[arg(long)]
    pub commit: String,
    /// A directory of this run's own, what the hosted job's `$RUNNER_TEMP` is: created when
    /// missing.
    #[arg(long)]
    pub scratch: PathBuf,
}

/// How a step's program is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    /// The Cargo that runs this xtask.
    Cargo,
    /// This xtask (`cargo xtask <verb>`).
    Xtask,
    /// A program of the Mac, by its name on the `PATH`.
    Tool(&'static str),
}

/// One step of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A program with its arguments; its standard output to the file `stdout` when given.
    Run {
        program: Program,
        args: Vec<String>,
        stdout: Option<String>,
    },
    /// `cp <from> <to>`.
    Copy { from: String, to: String },
    /// `cmp <a> <b>`: the two files are the same bytes.
    Compare { a: String, b: String },
    /// `mkdir <path>`: the directory is made, and is not there already.
    Mkdir { path: String },
}

impl Step {
    /// The step as the hosted job's shell command says it, without the quotes:
    /// `cargo xtask package --binary target/...`.
    #[must_use]
    pub fn shown(&self) -> String {
        match self {
            Step::Run {
                program,
                args,
                stdout,
            } => {
                let mut words = match program {
                    Program::Cargo => vec!["cargo".to_owned()],
                    Program::Xtask => vec!["cargo".to_owned(), "xtask".to_owned()],
                    Program::Tool(name) => vec![(*name).to_owned()],
                };
                words.extend(args.iter().cloned());
                if let Some(file) = stdout {
                    words.extend([">".to_owned(), file.clone()]);
                }
                words.join(" ")
            }
            Step::Copy { from, to } => format!("cp {from} {to}"),
            Step::Compare { a, b } => format!("cmp {a} {b}"),
            Step::Mkdir { path } => format!("mkdir {path}"),
        }
    }
}

fn run(program: Program, args: &[&str]) -> Step {
    Step::Run {
        program,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        stdout: None,
    }
}

/// The steps that build, package and check the macOS release files of `version` (`X.Y.Z`) and
/// `commit` (a full SHA), with `scratch` the run's own directory, in their order.
#[must_use]
pub fn steps(version: &str, commit: &str, scratch: &str) -> Vec<Step> {
    let platform = PLATFORM.name();
    let binary = format!("target/{TARGET}/release/tierstore");
    let package_version = format!("{version}+{}", &commit[..commit.len().min(12)]);
    let lock = format!("{scratch}/Cargo.lock.release");
    let metadata = format!("{scratch}/metadata.json");
    let licenses = format!("{scratch}/THIRD-PARTY-LICENSES.txt");
    let unpacked = format!("{scratch}/unpacked");
    let hostkeeper = format!("{scratch}/hostkeeper");
    let expanded = format!("{scratch}/pkg-expanded");
    let tarball = format!("dist/tierstore-{commit}-{platform}.tar.gz");
    let pkg = format!("pkg/tierstore-{version}-{platform}.pkg");
    let unpacked_binary = format!("{unpacked}/tierstore");
    let hostkeeper_binary =
        format!("{hostkeeper}/packages/tierstore/{package_version}/bin/tierstore");
    let stage = format!("{scratch}/pkg-root");
    vec![
        run(Program::Cargo, &["cyclonedx", "--version"]),
        run(
            Program::Xtask,
            &["release", "set-version", "--version", version],
        ),
        Step::Copy {
            from: "Cargo.lock".to_owned(),
            to: lock.clone(),
        },
        run(
            Program::Cargo,
            &[
                "build",
                "--locked",
                "--release",
                "--target",
                TARGET,
                "-p",
                "tierstore",
                "--features",
                "server",
                "--bin",
                "tierstore",
            ],
        ),
        run(
            Program::Xtask,
            &["build-commit", &binary, commit, "--version", version],
        ),
        run(
            Program::Cargo,
            &[
                "cyclonedx",
                "--format",
                "json",
                "--spec-version",
                "1.5",
                "--describe",
                "binaries",
                "--target",
                TARGET,
            ],
        ),
        Step::Compare {
            a: "Cargo.lock".to_owned(),
            b: lock,
        },
        Step::Run {
            program: Program::Cargo,
            args: [
                "metadata",
                "--locked",
                "--format-version",
                "1",
                "--manifest-path",
                "crates/tierstore/Cargo.toml",
                "--features",
                "server",
                "--filter-platform",
                TARGET,
            ]
            .map(str::to_owned)
            .to_vec(),
            stdout: Some(metadata.clone()),
        },
        run(
            Program::Xtask,
            &[
                "licenses",
                "--metadata",
                &metadata,
                "--package",
                "tierstore",
                "--out",
                &licenses,
            ],
        ),
        run(
            Program::Xtask,
            &[
                "package",
                "--binary",
                &binary,
                "--sbom",
                "crates/tierstore/tierstore_bin.cdx.json",
                "--licenses",
                &licenses,
                "--platform",
                platform,
                "--commit",
                commit,
                "--out",
                "dist",
            ],
        ),
        Step::Mkdir {
            path: unpacked.clone(),
        },
        run(
            Program::Tool("tar"),
            &["-xzf", &tarball, "-C", &unpacked, "tierstore"],
        ),
        run(
            Program::Xtask,
            &[
                "build-commit",
                &unpacked_binary,
                commit,
                "--version",
                version,
            ],
        ),
        run(
            Program::Xtask,
            &[
                "release",
                "hostkeeper-package",
                "--binary",
                &binary,
                "--version",
                &package_version,
                "--min-hostkeeper",
                MIN_HOSTKEEPER,
                "--platform",
                platform,
                "--out",
                "dist",
            ],
        ),
        run(
            Program::Xtask,
            &[
                "verify-assets",
                "hostkeeper-package",
                "dist",
                "--version",
                &package_version,
                "--platform",
                platform,
                "--scratch",
                &hostkeeper,
            ],
        ),
        run(
            Program::Xtask,
            &[
                "release",
                "tap-tarball",
                "dist",
                "--commit",
                commit,
                "--version",
                version,
                "--platform",
                platform,
            ],
        ),
        run(
            Program::Tool("codesign"),
            &["--verify", "--strict", &hostkeeper_binary],
        ),
        Step::Mkdir {
            path: "pkg".to_owned(),
        },
        run(
            Program::Xtask,
            &[
                "release",
                "macos-pkg",
                "--version",
                version,
                "--binary",
                &binary,
                "--stage",
                &stage,
                "--out",
                "pkg",
            ],
        ),
        run(Program::Tool("pkgutil"), &["--expand", &pkg, &expanded]),
        run(
            Program::Xtask,
            &[
                "release",
                "check-macos-pkg",
                &expanded,
                "--version",
                version,
                "--binary",
                &binary,
            ],
        ),
    ]
}

/// Starts a step's program: the program, its arguments, the file its standard output goes to
/// (none: this process's), and the checkout it runs in. Whether it exited 0, or why it could
/// not be started.
pub type Exec<'a> = dyn FnMut(&Path, &[String], Option<&Path>, &Path) -> Result<bool, Error> + 'a;

/// Runs `program` with `args` from `root`, its standard output to `stdout` when given; whether it
/// exited 0.
///
/// # Errors
/// [`Error::Spawn`] when it cannot be started, or `stdout` cannot be made.
pub fn exec(
    program: &Path,
    args: &[String],
    stdout: Option<&Path>,
    root: &Path,
) -> Result<bool, Error> {
    let mut command = Command::new(program);
    command.args(args).current_dir(root).stdin(Stdio::null());
    if let Some(file) = stdout {
        let file = std::fs::File::create(file)
            .map_err(Error::spawn(format!("create {}", file.display())))?;
        command.stdout(file);
    }
    let status = command
        .status()
        .map_err(Error::spawn(format!("run {}", program.display())))?;
    Ok(status.success())
}

/// What `program` is started as: `cargo` the Cargo that runs this xtask, `xtask` this program.
pub struct Programs {
    pub cargo: PathBuf,
    pub xtask: PathBuf,
}

/// Runs `steps` in order from `root`, each process step by `exec`, each line it says into `log`:
/// the lines to print.
///
/// # Errors
/// [`Error::Failed`] naming the first step that fails; no step after it runs.
pub fn run_steps(
    steps: &[Step],
    root: &Path,
    programs: &Programs,
    exec: &mut Exec<'_>,
    log: &mut dyn Write,
) -> Result<Vec<String>, Error> {
    for (at, step) in steps.iter().enumerate() {
        let shown = step.shown();
        let _ = writeln!(
            log,
            "macos-assets: step {} of {}: {shown}",
            at + 1,
            steps.len()
        );
        let failed = |why: String| Error::Failed(format!("`{shown}` failed: {why}"));
        match step {
            Step::Run {
                program,
                args,
                stdout,
            } => {
                let path = match program {
                    Program::Cargo => programs.cargo.clone(),
                    Program::Xtask => programs.xtask.clone(),
                    Program::Tool(name) => PathBuf::from(name),
                };
                let stdout = stdout.as_ref().map(|f| root.join(f));
                if !exec(&path, args, stdout.as_deref(), root)? {
                    return Err(failed("it exited non-zero".to_owned()));
                }
            }
            Step::Copy { from, to } => {
                std::fs::copy(root.join(from), root.join(to)).map_err(|e| failed(e.to_string()))?;
            }
            Step::Compare { a, b } => {
                let read = |p: &str| std::fs::read(root.join(p)).map_err(|e| failed(e.to_string()));
                if read(a)? != read(b)? {
                    return Err(failed(format!("{a} and {b} differ")));
                }
            }
            Step::Mkdir { path } => {
                std::fs::create_dir(root.join(path)).map_err(|e| failed(e.to_string()))?;
            }
        }
    }
    Ok(vec![format!(
        "ok: the {} release files are in dist/ and pkg/, every step passed",
        PLATFORM.name()
    )])
}

/// `version` when it is `<major>.<minor>.<patch>`, each part digits.
fn plain_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The steps `args` name, checked: the version is `X.Y.Z` and the commit a full SHA (both go
/// into file names), and the scratch directory is made.
///
/// # Errors
/// [`Error::Failed`] for a version that is not `X.Y.Z`, [`Error::NotACommit`] for a commit that
/// is not 40 lowercase hex digits, [`Error::Spawn`] for a scratch directory that cannot be made.
pub fn checked_steps(args: &Args, root: &Path) -> Result<Vec<Step>, Error> {
    if !plain_version(&args.version) {
        return Err(Error::Failed(format!(
            "--version {:?} is not <major>.<minor>.<patch>",
            args.version
        )));
    }
    let hex = args
        .commit
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if args.commit.len() != 40 || !hex {
        return Err(Error::NotACommit(args.commit.clone()));
    }
    let scratch = root.join(&args.scratch);
    std::fs::create_dir_all(&scratch)
        .map_err(Error::spawn(format!("create {}", scratch.display())))?;
    Ok(steps(
        &args.version,
        &args.commit,
        &args.scratch.to_string_lossy(),
    ))
}

/// The verb, on the checkout at `root`, with `cargo` the Cargo that runs it: the lines to print.
///
/// # Errors
/// [`Error`] for flags that are wrong or a step that fails.
pub fn macos_assets(args: &Args, root: &Path, cargo: &Path) -> Result<Vec<String>, Error> {
    let steps = checked_steps(args, root)?;
    let xtask = std::env::current_exe().map_err(Error::spawn("find this program".to_owned()))?;
    let programs = Programs {
        cargo: cargo.to_owned(),
        xtask,
    };
    run_steps(&steps, root, &programs, &mut exec, &mut std::io::stderr())
}
