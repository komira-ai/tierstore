//! The flags of the releases before the entries that this release accepts and ignores.
//!
//! The per-node journal, its linger and retention, the reconciler's interval and the removal of
//! registrations by the journal reconciler are gone. A unit file that still passes one of their
//! flags starts all the same: the flag and its value are taken out of the arguments before they
//! are parsed, and one line on stderr says that the flag is ignored. The flags are not in
//! `--help` and have no key in the config file (a file that has one of the keys gets the warning
//! of the loader for a key it does not know).

use std::ffi::OsString;

/// Each removed flag, without `--`, and what replaced it.
const REMOVED: [(&str, &str); 4] = [
    (
        "journal-linger",
        "writes are made on the entry of their key, and there is no journal to batch",
    ),
    (
        "journal-retention",
        "there is no per-node journal to keep; the old objects are removed by the release after this one",
    ),
    (
        "reconcile-interval",
        "the reconciler waits as long as its own idle wait says, and not at all while it makes progress",
    ),
    (
        "registration-retention",
        "the registration of a proxy is no longer removed by a journal check",
    ),
];

/// The removed flag that `arg` is, as `--flag` or `--flag=value`, and whether it has its value in
/// itself (the `=` form).
fn removed_flag(arg: &OsString) -> Option<(&'static str, bool)> {
    let given = arg.to_str()?.strip_prefix("--")?;
    REMOVED.iter().find_map(|(flag, _)| {
        if given == *flag {
            return Some((*flag, false));
        }
        let more = given.strip_prefix(flag)?;
        more.starts_with('=').then_some((*flag, true))
    })
}

/// The arguments without the removed flags and their values, and one warning for each removed
/// flag that was given (however often, and in either form, `--flag value` or `--flag=value`).
/// The first argument is the program name and is kept.
pub(super) fn drop_removed(args: Vec<OsString>) -> (Vec<OsString>, Vec<String>) {
    let mut kept = Vec::with_capacity(args.len());
    let mut warned: Vec<&str> = Vec::new();
    let mut rest = args.into_iter();
    kept.extend(rest.next());
    while let Some(arg) = rest.next() {
        match removed_flag(&arg) {
            None => kept.push(arg),
            Some((flag, has_value)) => {
                if !has_value {
                    // The value is the next argument.
                    let _value = rest.next();
                }
                if !warned.contains(&flag) {
                    warned.push(flag);
                }
            }
        }
    }
    let warnings = REMOVED
        .iter()
        .filter(|(flag, _)| warned.contains(flag))
        .map(|(flag, why)| format!("tierstore: --{flag} is ignored: {why}"))
        .collect();
    (kept, warnings)
}

#[cfg(test)]
#[path = "removed_flags_tests.rs"]
mod tests;
