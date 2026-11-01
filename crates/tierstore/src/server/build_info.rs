//! What this build says of itself: its version, the commit it was built from, and the
//! capabilities a host updater may ask for with `tierstore --capabilities`.

/// The crate version, which `--version`, the start line and `tierstore_build_info` name.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this binary was built from: twelve hex digits, or `unknown` where the build had no
/// git checkout. `build.rs` embeds it.
pub const COMMIT: &str = env!("TIERSTORE_BUILD_COMMIT");

/// What `--version` prints after the program's name: the crate version, `+`, and the commit, the
/// form `cargo xtask build-commit check` reads (`unknown` after the `+` for a build that has no
/// commit).
pub const RELEASE_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "+",
    env!("TIERSTORE_BUILD_COMMIT")
);

/// The capabilities `tierstore --capabilities` prints, one to a line, in this order. A name is
/// lowercase letters, digits and `-`.
///
/// **This list is empty on purpose, and it must not name `handover-v1` yet.** `handover-v1` says
/// the release has the lock-based read-only mode, the lingering close in the drain and the
/// wildcard listeners; the host updater's `prepare` step reads it to choose a handover over a
/// plain restart, so a release that names it without all three would be handed over to and fail.
/// Those three arrive in other changes, and the one that completes them adds `handover-v1` here,
/// in this one place and nowhere else.
pub const CAPABILITIES: &[&str] = &[];

/// The text `--capabilities` prints for `names`: each name and a newline, nothing for none.
#[must_use]
pub fn capabilities_text(names: &[&str]) -> String {
    names.iter().map(|name| format!("{name}\n")).collect()
}

/// The `tierstore_build_info` block of `/metricz`: the type line and one sample of value 1 that
/// carries the version and the commit as labels, and after them the `slot` (the port `--slot`
/// names) when there is one. Without a slot the text has no `slot` label at all, so that a node
/// that is not a slot renders what it always did. None of them can hold a quote or a backslash:
/// one is a Cargo version, the other hex digits or `unknown`, the third a number. The hook's
/// probe reads the slot back with `update_hook::build_of`.
#[must_use]
pub fn gauge(version: &str, commit: &str, slot: Option<u16>) -> String {
    let slot = slot.map_or_else(String::new, |port| format!(",slot=\"{port}\""));
    format!(
        "# TYPE tierstore_build_info gauge\n\
         tierstore_build_info{{version=\"{version}\",commit=\"{commit}\"{slot}}} 1\n"
    )
}

#[cfg(test)]
#[path = "build_info_tests.rs"]
mod tests;
