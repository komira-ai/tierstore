use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use super::*;

const V: &str = "1.2.3+0123456789ab";

/// The answer of `releases converge` for a farm of two roles.
fn answer(desired: &str, state: &str, roles: &[(&str, u64, u64, u64)]) -> String {
    let roles: Vec<Value> = roles
        .iter()
        .map(|(role, updating, failed, down)| {
            serde_json::json!({
                "role": role, "atDesired": 2, "updating": updating, "failed": failed, "down": down
            })
        })
        .collect();
    serde_json::json!({
        "convergence": {
            "desired": desired,
            "state": format!("CONVERGENCE_STATE_{state}"),
            "roles": roles,
        }
    })
    .to_string()
}

fn healthy() -> String {
    answer(V, "CONVERGED", &[("hot", 0, 0, 0), ("cold", 0, 0, 0)])
}

/// A reader that answers from a script, then repeats its last answer; it counts the reads.
struct Script {
    reads: Cell<u32>,
    answers: RefCell<VecDeque<Read>>,
}

impl Script {
    fn of(answers: Vec<Read>) -> Script {
        Script {
            reads: Cell::new(0),
            answers: RefCell::new(answers.into()),
        }
    }

    fn always(text: String) -> Script {
        Script::of(vec![Read::Reply(text)])
    }
}

impl Reader for Script {
    fn read(&self) -> Result<Read, Error> {
        self.reads.set(self.reads.get() + 1);
        let mut answers = self.answers.borrow_mut();
        if answers.len() > 1 {
            return Ok(answers.pop_front().expect("a first answer"));
        }
        Ok(answers.front().expect("a last answer").clone())
    }
}

/// A clock that starts at a time and records every wait.
struct Recording {
    now: u64,
    waits: Vec<u64>,
}

impl Clock for Recording {
    fn now(&mut self) -> u64 {
        self.now
    }

    fn wait(&mut self, secs: u64) {
        self.waits.push(secs);
        self.now += secs;
    }
}

fn request(hours: u32, interval_secs: u64) -> Request<'static> {
    Request {
        hours,
        reached_at: 1000,
        version: V,
        interval_secs,
        max_silent_reads: 3,
    }
}

fn clock() -> Recording {
    Recording {
        now: 1000,
        waits: Vec::new(),
    }
}

fn failed(result: Result<Vec<String>, Error>) -> String {
    match result {
        Err(Error::Failed(why)) => why,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn usage(result: Result<Vec<String>, Error>) -> String {
    match result {
        Err(Error::Usage(why)) => why,
        other => panic!("expected a usage error, got {other:?}"),
    }
}

#[test]
fn a_soak_of_zero_hours_ends_at_once_and_reads_nothing() {
    // Catches: the default of FARM_SOAK_HOURS (0) holding a release back, or asking the
    // coordinator for a read nobody needs.
    let reader = Script::always(healthy());
    let mut time = clock();
    let said = soak(&request(0, 60), &mut time, Some(&reader)).expect("0 hours");
    assert_eq!(
        said,
        ["soak: 0 hours, so the public channels follow at once"]
    );
    assert_eq!(reader.reads.get(), 0);
    assert!(time.waits.is_empty());
    // And it needs no reading program at all.
    soak(&request(0, 60), &mut time, None).expect("0 hours without a reader");
}

#[test]
fn more_than_five_hours_is_refused_before_anything_is_read() {
    // Catches: a job that ends silently at the 6-hour limit, which would let the public channels
    // follow without the soak having passed.
    let reader = Script::always(healthy());
    let why = usage(soak(&request(6, 60), &mut clock(), Some(&reader)));
    assert!(why.contains("--hours 6 is more than 5"), "{why}");
    assert_eq!(reader.reads.get(), 0);
    // Five is the most.
    soak(&request(5, 3600), &mut clock(), Some(&reader)).expect("5 hours");
}

#[test]
fn a_soak_that_must_read_without_a_reading_program_is_refused() {
    // Catches: a soak of N hours that passes without having looked at the farm once.
    let why = usage(soak(&request(1, 60), &mut clock(), None));
    assert!(why.contains("needs --read-command"), "{why}");
}

#[test]
fn flags_that_cannot_work_are_refused() {
    // Catches: an interval of 0 that spins, no tolerated silence, and a version that matches
    // nothing.
    let reader = Script::always(healthy());
    let zero_interval = request(1, 0);
    assert!(usage(soak(&zero_interval, &mut clock(), Some(&reader))).contains("at least 1"));
    let mut no_silence = request(1, 60);
    no_silence.max_silent_reads = 0;
    assert!(usage(soak(&no_silence, &mut clock(), Some(&reader))).contains("at least 1"));
    let mut no_version = request(1, 60);
    no_version.version = "";
    assert_eq!(
        usage(soak(&no_version, &mut clock(), Some(&reader))),
        "--version is empty"
    );
    assert_eq!(reader.reads.get(), 0);
}

#[test]
fn a_healthy_farm_is_read_at_the_start_every_interval_and_at_the_end() {
    // Catches: a soak that checks the farm once at the start and not at the end, or that waits
    // past the N hours.
    let reader = Script::always(healthy());
    let mut time = clock();
    let said = soak(&request(1, 1800), &mut time, Some(&reader)).expect("healthy");
    assert_eq!(
        said,
        ["soak: the farm stayed healthy on 1.2.3+0123456789ab for 1 hours (3 reads)"]
    );
    assert_eq!(reader.reads.get(), 3);
    assert_eq!(time.waits, [1800, 1800]);
    assert_eq!(time.now, 1000 + 3600);
}

#[test]
fn the_last_wait_is_cut_to_the_time_that_is_left() {
    // Catches: a soak that sleeps a whole interval past its end.
    let reader = Script::always(healthy());
    let mut time = clock();
    soak(&request(1, 2000), &mut time, Some(&reader)).expect("healthy");
    assert_eq!(time.waits, [2000, 1600]);
    assert_eq!(reader.reads.get(), 3);
}

#[test]
fn a_farm_already_past_the_end_is_read_once() {
    // Catches: a re-run long after the farm reached the version waiting again for nothing, or
    // passing without a read.
    let reader = Script::always(healthy());
    let mut time = clock();
    time.now = 1000 + 7200;
    soak(&request(1, 60), &mut time, Some(&reader)).expect("healthy");
    assert_eq!(reader.reads.get(), 1);
    assert!(time.waits.is_empty());
}

#[test]
fn the_first_read_that_shows_the_farm_not_healthy_ends_the_soak_and_says_why() {
    // Catches: each way the farm can stop being healthy being let through: a rollback or a newer
    // release (the desired version moves), a halt, an update that is not done, a host that went
    // quiet, a failed host, and an answer that is not the coordinator's.
    let cases: Vec<(String, &str)> = vec![
        (
            answer("1.2.2+aaaaaaaaaaaa", "CONVERGED", &[]),
            "the desired version is 1.2.2+aaaaaaaaaaaa, not 1.2.3+0123456789ab (a rollback or a newer release replaced it)",
        ),
        (
            "{\"convergence\":{\"state\":\"CONVERGENCE_STATE_CONVERGED\"}}".to_owned(),
            "the desired version is none, not 1.2.3+0123456789ab",
        ),
        (
            answer(V, "HALTED", &[]),
            "the state is halted, not converged",
        ),
        (
            answer(V, "IN_PROGRESS", &[("hot", 1, 0, 0)]),
            "the state is in_progress, not converged",
        ),
        ("{}".to_owned(), "the desired version is none"),
        (
            answer(V, "CONVERGED", &[("hot", 0, 0, 0), ("cold", 0, 0, 2)]),
            "role \"cold\" has 2 host(s) down",
        ),
        (
            answer(V, "CONVERGED", &[("hot", 0, 1, 0)]),
            "role \"hot\" has 1 host(s) failed",
        ),
        (
            answer(V, "CONVERGED", &[("hot", 3, 0, 0)]),
            "role \"hot\" has 3 host(s) updating",
        ),
        ("not json".to_owned(), "the answer is not JSON"),
    ];
    for (text, says) in cases {
        let reader = Script::of(vec![Read::Reply(healthy()), Read::Reply(text)]);
        let mut time = clock();
        let why = failed(soak(&request(1, 1800), &mut time, Some(&reader)));
        assert!(
            why.starts_with("soak: read 2 shows the farm not healthy on 1.2.3+0123456789ab: ")
                && why.contains(says),
            "{says}: {why}"
        );
        // It stopped at once: no third read, no wait after it.
        assert_eq!(reader.reads.get(), 2, "{says}");
        assert_eq!(time.waits, [1800], "{says}");
    }
}

#[test]
fn silence_is_tolerated_up_to_the_limit_in_a_row_and_a_reply_resets_it() {
    // Catches: one lost read failing a five-hour soak, and a coordinator that is gone being
    // waited for until the end.
    let silent = || Read::Silent("no route".to_owned());
    let reader = Script::of(vec![
        silent(),
        silent(),
        Read::Reply(healthy()),
        silent(),
        silent(),
        Read::Reply(healthy()),
    ]);
    soak(&request(1, 1800), &mut clock(), Some(&reader)).expect("two silent in a row, twice");

    let gone = Script::of(vec![Read::Reply(healthy()), silent()]);
    let why = failed(soak(&request(1, 60), &mut clock(), Some(&gone)));
    assert_eq!(
        why,
        "soak: the coordinator gave no answer to 3 reads in a row (last: \"no route\")"
    );
    // One reply, then the third silent read in a row ends it.
    assert_eq!(gone.reads.get(), 4);
}

#[test]
fn a_reading_program_that_cannot_start_ends_the_soak_at_the_first_read() {
    // Catches: a missing program taken for a farm that is healthy.
    struct Missing;
    impl Reader for Missing {
        fn read(&self) -> Result<Read, Error> {
            Err(Error::Spawn {
                what: "hostkeeper".to_owned(),
                why: "not found".to_owned(),
            })
        }
    }
    let result = soak(&request(1, 60), &mut clock(), Some(&Missing));
    assert!(matches!(result, Err(Error::Spawn { .. })), "{result:?}");
}

#[test]
fn the_machine_clock_tells_a_plausible_time_and_waits() {
    // Catches: a clock stuck at 0 (every deadline already passed) or one that cannot wait.
    let mut clock = SystemClock;
    assert!(clock.now() > 1_700_000_000);
    clock.wait(0);
}

#[test]
fn the_fixed_clock_moves_only_when_it_waits_and_does_not_overflow() {
    // Catches: a test clock that drifts with the machine's, or wraps at the top.
    let mut clock = FixedClock(10);
    assert_eq!(clock.now(), 10);
    clock.wait(5);
    assert_eq!(clock.now(), 15);
    let mut top = FixedClock(u64::MAX - 1);
    top.wait(10);
    assert_eq!(top.now(), u64::MAX);
}
