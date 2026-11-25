//! [`CredentialSource::Profile`]: a profile of the shared config and credentials files.
//!
//! The profile is the one [`AwsEnv::with_profile`] or `AWS_PROFILE` names, or `default`. Its keys
//! are those of its section in the config file (`[profile <name>]`, or `[default]` for the
//! default profile) with those of its section in the credentials file (`[<name>]`) over them.
//! It gives:
//!
//! - with `role_arn` and `web_identity_token_file` (and `role_session_name`, if given): the
//!   role's credentials, taken with the token (`sts.rs`);
//! - with `aws_access_key_id` and `aws_secret_access_key` (and `aws_session_token`, if given):
//!   those keys.
//!
//! A role taken with the credentials of another profile or source (`source_profile`,
//! `credential_source`), `credential_process` and IAM Identity Center (`sso_session`,
//! `sso_start_url`) are refused by name. A profile that is not in either file, or has no key of
//! these, is not set up, unless it was named: a profile asked for by name that is not there is an
//! error.
//!
//! The files are read as the AWS CLI writes them: `[section]` lines, `key = value` lines, and
//! comment lines starting with `#` or `;`. An indented line is a nested value of the key before
//! it, and no key read here has one, so it is skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::live::{Found, Keys, Plan};
use super::sts::Role;
use super::{AwsEnv, CredentialSource, CredentialsError};

/// The sections of an INI file, and the keys of each.
pub(super) type Sections = BTreeMap<String, BTreeMap<String, String>>;

/// The plan of the profile of `env`, if it is set up.
///
/// # Errors
/// A file that exists and cannot be read; a profile named and not in either file; a profile that
/// asks for what is not supported, or has half of a pair of keys.
pub(super) fn find(env: &AwsEnv) -> Result<Found, CredentialsError> {
    let (name, named) = env.profile();
    let credentials_path = env.shared_file("AWS_SHARED_CREDENTIALS_FILE", "credentials");
    let config_path = env.shared_file("AWS_CONFIG_FILE", "config");
    let (Some(credentials_path), Some(config_path)) = (credentials_path, config_path) else {
        return Ok(Found::Unset(
            "HOME is not set, and AWS_SHARED_CREDENTIALS_FILE and AWS_CONFIG_FILE are not both set"
                .to_owned(),
        ));
    };
    let credentials = read(&credentials_path)?;
    let config = read(&config_path)?;
    let mut keys = BTreeMap::new();
    let config_section = config.as_ref().and_then(|sections| {
        let section = sections.get(&format!("profile {name}"));
        section.or_else(|| (name == "default").then(|| sections.get(name)).flatten())
    });
    let credentials_section = credentials.as_ref().and_then(|sections| sections.get(name));
    for section in [config_section, credentials_section].into_iter().flatten() {
        keys.extend(section.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    if config_section.is_none() && credentials_section.is_none() {
        let why = format!(
            "no profile {name:?} in {} or {}",
            credentials_path.display(),
            config_path.display()
        );
        if named {
            return Err(failed(why));
        }
        return Ok(Found::Unset(why));
    }
    profile_plan(name, &keys)
}

/// The plan of the profile `name` whose keys are `keys`.
fn profile_plan(name: &str, keys: &BTreeMap<&str, &str>) -> Result<Found, CredentialsError> {
    let get = |key: &str| keys.get(key).copied().filter(|value| !value.is_empty());
    let unsupported = |feature| {
        Err(CredentialsError::Unsupported {
            profile: name.to_owned(),
            feature,
        })
    };
    if let Some(role_arn) = get("role_arn") {
        if let Some(token_file) = get("web_identity_token_file") {
            return Ok(Found::Plan(Plan::Role(Role {
                from: CredentialSource::Profile,
                role_arn: role_arn.to_owned(),
                token_file: PathBuf::from(token_file),
                session_name: get("role_session_name").map(str::to_owned),
            })));
        }
        if get("source_profile").is_some() {
            return unsupported("role_arn with source_profile");
        }
        if get("credential_source").is_some() {
            return unsupported("role_arn with credential_source");
        }
        return Err(failed(format!(
            "profile {name:?} has role_arn and no web_identity_token_file"
        )));
    }
    match (get("aws_access_key_id"), get("aws_secret_access_key")) {
        (Some(id), Some(secret)) => {
            return Ok(Found::Plan(Plan::Fixed(
                CredentialSource::Profile,
                Keys {
                    access_key_id: id.to_owned(),
                    secret_access_key: secret.to_owned(),
                    session_token: get("aws_session_token").map(str::to_owned),
                },
            )));
        }
        (Some(_), None) => {
            return Err(failed(format!(
                "profile {name:?} has aws_access_key_id and no aws_secret_access_key"
            )));
        }
        (None, Some(_)) => {
            return Err(failed(format!(
                "profile {name:?} has aws_secret_access_key and no aws_access_key_id"
            )));
        }
        (None, None) => {}
    }
    if get("credential_process").is_some() {
        return unsupported("credential_process");
    }
    if get("sso_session").is_some() || get("sso_start_url").is_some() {
        return unsupported("IAM Identity Center (sso_session, sso_start_url)");
    }
    Ok(Found::Unset(format!(
        "profile {name:?} has no aws_access_key_id, and no role_arn with web_identity_token_file"
    )))
}

fn failed(why: String) -> CredentialsError {
    CredentialsError::Failed {
        from: CredentialSource::Profile,
        why,
    }
}

/// The sections of the file at `path`, or `None` if there is no file.
///
/// # Errors
/// The file exists and cannot be read as UTF-8 text.
fn read(path: &Path) -> Result<Option<Sections>, CredentialsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(sections(&text))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(failed(format!("reading {}: {e}", path.display()))),
    }
}

/// The sections of the INI text `text` (see the module docs). A key before the first section,
/// and a line that is not a section, a key or a comment, are skipped; a section given twice has
/// the keys of both, the later value of a key given twice.
pub(super) fn sections(text: &str) -> Sections {
    let mut sections = Sections::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            sections.entry(name.clone()).or_default();
            current = Some(name);
            continue;
        }
        let (Some(section), Some((key, value))) = (&current, line.split_once('=')) else {
            continue;
        };
        let keys = sections.entry(section.clone()).or_default();
        keys.insert(key.trim().to_owned(), value.trim().to_owned());
    }
    sections
}
