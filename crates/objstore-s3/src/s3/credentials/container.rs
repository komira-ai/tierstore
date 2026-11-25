//! [`CredentialSource::Container`]: the credentials endpoint of an ECS task or an EKS pod.
//!
//! The endpoint is `http://169.254.170.2` with the path of
//! `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` (ECS), or else the URL of
//! `AWS_CONTAINER_CREDENTIALS_FULL_URI` (EKS Pod Identity, and other agents), which must be
//! `https`, or `http` to a loopback address or to an address of the ECS or EKS agent
//! (`169.254.170.2`, `169.254.170.23`, `fd00:ec2::23`), as the AWS SDKs require. The request carries
//! the token of `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE` (read at each fetch: EKS rotates it), or
//! else of `AWS_CONTAINER_AUTHORIZATION_TOKEN`, as its `Authorization`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

use reqwest::Url;
use reqwest::header::AUTHORIZATION;

use super::answer::{Http, json_credentials};
use super::live::{Fetched, Found, Plan};
use super::{AwsEnv, CredentialSource, CredentialsError};

/// The host of the ECS agent's credentials endpoint.
const ECS_HOST: Ipv4Addr = Ipv4Addr::new(169, 254, 170, 2);

/// The host of the EKS Pod Identity agent, over IPv4.
const EKS_HOST: Ipv4Addr = Ipv4Addr::new(169, 254, 170, 23);

/// The host of the EKS Pod Identity agent, over IPv6.
const EKS_HOST_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x23);

/// Where to ask, and with what authorization.
pub(super) struct Endpoint {
    pub url: String,
    auth: Auth,
}

/// The `Authorization` of the request.
enum Auth {
    None,
    /// The token itself.
    Token(String),
    /// The file the token is read from at each fetch.
    File(PathBuf),
}

/// The endpoint of `env`, if one of the two variables is set.
///
/// # Errors
/// The variable is not a URL that may be asked.
pub(super) fn find(env: &AwsEnv) -> Result<Found, CredentialsError> {
    let url = if let Some(relative) = env.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        if !relative.starts_with('/') {
            return Err(failed(
                "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI does not start with /".to_owned(),
            ));
        }
        format!("http://{ECS_HOST}{relative}")
    } else if let Some(full) = env.var("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
        let url = Url::parse(full).map_err(|e| {
            failed(format!(
                "AWS_CONTAINER_CREDENTIALS_FULL_URI is not a URL: {e}"
            ))
        })?;
        allowed(&url)?;
        full.to_owned()
    } else {
        return Ok(Found::Unset(
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI and AWS_CONTAINER_CREDENTIALS_FULL_URI are not set"
                .to_owned(),
        ));
    };
    let auth = match (
        env.var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"),
        env.var("AWS_CONTAINER_AUTHORIZATION_TOKEN"),
    ) {
        (Some(file), _) => Auth::File(PathBuf::from(file)),
        (None, Some(token)) => Auth::Token(token.to_owned()),
        (None, None) => Auth::None,
    };
    Ok(Found::Plan(Plan::Container(Endpoint { url, auth })))
}

/// Refuses a full URI that is `http` to a host that is not loopback or an agent's, or is
/// neither `http` nor `https`.
fn allowed(url: &Url) -> Result<(), CredentialsError> {
    match url.scheme() {
        "https" => return Ok(()),
        "http" => {}
        scheme => {
            return Err(failed(format!(
                "AWS_CONTAINER_CREDENTIALS_FULL_URI has the scheme {scheme:?}, not http or https"
            )));
        }
    }
    let host = url.host_str().unwrap_or_default();
    let ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .ok();
    let agent = |ip: IpAddr| {
        ip.is_loopback()
            || ip == IpAddr::V4(ECS_HOST)
            || ip == IpAddr::V4(EKS_HOST)
            || ip == IpAddr::V6(EKS_HOST_V6)
    };
    if ip.is_some_and(agent) {
        return Ok(());
    }
    Err(failed(
        "AWS_CONTAINER_CREDENTIALS_FULL_URI is http to a host that is not a loopback address or \
         the ECS or EKS agent's; it must be https"
            .to_owned(),
    ))
}

fn failed(why: String) -> CredentialsError {
    CredentialsError::Failed {
        from: CredentialSource::Container,
        why,
    }
}

/// The credentials the endpoint gives.
///
/// # Errors
/// What failed: the token file, the request, or the answer.
pub(super) async fn fetch(http: &Http, endpoint: &Endpoint) -> Result<Fetched, String> {
    let token = match &endpoint.auth {
        Auth::None => None,
        Auth::Token(token) => Some(token.clone()),
        Auth::File(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                format!(
                    "reading the authorization token file {}: {e}",
                    path.display()
                )
            })?;
            Some(text.trim().to_owned())
        }
    };
    let mut request = http.client.get(&endpoint.url);
    if let Some(token) = token {
        if token.contains(['\r', '\n']) {
            return Err("the authorization token has a line break in it".to_owned());
        }
        request = request.header(AUTHORIZATION, token);
    }
    let answer = http.send(request).await?;
    if answer.status != 200 {
        return Err(format!(
            "the container credentials endpoint answered {}",
            answer.status
        ));
    }
    json_credentials(&answer.body)
}
