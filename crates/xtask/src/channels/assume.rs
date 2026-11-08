//! `channels assume`: the job's AWS credentials from the OIDC token of the run, without a
//! third-party action (the repository's Actions policy admits GitHub's own actions and a short
//! list of others, and `aws-actions/configure-aws-credentials` is not on it).
//!
//! 1. The runner offers a token request (`ACTIONS_ID_TOKEN_REQUEST_URL`, authorised by
//!    `ACTIONS_ID_TOKEN_REQUEST_TOKEN`) to a job that holds `id-token: write`. Both are set by the
//!    platform, the second is a secret; they are the only environment this verb reads, each by
//!    name in [`platform_value`].
//! 2. The token is asked for with the audience `sts.amazonaws.com` over HTTPS.
//! 3. `aws sts assume-role-with-web-identity` exchanges it for the role's credentials.
//! 4. `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` and the region are
//!    appended to the env file (`GITHUB_ENV`), so the steps after this one carry them.
//!
//! Every secret value is masked (`::add-mask::`) on the log before it is used for anything else,
//! and none is ever printed or put in an error.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use super::aws::Aws;
use crate::Error;
use crate::fetch::{host_roots, tls_config};

/// The audience the AWS STS trusts a GitHub token for.
pub const AUDIENCE: &str = "sts.amazonaws.com";

/// How long the credentials last, in seconds (the shortest AWS allows for a role chain is 900;
/// the most a release job needs is far less than this).
const DURATION_SECONDS: &str = "3600";

/// How long the token request may take.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(60);

/// The most the token answer may be.
const MAX_TOKEN_ANSWER: u64 = 1024 * 1024;

/// The token request the runner offers the job.
pub struct Oidc {
    /// `ACTIONS_ID_TOKEN_REQUEST_URL`.
    pub url: String,
    /// `ACTIONS_ID_TOKEN_REQUEST_TOKEN`, a secret.
    pub bearer: String,
}

/// One non-empty value the platform sets in the environment of a job.
#[expect(
    clippy::disallowed_methods,
    reason = "the OIDC token request is offered by the runner in its environment and nowhere else (AGENTS.md S2: a platform-set value, and secret material)"
)]
pub(crate) fn platform_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The token request of this job.
///
/// # Errors
/// The runner offers none: the job lacks `id-token: write`.
pub fn oidc_from_env() -> Result<Oidc, Error> {
    oidc_from_env_with(platform_value)
}

/// [`oidc_from_env`], reading the values through `value` (the tests pass a stand-in).
fn oidc_from_env_with(value: impl Fn(&str) -> Option<String>) -> Result<Oidc, Error> {
    let missing = || {
        Error::Failed(
            "the runner offers no OIDC token (the job needs `permissions: id-token: write`)"
                .to_owned(),
        )
    };
    Ok(Oidc {
        url: value("ACTIONS_ID_TOKEN_REQUEST_URL").ok_or_else(missing)?,
        bearer: value("ACTIONS_ID_TOKEN_REQUEST_TOKEN").ok_or_else(missing)?,
    })
}

/// What `assume` is asked for.
pub struct Request<'a> {
    /// The role to assume.
    pub role_arn: &'a str,
    /// The session name AWS records.
    pub session_name: &'a str,
    /// The region the later steps use.
    pub region: &'a str,
    /// The file the `AWS_*` lines are appended to (`GITHUB_ENV`).
    pub env_file: &'a Path,
    /// Whether the token request must be HTTPS (it always is, bar the tests of the request on the
    /// loopback address).
    pub https_only: bool,
}

/// A value that may be written as one `NAME=value` line of an env file: not empty, one line, no
/// control character.
fn plain(what: &str, value: &str) -> Result<(), Error> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(Error::Usage(format!(
            "{what} is empty or has a control character"
        )));
    }
    Ok(())
}

/// A session name as AWS takes it: letters, digits and `_+=,.@-`, up to 64.
fn session_ok(name: &str) -> bool {
    (2..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_+=,.@-".contains(&b))
}

/// Asks the runner for the token: the JWT.
fn request_token(oidc: &Oidc, https_only: bool) -> Result<String, Error> {
    let url = format!(
        "{}{}audience={AUDIENCE}",
        oidc.url,
        if oidc.url.contains('?') { '&' } else { '?' }
    );
    // An error here never repeats the request: its url and bearer are the runner's.
    let failed = |why: String| Error::Failed(format!("the OIDC token request failed: {why}"));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(https_only)
        .timeout_global(Some(TOKEN_TIMEOUT))
        .tls_config(tls_config(&host_roots()))
        .build()
        .into();
    let mut response = agent
        .get(&url)
        .header("Authorization", format!("Bearer {}", oidc.bearer))
        .call()
        .map_err(|e| failed(e.to_string()))?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_TOKEN_ANSWER)
        .read_to_vec()
        .map_err(|e| failed(e.to_string()))?;
    let answer: Value =
        serde_json::from_slice(&body).map_err(|_| failed("the answer is not JSON".to_owned()))?;
    answer
        .get("value")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| failed("the answer has no `value`".to_owned()))
}

/// The three credentials of an STS answer (`--query Credentials`).
fn credentials(stdout: &str) -> Result<[String; 3], Error> {
    let unusable = || Error::Failed("the credentials answer of STS is unusable".to_owned());
    let answer: Value = serde_json::from_str(stdout).map_err(|_| unusable())?;
    let field = |name: &str| {
        answer
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty() && !v.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or_else(unusable)
    };
    Ok([
        field("AccessKeyId")?,
        field("SecretAccessKey")?,
        field("SessionToken")?,
    ])
}

/// Writes `::add-mask::<value>` to the log, now.
fn mask(log: &mut dyn Write, value: &str) -> Result<(), Error> {
    log.write_all(format!("::add-mask::{value}\n").as_bytes())
        .and_then(|()| log.flush())
        .map_err(Error::spawn("write the log".to_owned()))
}

/// The `assume` verb: the lines to print after the masks.
///
/// # Errors
/// An empty role, a session name or region that is not plain, a token request that fails, a
/// refusal by STS, or an env file that cannot be appended to. No credential is in the message.
pub fn assume(
    aws: &dyn Aws,
    log: &mut dyn Write,
    oidc: &dyn Fn() -> Result<Oidc, Error>,
    request: &Request,
) -> Result<Vec<String>, Error> {
    if request.role_arn.is_empty() {
        return Err(Error::Usage("--role-arn is empty".to_owned()));
    }
    plain("--role-arn", request.role_arn)?;
    plain("--region", request.region)?;
    if !session_ok(request.session_name) {
        return Err(Error::Usage(
            "--session-name is not 2 to 64 letters, digits and `_+=,.@-`".to_owned(),
        ));
    }
    let oidc = oidc()?;
    // The role carries the account id: kept out of the log, as the action's
    // `mask-aws-account-id` did.
    mask(log, request.role_arn)?;
    mask(log, &oidc.bearer)?;
    let jwt = request_token(&oidc, request.https_only)?;
    mask(log, &jwt)?;
    let answer = aws.run(
        &[
            "sts",
            "assume-role-with-web-identity",
            "--role-arn",
            request.role_arn,
            "--role-session-name",
            request.session_name,
            "--web-identity-token",
            &jwt,
            "--duration-seconds",
            DURATION_SECONDS,
            "--query",
            "Credentials",
            "--output",
            "json",
        ]
        .map(str::to_owned),
    )?;
    if !answer.ok {
        // The refusal of STS names the role and the reason, never a credential; the token it
        // may quote is masked already.
        return Err(Error::Failed(format!(
            "assuming the role failed: {}",
            answer.stderr.trim()
        )));
    }
    let [key, secret, session] = credentials(&answer.stdout)?;
    for value in [&key, &secret, &session] {
        mask(log, value)?;
    }
    crate::release::append_output(
        request.env_file,
        &[
            format!("AWS_ACCESS_KEY_ID={key}"),
            format!("AWS_SECRET_ACCESS_KEY={secret}"),
            format!("AWS_SESSION_TOKEN={session}"),
            format!("AWS_REGION={}", request.region),
            format!("AWS_DEFAULT_REGION={}", request.region),
        ],
    )?;
    Ok(vec![
        "assumed the role; its credentials are masked and reach the steps after this one"
            .to_owned(),
    ])
}

#[cfg(test)]
mod tests;
