//! The command line of the hook.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::time::Duration;

use super::{ArgsError, HostArgs, Invocation, Step, host_flags};

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

/// Asserts that without a flag of the host the host's own defaults hold (the root `/`, root's
/// uid, `/usr/sbin/nft`, `/usr/bin/systemctl`, port 9300, the steps' own poll), and that the six
/// flags of a conformance build are read before the step (where the updater's `hook_args` go),
/// after it, and with `restore`, whose other arguments are refused. It would catch a flag taken
/// for the step or the artifact, or a default that is not the host's.
#[test]
fn the_flags_of_the_host_are_read_around_the_step() {
    let bare = parse(&["restore"]).unwrap();
    assert_eq!(bare.host, HostArgs::default());
    assert_eq!(bare.host.root, Path::new("/"));
    assert_eq!(bare.host.root_uid, 0);
    assert_eq!(bare.host.nft, Path::new("/usr/sbin/nft"));
    assert_eq!(bare.host.systemctl, Path::new("/usr/bin/systemctl"));
    assert_eq!(bare.host.public_port, 9300);
    assert_eq!(bare.host.poll, None);
    let given = parse(&[
        "--root",
        "/scratch",
        "--root-uid",
        "1000",
        "verify",
        "--params",
        "/p.toml",
        "--nft",
        "/fake/nft",
        "--systemctl",
        "/fake/systemctl",
        "--public-port",
        "19300",
        "--poll-interval-ms",
        "1",
    ])
    .unwrap();
    assert_eq!(given.step, Step::Verify);
    assert_eq!(given.params.as_deref(), Some(Path::new("/p.toml")));
    assert_eq!(
        given.host,
        HostArgs {
            root: "/scratch".into(),
            root_uid: 1000,
            nft: "/fake/nft".into(),
            systemctl: "/fake/systemctl".into(),
            public_port: 19300,
            poll: Some(Duration::from_millis(1)),
        }
    );
    let restore = parse(&["restore", "--root", "/scratch"]).unwrap();
    assert_eq!(restore.host.root, Path::new("/scratch"));
    let prepare = parse(&[
        "prepare",
        "--nft",
        "/fake/nft",
        "/releases/new",
        "--params",
        "/p.toml",
    ])
    .unwrap();
    assert_eq!(
        prepare.artifact.as_deref(),
        Some(Path::new("/releases/new"))
    );
    assert_eq!(prepare.host.nft, Path::new("/fake/nft"));
}

/// Asserts each refusal of a flag of the host, and its words. It would catch a hook that runs
/// with a relative program (found through a search path), a root spelt with `..`, a program
/// path whose bytes are not text (which would be run under another name), a port of 0, or a flag
/// given twice of which one silently wins.
#[test]
fn a_wrong_flag_of_the_host_is_refused() {
    let cases: [(&[&str], ArgsError); 13] = [
        (&["restore", "--root"], ArgsError::MissingValue("--root")),
        (&["--root", "/a"], ArgsError::MissingStep),
        (
            &["restore", "--root", "/a", "--root", "/b"],
            ArgsError::DuplicateFlag("--root"),
        ),
        (&["restore", "--nft", "nft"], ArgsError::NotPlain("--nft")),
        (
            &["restore", "--systemctl", "/usr/bin/../bin/systemctl"],
            ArgsError::NotPlain("--systemctl"),
        ),
        (&["restore", "--root", "/a/"], ArgsError::NotPlain("--root")),
        (
            &["restore", "--root-uid", "-1"],
            ArgsError::NotNumber("--root-uid"),
        ),
        (
            &["restore", "--root-uid", "root"],
            ArgsError::NotNumber("--root-uid"),
        ),
        (
            &["restore", "--root-uid", "0", "--root-uid", "0"],
            ArgsError::DuplicateFlag("--root-uid"),
        ),
        (
            &["restore", "--public-port", "0"],
            ArgsError::NotNumber("--public-port"),
        ),
        (
            &["restore", "--public-port", "70000"],
            ArgsError::NotNumber("--public-port"),
        ),
        (
            &["restore", "--poll-interval-ms", "soon"],
            ArgsError::NotNumber("--poll-interval-ms"),
        ),
        (
            &["restore", "--poll-interval-ms"],
            ArgsError::MissingValue("--poll-interval-ms"),
        ),
    ];
    for (words, why) in cases {
        assert_eq!(parse(words), Err(why), "{words:?}");
    }
    let not_text = OsString::from_vec(b"/usr/sbin/nft\xff".to_vec());
    let words = [OsString::from("restore"), OsString::from("--nft"), not_text];
    assert_eq!(Invocation::parse(words), Err(ArgsError::NotPlain("--nft")));
    let not_text = OsString::from_vec(b"9\xff".to_vec());
    let words = [
        OsString::from("restore"),
        OsString::from("--root-uid"),
        not_text,
    ];
    assert_eq!(
        Invocation::parse(words),
        Err(ArgsError::NotNumber("--root-uid"))
    );
    assert_eq!(
        ArgsError::MissingValue("--nft").to_string(),
        "--nft needs a value"
    );
    assert_eq!(
        ArgsError::NotPlain("--root").to_string(),
        "--root must be an absolute UTF-8 path of plain components"
    );
    assert_eq!(
        ArgsError::DuplicateFlag("--root").to_string(),
        "--root given twice"
    );
    assert_eq!(
        ArgsError::NotNumber("--public-port").to_string(),
        "--public-port must be a whole number in its range"
    );
}

/// Asserts the command line of a release, which is built without `hook-conformance`: it takes
/// none of the six flags of the host, so each is refused, before the step as an unknown step and
/// after it as an unexpected argument, and the host is always the defaults. Parsing comes before
/// the declaration, the record and any program (a refused command line runs no step), so a
/// release cannot be pointed at another root, owner, program, port or poll. It would catch the
/// overrides leaking into a release.
#[test]
fn a_release_takes_no_flag_of_the_host() {
    assert!(host_flags(false).is_empty());
    assert_eq!(
        host_flags(true),
        [
            "--root",
            "--root-uid",
            "--nft",
            "--systemctl",
            "--public-port",
            "--poll-interval-ms"
        ]
    );
    let release = |words: &[&str]| {
        Invocation::parse_taking(words.iter().map(OsString::from), host_flags(false))
    };
    for &flag in host_flags(true) {
        assert_eq!(
            release(&[flag, "/x", "restore"]),
            Err(ArgsError::UnknownStep(flag.to_owned())),
            "{flag}"
        );
        assert_eq!(
            release(&["restore", flag, "/x"]),
            Err(ArgsError::Unexpected(OsString::from(flag))),
            "{flag}"
        );
        assert_eq!(
            release(&["verify", "--params", "/p.toml", flag, "/x"]),
            Err(ArgsError::Unexpected(OsString::from(flag))),
            "{flag}"
        );
    }
    assert_eq!(release(&["restore"]).unwrap().host, HostArgs::default());
}
