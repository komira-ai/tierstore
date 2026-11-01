//! The pages that describe the hook: the operator's page and the design of record.

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
        "`declared_restart`",
        "must be the string \"restart\", the one mode it can force",
        "`--root <dir>`",
        "`--nft <program>`",
        "`--systemctl <program>`",
        "`--root-uid <uid>`",
    ] {
        assert!(PAGE.contains(needle), "update-hook.md lacks {needle}");
    }
    for needle in [
        "`mode = \"restart\"` records `mode=restart` whatever the capabilities say",
        "packaging sets `mode = \"restart\"` in `[hook.params]`",
        "`mode_reason`",
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
