//! The driver's flags, and its run of the cases over rigs of the tests (the model of a node of
//! the cases' tests); the program itself is in `tests/driver.rs`.

use std::collections::VecDeque;

use super::*;
use crate::cases::model::Model;

fn args(flags: &[&str]) -> Result<Args, clap::Error> {
    let mut all = vec![
        "tierstore-blackbox",
        "--tierstore",
        "/t",
        "--fake-s3",
        "/f",
        "--scratch",
        "/s",
    ];
    all.extend_from_slice(flags);
    Args::try_parse_from(all)
}

/// Asserts that with no flag but the three paths the cases' numbers are the plan's, and that
/// each number's flag sets it.
#[test]
fn the_numbers_are_the_plans_unless_a_flag_sets_them() {
    assert_eq!(args(&[]).unwrap().params(), Params::defaults());
    let set = args(&[
        "--cycles",
        "7",
        "--max-placements",
        "4",
        "--burst",
        "2",
        "--prepares",
        "30",
        "--arrival-secs",
        "3",
        "--stall-secs",
        "9",
        "--stall-files",
        "100",
        "--memory-bound-mib",
        "64",
        "--errors-secs",
        "5",
        "--sample-secs",
        "2",
    ])
    .unwrap()
    .params();
    assert_eq!(
        set,
        Params {
            cycles: 7,
            max_placements: 4,
            burst: 2,
            prepares: 30,
            arrival: Duration::from_secs(3),
            stall: Duration::from_secs(9),
            stall_files: 100,
            memory_bound: 64 << 20,
            errors_for: Duration::from_secs(5),
            sample: Duration::from_secs(2),
            ..Params::defaults()
        }
    );
}

/// Asserts that flags the cases cannot run with are refused: a case of no name (the names
/// given), no `--max-placements`, `--prepares` or `--sample-secs` of 0, and a missing path.
#[test]
fn flags_the_cases_cannot_run_with_are_refused() {
    let refused = args(&["--case", "t9"]).unwrap_err().to_string();
    assert!(
        refused.contains(
            "no case \"t9\"; the cases: t1-cycles, t2-placements, t3-stall, restart, upstream-errors, disk-full"
        ),
        "{refused}"
    );
    for zero in ["--max-placements", "--prepares", "--sample-secs"] {
        assert!(args(&[zero, "0"]).is_err(), "{zero}");
    }
    assert!(Args::try_parse_from(["tierstore-blackbox", "--tierstore", "/t"]).is_err());
}

/// Asserts that the cases run in the driver's order whatever order they are named in, each once;
/// that with none named every case runs; and that `disk-full` is refused without a small disk,
/// named or not.
#[test]
fn the_cases_run_in_order_and_disk_full_needs_a_small_disk() {
    let named = args(&[
        "--case",
        "restart",
        "--case",
        "t1-cycles",
        "--case",
        "restart",
    ]);
    assert_eq!(
        named.unwrap().cases(),
        Ok(vec![Case::Cycles, Case::Restart])
    );
    let every = args(&["--small-disk", "/d"]).unwrap().cases();
    assert_eq!(every, Ok(Case::ALL.to_vec()));
    let refused = "the case disk-full needs --small-disk, a file system of its own that it fills; give one, or name the other cases with --case";
    assert_eq!(args(&[]).unwrap().cases(), Err(refused.to_owned()));
    assert_eq!(
        args(&["--case", "disk-full"]).unwrap().cases(),
        Err(refused.to_owned())
    );
}

/// Rigs of the tests: a healthy model for each case but those it fails to start, and models whose
/// stop fails, if set.
struct Models {
    refused: Vec<Case>,
    stop: VecDeque<Option<String>>,
    started: Vec<Case>,
}

impl Rigs for Models {
    type Rig = Model;

    async fn start(&mut self, case: Case, _params: &Params) -> Result<Model, String> {
        self.started.push(case);
        if self.refused.contains(&case) {
            return Err("no node".to_owned());
        }
        let mut model = Model::healthy();
        model.stop_fails = self.stop.pop_front().flatten();
        Ok(model)
    }
}

/// Asserts that the driver runs each case on a rig of its own and prints a line for it: PASS
/// with what it saw, and FAIL with what failed, for a rig that did not start and for one that
/// did not stop; and that it says whether every case passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_case_runs_on_its_own_rig_and_gets_a_line() {
    let mut models = Models {
        refused: Vec::new(),
        stop: VecDeque::new(),
        started: Vec::new(),
    };
    let mut out = Vec::new();
    let cases = [Case::Cycles, Case::Restart];
    assert!(drive(&mut models, &cases, &Params::defaults(), &mut out).await);
    assert_eq!(models.started, cases);
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "PASS t1-cycles: 2100 cycles in 0ns; no placement left 0ns after the last Release"
    );
    assert!(lines[1].starts_with("PASS restart: "), "{text}");

    let mut models = Models {
        refused: vec![Case::Cycles],
        stop: VecDeque::from([Some("SIGTERM to tierstore: no such process".to_owned())]),
        started: Vec::new(),
    };
    let mut out = Vec::new();
    assert!(!drive(&mut models, &cases, &Params::defaults(), &mut out).await);
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "FAIL t1-cycles: the rig did not start: no node (saw: )"
    );
    assert!(
        lines[1].starts_with(
            "FAIL restart: stopping the rig: SIGTERM to tierstore: no such process (saw: killed right after"
        ),
        "{text}"
    );
}

/// Asserts the exit status of a run: 0 when every case passed, 1 when one failed, and 1, said on
/// standard error, when the run's runtime could not be made.
#[test]
fn a_run_exits_0_when_every_case_passed_and_1_otherwise() {
    let mut err = Vec::new();
    assert_eq!(exit_of(Ok(true), &mut err), ExitCode::SUCCESS);
    assert_eq!(exit_of(Ok(false), &mut err), ExitCode::from(1));
    assert!(err.is_empty());
    let refused = std::io::Error::other("no threads");
    assert_eq!(exit_of(Err(refused), &mut err), ExitCode::from(1));
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "tierstore-blackbox: the runtime: no threads\n"
    );
}
