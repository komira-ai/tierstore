//! What the credential endpoints share: a request sent and its answer read whole (it is small),
//! and the JSON credentials of the container endpoint and the instance metadata service.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::BytesMut;
use reqwest::RequestBuilder;
use serde::Deserialize;

use super::live::{Fetched, Keys};
use crate::s3::times::iso8601_millis;

/// The most an answer of a credential endpoint may be.
pub(super) const MAX_ANSWER: usize = 64 * 1024;

/// How the credential endpoints are asked: the client, and the limit on each request.
pub(crate) struct Http {
    pub(crate) client: reqwest::Client,
    pub(crate) timeout: Duration,
}

/// An answer: its status and its body.
pub(super) struct Answer {
    pub status: u16,
    pub body: String,
}

impl Http {
    /// Sends `request` and reads its answer. The error says what failed, never with the body.
    pub(super) async fn send(&self, request: RequestBuilder) -> Result<Answer, String> {
        let mut resp = request
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| format!("the request failed: {}", causes(&e)))?;
        let status = resp.status().as_u16();
        let mut body = BytesMut::new();
        loop {
            let piece = resp
                .chunk()
                .await
                .map_err(|e| format!("reading the answer failed: {}", causes(&e)))?;
            let Some(piece) = piece else { break };
            if body.len() + piece.len() > MAX_ANSWER {
                return Err(format!("the answer is longer than {MAX_ANSWER} bytes"));
            }
            body.extend_from_slice(&piece);
        }
        let body = String::from_utf8(body.to_vec())
            .map_err(|_| "the answer is not UTF-8 text".to_owned())?;
        Ok(Answer { status, body })
    }
}

/// `e` and each of its sources, joined by `: `.
fn causes(e: &(dyn std::error::Error + 'static)) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The time of an `Expiration`, `YYYY-MM-DDTHH:MM:SS[.fff]Z`.
pub(super) fn expiry(text: &str) -> Result<SystemTime, String> {
    let millis = iso8601_millis(text.trim())
        .ok_or_else(|| "its Expiration is not a UTC time YYYY-MM-DDTHH:MM:SSZ".to_owned())?;
    Ok(UNIX_EPOCH + Duration::from_millis(millis))
}

/// The JSON of temporary credentials (ECS, EKS Pod Identity and the instance metadata service
/// all answer it). Fields that are not read are ignored.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Json {
    code: Option<String>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    token: Option<String>,
    expiration: Option<String>,
}

/// The temporary credentials of the JSON `text`. A `Code` other than `Success` (the instance
/// metadata service says why it has none so) is refused with the code. The error never repeats
/// a value of the answer but the code.
pub(super) fn json_credentials(text: &str) -> Result<Fetched, String> {
    let json: Json = serde_json::from_str(text).map_err(|e| {
        format!(
            "the answer is not the JSON of credentials (at line {} column {})",
            e.line(),
            e.column()
        )
    })?;
    if let Some(code) = json.code.as_deref()
        && code != "Success"
    {
        return Err(format!("the answer's Code is {code:?}, not \"Success\""));
    }
    let field = |value: Option<String>, name: &str| {
        value
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("the answer has no {name}"))
    };
    let access_key_id = field(json.access_key_id, "AccessKeyId")?;
    let secret_access_key = field(json.secret_access_key, "SecretAccessKey")?;
    let token = field(json.token, "Token")?;
    let expires = expiry(&field(json.expiration, "Expiration")?)?;
    Ok(Fetched::temporary(
        Keys {
            access_key_id,
            secret_access_key,
            session_token: Some(token),
        },
        expires,
    ))
}
