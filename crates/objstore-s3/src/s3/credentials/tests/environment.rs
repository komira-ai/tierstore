//! [`CredentialSource::Environment`] and [`AwsEnv`] itself.

use super::super::env as env_source;
use super::*;

/// The error of the keys of `source` in `env`, which must fail.
async fn error_of(source: CredentialSource, env: AwsEnv) -> super::super::CredentialsError {
    let clock = TestClock::at(T0);
    live(source, env, &clock).keys().await.err().unwrap()
}

/// Asserts that the key pair of `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` is signed with,
/// with no session token when `AWS_SESSION_TOKEN` is not set.
#[tokio::test]
async fn the_key_pair_of_the_environment_has_no_session_token_without_one() {
    let env = env(&[
        ("AWS_ACCESS_KEY_ID", "AKIAENV"),
        ("AWS_SECRET_ACCESS_KEY", "env-secret"),
    ]);
    let clock = TestClock::at(T0);
    let keys = live(CredentialSource::Environment, env, &clock)
        .keys()
        .await
        .unwrap();
    assert_eq!(keys.access_key_id, "AKIAENV");
    assert_eq!(keys.secret_access_key, "env-secret");
    assert_eq!(keys.session_token, None);
}

/// Asserts that `AWS_SESSION_TOKEN` is the session token of the keys of the environment, and
/// that such keys never need renewing (the clock moved far ahead gives the same keys).
#[tokio::test]
async fn the_session_token_of_the_environment_is_kept_with_the_keys() {
    let env = env(&[
        ("AWS_ACCESS_KEY_ID", "ASIAENV"),
        ("AWS_SECRET_ACCESS_KEY", "env-secret"),
        ("AWS_SESSION_TOKEN", "env-token"),
    ]);
    let clock = TestClock::at(T0);
    let live = live(CredentialSource::Environment, env, &clock);
    let first = live.keys().await.unwrap();
    assert_eq!(first.session_token.as_deref(), Some("env-token"));
    clock.set(T0 + 10 * 365 * 86_400);
    let later = live.keys().await.unwrap();
    assert!(std::sync::Arc::ptr_eq(&first, &later));
}

/// Asserts that an access key id with no secret key is refused, naming both variables and
/// holding neither value.
#[tokio::test]
async fn an_access_key_id_without_a_secret_is_refused() {
    let env = env(&[("AWS_ACCESS_KEY_ID", "AKIAHALF")]);
    let error = error_of(CredentialSource::Environment, env).await;
    assert_eq!(
        error.to_string(),
        "credential source environment: AWS_ACCESS_KEY_ID is set and AWS_SECRET_ACCESS_KEY is not"
    );
}

/// Asserts that a secret key with no access key id is refused, and that the secret is not in the
/// error.
#[tokio::test]
async fn a_secret_without_an_access_key_id_is_refused() {
    let env = env(&[("AWS_SECRET_ACCESS_KEY", "lonely-secret")]);
    let error = error_of(CredentialSource::Environment, env).await;
    assert_eq!(
        error.to_string(),
        "credential source environment: AWS_SECRET_ACCESS_KEY is set and AWS_ACCESS_KEY_ID is not"
    );
    assert!(!error.to_string().contains("lonely-secret"));
}

/// Asserts that the source named `Environment` with none of the variables set is not set up,
/// and says what it needs.
#[tokio::test]
async fn the_environment_source_with_nothing_set_is_not_set() {
    let error = error_of(CredentialSource::Environment, env(&[])).await;
    assert_eq!(
        error.to_string(),
        "credential source environment: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are not set"
    );
    assert!(matches!(
        error,
        super::super::CredentialsError::NotSet {
            from: CredentialSource::Environment,
            ..
        }
    ));
}

/// Asserts that empty values count as not set: two empty variables are not set up, and an empty
/// key id beside a secret is the half pair.
#[tokio::test]
async fn empty_values_count_as_unset() {
    let both_empty = env(&[("AWS_ACCESS_KEY_ID", ""), ("AWS_SECRET_ACCESS_KEY", "")]);
    assert_eq!(
        error_of(CredentialSource::Environment, both_empty)
            .await
            .to_string(),
        "credential source environment: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are not set"
    );
    let empty_id = env(&[
        ("AWS_ACCESS_KEY_ID", ""),
        ("AWS_SECRET_ACCESS_KEY", "some-secret"),
    ]);
    assert_eq!(
        error_of(CredentialSource::Environment, empty_id)
            .await
            .to_string(),
        "credential source environment: AWS_SECRET_ACCESS_KEY is set and AWS_ACCESS_KEY_ID is not"
    );
    let empty_token = env(&[
        ("AWS_ACCESS_KEY_ID", "AKIA"),
        ("AWS_SECRET_ACCESS_KEY", "s"),
        ("AWS_SESSION_TOKEN", ""),
    ]);
    let keys = env_source::keys(&empty_token).unwrap().unwrap();
    assert_eq!(keys.session_token, None);
}

/// Asserts that `AwsEnv::from_vars` keeps the `AWS_*` variables and nothing else (`HOME`, `AWSX`,
/// `PATH` and a lower-case `aws_` name are dropped), and that its `Debug` shows names and no
/// value.
#[test]
fn from_vars_keeps_only_aws_variables() {
    let env = env(&[
        ("AWS_SECRET_ACCESS_KEY", "the-secret"),
        ("HOME", "/home/someone"),
        ("PATH", "/usr/bin"),
        ("AWSX", "x"),
        ("aws_region", "r"),
        ("SECRET", "hidden"),
    ]);
    assert_eq!(env.var("PATH"), None);
    assert_eq!(env.var("AWSX"), None);
    assert_eq!(env.var("aws_region"), None);
    assert_eq!(env.var("AWS_SECRET_ACCESS_KEY"), Some("the-secret"));
    assert_eq!(env.var("HOME"), None);
    assert_eq!(
        format!("{env:?}"),
        r#"AwsEnv { vars: ["AWS_SECRET_ACCESS_KEY"], profile: None }"#
    );
}

/// Asserts that `Debug` of an environment with a chosen profile shows the profile name and still
/// no value of a variable.
#[test]
fn debug_shows_names_not_values() {
    let env = env(&[("AWS_SESSION_TOKEN", "secret-token-value")]).with_profile("work");
    let shown = format!("{env:?}");
    assert_eq!(
        shown,
        r#"AwsEnv { vars: ["AWS_SESSION_TOKEN"], profile: Some("work") }"#
    );
    assert!(!shown.contains("secret-token-value"));
}

/// Asserts that the environment taken from this process names only `AWS_*` variables (the values
/// are the host's and are not looked at).
#[test]
fn from_process_names_only_aws_variables() {
    let shown = format!("{:?}", AwsEnv::from_process());
    let names = shown
        .split_once("vars: [")
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(names, _)| names)
        .unwrap();
    for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let name = name.trim_matches('"');
        assert!(name.starts_with("AWS_"), "unexpected variable {name}");
    }
    assert!(shown.ends_with("profile: None }"));
}

/// Asserts that the profile is `default` and not named when nothing names one, that
/// `AWS_PROFILE` names one, that an empty `AWS_PROFILE` names none, and that `with_profile`
/// overrides `AWS_PROFILE`.
#[test]
fn with_profile_overrides_aws_profile() {
    assert_eq!(env(&[]).profile(), ("default", false));
    assert_eq!(env(&[("AWS_PROFILE", "")]).profile(), ("default", false));
    let named = env(&[("AWS_PROFILE", "from-env")]);
    assert_eq!(named.profile(), ("from-env", true));
    let chosen = named.with_profile("chosen");
    assert_eq!(chosen.profile(), ("chosen", true));
}

/// Asserts that a shared file is the path its variable gives and nothing else: with the variable
/// unset or empty there is none, whatever `HOME` is.
#[test]
fn shared_file_is_the_variable_alone() {
    let given = env(&[
        ("AWS_CONFIG_FILE", "/given/config"),
        ("HOME", "/home/someone"),
    ]);
    assert_eq!(
        given.shared_file("AWS_CONFIG_FILE"),
        Some("/given/config".into())
    );
    assert_eq!(given.shared_file("AWS_SHARED_CREDENTIALS_FILE"), None);
    assert_eq!(
        env(&[("AWS_CONFIG_FILE", "")]).shared_file("AWS_CONFIG_FILE"),
        None
    );
}

/// Asserts that the region is `AWS_REGION`, else `AWS_DEFAULT_REGION`, else none, and that an
/// empty `AWS_REGION` is passed over.
#[test]
fn region_is_aws_region_then_aws_default_region() {
    let both = env(&[
        ("AWS_REGION", "eu-west-1"),
        ("AWS_DEFAULT_REGION", "ap-south-1"),
    ]);
    assert_eq!(both.region(), Some("eu-west-1"));
    let default_only = env(&[("AWS_DEFAULT_REGION", "ap-south-1")]);
    assert_eq!(default_only.region(), Some("ap-south-1"));
    let empty = env(&[("AWS_REGION", ""), ("AWS_DEFAULT_REGION", "ap-south-1")]);
    assert_eq!(empty.region(), Some("ap-south-1"));
    assert_eq!(env(&[]).region(), None);
}
