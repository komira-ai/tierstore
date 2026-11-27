//! What the mounts report of the attaches and detaches of runs, as the cache counts and renders
//! it: the reports read back as written and every other form refused; the families at zero before
//! any report; an attach counted under its outcome with each of its parts; a detach under its
//! outcome.

use super::{RunAttach, RunDetach, RunOutcome};
use crate::server::local_cache::testing::Rig;

/// The lines of the families in the cache's `/metricz` text whose names start with `name`.
fn lines(rig: &Rig, name: &str) -> Vec<String> {
    rig.cache
        .metrics()
        .lines()
        .filter(|line| line.contains(name))
        .map(str::to_owned)
        .collect()
}

/// The line of the series `series` in the cache's text: its value.
fn value(rig: &Rig, series: &str) -> String {
    let prefix = format!("{series} ");
    rig.cache
        .metrics()
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
        .unwrap_or_else(|| panic!("no {series}"))
}

/// Asserts that the reports read back as written, the largest numbers included, and that a
/// report with an outcome that is not one, a number fewer or more, an empty word, a number with a
/// sign or a non-digit, a number over 64 bits, and no words at all are refused.
#[test]
fn the_reports_read_back_as_written_and_any_other_form_is_refused() {
    let attach = RunAttach {
        outcome: RunOutcome::Failed,
        ms: 35,
        parts: [25, 7, 3],
    };
    assert_eq!(attach.line(), "run-attached failed 35 25 7 3");
    assert_eq!(RunAttach::parse("failed 35 25 7 3"), Some(attach));
    let most = RunAttach {
        outcome: RunOutcome::Ok,
        ms: u64::MAX,
        parts: [u64::MAX; 3],
    };
    let line = most.line();
    let rest = line.strip_prefix("run-attached ").unwrap();
    assert_eq!(RunAttach::parse(rest), Some(most));
    let big = format!("ok {}0 0 0 0", u64::MAX);
    for refused in [
        "",
        "ok",
        "absent 1 1 0 0",
        "ok 1 1 0",
        "ok 1 1 0 0 0",
        "ok 1  1 0 0",
        "ok +1 1 0 0",
        "ok 1x 1 0 0",
        big.as_str(),
    ] {
        assert_eq!(RunAttach::parse(refused), None, "{refused:?}");
    }
    let detach = RunDetach {
        outcome: RunOutcome::Ok,
        ms: 4,
    };
    assert_eq!(detach.line(), "run-detached ok 4");
    assert_eq!(RunDetach::parse("ok 4"), Some(detach));
    let big = format!("ok {}0", u64::MAX);
    for refused in ["", "ok", "ok ", "gone 4", "ok 4 4", "ok -4", big.as_str()] {
        assert_eq!(RunDetach::parse(refused), None, "{refused:?}");
    }
}

/// Asserts that before any report the three families have their `# TYPE` lines and a series of
/// each outcome (and part) at zero, in the order they are rendered.
#[test]
fn the_families_are_at_zero_before_a_report() {
    let rig = Rig::new("runs-none");
    let typed: Vec<String> = lines(&rig, "# TYPE tierstore_fs_run_");
    assert_eq!(
        typed,
        [
            "# TYPE tierstore_fs_run_attach_seconds histogram",
            "# TYPE tierstore_fs_run_attach_part_seconds histogram",
            "# TYPE tierstore_fs_run_detach_seconds histogram",
        ]
    );
    let counts: Vec<String> = lines(&rig, "tierstore_fs_run_")
        .into_iter()
        .filter(|line| line.contains("_count{"))
        .collect();
    let mut want = Vec::new();
    for outcome in ["ok", "failed"] {
        want.push(format!(
            "tierstore_fs_run_attach_seconds_count{{outcome=\"{outcome}\"}} 0"
        ));
    }
    for part in ["fetch", "flatten", "append"] {
        for outcome in ["ok", "failed"] {
            want.push(format!(
                "tierstore_fs_run_attach_part_seconds_count{{part=\"{part}\",outcome=\"{outcome}\"}} 0"
            ));
        }
    }
    for outcome in ["ok", "failed"] {
        want.push(format!(
            "tierstore_fs_run_detach_seconds_count{{outcome=\"{outcome}\"}} 0"
        ));
    }
    assert_eq!(counts, want);
}

/// Asserts that an attach is one sample under its outcome and one of each of its parts under the
/// same outcome, each in the bucket of its span, and that a failed attach is counted apart from
/// an attach that was done.
#[test]
fn an_attach_is_counted_under_its_outcome_with_each_part() {
    let rig = Rig::new("runs-attach");
    rig.cache.run_attached(RunAttach {
        outcome: RunOutcome::Ok,
        ms: 35,
        parts: [25, 7, 3],
    });
    rig.cache.run_attached(RunAttach {
        outcome: RunOutcome::Failed,
        ms: 9,
        parts: [9, 0, 0],
    });
    let attach = "tierstore_fs_run_attach_seconds";
    let part = "tierstore_fs_run_attach_part_seconds";
    assert_eq!(
        value(&rig, &format!("{attach}_sum{{outcome=\"ok\"}}")),
        "0.035"
    );
    assert_eq!(
        value(&rig, &format!("{attach}_count{{outcome=\"ok\"}}")),
        "1"
    );
    // 35 ms: above 0.025 s, at or under 0.05 s.
    let le = |le: &str| format!("{attach}_bucket{{outcome=\"ok\",le=\"{le}\"}}");
    assert_eq!(value(&rig, &le("0.025")), "0");
    assert_eq!(value(&rig, &le("0.05")), "1");
    assert_eq!(
        value(&rig, &format!("{attach}_sum{{outcome=\"failed\"}}")),
        "0.009"
    );
    for (name, sum, count) in [
        ("fetch\",outcome=\"ok", "0.025", "1"),
        ("flatten\",outcome=\"ok", "0.007", "1"),
        ("append\",outcome=\"ok", "0.003", "1"),
        ("fetch\",outcome=\"failed", "0.009", "1"),
        ("flatten\",outcome=\"failed", "0", "1"),
        ("append\",outcome=\"failed", "0", "1"),
    ] {
        assert_eq!(value(&rig, &format!("{part}_sum{{part=\"{name}\"}}")), sum);
        assert_eq!(
            value(&rig, &format!("{part}_count{{part=\"{name}\"}}")),
            count
        );
    }
    let le = format!("{part}_bucket{{part=\"flatten\",outcome=\"failed\",le=\"0.001\"}}");
    assert_eq!(value(&rig, &le), "1");
}

/// Asserts that a detach is one sample under its outcome.
#[test]
fn a_detach_is_counted_under_its_outcome() {
    let rig = Rig::new("runs-detach");
    rig.cache.run_detached(RunDetach {
        outcome: RunOutcome::Ok,
        ms: 4,
    });
    rig.cache.run_detached(RunDetach {
        outcome: RunOutcome::Failed,
        ms: 0,
    });
    rig.cache.run_detached(RunDetach {
        outcome: RunOutcome::Failed,
        ms: 2,
    });
    let detach = "tierstore_fs_run_detach_seconds";
    assert_eq!(
        value(&rig, &format!("{detach}_sum{{outcome=\"ok\"}}")),
        "0.004"
    );
    assert_eq!(
        value(&rig, &format!("{detach}_count{{outcome=\"ok\"}}")),
        "1"
    );
    assert_eq!(
        value(&rig, &format!("{detach}_sum{{outcome=\"failed\"}}")),
        "0.002"
    );
    assert_eq!(
        value(&rig, &format!("{detach}_count{{outcome=\"failed\"}}")),
        "2"
    );
}
