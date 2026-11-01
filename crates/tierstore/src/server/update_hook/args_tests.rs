//! The command line of the hook.

use std::ffi::OsString;

use super::{ArgsError, Invocation, Step};

fn parse(words: &[&str]) -> Result<Invocation, ArgsError> {
    Invocation::parse(words.iter().map(OsString::from))
}

/// Asserts that every step is read from its own word and printed as it, and that a word that is
/// none of them is named in the refusal. It would catch a step added to the enum and not to the
/// parser, which the updater would then see as `unknown step`.
#[test]
fn every_step_is_read_from_its_word() {
    for step in Step::ALL {
        assert_eq!(step.name().parse::<Step>().unwrap(), step);
        assert_eq!(step.to_string(), step.name());
    }
    assert_eq!(
        "cutover!".parse::<Step>(),
        Err(ArgsError::UnknownStep("cutover!".to_owned()))
    );
}

/// Asserts what each shape of command line gives: `prepare` takes the staged release and the
/// params file, the other four steps only the params file, `restore` nothing. It would catch an
/// artifact accepted by a step that has none, or `restore` demanding a params file at boot, when
/// no update has run and there is none.
#[test]
fn each_step_takes_its_own_arguments() {
    let prepare = parse(&["prepare", "/releases/new", "--params", "/p.toml"]).unwrap();
    assert_eq!(prepare.step, Step::Prepare);
    assert_eq!(
        prepare.artifact.as_deref(),
        Some(std::path::Path::new("/releases/new"))
    );
    assert_eq!(
        prepare.params.as_deref(),
        Some(std::path::Path::new("/p.toml"))
    );
    let flag_first = parse(&["prepare", "--params", "/p.toml", "/releases/new"]).unwrap();
    assert_eq!(flag_first, prepare);
    for word in ["cutover", "verify", "rollback", "cleanup"] {
        let inv = parse(&[word, "--params", "/p.toml"]).unwrap();
        assert_eq!((inv.artifact, inv.params), (None, Some("/p.toml".into())));
    }
    let restore = parse(&["restore"]).unwrap();
    assert_eq!(restore.step, Step::Restore);
    assert_eq!((restore.artifact, restore.params), (None, None));
}

/// Asserts each refusal of a command line. It would catch a hook that runs a step on a command
/// line the updater did not mean, such as a second artifact or `--params` given twice.
#[test]
fn a_wrong_command_line_is_refused() {
    let cases: [(&[&str], ArgsError); 9] = [
        (&[], ArgsError::MissingStep),
        (
            &["bounce", "--params", "p"],
            ArgsError::UnknownStep("bounce".into()),
        ),
        (&["prepare", "--params", "p"], ArgsError::MissingArtifact),
        (
            &["prepare", "/a", "/b", "--params", "p"],
            ArgsError::Unexpected("/b".into()),
        ),
        (
            &["verify", "/a", "--params", "p"],
            ArgsError::Unexpected("/a".into()),
        ),
        (&["verify", "--params"], ArgsError::MissingParamsFile),
        (
            &["verify", "--params", "a", "--params", "b"],
            ArgsError::DuplicateParams,
        ),
        (&["verify"], ArgsError::MissingParams),
        (
            &["restore", "--params", "p"],
            ArgsError::Unexpected("--params".into()),
        ),
    ];
    for (words, why) in cases {
        assert_eq!(parse(words), Err(why), "{words:?}");
    }
}
