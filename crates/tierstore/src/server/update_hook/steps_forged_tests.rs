//! A record, a declaration or a params file that cannot be trusted changes nothing.

use super::testing::{DECLARATION, Fake, NEW};
use super::{Class, Marker, Outcome, Params, Reason, Record, Step, run_step};

const ALL: [Step; 6] = Step::ALL;

const GOOD: &str =
    r#"{"schema":1,"serving":9302,"previous":9301,"mode":"handover","markers":["swapped"]}"#;

/// The calls that touch the machine: none may be made before the record is trusted.
fn touched(fake: &Fake) -> Vec<String> {
    fake.log()
        .into_iter()
        .filter(|c| {
            [
                "nft.",
                "units.",
                "slots.",
                "get ",
                "host.capabilities",
                "host.conntrack",
            ]
            .iter()
            .any(|p| c.starts_with(p))
        })
        .collect()
}

fn refused(fake: &Fake, step: Step, why: &str) {
    let done = fake.run(step);
    assert_eq!(done.outcome, Outcome::Failure, "{step} {why}: {done:?}");
    assert_eq!(
        done.report.reason,
        Some(Reason::StateInvalid),
        "{step} {why}"
    );
    assert_eq!(done.report.class, Class::Environment);
    assert!(
        touched(fake).is_empty(),
        "{step} {why}: {:?}",
        touched(fake)
    );
    assert!(fake.changes().is_empty(), "{step} {why}");
    assert_eq!(fake.live_locks(), 0);
}

/// Asserts that a `state.json` with a port outside the two, an unknown mode or marker, equal
/// `serving` and `previous`, a newer or older schema, markers that do not follow from one
/// another, or text that is not JSON makes every step and `restore` exit 1 with `state_invalid`
/// and with no `nft`, unit, link or instance call. It would catch a hook that takes a port from
/// the record, and so starts and stops what a forged file names.
#[test]
fn a_record_that_cannot_be_trusted_makes_every_step_refuse_before_any_call() {
    let plants = [
        GOOD.replace("9302", "9999"),
        GOOD.replace("9301", "9999"),
        GOOD.replace("9301", "9302"),
        GOOD.replace("handover", "reboot"),
        GOOD.replace("swapped", "retired"),
        GOOD.replace("\"schema\":1", "\"schema\":2"),
        GOOD.replace("\"schema\":1", "\"schema\":0"),
        GOOD.replace("\"swapped\"", "\"verified\""),
        GOOD.replace("\"swapped\"", "\"swapped\",\"stopping\""),
        GOOD.replace("9301", "null"),
        "{ not json".to_owned(),
        String::new(),
    ];
    for plant in plants {
        for step in ALL {
            let fake = Fake::new();
            fake.set_record_bytes(Some(plant.as_bytes()));
            refused(&fake, step, &plant);
        }
    }
    let sane = Fake::new();
    sane.set_record_bytes(Some(GOOD.as_bytes()));
    assert_eq!(sane.run(Step::Verify).report.reason, Some(Reason::NotReady));
}

/// Asserts that a declaration whose ports or units are not the two slots refuses every step the
/// same way. It would catch a hook that builds a unit name from a port it was not declared.
#[test]
fn a_declaration_that_is_not_the_two_slots_makes_every_step_refuse() {
    for step in ALL {
        let fake = Fake::new();
        fake.set_declaration(&DECLARATION.replace("[9301, 9302]", "[9301, 9303]"));
        refused(&fake, step, "declaration");
    }
}

/// Asserts that a staged release inside the state directory is refused for every step that was
/// given params. It would catch a release the service-writable area could swap.
#[test]
fn a_staged_release_in_the_state_directory_is_refused() {
    let fake = Fake::new();
    let text = "promotion_bound_secs = 30\n\"updater.staged_release\" = \"/var/lib/updater/hooks/tierstore/slots/9302\"\n\"updater.target_version\" = \"0.4.0\"\n";
    let params = Params::parse(text).unwrap();
    for step in [
        Step::Prepare,
        Step::Cutover,
        Step::Verify,
        Step::Rollback,
        Step::Cleanup,
    ] {
        let done = run_step(step, Some(&params), &fake.ports());
        assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{step}");
    }
    assert!(touched(&fake).is_empty());
}

/// Asserts that with no record the five steps refuse (the first install lays the record down)
/// with no call, while `restore` has nothing to do. It would catch a step that invents a record.
#[test]
fn without_a_record_the_steps_refuse() {
    for step in [
        Step::Prepare,
        Step::Cutover,
        Step::Verify,
        Step::Rollback,
        Step::Cleanup,
    ] {
        let fake = Fake::new();
        fake.set_record_bytes(None);
        refused(&fake, step, "no record");
    }
}

/// Asserts that a step that needs the params file and has none is `bad_params` and touches
/// nothing, and that `rollback` and `cleanup`, which read nothing from it, run without it. It
/// would catch a step that runs on a default it made up.
#[test]
fn a_step_that_needs_the_params_file_refuses_without_it() {
    for step in [Step::Prepare, Step::Cutover, Step::Verify] {
        let fake = Fake::new();
        fake.advance(&[Step::Prepare, Step::Cutover]);
        let mark = fake.changes().len();
        let done = run_step(step, None, &fake.ports());
        assert_eq!(done.report.reason, Some(Reason::BadParams), "{step}");
        assert_eq!(done.report.class, Class::Environment);
        assert_eq!(fake.changes().len(), mark);
    }
    let fake = Fake::new();
    fake.advance(&[Step::Prepare]);
    assert_eq!(
        run_step(Step::Rollback, None, &fake.ports()).outcome,
        Outcome::Success
    );
    assert_eq!(fake.record(), Record::idle(9301));
    assert!(!fake.record().markers.contains(&Marker::Swapped));
}

/// Asserts that a second step while another holds the lock does nothing and says `locked`, and
/// that the lock is released after every step, whatever its outcome. It would catch two steps
/// that write the record at once.
#[test]
fn a_busy_lock_stops_the_step_before_anything_is_read() {
    let fake = Fake::new();
    fake.set_busy(true);
    let done = fake.run(Step::Cutover);
    assert_eq!(done.report.reason, Some(Reason::Locked));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(fake.log(), ["state.lock"]);
    fake.set_busy(false);
    fake.advance(&[Step::Prepare]);
    assert_eq!(fake.live_locks(), 0);
    let _ = fake.run(Step::Cleanup);
    assert_eq!(fake.live_locks(), 0);
}

/// Asserts that the params the fake builds name the staged release (so the forged-params test
/// above is a change of one path and nothing else).
#[test]
fn the_fakes_params_name_the_staged_release() {
    assert_eq!(Fake::new().params().staged_release.to_str(), Some(NEW));
}
