//! The declaration.

use super::testing::DECLARATION;
use super::{Declaration, DeclarationError, unit_name};

/// Asserts that the declaration is read and the unit of a port is named from the port. It would
/// catch a unit name built from anything but a port.
#[test]
fn the_ports_and_the_state_directory_are_read() {
    let decl = Declaration::parse(DECLARATION).unwrap();
    assert_eq!(decl.ports, [9301, 9302]);
    assert_eq!(
        decl.state_dir.to_str(),
        Some("/var/lib/updater/hooks/tierstore")
    );
    assert_eq!(unit_name(9302), "tierstore@9302.service");
    assert!(decl.has(9301) && decl.has(9302) && !decl.has(9300));
    assert_eq!(decl.other(9301), 9302);
    assert_eq!(decl.other(9302), 9301);
}

/// Asserts each refusal of a declaration. It would catch a hook that trusts ports or units that
/// are not the two slots, which it would then start and stop with the rights of the declaration.
#[test]
fn a_declaration_that_is_not_the_two_slots_is_refused() {
    let with = |from: &str, to: &str| Declaration::parse(&DECLARATION.replace(from, to));
    assert!(matches!(
        Declaration::parse("not = [toml"),
        Err(DeclarationError::Parse(_))
    ));
    assert!(matches!(
        Declaration::parse("a = 1"),
        Err(DeclarationError::Parse(_))
    ));
    for ports in [
        "[9301]",
        "[9301, 9302, 9303]",
        "[9301, 9301]",
        "[0, 9302]",
        "[9300, 9302]",
    ] {
        let err = with("[9301, 9302]", ports).unwrap_err();
        assert!(matches!(err, DeclarationError::Ports), "{ports}");
    }
    let err = with("tierstore@9302.service", "tierstore@9303.service").unwrap_err();
    assert!(matches!(err, DeclarationError::Units));
    let err = with(", \"tierstore@9302.service\"", "").unwrap_err();
    assert!(matches!(err, DeclarationError::Units));
    let err = with("/var/lib/updater/hooks/tierstore", "var/lib/../x").unwrap_err();
    assert!(matches!(err, DeclarationError::NotPlain));
    assert!(err.to_string().contains("plain components"));
}

/// Asserts that the units may be listed in either order. It would catch a comparison that depends
/// on the order of a list the updater may rewrite.
#[test]
fn the_units_may_be_listed_in_either_order() {
    let swapped = DECLARATION.replace(
        "[\"tierstore@9301.service\", \"tierstore@9302.service\"]",
        "[\"tierstore@9302.service\", \"tierstore@9301.service\"]",
    );
    assert!(Declaration::parse(&swapped).is_ok());
}

/// Asserts that the service account is read from the top-level `uid`, and is none when the
/// declaration has no such key.
#[test]
fn the_service_account_is_read_from_uid() {
    let named = format!("uid = \"tierstore\"\n{DECLARATION}");
    assert_eq!(
        Declaration::parse(&named).unwrap().service.as_deref(),
        Some("tierstore")
    );
    assert_eq!(Declaration::parse(DECLARATION).unwrap().service, None);
}

/// Asserts that a `uid` that is not an account name (empty, leading hyphen, a space, a slash, a
/// colon, non-ASCII, 33 characters) is refused: it is matched against the lines of `/etc/group`.
#[test]
fn a_uid_that_is_not_an_account_name_is_refused() {
    for uid in ["", "-x", "a b", "a/b", "a:b", "tierstoré", &"a".repeat(33)] {
        let text = format!("uid = \"{uid}\"\n{DECLARATION}");
        let err = Declaration::parse(&text).unwrap_err();
        assert!(matches!(err, DeclarationError::Service), "{uid:?}");
        assert!(err.to_string().contains("account name"));
    }
    let longest = format!("uid = \"{}\"\n{DECLARATION}", "a".repeat(32));
    assert!(Declaration::parse(&longest).is_ok());
}

/// Asserts that a record directory that is a reserved path, or under a reserved tree (the root,
/// system directories, the service's cache and state), is refused, and
/// that its neighbours are not (the match is by whole components). It would catch a declaration
/// that makes the hook use `/etc` or the service's own state as its record directory.
#[test]
fn a_reserved_state_directory_is_refused() {
    let with = |dir: &str| {
        Declaration::parse(&DECLARATION.replace("/var/lib/updater/hooks/tierstore", dir))
    };
    for dir in [
        "/",
        "/var",
        "/var/lib",
        "/etc",
        "/etc/updater.d",
        "/usr",
        "/usr/lib/x",
        "/proc",
        "/sys/x",
        "/dev/shm",
        "/run/x",
        "/boot",
        "/var/cache/tierstore",
        "/var/lib/tierstore",
        "/var/lib/tierstore/state",
        "/bin",
        "/sbin",
        "/lib64/x",
    ] {
        let err = with(dir).unwrap_err();
        assert!(matches!(err, DeclarationError::Reserved(_)), "{dir}");
        assert!(err.to_string().contains(dir), "{dir}");
    }
    for dir in [
        "/var/lib/updater/hooks/tierstore",
        "/var/lib/tierstore-update",
        "/var/lib/tierstorex",
        "/srv/hook",
        "/etcetera",
        "/var/libx/y",
    ] {
        assert!(with(dir).is_ok(), "{dir}");
    }
}

/// Asserts that a record directory with whitespace anywhere in it, or with a component that
/// starts with `-`, `+` or `!` (which systemd reads as a prefix of a path), is refused as not
/// plain, and that a hyphen, plus or bang inside a component is not.
#[test]
fn a_state_directory_with_whitespace_or_a_prefix_character_is_refused() {
    let with = |dir: &str| {
        Declaration::parse(&DECLARATION.replace("/var/lib/updater/hooks/tierstore", dir))
    };
    for dir in [
        "/srv/a b",
        "/srv/ab ",
        "/ srv/ab",
        "/srv/a\\tb",
        "/srv/a\\nb",
        "/srv/-hook",
        "/srv/+hook",
        "/srv/!hook",
        "/-srv/hook",
        "/srv/ok/-x/y",
        "/srv/ok/+x",
        "/srv/ok/!x/y",
        "/srv/./hook",
        "/srv/hook/",
        "/srv//hook",
    ] {
        let err = with(dir).unwrap_err();
        assert!(matches!(err, DeclarationError::NotPlain), "{dir}");
    }
    for dir in ["/srv/a-b", "/srv/a+b", "/srv/a!b", "/srv/a.b"] {
        assert!(with(dir).is_ok(), "{dir}");
    }
}

/// Asserts that the user's and the system's data directories and logs (`/root`, `/home`, `/tmp`,
/// `/var/tmp`, `/var/log`, `/opt`), the updater's own directories, and a directory that holds a
/// reserved one are refused, and that their neighbours are not.
#[test]
fn the_home_temporary_log_and_updater_directories_are_reserved() {
    let with = |dir: &str| {
        Declaration::parse(&DECLARATION.replace("/var/lib/updater/hooks/tierstore", dir))
    };
    for dir in [
        "/root",
        "/root/x",
        "/home",
        "/home/u/x",
        "/tmp",
        "/tmp/x",
        "/var/tmp",
        "/var/tmp/x",
        "/var/log",
        "/var/log/x",
        "/opt",
        "/opt/x",
        "/var/lib/updater",
        "/var/lib/updater/hooks",
    ] {
        let err = with(dir).unwrap_err();
        assert!(matches!(err, DeclarationError::Reserved(_)), "{dir}");
    }
    for dir in [
        "/var/lib/updater/hooks/tierstore",
        "/rootx",
        "/homes/x",
        "/tmpx",
        "/optional",
        "/var/tmpx",
        "/var/logs/x",
    ] {
        assert!(with(dir).is_ok(), "{dir}");
    }
}
