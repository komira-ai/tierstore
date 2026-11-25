//! [`CredentialSource::InstanceMetadata`]: the role of the EC2 instance profile, from the
//! instance metadata service, with IMDSv2 alone.
//!
//! Each fetch asks for a session token first (`PUT /latest/api/token`), and sends it with the
//! two reads that follow (`x-aws-ec2-metadata-token`): the name of the role
//! (`GET /latest/meta-data/iam/security-credentials/`) and its credentials
//! (`GET /latest/meta-data/iam/security-credentials/<role>`). There is no fall-back to IMDSv1,
//! whose reads need no token.
//!
//! The service is `http://169.254.169.254` (`http://[fd00:ec2::254]` with
//! `AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE=IPv6`), or `AWS_EC2_METADATA_SERVICE_ENDPOINT`;
//! `AWS_EC2_METADATA_DISABLED=true` turns the source off.

use reqwest::Url;

use super::answer::{Http, json_credentials};
use super::live::{Fetched, Found, Plan};
use super::{AwsEnv, CredentialSource, CredentialsError};
use crate::s3::sigv4::uri_encode;

/// How long a session token asked for lasts, in seconds (the most the service gives).
pub(super) const TOKEN_TTL: &str = "21600";

/// The header a session token is asked for with, saying for how long.
pub(super) const TTL_HEADER: &str = "x-aws-ec2-metadata-token-ttl-seconds";

/// The header a session token is sent in.
pub(super) const TOKEN_HEADER: &str = "x-aws-ec2-metadata-token";

/// The instance metadata service to ask.
pub(super) struct Endpoint {
    /// Its base URL, with no `/` at the end.
    base: String,
}

/// The service of `env`, unless it is turned off.
///
/// # Errors
/// The endpoint or the endpoint mode is not one that can be asked.
pub(super) fn find(env: &AwsEnv) -> Result<Found, CredentialsError> {
    if env
        .var("AWS_EC2_METADATA_DISABLED")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    {
        return Ok(Found::Unset("AWS_EC2_METADATA_DISABLED is true".to_owned()));
    }
    let base = if let Some(endpoint) = env.var("AWS_EC2_METADATA_SERVICE_ENDPOINT") {
        let url = Url::parse(endpoint).map_err(|e| {
            failed(format!(
                "AWS_EC2_METADATA_SERVICE_ENDPOINT is not a URL: {e}"
            ))
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(failed(
                "AWS_EC2_METADATA_SERVICE_ENDPOINT is not an http or https URL".to_owned(),
            ));
        }
        endpoint.trim_end_matches('/').to_owned()
    } else {
        match env.var("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE") {
            None => "http://169.254.169.254".to_owned(),
            Some(mode) if mode.eq_ignore_ascii_case("ipv4") => "http://169.254.169.254".to_owned(),
            Some(mode) if mode.eq_ignore_ascii_case("ipv6") => "http://[fd00:ec2::254]".to_owned(),
            Some(mode) => {
                return Err(failed(format!(
                    "AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE is {mode:?}, not IPv4 or IPv6"
                )));
            }
        }
    };
    Ok(Found::Plan(Plan::Imds(Endpoint { base })))
}

fn failed(why: String) -> CredentialsError {
    CredentialsError::Failed {
        from: CredentialSource::InstanceMetadata,
        why,
    }
}

/// The credentials of the instance's role.
///
/// # Errors
/// What failed: the token request, the read of the role, or the read of its credentials.
pub(super) async fn fetch(http: &Http, endpoint: &Endpoint) -> Result<Fetched, String> {
    let base = &endpoint.base;
    let request = http
        .client
        .put(format!("{base}/latest/api/token"))
        .header(TTL_HEADER, TOKEN_TTL);
    let answer = http
        .send(request)
        .await
        .map_err(|why| format!("the IMDSv2 token request: {why}"))?;
    if answer.status != 200 {
        return Err(format!(
            "the IMDSv2 token request answered {}",
            answer.status
        ));
    }
    let token = answer.body.trim().to_owned();
    if token.is_empty() || token.contains(['\r', '\n']) {
        return Err("the IMDSv2 token request answered no token".to_owned());
    }
    let roles = format!("{base}/latest/meta-data/iam/security-credentials/");
    let request = http.client.get(&roles).header(TOKEN_HEADER, &token);
    let answer = http
        .send(request)
        .await
        .map_err(|why| format!("reading the instance's role: {why}"))?;
    match answer.status {
        200 => {}
        404 => return Err("the instance has no instance profile (no IAM role)".to_owned()),
        status => return Err(format!("reading the instance's role answered {status}")),
    }
    let Some(role) = answer.body.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return Err("the instance has no instance profile (no IAM role)".to_owned());
    };
    let request = http
        .client
        .get(format!("{roles}{}", uri_encode(role)))
        .header(TOKEN_HEADER, &token);
    let answer = http
        .send(request)
        .await
        .map_err(|why| format!("reading the credentials of role {role:?}: {why}"))?;
    if answer.status != 200 {
        return Err(format!(
            "reading the credentials of role {role:?} answered {}",
            answer.status
        ));
    }
    json_credentials(&answer.body)
}
