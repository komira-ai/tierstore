//! The pages that describe the hook: the operator's page and the design of record.

use super::HANDOVER_CAPABILITY;
use crate::server::build_info::CAPABILITIES;

/// The operator's page of the hook.
const PAGE: &str = include_str!("../../../docs/update-hook.md");
/// The design of the hand-over.
const DESIGN: &str = include_str!("../../../../../docs/design/tierstore-handover.md");

/// Asserts that the operator's page describes the params key that forces a restart: its name and
/// one value, the `dir:` hot tier it is for, the reason the record keeps, and the refusal; and
/// that the design says the same in its steps and its storage-node section. It would catch a key
/// that packaging must set and no page names.
#[test]
fn the_pages_describe_the_mode_key() {
    for needle in [
        "`mode = \"restart\"`",
        "`\"restart\"` is the only value",
        "hot tier is a `dir:` tier",
        "mode restart (declared_restart)",
        "must be the string \"restart\", the one mode it can force",
        "`--root <dir>`",
        "`--nft <program>`",
        "`--systemctl <program>`",
        "`--root-uid <uid>`",
        "`--public-port <port>`",
        "`--poll-interval-ms <ms>`",
        "`hook-conformance`",
    ] {
        assert!(PAGE.contains(needle), "update-hook.md lacks {needle}");
    }
    for needle in [
        "`mode = \"restart\"` records `mode=restart` whatever the capabilities say",
        "packaging sets `mode = \"restart\"` in `[hook.params]`",
        "`declared_restart`",
    ] {
        assert!(DESIGN.contains(needle), "the design lacks {needle}");
    }
}

/// Asserts that neither page promises that a pending promotion names the holder of the cache
/// lock: no instance reports it, and `verify` does not say it. It would catch the promise coming
/// back, which an operator would wait on in vain.
#[test]
fn no_page_promises_the_holder_of_the_lock() {
    for (name, page) in [("update-hook.md", PAGE), ("the design", DESIGN)] {
        for promise in [
            "holder of the cache lock is not yet named",
            "the holder of the lock in `detail`",
        ] {
            assert!(!page.contains(promise), "{name} still says {promise:?}");
        }
    }
    assert!(PAGE.contains("it does not name who holds the cache lock"));
}

/// Asserts that the build lists the capability the hook's `prepare` step looks for, and that
/// neither page says it does not: the operator's page says `--capabilities` lists `handover-v1`,
/// and the design no longer says the shipped `CAPABILITIES` are unchanged. It would catch the
/// page telling an operator that no build hands over while every build does, and a build whose
/// name for the capability is not the one the hook reads.
#[test]
fn the_pages_say_the_build_lists_handover_v1() {
    assert_eq!(CAPABILITIES, [HANDOVER_CAPABILITY]);
    for (name, page, stale) in [
        ("update-hook.md", PAGE, "does not yet list `handover-v1`"),
        ("the design", DESIGN, "so `CAPABILITIES` is unchanged"),
    ] {
        assert!(!page.contains(stale), "{name} still says {stale:?}");
    }
    assert!(PAGE.contains("`tierstore --capabilities` lists `handover-v1`, since the binary has"));
}
