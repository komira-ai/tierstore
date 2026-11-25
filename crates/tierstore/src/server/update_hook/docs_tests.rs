//! The pages that describe the hook: the operator's page and the design of record.

use super::{
    CACHE_HANDOFF_CAPABILITY, CONFIG_FILE_KEY, FS_CONFIG_FILE, FS_LAUNCHD_LABEL_KEY,
    FS_LAUNCHD_PLIST, HANDOFF_CAPABILITY, HANDOFF_EXEC_CAPABILITY, HANDOFF_SERVICE_CAPABILITY,
    HANDOFF_SOCKET_KEY, HANDOFF_STATE_DIR_CAPABILITY, LAUNCHD_LABEL_KEY, LAUNCHD_PLIST, MARKER_DIR,
    MODE_KEY, PREVIOUS_RELEASE_KEY, PREVIOUS_VERSION_KEY, PROMOTION_CEILING_KEY, RESTART_MODE,
    Reason, SERVICE_MANAGER_KEY, STAGED_RELEASE_KEY, STATE_DIR, Step, TARGET_VERSION_KEY,
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
        FS_LAUNCHD_LABEL_KEY,
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
        FS_LAUNCHD_PLIST,
        FS_CONFIG_FILE,
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
    let mut words = vec![
        "handover-v1",
        HANDOFF_CAPABILITY,
        CACHE_HANDOFF_CAPABILITY,
        HANDOFF_STATE_DIR_CAPABILITY,
        crate::server::build_info::HANDOFF_FS_CAPABILITY,
    ];
    if cfg!(target_os = "linux") {
        words.push(HANDOFF_SERVICE_CAPABILITY);
    }
    if cfg!(target_vendor = "apple") {
        words.push(HANDOFF_EXEC_CAPABILITY);
    }
    assert_eq!(CAPABILITIES, words);
    for needle in [
        "`handoff-socket-v1`",
        "`handoff-cache-v1`",
        "`handoff-state-dir-v1`",
        "`handoff-fs-v1`",
        "`handoff-service-v1`",
        "`handoff-exec-v1`",
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
    assert!(PAGE.contains("**hand-over** when it has a previous release"));
    assert!(!PAGE.contains("A Mac never hands over"));
}

/// The operator's page of the file system process and the unit file of its template.
const OPERATIONS: &str = include_str!("../../../docs/operations.md");
const FS_UNIT: &str =
    include_str!("../../../../../packaging/hostkeeper/units/tierstore-fs@.service");

/// Asserts that the page, the operations page and the header of the fs unit file describe the
/// second unit as the hook acts on it: the page names the record keys, the marker, the rule that
/// the instance that holds the mount is never stopped and the capability both releases need, how
/// `verify` checks the binary of the fs process and the sentence it says of one left alone, and
/// none of the three says any longer that the hook does not act on the unit or that nothing writes
/// its marker. Catches a page left behind when the code learned the unit.
#[test]
fn the_pages_describe_the_second_unit_as_the_hook_acts_on_it() {
    for needle in [
        "`fs_live`",
        "`fs_next`",
        "run-fs-<instance>`",
        "never stopped",
        "`handoff-fs-v1`",
        "left on its release",
        "`tierstore-fs@<fs_next>`",
        "`/proc/<pid>/exe`",
        "still runs release 0.4.0+0123456789ab, and the update installed 0.5.0+0123456789ab: it \
         was left on its release",
        "/usr/local/libexec/tierstore/tierstore (deleted)",
        "`/proc/<pid>/exe --version`",
    ] {
        assert!(
            PAGE.contains(needle),
            "update-hook.md does not name {needle}"
        );
    }
    for stale in [
        "does not act on the unit yet",
        "does not act on this unit yet",
        "which nothing writes yet",
        "Nothing writes it yet",
        "until the update hook learns the unit",
        "chooses a restart, which the store carries across, when either release",
    ] {
        for (name, text) in [
            ("update-hook.md", PAGE),
            ("operations.md", OPERATIONS),
            ("tierstore-fs@.service", FS_UNIT),
        ] {
            assert!(!text.contains(stale), "{name} still says {stale:?}");
        }
    }
    assert!(FS_UNIT.contains("hook writes and removes it"));
}
