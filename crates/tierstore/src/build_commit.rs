//! Which commit `build.rs` embeds as `TIERSTORE_BUILD_COMMIT`. The build script includes this
//! file by path, and the library compiles it only for its unit tests, so the rule the build
//! applies is the rule the tests check.
//!
//! The commit is what `git rev-parse --short=12 HEAD` gives in the checkout being built, or
//! `unknown` where git gives nothing (a source archive, no `git`). The build reads no
//! environment variable for it.

/// The commit to embed. `git_head` is what `git rev-parse --short=12 HEAD` gave (`None` when it
/// failed); an empty answer counts as none.
pub fn choose(git_head: Option<String>) -> String {
    git_head
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::choose;

    /// Catches: a commit other than git's embedded.
    #[test]
    fn git_decides_the_commit() {
        assert_eq!(choose(Some("fedcba987654".to_owned())), "fedcba987654");
    }

    /// Catches: no fallback when git gives nothing (it fails, or answers with nothing).
    #[test]
    fn without_a_commit_from_git_the_commit_is_unknown() {
        assert_eq!(choose(None), "unknown");
        assert_eq!(choose(Some(String::new())), "unknown");
    }
}
