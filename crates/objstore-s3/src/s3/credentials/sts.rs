//! [`CredentialSource::WebIdentity`]: a web identity token exchanged with STS
//! `AssumeRoleWithWebIdentity` for the credentials of a role.
//!
//! The token is read from its file at each exchange (a token of EKS is rotated in place), and
//! sent in the body of a POST, never in a URL. The request is not signed: the token is the proof.
//! STS is asked at `AWS_ENDPOINT_URL_STS` if it is set, and otherwise at the regional endpoint of
//! `AWS_REGION`, `AWS_DEFAULT_REGION` or the store's region
//! (`https://sts.<region>.amazonaws.com`, `.amazonaws.com.cn` for a region of China).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::header::CONTENT_TYPE;

use super::answer::{Http, expiry};
use super::live::{Fetched, Found, Keys, Plan};
use super::{AwsEnv, CredentialSource, CredentialsError};
use crate::s3::sigv4::uri_encode;

/// A role to take with a web identity token.
pub(super) struct Role {
    /// The source that named the role: [`CredentialSource::WebIdentity`] or a profile.
    pub from: CredentialSource,
    pub role_arn: String,
    pub token_file: PathBuf,
    /// The name of the role session; by default `objstore-s3-<seconds since the epoch>`.
    pub session_name: Option<String>,
}

/// The role of `AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_ROLE_ARN` and `AWS_ROLE_SESSION_NAME`, if the
/// first is set.
///
/// # Errors
/// The token file is set and the role is not.
pub(super) fn find(env: &AwsEnv) -> Result<Found, CredentialsError> {
    let Some(token_file) = env.var("AWS_WEB_IDENTITY_TOKEN_FILE") else {
        return Ok(Found::Unset(
            "AWS_WEB_IDENTITY_TOKEN_FILE is not set".to_owned(),
        ));
    };
    let Some(role_arn) = env.var("AWS_ROLE_ARN") else {
        return Err(CredentialsError::Failed {
            from: CredentialSource::WebIdentity,
            why: "AWS_WEB_IDENTITY_TOKEN_FILE is set and AWS_ROLE_ARN is not".to_owned(),
        });
    };
    Ok(Found::Plan(Plan::Role(Role {
        from: CredentialSource::WebIdentity,
        role_arn: role_arn.to_owned(),
        token_file: PathBuf::from(token_file),
        session_name: env.var("AWS_ROLE_SESSION_NAME").map(str::to_owned),
    })))
}

/// The STS endpoint for `region` in `env` (see the module docs).
pub(super) fn endpoint(env: &AwsEnv, region: &str) -> String {
    if let Some(url) = env.var("AWS_ENDPOINT_URL_STS") {
        return url.trim_end_matches('/').to_owned();
    }
    let domain = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    format!("https://sts.{region}.{domain}")
}

/// The credentials of `role`, from the STS of `region`, at `now`.
///
/// # Errors
/// What failed: the token file, the request, or STS's answer (its error code and message).
pub(super) async fn fetch(
    http: &Http,
    role: &Role,
    env: &AwsEnv,
    region: &str,
    now: SystemTime,
) -> Result<Fetched, String> {
    let path = role.token_file.display();
    let token = std::fs::read_to_string(&role.token_file)
        .map_err(|e| format!("reading the web identity token file {path}: {e}"))?;
    let token = token.trim();
    if token.is_empty() {
        return Err(format!("the web identity token file {path} is empty"));
    }
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let session = role
        .session_name
        .clone()
        .unwrap_or_else(|| format!("objstore-s3-{secs}"));
    let form = [
        ("Action", "AssumeRoleWithWebIdentity"),
        ("Version", "2011-06-15"),
        ("RoleArn", role.role_arn.as_str()),
        ("RoleSessionName", session.as_str()),
        ("WebIdentityToken", token),
    ];
    let body = form
        .iter()
        .map(|(name, value)| format!("{name}={}", uri_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let url = format!("{}/", endpoint(env, region));
    let request = http
        .client
        .post(url)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body);
    let answer = http
        .send(request)
        .await
        .map_err(|why| format!("STS AssumeRoleWithWebIdentity: {why}"))?;
    if answer.status != 200 {
        return Err(refusal(answer.status, &answer.body));
    }
    credentials(&answer.body)
}

/// What STS's error answer `body` of `status` says: its code and message.
fn refusal(status: u16, body: &str) -> String {
    let said = roxmltree::Document::parse(body).ok().and_then(|doc| {
        let text = |name: &str| {
            doc.descendants()
                .find(|node| node.has_tag_name(name))
                .and_then(|node| node.text())
                .map(str::to_owned)
        };
        Some((text("Code")?, text("Message").unwrap_or_default()))
    });
    match said {
        Some((code, message)) => {
            format!("STS AssumeRoleWithWebIdentity answered {status} {code}: {message}")
        }
        None => format!("STS AssumeRoleWithWebIdentity answered {status} with no error code"),
    }
}

/// The credentials of the answer `body` of `AssumeRoleWithWebIdentity`.
fn credentials(body: &str) -> Result<Fetched, String> {
    let doc = roxmltree::Document::parse(body)
        .map_err(|_| "STS AssumeRoleWithWebIdentity answered what is not XML".to_owned())?;
    let node = doc
        .descendants()
        .find(|node| node.has_tag_name("Credentials"))
        .ok_or_else(|| "STS AssumeRoleWithWebIdentity answered no Credentials".to_owned())?;
    let field = |name: &str| {
        node.children()
            .find(|child| child.has_tag_name(name))
            .and_then(|child| child.text())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("STS AssumeRoleWithWebIdentity answered no {name}"))
    };
    let keys = Keys {
        access_key_id: field("AccessKeyId")?,
        secret_access_key: field("SecretAccessKey")?,
        session_token: Some(field("SessionToken")?),
    };
    let expires = expiry(&field("Expiration")?)
        .map_err(|why| format!("STS AssumeRoleWithWebIdentity: {why}"))?;
    Ok(Fetched::temporary(keys, expires))
}
