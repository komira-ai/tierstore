//! What this build says of itself: its version, the commit it was built from, and the
//! capabilities a host updater may ask for with `tierstore --capabilities`.

pub(crate) mod stderr_log;

#[cfg(test)]
mod stderr_log_tests;

/// The version of this build, which `--version`, the start line and `tierstore_build_info` name:
/// the version of `Cargo.toml`. The release workflow writes the version of the tag there before it
/// builds (`cargo xtask release set-version`). `build.rs` embeds it.
pub const VERSION: &str = env!("TIERSTORE_BUILD_VERSION");

/// The commit this binary was built from: twelve hex digits, or `unknown` where the build had no
/// git checkout. `build.rs` embeds it.
pub const COMMIT: &str = env!("TIERSTORE_BUILD_COMMIT");

/// What `--version` prints after the program's name: the build's version, `+`, and the commit, the
/// form `cargo xtask build-commit` and `release check-binary` read (`unknown` after the `+` for a
/// build that has no commit).
pub const RELEASE_VERSION: &str = concat!(
    env!("TIERSTORE_BUILD_VERSION"),
    "+",
    env!("TIERSTORE_BUILD_COMMIT")
);

/// The capabilities `tierstore --capabilities` prints, one to a line, in this order. A name is
/// lowercase letters, digits and `-`.
///
/// `handover-v1` says the release has the lock-based read-only mode, the lingering close in the
/// drain of both fronts and the wildcard listeners of both fronts, and this build has all three.
/// The test `handover_v1_is_listed_with_the_three_behaviours_it_promises` (tests/oci_front) runs
/// the binary against each of them, so a release that named it without all three would fail it.
/// The update hook no longer reads it.
///
/// [`HANDOFF_SOCKET_CAPABILITY`] says the binary serves through the hand-off of its listening
/// sockets when `server.handoff_socket` is set (see `server::handoff`). The update hook's
/// `prepare` step reads the list of the staged and of the previous release for it
/// (`update_hook::HANDOFF_CAPABILITY`, the same constant) and records a hand-over only when both
/// list it.
///
/// [`HANDOFF_CACHE_CAPABILITY`] says the same of a build node and of a storage node with
/// `[local_cache]`. `prepare` needs both releases to list it too when the node's configuration has
/// `[local_cache]`: a release with only `handoff-socket-v1` refuses the key in that shape, so its
/// instance has no control socket to take from. This list is all `--capabilities` prints.
pub const CAPABILITIES: &[&str] = &[
    "handover-v1",
    HANDOFF_SOCKET_CAPABILITY,
    HANDOFF_CACHE_CAPABILITY,
];

/// `handoff-socket-v1`: a storage node started with `server.handoff_socket` takes the listening
/// sockets of the instance that answers on that control socket (or binds its own where none
/// answers), offers them there in its turn, and drains when a successor takes them; the wait for
/// the running instance is `server.handoff_wait`.
pub const HANDOFF_SOCKET_CAPABILITY: &str = "handoff-socket-v1";

/// `handoff-cache-v1`: a build node and a storage node with `[local_cache]` read
/// `server.handoff_socket` and `server.handoff_wait` and hand their listening sockets over as a
/// storage node does (see [`HANDOFF_SOCKET_CAPABILITY`]); the cache directory lock keeps the new
/// instance read-only until the old one has exited.
pub const HANDOFF_CACHE_CAPABILITY: &str = "handoff-cache-v1";

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
/// probe reads the version and the commit back with `update_hook::build_of`.
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
