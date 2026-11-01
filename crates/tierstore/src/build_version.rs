//! Which version `build.rs` embeds as `TIERSTORE_BUILD_VERSION`. The build script includes this
//! file by path, and the library compiles it only for its unit tests, so the rule the build
//! applies is the rule the tests check.
//!
//! The version is the value of [`OVERRIDE`] when that variable is set, and otherwise the
//! crate's own version. The release workflow sets it to the version it is about to publish, so
//! the binary reports the tag it ships under; a build without it (a developer's, a test's)
//! keeps the crate's version.

/// The variable that replaces the crate's version.
pub const OVERRIDE: &str = "TIERSTORE_BUILD_VERSION_OVERRIDE";

/// The version to embed. `override_value` is [`OVERRIDE`] as the build environment holds it
/// (`None` when unset); `package` is the crate's own version.
///
/// # Errors
/// The override is set but is not exactly `<major>.<minor>.<patch>`: decimal numbers without a
/// leading zero, no `v`, no suffix, no spaces. An empty value is refused too: a script whose
/// variable expanded to nothing must fail, not quietly build under the crate's version.
pub fn choose(_override_value: Option<&std::ffi::OsStr>, _package: &str) -> Result<String, String> {
    Err("not written".to_owned())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{OVERRIDE, choose};

    fn from(value: &str) -> Result<String, String> {
        choose(Some(OsStr::new(value)), "0.3.0")
    }

    /// Catches: an override that is ignored (the crate's version embedded instead), and a
    /// version with several digits cut short.
    #[test]
    fn an_override_replaces_the_crate_version() {
        assert_eq!(from("0.3.2"), Ok("0.3.2".to_owned()));
        assert_eq!(from("1.20.300"), Ok("1.20.300".to_owned()));
        assert_eq!(from("0.0.0"), Ok("0.0.0".to_owned()));
    }

    /// Catches: an override of another shape accepted (empty, a `v`, a missing part, a fourth
    /// part, a suffix, build metadata, a leading zero, a space, not digits), so a stamp that is
    /// not the tag's version reaches `--version`.
    #[test]
    fn an_override_of_another_shape_is_refused() {
        for bad in [
            "",
            "v0.3.2",
            "0.3",
            "0.3.2.1",
            "0.3.2-rc1",
            "0.3.2+0123456789ab",
            "00.3.2",
            "0.03.2",
            "0.3.02",
            " 0.3.2",
            "0.3.2 ",
            "0.3.x",
            "0..2",
            "-1.3.2",
        ] {
            let err = from(bad).expect_err(bad);
            assert!(err.contains(OVERRIDE), "{bad}: {err}");
        }
        assert_eq!(OVERRIDE, "TIERSTORE_BUILD_VERSION_OVERRIDE");
    }

    /// Catches: a value that is not text being read as unset.
    #[cfg(unix)]
    #[test]
    fn an_override_that_is_not_text_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let err = choose(Some(OsStr::from_bytes(b"0.3.\xff")), "0.3.0").expect_err("bytes");
        assert!(err.contains(OVERRIDE), "{err}");
    }

    /// Catches: without an override, a version other than the crate's.
    #[test]
    fn without_an_override_the_crate_version_stands() {
        assert_eq!(choose(None, "0.3.0"), Ok("0.3.0".to_owned()));
        assert_eq!(choose(None, "1.2.3"), Ok("1.2.3".to_owned()));
    }
}
