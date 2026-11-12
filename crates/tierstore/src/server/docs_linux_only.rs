//! The TOML files of the pages that load on Linux alone.
//!
//! A file that turns on what this release serves on Linux alone (the put directory, `[fs.put]`
//! with `enabled = true`, which the FUSE frontend serves and the NFS frontend does not yet) is
//! fenced as ```` ```toml linux ````. On Linux it must load like any ```` ```toml ```` file; off
//! Linux the loader must refuse it, and with exactly [`OFF_LINUX_REFUSAL`]: a file it takes there
//! means the mark is stale, and any other refusal is a fault of the file that Linux would show too.

/// The info string of a whole file that loads on Linux alone.
pub(super) const LINUX_ONLY: &str = "toml linux";

/// What the loader says off Linux of a file that turns the put directory on.
pub(super) const OFF_LINUX_REFUSAL: &str = "fs.put.enabled: the put directory is served by the \
                                            FUSE frontend (Linux) alone in this release";

/// The problem with what the loader made of a whole file, or `None`: `loaded` is the loader's
/// answer, `linux_only` whether the block is marked [`LINUX_ONLY`], and `on_linux` whether this
/// is Linux (`cfg!(target_os = "linux")` in the checker; both values in the tests below).
pub(super) fn judged(
    loaded: Result<(), String>,
    linux_only: bool,
    on_linux: bool,
) -> Option<String> {
    match (loaded, linux_only && !on_linux) {
        (Ok(()), false) => None,
        (Err(why), false) => Some(format!("the loader refuses the file: {why}")),
        (Err(why), true) if why == OFF_LINUX_REFUSAL => None,
        (Err(why), true) => Some(format!(
            "the loader refuses the file marked `{LINUX_ONLY}` off Linux, and not for the put \
             directory: {why}"
        )),
        (Ok(()), true) => Some(format!(
            "the loader takes the file marked `{LINUX_ONLY}` off Linux, where it must refuse it \
             with: {OFF_LINUX_REFUSAL}"
        )),
    }
}

#[cfg(test)]
#[path = "docs_linux_only_tests.rs"]
mod tests;
