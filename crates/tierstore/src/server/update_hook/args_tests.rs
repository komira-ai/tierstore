//! The command line of the hook.

use std::ffi::OsString;
use std::path::Path;

use super::{ArgsError, Invocation, Step};

fn parse(words: &[&str]) -> Result<Invocation, ArgsError> {
    Invocation::parse(words.iter().map(OsString::from))
}

/// Asserts that every step is read from its own word and printed as it, that the steps are the
/// seven of the contract in order, and that a word that is none of them is named in the refusal.
/// It would catch a step added to the enum and not to the parser, which hostkeeper would then see
/// as `unknown step`.
#[test]
fn every_step_is_read_from_its_word() {
    let words: Vec<&str> = Step::ALL.iter().map(|step| step.name()).collect();
    assert_eq!(
        words,
        [
            "prepare",
            "cutover",
            "verify",
            "rollback",
            "cleanup",
            "uninstall",
            "restore"
        ]
    );
    for step in Step::ALL {
        assert_eq!(step.name().parse::<Step>().unwrap(), step);
        assert_eq!(step.to_string(), step.name());
    }
    assert_eq!(
        "cutover!".parse::<Step>(),
        Err(ArgsError::UnknownStep("cutover!".to_owned()))
    );
    assert_eq!(
        "Prepare".parse::<Step>(),
        Err(ArgsError::UnknownStep("Prepare".to_owned()))
    );
}

/// Asserts that `prepare` takes the staged release and the params file in either order, and that
/// the artifact is the staged release as given.
#[test]
fn prepare_takes_the_staged_release_and_the_params_file() {
    let prepare = parse(&["prepare", "/releases/new", "--params", "/p.toml"]).unwrap();
    assert_eq!(prepare.step, Step::Prepare);
    assert_eq!(
        prepare.artifact.as_deref(),
        Some(Path::new("/releases/new"))
    );
    assert_eq!(prepare.params, Path::new("/p.toml"));
    let flag_first = parse(&["prepare", "--params", "/p.toml", "/releases/new"]).unwrap();
    assert_eq!(flag_first, prepare);
}

/// Asserts that every step but `prepare`, the boot step `restore` included, takes the params
/// file and no artifact. It would catch a boot step that needs no params file when hostkeeper
/// always writes one, or an artifact taken by a step that has none.
#[test]
fn the_other_steps_take_only_the_params_file() {
    for step in Step::ALL.into_iter().filter(|step| *step != Step::Prepare) {
        let invocation = parse(&[step.name(), "--params", "/p.toml"]).unwrap();
        assert_eq!(invocation.step, step);
        assert_eq!(invocation.artifact, None);
        assert_eq!(invocation.params, Path::new("/p.toml"));
    }
}

/// Asserts each refusal of a command line, with its words. It would catch a hook that runs a
/// step on a command line hostkeeper did not mean, such as a second artifact, an artifact on a
/// step that has none, or `--params` given twice.
#[test]
fn a_wrong_command_line_is_refused() {
    let cases: [(&[&str], ArgsError, &str); 10] = [
        (&[], ArgsError::MissingStep, "no step given"),
        (
            &["bounce", "--params", "p"],
            ArgsError::UnknownStep("bounce".into()),
            "unknown step \"bounce\"",
        ),
        (
            &["prepare", "--params", "p"],
            ArgsError::MissingArtifact,
            "prepare needs the staged release directory",
        ),
        (
            &["prepare", "/a", "/b", "--params", "p"],
            ArgsError::Unexpected("/b".into()),
            "unexpected argument \"/b\"",
        ),
        (
            &["verify", "/a", "--params", "p"],
            ArgsError::Unexpected("/a".into()),
            "unexpected argument \"/a\"",
        ),
        (
            &["verify", "--params"],
            ArgsError::MissingParamsFile,
            "--params needs a file",
        ),
        (
            &["verify", "--params", "a", "--params", "b"],
            ArgsError::DuplicateParams,
            "--params given twice",
        ),
        (
            &["verify"],
            ArgsError::MissingParams,
            "--params <file> is required",
        ),
        (
            &["restore"],
            ArgsError::MissingParams,
            "--params <file> is required",
        ),
        (
            &["prepare", "/a"],
            ArgsError::MissingParams,
            "--params <file> is required",
        ),
    ];
    for (words, why, text) in cases {
        let got = parse(words);
        assert_eq!(got, Err(why), "{words:?}");
        assert_eq!(got.unwrap_err().to_string(), text, "{words:?}");
    }
}
