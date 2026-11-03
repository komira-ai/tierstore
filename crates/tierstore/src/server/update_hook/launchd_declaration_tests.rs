//! The declaration of a macOS host: what it accepts and every rule it refuses by.

use std::path::PathBuf;

use super::machine::LAUNCHD_LABEL;
use super::{Declaration, DeclarationError, LaunchdDeclaration};

/// The declaration packaging ships for a Mac.
const SHIPPED: &str = include_str!("../../../../../packaging/macos/updater.d/tierstore.toml");

/// A declaration with `state_dir` and `rest` (the account line, the ports and the hook table).
fn text(state_dir: &str, rest: &str) -> String {
    format!("state_dir = \"{state_dir}\"\n{rest}")
}

const GOOD_REST: &str =
    "uid = \"_tierstore\"\nhealth_ports = [9300]\n[hook]\nservice = \"com.komira.tierstore\"\n";

fn parse_with_state(state_dir: &str) -> Result<LaunchdDeclaration, DeclarationError> {
    LaunchdDeclaration::parse(&text(state_dir, GOOD_REST))
}

/// Asserts that the declaration packaging ships for a Mac parses: the record directory under
/// `/var/db/updater/hooks`, the one port 9300, and the account `_tierstore`; and that the keys of
/// the updater beside them (the hook's account, its paths, its timeouts, the params) do not
/// stand in the way. It would catch packaging and the hook disagreeing on a key.
#[test]
fn the_shipped_macos_declaration_is_accepted() {
    assert_eq!(
        LaunchdDeclaration::parse(SHIPPED).unwrap(),
        LaunchdDeclaration {
            state_dir: PathBuf::from("/var/db/updater/hooks/tierstore"),
            port: 9300,
            service: "_tierstore".to_owned(),
        }
    );
}

/// Asserts that a record directory that is relative, holds `..` or `.`, a doubled or trailing
/// slash, whitespace, or a component a service manager reads as a prefix is refused as not
/// plain. It would catch a record directory that resolves somewhere other than it reads.
#[test]
fn a_state_dir_that_is_not_plain_is_refused() {
    for state_dir in [
        "var/db/updater/hooks/tierstore",
        "/var/db/updater/hooks/../tierstore",
        "/var/db/updater/hooks/./tierstore",
        "/var/db/updater/hooks//tierstore",
        "/var/db/updater/hooks/tierstore/",
        "/var/db/updater/hooks/tier store",
        "/var/db/updater/hooks/-tierstore",
    ] {
        let refused = parse_with_state(state_dir).unwrap_err();
        assert!(
            matches!(refused, DeclarationError::NotPlain),
            "{state_dir}: {refused}"
        );
    }
}

/// Asserts that every macOS system tree, and anything under it, is refused as a record
/// directory (the system's, the users', the links into `/private` and `/private` itself, the
/// programs and settings under `/usr/local`, the other services' and tierstore's own tree), and
/// so are the directories every other path sits under, whole; and that the record directory
/// the updater keeps for the hooks is accepted. It would catch a record directory the service,
/// another account or the system could write to.
#[test]
fn the_macos_system_trees_are_refused() {
    for state_dir in [
        "/",
        "/usr",
        "/usr/local",
        "/var",
        "/var/db",
        "/var/db/updater",
        "/var/db/updater/hooks",
        "/Applications/x",
        "/Library/LaunchDaemons",
        "/System/Library/x",
        "/Users/runner/state",
        "/Volumes/data/state",
        "/bin/x",
        "/cores/x",
        "/dev/x",
        "/etc/updater",
        "/opt/x",
        "/private",
        "/private/var/db/updater/hooks/tierstore",
        "/sbin/x",
        "/tmp/state",
        "/usr/bin/x",
        "/usr/lib/x",
        "/usr/libexec/x",
        "/usr/sbin/x",
        "/usr/share/x",
        "/usr/local/bin/x",
        "/usr/local/etc/updater.d",
        "/usr/local/kbf/state",
        "/usr/local/lib/x",
        "/usr/local/libexec/kbf",
        "/usr/local/tierstore",
        "/usr/local/tierstore/state",
        "/var/folders/x",
        "/var/log/x",
        "/var/root/x",
        "/var/run/x",
        "/var/tmp/x",
        "/var/vm/x",
    ] {
        let refused = parse_with_state(state_dir).unwrap_err();
        assert!(
            matches!(&refused, DeclarationError::Reserved(path) if path.to_str() == Some(state_dir)),
            "{state_dir}: {refused}"
        );
    }
    for state_dir in [
        "/var/db/updater/hooks/tierstore",
        "/var/db/tierstore-update",
        "/usr/local/var/tierstore-update",
    ] {
        assert!(parse_with_state(state_dir).is_ok(), "{state_dir}");
    }
}

/// A declaration's text and the refusal it must meet.
type Case = (String, fn(&DeclarationError) -> bool);

/// Asserts that the declaration is refused without an account (`uid` is required on a Mac, the
/// account the plist runs as), with an account that is not one, with no port, two ports or port
/// 0, with slot units, with a label other than the plist's, and without the hook table's
/// service; each with the error that names the rule. It would catch a declaration of the
/// two-slot hook taken on a Mac, or one that names a job the hook does not manage.
#[test]
fn a_macos_declaration_that_breaks_a_rule_is_refused() {
    let state = "state_dir = \"/var/db/updater/hooks/tierstore\"\n";
    let hook = "[hook]\nservice = \"com.komira.tierstore\"\n";
    let cases: [Case; 9] = [
        (format!("{state}health_ports = [9300]\n{hook}"), |e| {
            matches!(e, DeclarationError::NoService)
        }),
        (
            format!("{state}uid = \"-x\"\nhealth_ports = [9300]\n{hook}"),
            |e| matches!(e, DeclarationError::Service),
        ),
        (
            format!("{state}uid = \"_tierstore\"\nhealth_ports = []\n{hook}"),
            |e| matches!(e, DeclarationError::OnePort),
        ),
        (
            format!("{state}uid = \"_tierstore\"\nhealth_ports = [9301, 9302]\n{hook}"),
            |e| matches!(e, DeclarationError::OnePort),
        ),
        (
            format!("{state}uid = \"_tierstore\"\nhealth_ports = [0]\n{hook}"),
            |e| matches!(e, DeclarationError::OnePort),
        ),
        (
            format!(
                "{state}uid = \"_tierstore\"\nhealth_ports = [9300]\n{hook}units = [\"tierstore@9300.service\"]\n"
            ),
            |e| matches!(e, DeclarationError::SlotUnits),
        ),
        (
            format!(
                "{state}uid = \"_tierstore\"\nhealth_ports = [9300]\n[hook]\nservice = \"com.komira.kbf-daemon\"\n"
            ),
            |e| matches!(e, DeclarationError::Label),
        ),
        (
            format!("{state}uid = \"_tierstore\"\nhealth_ports = [9300]\n[hook]\n"),
            |e| matches!(e, DeclarationError::Parse(_)),
        ),
        (
            "uid = \"_tierstore\"\nhealth_ports = [9300]\n[hook]\nservice = 1\n".to_owned(),
            |e| matches!(e, DeclarationError::Parse(_)),
        ),
    ];
    for (declaration, expected) in cases {
        let refused = LaunchdDeclaration::parse(&declaration).unwrap_err();
        assert!(expected(&refused), "{declaration}: {refused}");
    }
    assert_eq!(
        DeclarationError::Label.to_string(),
        format!("hook.service must be the launchd label {LAUNCHD_LABEL}")
    );
}

/// Asserts that neither parser takes the other platform's declaration: the two-slot parser
/// refuses the Mac's (no slot units), and the Mac's refuses the two-slot one (no service label). It
/// would catch a hook that runs the wrong machine's steps on a host.
#[test]
fn neither_parser_takes_the_other_platforms_declaration() {
    assert!(matches!(
        Declaration::parse(SHIPPED).unwrap_err(),
        DeclarationError::Parse(_)
    ));
    let linux = include_str!("../../../../../packaging/updater.d/tierstore.toml");
    assert!(Declaration::parse(linux).is_ok());
    assert!(matches!(
        LaunchdDeclaration::parse(linux).unwrap_err(),
        DeclarationError::Parse(_)
    ));
}
