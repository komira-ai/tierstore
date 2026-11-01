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
