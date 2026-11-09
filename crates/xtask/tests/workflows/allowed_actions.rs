//! The actions a workflow may use are the ones the repository's Actions policy admits. GitHub
//! refuses a whole workflow run (`startup_failure`, no job, no log) that names an action the
//! policy does not admit, and a pull request's checks never run the release workflow, so nothing
//! else would notice until a release did not happen. This lint says it before merge.

/// The patterns of the setting "Allow select actions and reusable workflows" of this repository
/// (Settings, Actions, General; `gh api repos/komira-ai/tierstore/actions/permissions/selected-actions`,
/// field `patterns_allowed`), word for word. A pattern is an `owner/repo@ref` where `*` stands for
/// any text; one with a tag in it does not admit a commit SHA, so an action on this list by tag is
/// not one this repository's pin rule can use.
pub const PATTERNS_ALLOWED: &[&str] = &[
    "tailscale/github-action@*",
    "sigstore/cosign-installer@*",
    "sebrollen/toml-action@v1.2.0",
    "dtolnay/rust-toolchain@v1",
    "swatinem/rust-cache@v2",
];

/// The owners whose actions the same setting admits without a pattern: GitHub's own (the setting
/// `github_owned_allowed`) and the owner of the repository (the policy "Allow OWNER, and select
/// non-OWNER, actions and reusable workflows"), whose reusable workflow set-desired.yml
/// hostkeeper-desired.yml and release.yml call.
pub const OWNERS_ALLOWED: &[&str] = &["actions/", "github/", "komira-ai/"];

/// Whether `text` matches `pattern`, where `*` stands for any text and the rest is literal.
fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let tail: Vec<&str> = parts.collect();
    let Some((last, middle)) = tail.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        let Some(at) = rest.find(part) else {
            return false;
        };
        rest = &rest[at + part.len()..];
    }
    rest.ends_with(last) && rest.len() >= last.len()
}

/// Whether the policy admits the `uses:` value `value`.
pub fn admitted(value: &str) -> bool {
    OWNERS_ALLOWED.iter().any(|o| value.starts_with(o))
        || PATTERNS_ALLOWED.iter().any(|p| glob(p, value))
}

/// One line per `uses:` value (with its line) the policy does not admit. A local path
/// (`./...`) is the pin rule's to name.
pub fn action_problems(uses: &[(usize, &str)]) -> Vec<String> {
    uses.iter()
        .filter(|(_, value)| !value.starts_with('.') && !admitted(value))
        .map(|(line, value)| {
            format!(
                "line {line}: `uses: {value}` is not admitted by the Actions policy of the repository (GitHub's own actions, the owner's, and {PATTERNS_ALLOWED:?}), so GitHub refuses the whole workflow run"
            )
        })
        .collect()
}
