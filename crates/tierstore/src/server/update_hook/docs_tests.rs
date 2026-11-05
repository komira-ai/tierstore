//! The pages that describe the hook: the operator's page and the design of record.

use super::{
    CACHE_HANDOFF_CAPABILITY, CONFIG_FILE_KEY, HANDOFF_CAPABILITY, HANDOFF_SOCKET_KEY,
    LAUNCHD_LABEL_KEY, LAUNCHD_PLIST, MARKER_DIR, MODE_KEY, PREVIOUS_RELEASE_KEY,
    PREVIOUS_VERSION_KEY, PROMOTION_CEILING_KEY, RESTART_MODE, Reason, SERVICE_MANAGER_KEY,
    STAGED_RELEASE_KEY, STATE_DIR, Step, TARGET_VERSION_KEY,
};
use crate::server::build_info::CAPABILITIES;

/// The operator's page of the hook.
const PAGE: &str = include_str!("../../../docs/update-hook.md");
/// The design of the hand-over.
const DESIGN: &str = include_str!("../../../../../docs/design/tierstore-handover.md");

/// Asserts that the operator's page has a row in its table of reasons for every reason the hook
/// can give, with the class the code blames. It would catch a reason added to the code and left
/// out of the page, and a class an operator reads wrongly.
#[test]
fn the_page_lists_every_reason_with_its_class() {
    for reason in Reason::ALL {
        let row = format!("| `{}` | {} |", reason.as_str(), reason.class().as_str());
        assert!(PAGE.contains(&row), "update-hook.md lacks the row {row}");
    }
}

/// Asserts that the page names every key of the params file the hook reads, and the one value
/// of `mode`. It would catch a key that a manifest must set and no page names.
#[test]
fn the_page_names_every_params_key() {
    for key in [
        STAGED_RELEASE_KEY,
        TARGET_VERSION_KEY,
        PREVIOUS_RELEASE_KEY,
        PREVIOUS_VERSION_KEY,
        PROMOTION_CEILING_KEY,
        SERVICE_MANAGER_KEY,
        LAUNCHD_LABEL_KEY,
        CONFIG_FILE_KEY,
        MODE_KEY,
    ] {
        let plain = format!("`{key}`");
        let quoted = format!("`\"{key}\"`");
        assert!(
            PAGE.contains(&plain) || PAGE.contains(&quoted),
            "update-hook.md lacks the key {key}"
        );
    }
    assert!(PAGE.contains(&format!("`{MODE_KEY} = \"{RESTART_MODE}\"`")));
}

/// Asserts that the page has a section for every step, under the name hostkeeper puts on the
/// command line. It would catch a step added without a description.
#[test]
fn the_page_describes_every_step() {
    for step in Step::ALL {
        let heading = format!("### {}\n", step.name());
        assert!(PAGE.contains(&heading), "update-hook.md lacks {heading:?}");
    }
}

/// Asserts that the page names the directories, the plist and the capability the hook works
/// with, and the configuration key of the hand-over socket. It would catch a path or a name that
/// changed in the code and not in the page.
#[test]
fn the_page_names_the_paths_and_the_capability() {
    for needle in [
        STATE_DIR,
        MARKER_DIR,
        LAUNCHD_PLIST,
        HANDOFF_CAPABILITY,
        HANDOFF_SOCKET_KEY,
    ] {
        assert!(PAGE.contains(needle), "update-hook.md lacks {needle}");
    }
}

/// Asserts that the page no longer describes what the hook no longer does: no port redirect, no
/// slot units, no declaration file and no hook-only account. It would catch a section of the old
/// two-port design left in the page.
#[test]
fn the_page_does_not_describe_the_removed_design() {
    for gone in [
        "nft",
        "slot",
        "polkit",
        "updater.d",
        "9301",
        "9302",
        "tierstore-update",
    ] {
        assert!(!PAGE.contains(gone), "update-hook.md still says {gone:?}");
    }
}

/// Asserts that the design says its sections on the port redirect, the slots and the old test
/// plan are superseded by the socket hand-off. It would catch a reader sent to the redirect as
/// the design of record.
#[test]
fn the_design_marks_the_redirect_sections_superseded() {
    assert!(DESIGN.contains("## 1. Shape (superseded)"));
    assert!(DESIGN.contains("## 10. Test plan (superseded)"));
    assert!(DESIGN.contains("`crates/tierstore/docs/update-hook.md`"));
}

/// Asserts that the build lists `handover-v1`, the capability the hook's `prepare` step looks for
/// and the one it looks for besides when the configuration has `[local_cache]`, and that the page
/// names both with the two configuration keys of the hand-off, no longer says the binary lacks it
/// and no longer says a build node cannot hand off. It would catch a build whose name for a
/// capability is not the one the hook reads, and a page telling an operator that every update of
/// a cache shape is a restart.
#[test]
fn the_page_says_the_build_lists_the_hand_off() {
    assert_eq!(
        CAPABILITIES,
        ["handover-v1", HANDOFF_CAPABILITY, CACHE_HANDOFF_CAPABILITY]
    );
    for needle in [
        "`handoff-socket-v1`",
        "`handoff-cache-v1`",
        "`[local_cache]`",
        "`server.handoff_socket`",
        "`server.handoff_wait`",
    ] {
        assert!(
            PAGE.contains(needle),
            "update-hook.md does not name {needle}"
        );
    }
    for stale in [
        "does not yet implement the hand-over socket",
        "does not use it yet",
        "refuses `handoff_socket`",
    ] {
        assert!(!PAGE.contains(stale), "update-hook.md still says {stale:?}");
    }
}

/// Asserts (T23, T24) that the page states the order of releases: the first release that accepts
/// the key goes in by `mode = "restart"`, the key is added only after it is live, and the next
/// release is the first hand-over; and that it names `config_refused` and the check it comes
/// from. Catches a page that tells operators to add the key before the live release accepts it.
#[test]
fn the_page_states_the_order_of_releases_and_the_configuration_check() {
    let start = PAGE
        .find("### The order of releases")
        .expect("update-hook.md has the order of releases");
    let order = &PAGE[start..];
    let order = &order[..order.find("\n## ").unwrap_or(order.len())];
    for needle in [
        "`mode = \"restart\"`",
        "`server.handoff_socket` added",
        "`config_refused`",
        "first hand-over",
        CACHE_HANDOFF_CAPABILITY,
    ] {
        assert!(order.contains(needle), "the order lacks {needle}");
    }
    assert!(PAGE.contains("--check-config"));
    assert!(PAGE.contains("A Mac never hands over"));
}
