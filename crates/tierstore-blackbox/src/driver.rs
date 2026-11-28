//! `tierstore-blackbox`: runs the cases, each against a node of its own, and reports them.
//!
//! ```text
//! tierstore-blackbox --tierstore <binary> --fake-s3 <binary> --scratch <dir>
//!     [--case <name>]... [--small-disk <dir>] [the numbers of the cases]
//! tierstore-blackbox <root> [--case <name>]...
//! ```
//!
//! The second form is the driver of a `blackbox_test` of buildrules, which runs it with one
//! argument, the root the package under test was unpacked onto: the binary is
//! `<root>/usr/libexec/tierstore/tierstore`, `fake-s3` is the program beside the driver's own,
//! the scratch directory is `<root>/var/tmp/tierstore-blackbox` (made if it is not there), and
//! with no `--case` only the cases that need no mount run (`start-without-fs`): such a run is an
//! ordinary user with no capabilities and no `/dev/fuse`.
//!
//! `--tierstore` is the binary of the package under test (`/usr/libexec/tierstore/tierstore` of
//! the `.deb`); it is started as `<binary> --config <node file>`. Each case gets the directory
//! `<scratch>/<case>`, which must not exist yet. `--small-disk` is a file system of its own (a
//! tmpfs of a few hundred MiB) for the case `disk-full`, which fills it; that case is refused
//! without it. With no `--case`, every case runs.
//!
//! It prints one line per case, `PASS <case>: <what it saw>` or `FAIL <case>: <what failed>`,
//! and exits 0 when every case passed, 1 when one failed, and 2 when its flags are refused.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use clap::Parser;

use crate::cases::{self, Case, Outcome, Params};
use crate::live::{Live, Setup};
use crate::os;
use crate::rig::Rig;

/// How long a node may take to serve its provider's socket after its start line.
const SOCKET_WAIT: Duration = Duration::from_secs(60);

/// The most budget of a case with its cache in its directory: less on a scratch file system of
/// under twice this size, such as a farm job's of 1 GiB.
const BUDGET_MIB: u64 = 1024;

/// The most of its file system's size a cache's budget takes (the small disk of `disk-full`, or
/// the scratch's): the node refuses a budget over the file system less its free floor.
const SMALL_DISK_SHARE: u64 = 2;

/// The flags of `tierstore-blackbox`.
#[derive(Debug, Parser)]
#[command(
    name = "tierstore-blackbox",
    about = "The black-box test of a released tierstore: its mount provider, driven from outside"
)]
struct Args {
    /// The root the package under test was unpacked onto, in place of the three flags below.
    #[arg(conflicts_with_all = ["tierstore", "fake_s3", "scratch"])]
    root: Option<PathBuf>,
    /// The binary of the package under test.
    #[arg(long, required_unless_present = "root")]
    tierstore: Option<PathBuf>,
    /// The `fake-s3` program, the node's upstream.
    #[arg(long, required_unless_present = "root")]
    fake_s3: Option<PathBuf>,
    /// The directory of the cases' directories.
    #[arg(long, required_unless_present = "root")]
    scratch: Option<PathBuf>,
    /// A case to run (start-without-fs, t1-cycles, t2-placements, t3-stall, restart,
    /// upstream-errors, disk-full);
    /// again for more. Every case when none is named.
    #[arg(long = "case", value_parser = case_of)]
    cases: Vec<Case>,
    /// A file system of its own for the cache of `disk-full`, which fills it.
    #[arg(long)]
    small_disk: Option<PathBuf>,
    /// T1: the cycles.
    #[arg(long, default_value_t = Params::defaults().cycles)]
    cycles: usize,
    /// T2: the node's trees.max_placements.
    #[arg(long, default_value_t = Params::defaults().max_placements, value_parser = clap::value_parser!(u32).range(1..))]
    max_placements: u32,
    /// T2: the Prepares past it.
    #[arg(long, default_value_t = Params::defaults().burst)]
    burst: usize,
    /// T3: the Prepares that arrive while the upstream stalls.
    #[arg(long, default_value_t = u32::try_from(Params::defaults().prepares).unwrap_or(u32::MAX), value_parser = clap::value_parser!(u32).range(1..))]
    prepares: u32,
    /// T3: the seconds they arrive over.
    #[arg(long, default_value_t = Params::defaults().arrival.as_secs())]
    arrival_secs: u64,
    /// T3: the seconds the upstream stalls.
    #[arg(long, default_value_t = Params::defaults().stall.as_secs())]
    stall_secs: u64,
    /// T3: the files of its tree.
    #[arg(long, default_value_t = Params::defaults().stall_files)]
    stall_files: usize,
    /// T3 and the case of errors: the MiB the resident memory may be above its idle size.
    #[arg(long, default_value_t = Params::defaults().memory_bound >> 20)]
    memory_bound_mib: u64,
    /// The case of errors: the seconds the upstream fails.
    #[arg(long, default_value_t = Params::defaults().errors_for.as_secs())]
    errors_secs: u64,
    /// The seconds a sample of the memory is read every.
    #[arg(long, default_value_t = Params::defaults().sample.as_secs(), value_parser = clap::value_parser!(u64).range(1..))]
    sample_secs: u64,
}

fn case_of(name: &str) -> Result<Case, String> {
    Case::of(name).ok_or_else(|| {
        let names: Vec<&str> = Case::ALL.iter().map(|case| case.name()).collect();
        format!("no case {name:?}; the cases: {}", names.join(", "))
    })
}

impl Args {
    /// The numbers of the cases.
    fn params(&self) -> Params {
        let defaults = Params::defaults();
        Params {
            cycles: self.cycles,
            max_placements: self.max_placements,
            burst: self.burst,
            prepares: self.prepares as usize,
            arrival: Duration::from_secs(self.arrival_secs),
            stall: Duration::from_secs(self.stall_secs),
            stall_files: self.stall_files,
            memory_bound: self.memory_bound_mib << 20,
            errors_for: Duration::from_secs(self.errors_secs),
            sample: Duration::from_secs(self.sample_secs),
            ..defaults
        }
    }

    /// The cases to run, in their order: those named, or every one (under a root, every one that
    /// needs no mount).
    fn cases(&self) -> Result<Vec<Case>, String> {
        let mut cases = match (self.cases.is_empty(), &self.root) {
            (false, _) => self.cases.clone(),
            (true, None) => Case::ALL.to_vec(),
            (true, Some(_)) => Case::ALL.into_iter().filter(|c| !c.mounts()).collect(),
        };
        cases.sort_unstable();
        cases.dedup();
        if cases.contains(&Case::Disk) && self.small_disk.is_none() {
            return Err(
                "the case disk-full needs --small-disk, a file system of its own that it fills; give one, or name the other cases with --case".to_owned(),
            );
        }
        Ok(cases)
    }
}

/// The programs a run starts and the directory of its cases.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Programs {
    tierstore: PathBuf,
    fake_s3: PathBuf,
    scratch: PathBuf,
}

/// Where the binary of the package is under the root it was unpacked onto.
const PACKAGED_BINARY: &str = "usr/libexec/tierstore/tierstore";

/// The scratch directory under the root.
const ROOT_SCRATCH: &str = "var/tmp/tierstore-blackbox";

impl Args {
    /// The programs and the scratch directory: the flags', or, under a root, the package's binary,
    /// the `fake-s3` beside `driver` (the path of this program) and the root's scratch directory,
    /// made if it is not there.
    fn programs(&self, driver: std::io::Result<PathBuf>) -> Result<Programs, String> {
        let Some(root) = &self.root else {
            return match (&self.tierstore, &self.fake_s3, &self.scratch) {
                (Some(tierstore), Some(fake_s3), Some(scratch)) => Ok(Programs {
                    tierstore: tierstore.clone(),
                    fake_s3: fake_s3.clone(),
                    scratch: scratch.clone(),
                }),
                _ => {
                    Err("--tierstore, --fake-s3 and --scratch are needed without a root".to_owned())
                }
            };
        };
        let driver = driver.map_err(|why| format!("the path of this program: {why}"))?;
        let beside = driver
            .parent()
            .ok_or_else(|| format!("{} is in no directory", driver.display()))?;
        let scratch = root.join(ROOT_SCRATCH);
        std::fs::create_dir_all(&scratch).map_err(|why| format!("{}: {why}", scratch.display()))?;
        Ok(Programs {
            tierstore: root.join(PACKAGED_BINARY),
            fake_s3: beside.join("fake-s3"),
            scratch,
        })
    }
}

/// Where the rigs of the cases come from: a rig per case, which the driver stops after it.
pub(crate) trait Rigs {
    type Rig: Rig;

    /// The rig of `case`.
    async fn start(&mut self, case: Case, params: &Params) -> Result<Self::Rig, String>;
}

/// The rigs of running nodes (`crate::live`).
struct Nodes<'a> {
    args: &'a Args,
    programs: &'a Programs,
}

impl Rigs for Nodes<'_> {
    type Rig = Live;

    async fn start(&mut self, case: Case, params: &Params) -> Result<Live, String> {
        let dir = self.programs.scratch.join(case.name());
        std::fs::create_dir(&dir).map_err(|why| format!("{}: {why}", dir.display()))?;
        let small_disk = (case == Case::Disk)
            .then(|| self.args.small_disk.clone())
            .flatten();
        let cache_on = small_disk.as_deref().unwrap_or(&self.programs.scratch);
        let size = os::disk_bytes(cache_on).map_err(|e| e.to_string())?;
        let budget_mib = budget_mib(size, small_disk.is_some());
        let max_placements = match case {
            Case::Placements => Some(params.max_placements),
            Case::Stall => Some(
                u32::try_from(params.prepares)
                    .unwrap_or(u32::MAX)
                    .saturating_add(1024),
            ),
            _ => None,
        };
        let setup = Setup {
            dir,
            fake_s3: self.programs.fake_s3.clone(),
            small_disk,
            budget_mib,
            max_placements,
            fs: case.mounts(),
            socket_wait: SOCKET_WAIT,
        };
        let tierstore = self.programs.tierstore.clone();
        let command = Box::new(move |file: &Path| {
            let mut command = Command::new(&tierstore);
            command.arg("--config").arg(file);
            command
        });
        Live::start(setup, command).await
    }
}

/// The budget, in MiB, of a cache on a file system of `size` bytes: a share of it, and at most
/// [`BUDGET_MIB`] unless the file system is the case's own disk.
fn budget_mib(size: u64, own_disk: bool) -> u64 {
    let share = ((size / SMALL_DISK_SHARE) >> 20).max(1);
    if own_disk {
        share
    } else {
        share.min(BUDGET_MIB)
    }
}

/// Runs `cases` with `params` on the rigs of `rigs`, writing a line per case to `out`: whether
/// every case passed.
pub(crate) async fn drive<S: Rigs>(
    rigs: &mut S,
    cases: &[Case],
    params: &Params,
    out: &mut impl Write,
) -> bool {
    let mut passed = true;
    for case in cases {
        let outcome = match rigs.start(*case, params).await {
            Ok(mut rig) => {
                let mut outcome = cases::run(*case, &mut rig, params).await;
                if let Err(why) = rig.stop().await {
                    outcome.failures.push(format!("stopping the rig: {why}"));
                }
                outcome
            }
            Err(why) => Outcome {
                failures: vec![format!("the rig did not start: {why}")],
                notes: Vec::new(),
            },
        };
        passed &= outcome.passed();
        let _ = writeln!(out, "{}", line(*case, &outcome));
        let _ = out.flush();
    }
    passed
}

/// The report line of `case`.
fn line(case: Case, outcome: &Outcome) -> String {
    if outcome.passed() {
        format!("PASS {}: {}", case.name(), outcome.notes.join("; "))
    } else {
        format!(
            "FAIL {}: {} (saw: {})",
            case.name(),
            outcome.failures.join("; "),
            outcome.notes.join("; ")
        )
    }
}

/// The program: the flags, then every case.
pub fn main() -> ExitCode {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(refused) => {
            let _ = refused.print();
            return ExitCode::from(u8::try_from(refused.exit_code()).unwrap_or(2));
        }
    };
    let cases = match args.cases() {
        Ok(cases) => cases,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "tierstore-blackbox: {why}");
            return ExitCode::from(2);
        }
    };
    let programs = match args.programs(std::env::current_exe()) {
        Ok(programs) => programs,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "tierstore-blackbox: {why}");
            return ExitCode::from(1);
        }
    };
    let params = args.params();
    let mut nodes = Nodes {
        args: &args,
        programs: &programs,
    };
    let ran = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map(|runtime| {
            runtime.block_on(drive(&mut nodes, &cases, &params, &mut std::io::stdout()))
        });
    exit_of(ran, &mut std::io::stderr())
}

/// The exit status of a run: 0 when every case passed, 1 when one failed or the runtime of the
/// run could not be made, which is said on `err`.
fn exit_of(ran: std::io::Result<bool>, err: &mut impl Write) -> ExitCode {
    match ran {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(why) => {
            let _ = writeln!(err, "tierstore-blackbox: the runtime: {why}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
