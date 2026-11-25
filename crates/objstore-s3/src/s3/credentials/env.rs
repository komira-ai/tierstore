//! [`AwsEnv`]: the variables the AWS credential sources read, taken once from the process (or
//! given by a test), and the keys of [`CredentialSource::Environment`].

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::live::Keys;
use super::{CredentialSource, CredentialsError};

/// The environment the credential sources are found in: the `AWS_*` variables, with the profile to
/// use.
///
/// These are the provider-standard variables of AWS credentials; nothing else of the process's
/// environment is kept. `HOME` is not read, so the shared files are not looked for under
/// `~/.aws`: `AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE` name them (a service runs with
/// no home of its own to read). `Debug` shows the names of the variables, never their values.
#[derive(Clone, Default)]
pub struct AwsEnv {
    vars: BTreeMap<String, String>,
    profile: Option<String>,
}

impl AwsEnv {
    /// The `AWS_*` variables of this process, those whose name and value are UTF-8.
    #[must_use]
    pub fn from_process() -> Self {
        #[expect(
            clippy::disallowed_methods,
            reason = "the provider-standard AWS credential variables (AGENTS.md S2); every other variable is dropped"
        )]
        let vars = std::env::vars_os();
        Self::from_vars(vars.filter_map(|(name, value)| {
            Some((name.into_string().ok()?, value.into_string().ok()?))
        }))
    }

    /// The environment of `vars` alone, of which the `AWS_*` variables are kept.
    #[must_use]
    pub fn from_vars(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        let vars = vars
            .into_iter()
            .filter(|(name, _)| name.starts_with("AWS_"))
            .collect();
        Self {
            vars,
            profile: None,
        }
    }

    /// This environment, with the profile `name` in place of the one `AWS_PROFILE` names.
    #[must_use]
    pub fn with_profile(mut self, name: impl Into<String>) -> Self {
        self.profile = Some(name.into());
        self
    }

    /// The value of the variable `name`, if it is set and not empty.
    pub(super) fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    /// The profile to use, and whether it was named (by [`AwsEnv::with_profile`] or
    /// `AWS_PROFILE`) rather than the `default` taken when none is.
    pub(super) fn profile(&self) -> (&str, bool) {
        match self.profile.as_deref().or_else(|| self.var("AWS_PROFILE")) {
            Some(name) => (name, true),
            None => ("default", false),
        }
    }

    /// The path of a shared file, as the variable `name` gives it.
    pub(super) fn shared_file(&self, name: &str) -> Option<PathBuf> {
        self.var(name).map(PathBuf::from)
    }

    /// The region of the AWS services this environment names: `AWS_REGION`, then
    /// `AWS_DEFAULT_REGION`.
    pub(super) fn region(&self) -> Option<&str> {
        self.var("AWS_REGION")
            .or_else(|| self.var("AWS_DEFAULT_REGION"))
    }
}

impl std::fmt::Debug for AwsEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsEnv")
            .field("vars", &self.vars.keys().collect::<Vec<_>>())
            .field("profile", &self.profile)
            .finish()
    }
}

/// The keys of the environment: `None` when neither `AWS_ACCESS_KEY_ID` nor
/// `AWS_SECRET_ACCESS_KEY` is set.
///
/// # Errors
/// [`CredentialsError::Failed`] when one of the two is set and the other is not.
pub(super) fn keys(env: &AwsEnv) -> Result<Option<Keys>, CredentialsError> {
    let id = env.var("AWS_ACCESS_KEY_ID");
    let secret = env.var("AWS_SECRET_ACCESS_KEY");
    let (id, secret) = match (id, secret) {
        (Some(id), Some(secret)) => (id, secret),
        (None, None) => return Ok(None),
        (Some(_), None) => return Err(half("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY")),
        (None, Some(_)) => return Err(half("AWS_SECRET_ACCESS_KEY", "AWS_ACCESS_KEY_ID")),
    };
    Ok(Some(Keys {
        access_key_id: id.to_owned(),
        secret_access_key: secret.to_owned(),
        session_token: env.var("AWS_SESSION_TOKEN").map(str::to_owned),
    }))
}

fn half(set: &str, unset: &str) -> CredentialsError {
    CredentialsError::Failed {
        from: CredentialSource::Environment,
        why: format!("{set} is set and {unset} is not"),
    }
}
