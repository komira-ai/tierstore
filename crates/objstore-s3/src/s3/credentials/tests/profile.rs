//! [`CredentialSource::Profile`]: the shared config and credentials files.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tierstore_testkit::dir::TestDir;

use super::super::CredentialsError;
use super::super::profile::{Sections, sections};
use super::*;

/// A scratch directory.
fn scratch() -> TestDir {
    TestDir::under(std::env::temp_dir(), "profile")
}

/// Writes `text` to `rel` under `dir`, making the directories above it, and returns the path.
fn put(dir: &TestDir, rel: &str, text: &str) -> PathBuf {
    let path = dir.path().join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

/// The environment whose two file variables name `.aws/credentials` and `.aws/config` under
/// `dir`, with `extra` variables.
fn files_env(dir: &TestDir, extra: &[(&str, &str)]) -> AwsEnv {
    let (credentials, config) = shown(dir);
    let mut vars = vec![
        ("AWS_SHARED_CREDENTIALS_FILE", credentials.as_str()),
        ("AWS_CONFIG_FILE", config.as_str()),
    ];
    vars.extend_from_slice(extra);
    env(&vars)
}

/// The paths of the credentials and config files under `.aws` of `dir`, as messages show them.
fn shown(dir: &TestDir) -> (String, String) {
    let aws = dir.path().join(".aws");
    (
        aws.join("credentials").display().to_string(),
        aws.join("config").display().to_string(),
    )
}

/// The result of the profile source in `env`.
async fn result_of(env: AwsEnv) -> Result<std::sync::Arc<super::super::Keys>, CredentialsError> {
    let clock = TestClock::at(T0);
    live(CredentialSource::Profile, env, &clock).keys().await
}

/// The error of the profile source in `env`, which must fail.
async fn error_of(env: AwsEnv) -> CredentialsError {
    result_of(env).await.err().unwrap()
}

/// Asserts that the INI reader skips comment lines (`#` and `;`), blank lines, indented
/// continuation lines (even ones that look like sections), keys before the first section or
/// after an unterminated `[` line, and lines with no `=`; merges a section given twice (the later value of a key wins); keeps `=`
/// in a value; and normalises the whitespace of a section name.
#[test]
fn the_ini_reader_follows_the_shared_file_format() {
    let text = "orphan = 1\n\
        # a comment\n\
        ; another\n\
        [default]\n\
        a = 1\n\
        \x20 nested = skipped\n\
        \tnested2 = skipped\n\
        b=2\n\
        \n\
        no equals here\n\
        [unterminated\n\
        \x20 [indented]\n\
        c =\n\
        [profile   spaced \t name ]\n\
        k = v = w\n\
        [default]\n\
        a = 3\n";
    let mut expected = Sections::new();
    expected.insert(
        "default".to_owned(),
        BTreeMap::from([
            ("a".to_owned(), "3".to_owned()),
            ("b".to_owned(), "2".to_owned()),
        ]),
    );
    expected.insert(
        "profile spaced name".to_owned(),
        BTreeMap::from([("k".to_owned(), "v = w".to_owned())]),
    );
    assert_eq!(sections(text), expected);
}

/// Asserts that a comment after a section header leaves the section's name alone (`[dev] # note`
/// is `dev`), and that a `[` line with no `]` starts no section: the keys after it belong to none
/// and do not land in the section before it.
#[test]
fn a_header_comment_is_ignored_and_an_unclosed_header_starts_no_section() {
    let text = "[dev] # a comment\nk = 1\n[broken\nlost = 2\n[other] ; note\nm = 3\n";
    let mut expected = Sections::new();
    expected.insert(
        "dev".to_owned(),
        BTreeMap::from([("k".to_owned(), "1".to_owned())]),
    );
    expected.insert(
        "other".to_owned(),
        BTreeMap::from([("m".to_owned(), "3".to_owned())]),
    );
    assert_eq!(sections(text), expected);
}

/// Asserts that a section with no keys is still a section, and that empty text has none.
#[test]
fn an_empty_section_is_a_section() {
    assert!(sections("").is_empty());
    let found = sections("[lonely]\n");
    assert_eq!(found.len(), 1);
    assert!(found["lonely"].is_empty());
}

/// Asserts that the keys of the default profile of the credentials file are signed with, with no
/// session token when the file has none.
#[tokio::test]
async fn static_keys_come_from_the_credentials_file() {
    let dir = scratch();
    put(
        &dir,
        ".aws/credentials",
        "[default]\naws_access_key_id = AKIAFILE\naws_secret_access_key = file-secret\n",
    );
    let keys = result_of(files_env(&dir, &[])).await.ok().unwrap();
    assert_eq!(keys.access_key_id, "AKIAFILE");
    assert_eq!(keys.secret_access_key, "file-secret");
    assert_eq!(keys.session_token, None);
}

/// Asserts that `aws_session_token` is the session token of the profile's keys.
#[tokio::test]
async fn the_session_token_of_a_profile_is_kept() {
    let dir = scratch();
    put(
        &dir,
        ".aws/credentials",
        "[default]\naws_access_key_id = ASIAFILE\naws_secret_access_key = s\naws_session_token = file-token\n",
    );
    let keys = result_of(files_env(&dir, &[])).await.ok().unwrap();
    assert_eq!(keys.session_token.as_deref(), Some("file-token"));
}

/// Asserts that the default profile of the config file is `[default]`, and also `[profile
/// default]`, and a named one is `[profile name]` and not `[name]` of the config file.
#[tokio::test]
async fn the_config_file_names_profiles_with_the_profile_prefix() {
    let dir = scratch();
    put(
        &dir,
        ".aws/config",
        "[default]\naws_access_key_id = FROMDEFAULT\naws_secret_access_key = s\n\
         [profile work]\naws_access_key_id = FROMWORK\naws_secret_access_key = s\n\
         [bare]\naws_access_key_id = BARE\naws_secret_access_key = s\n",
    );
    let default = result_of(files_env(&dir, &[])).await.ok().unwrap();
    assert_eq!(default.access_key_id, "FROMDEFAULT");
    let work = result_of(files_env(&dir, &[("AWS_PROFILE", "work")]))
        .await
        .ok()
        .unwrap();
    assert_eq!(work.access_key_id, "FROMWORK");
    let bare = error_of(files_env(&dir, &[("AWS_PROFILE", "bare")])).await;
    let (credentials, config) = shown(&dir);
    assert_eq!(
        bare.to_string(),
        format!("credential source profile: no profile \"bare\" in {credentials} or {config}")
    );

    let dir = scratch();
    put(
        &dir,
        ".aws/config",
        "[profile default]\naws_access_key_id = PREFIXED\naws_secret_access_key = s\n",
    );
    let keys = result_of(files_env(&dir, &[])).await.ok().unwrap();
    assert_eq!(keys.access_key_id, "PREFIXED");
}

/// Asserts that the keys of the credentials file override those of the config file one by one:
/// a key the credentials file lacks is taken from the config file.
#[tokio::test]
async fn the_credentials_file_overrides_the_config_file_key_by_key() {
    let dir = scratch();
    put(
        &dir,
        ".aws/config",
        "[profile p]\naws_access_key_id = CONFIGID\naws_secret_access_key = config-secret\n\
         aws_session_token = config-token\nregion = eu-west-1\n",
    );
    put(
        &dir,
        ".aws/credentials",
        "[p]\naws_access_key_id = CREDSID\n",
    );
    let keys = result_of(files_env(&dir, &[("AWS_PROFILE", "p")]))
        .await
        .ok()
        .unwrap();
    assert_eq!(keys.access_key_id, "CREDSID");
    assert_eq!(keys.secret_access_key, "config-secret");
    assert_eq!(keys.session_token.as_deref(), Some("config-token"));
}

/// Asserts that `AWS_PROFILE` picks the profile, and that `with_profile` picks it over
/// `AWS_PROFILE`.
#[tokio::test]
async fn aws_profile_and_with_profile_pick_the_profile() {
    let dir = scratch();
    put(
        &dir,
        ".aws/credentials",
        "[default]\naws_access_key_id = DEFAULTID\naws_secret_access_key = s\n\
         [one]\naws_access_key_id = ONEID\naws_secret_access_key = s\n\
         [two]\naws_access_key_id = TWOID\naws_secret_access_key = s\n",
    );
    let by_env = result_of(files_env(&dir, &[("AWS_PROFILE", "one")]))
        .await
        .ok()
        .unwrap();
    assert_eq!(by_env.access_key_id, "ONEID");
    let by_call = result_of(files_env(&dir, &[("AWS_PROFILE", "one")]).with_profile("two"))
        .await
        .ok()
        .unwrap();
    assert_eq!(by_call.access_key_id, "TWOID");
}

/// Asserts that `AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE` give the files wherever they
/// are, the keys of the credentials file over those of the config file, and that `HOME` is not
/// read: the files under `HOME/.aws` are not looked at.
#[tokio::test]
async fn the_file_variables_name_the_files_and_home_is_not_read() {
    let dir = scratch();
    let credentials = put(
        &dir,
        "elsewhere/creds",
        "[default]\naws_access_key_id = FROMVARS\n",
    );
    let config = put(
        &dir,
        "elsewhere/conf",
        "[default]\naws_secret_access_key = vars-secret\n",
    );
    put(
        &dir,
        ".aws/credentials",
        "[default]\naws_access_key_id = HOMEID\naws_secret_access_key = home-secret\n",
    );
    let home = dir.path().to_str().unwrap();
    let env = env(&[
        ("HOME", home),
        ("AWS_SHARED_CREDENTIALS_FILE", credentials.to_str().unwrap()),
        ("AWS_CONFIG_FILE", config.to_str().unwrap()),
    ]);
    let keys = result_of(env).await.ok().unwrap();
    assert_eq!(keys.access_key_id, "FROMVARS");
    assert_eq!(keys.secret_access_key, "vars-secret");
    let only_home = error_of(env_of_home(home)).await;
    assert!(matches!(only_home, CredentialsError::NotSet { .. }));
}

/// The environment of `HOME` alone.
fn env_of_home(home: &str) -> AwsEnv {
    env(&[("HOME", home)])
}

/// Asserts that a profile named and not in either file fails, naming both files.
#[tokio::test]
async fn a_named_profile_that_is_not_there_fails() {
    let dir = scratch();
    put(&dir, ".aws/credentials", "[other]\naws_access_key_id = X\n");
    let error = error_of(files_env(&dir, &[("AWS_PROFILE", "absent")])).await;
    let (credentials, config) = shown(&dir);
    assert_eq!(
        error.to_string(),
        format!("credential source profile: no profile \"absent\" in {credentials} or {config}")
    );
    assert!(matches!(error, CredentialsError::Failed { .. }));
    let by_call = error_of(files_env(&dir, &[]).with_profile("absent")).await;
    assert_eq!(by_call, error);
}

/// Asserts that, with the profile `default` not named and not in a file (or no file at all), the
/// explicit source is not set up and says where it looked.
#[tokio::test]
async fn a_default_profile_that_is_not_there_is_not_set() {
    let dir = scratch();
    let (credentials, config) = shown(&dir);
    let expected =
        format!("credential source profile: no profile \"default\" in {credentials} or {config}");
    let no_files = error_of(files_env(&dir, &[])).await;
    assert_eq!(no_files.to_string(), expected);
    assert!(matches!(no_files, CredentialsError::NotSet { .. }));
    put(&dir, ".aws/credentials", "[other]\naws_access_key_id = X\n");
    assert_eq!(error_of(files_env(&dir, &[])).await.to_string(), expected);
}

/// Asserts that with neither file variable set the source is not set up, whatever `HOME` is.
#[tokio::test]
async fn without_a_file_variable_the_source_is_not_set() {
    let expected =
        "credential source profile: AWS_SHARED_CREDENTIALS_FILE and AWS_CONFIG_FILE are not set";
    let none = error_of(env(&[])).await;
    assert_eq!(none.to_string(), expected);
    assert!(matches!(none, CredentialsError::NotSet { .. }));
    let home = error_of(env(&[("HOME", "/unused/home"), ("AWS_CONFIG_FILE", "")])).await;
    assert_eq!(home.to_string(), expected);
}

/// Asserts that either file variable alone is enough: the credentials file alone and the config
/// file alone each give a profile's keys.
#[tokio::test]
async fn one_file_variable_is_enough() {
    let dir = scratch();
    let credentials = put(
        &dir,
        "creds",
        "[default]\naws_access_key_id = ONLYCREDS\naws_secret_access_key = s\n",
    );
    let config = put(
        &dir,
        "conf",
        "[default]\naws_access_key_id = ONLYCONF\naws_secret_access_key = s\n",
    );
    let keys = result_of(env(&[(
        "AWS_SHARED_CREDENTIALS_FILE",
        credentials.to_str().unwrap(),
    )]))
    .await
    .ok()
    .unwrap();
    assert_eq!(keys.access_key_id, "ONLYCREDS");
    let keys = result_of(env(&[("AWS_CONFIG_FILE", config.to_str().unwrap())]))
        .await
        .ok()
        .unwrap();
    assert_eq!(keys.access_key_id, "ONLYCONF");
}

/// Asserts that a named profile that is missing names only the files whose variables are set:
/// the credentials file alone, and the config file alone.
#[tokio::test]
async fn a_missing_profile_names_only_the_files_that_are_set() {
    let dir = scratch();
    let (credentials, config) = shown(&dir);
    let error = error_of(env(&[
        ("AWS_PROFILE", "x"),
        ("AWS_SHARED_CREDENTIALS_FILE", &credentials),
    ]))
    .await;
    assert_eq!(
        error.to_string(),
        format!("credential source profile: no profile \"x\" in {credentials}")
    );
    let error = error_of(env(&[("AWS_PROFILE", "x"), ("AWS_CONFIG_FILE", &config)])).await;
    assert_eq!(
        error.to_string(),
        format!("credential source profile: no profile \"x\" in {config}")
    );
}

/// Asserts that a profile with a region and no keys is not set up, and says what it lacks.
#[tokio::test]
async fn a_profile_with_only_a_region_is_not_set() {
    let dir = scratch();
    put(&dir, ".aws/config", "[default]\nregion = eu-west-1\n");
    let error = error_of(files_env(&dir, &[])).await;
    assert_eq!(
        error.to_string(),
        "credential source profile: profile \"default\" has no aws_access_key_id, and no role_arn with web_identity_token_file"
    );
    assert!(matches!(error, CredentialsError::NotSet { .. }));
}

/// Asserts that a key id with no secret, and a secret with no key id (also an empty key id), are
/// refused, and the secret is not in the error.
#[tokio::test]
async fn half_a_key_pair_is_refused_both_ways() {
    let dir = scratch();
    put(
        &dir,
        ".aws/credentials",
        "[default]\naws_access_key_id = AKIAHALF\n\
         [other]\naws_secret_access_key = half-secret\n\
         [empty]\naws_access_key_id =\naws_secret_access_key = other-secret\n",
    );
    let id_only = error_of(files_env(&dir, &[])).await;
    assert_eq!(
        id_only.to_string(),
        "credential source profile: profile \"default\" has aws_access_key_id and no aws_secret_access_key"
    );
    let secret_only = error_of(files_env(&dir, &[("AWS_PROFILE", "other")])).await;
    assert_eq!(
        secret_only.to_string(),
        "credential source profile: profile \"other\" has aws_secret_access_key and no aws_access_key_id"
    );
    assert!(!secret_only.to_string().contains("half-secret"));
    let empty_id = error_of(files_env(&dir, &[("AWS_PROFILE", "empty")])).await;
    assert_eq!(
        empty_id.to_string(),
        "credential source profile: profile \"empty\" has aws_secret_access_key and no aws_access_key_id"
    );
}

/// Asserts that `role_arn` with `web_identity_token_file` asks STS with the token of the file
/// and the profile's `role_session_name`, and that the role's keys are signed with.
#[tokio::test]
async fn a_profile_role_with_a_token_file_is_taken_from_sts() {
    let answer = sts_answer("ASIAPROFILE", "profile-secret", "profile-token", T0 + 3600);
    let fake = Fake::start(move |_| (200, answer.clone())).await;
    let dir = scratch();
    let token = put(&dir, "token", "profile-web-token\n");
    put(
        &dir,
        ".aws/config",
        &format!(
            "[profile r]\nrole_arn = arn:aws:iam::1:role/r\nweb_identity_token_file = {}\n\
             role_session_name = profile-session\n",
            token.display()
        ),
    );
    let env = files_env(
        &dir,
        &[("AWS_PROFILE", "r"), ("AWS_ENDPOINT_URL_STS", fake.url())],
    );
    let keys = result_of(env).await.ok().unwrap();
    assert_eq!(keys.access_key_id, "ASIAPROFILE");
    assert_eq!(keys.secret_access_key, "profile-secret");
    assert_eq!(keys.session_token.as_deref(), Some("profile-token"));
    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(
        seen[0].body,
        "Action=AssumeRoleWithWebIdentity&Version=2011-06-15&RoleArn=arn%3Aaws%3Aiam%3A%3A1%3Arole%2Fr\
         &RoleSessionName=profile-session&WebIdentityToken=profile-web-token"
    );
}

/// Asserts that a failure of STS for a profile's role is reported as the profile source's, and
/// holds no token.
#[tokio::test]
async fn a_profile_role_that_sts_refuses_fails_as_the_profile() {
    let body = "<ErrorResponse><Error><Code>AccessDenied</Code><Message>no</Message></Error></ErrorResponse>";
    let fake = Fake::start(move |_| (403, body.to_owned())).await;
    let dir = scratch();
    let token = put(&dir, "token", "profile-web-token");
    put(
        &dir,
        ".aws/config",
        &format!(
            "[default]\nrole_arn = arn:aws:iam::1:role/r\nweb_identity_token_file = {}\n",
            token.display()
        ),
    );
    let error = error_of(files_env(&dir, &[("AWS_ENDPOINT_URL_STS", fake.url())])).await;
    assert_eq!(
        error.to_string(),
        "credential source profile: STS AssumeRoleWithWebIdentity answered 403 AccessDenied: no"
    );
    assert!(!error.to_string().contains("profile-web-token"));
}

/// Asserts that `role_arn` with no token file and no other way to get credentials is refused.
#[tokio::test]
async fn a_role_without_a_token_file_fails() {
    let dir = scratch();
    put(
        &dir,
        ".aws/config",
        "[default]\nrole_arn = arn:aws:iam::1:role/r\n",
    );
    assert_eq!(
        error_of(files_env(&dir, &[])).await.to_string(),
        "credential source profile: profile \"default\" has role_arn and no web_identity_token_file"
    );
}

/// Asserts that each setting this crate does not support is refused by name.
#[tokio::test]
async fn unsupported_settings_are_refused_by_name() {
    let cases = [
        (
            "role_arn = arn:aws:iam::1:role/r\nsource_profile = base\n",
            "role_arn with source_profile",
        ),
        (
            "role_arn = arn:aws:iam::1:role/r\ncredential_source = Ec2InstanceMetadata\n",
            "role_arn with credential_source",
        ),
        ("credential_process = /bin/creds\n", "credential_process"),
        (
            "sso_session = corp\n",
            "IAM Identity Center (sso_session, sso_start_url)",
        ),
        (
            "sso_start_url = https://example.invalid/start\n",
            "IAM Identity Center (sso_session, sso_start_url)",
        ),
    ];
    for (text, feature) in cases {
        let dir = scratch();
        put(&dir, ".aws/config", &format!("[profile p]\n{text}"));
        let error = error_of(files_env(&dir, &[("AWS_PROFILE", "p")])).await;
        assert_eq!(
            error.to_string(),
            format!(
                "credential source profile: profile \"p\" uses {feature}, which is not supported"
            )
        );
        assert!(matches!(error, CredentialsError::Unsupported { .. }));
    }
}

/// Asserts that a credentials file that is a directory (a read error that is not "not found")
/// fails the source naming the path, and that so does a config file that is one.
#[tokio::test]
async fn a_shared_file_that_cannot_be_read_fails() {
    let dir = scratch();
    let folder = dir.path().join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    let absent = dir.path().join("absent");
    let as_credentials = env(&[
        ("AWS_SHARED_CREDENTIALS_FILE", folder.to_str().unwrap()),
        ("AWS_CONFIG_FILE", absent.to_str().unwrap()),
    ]);
    let error = error_of(as_credentials).await;
    assert!(matches!(error, CredentialsError::Failed { .. }));
    assert!(
        error.to_string().starts_with(&format!(
            "credential source profile: reading {}: ",
            folder.display()
        )),
        "{error}"
    );
    let as_config = env(&[
        ("AWS_SHARED_CREDENTIALS_FILE", absent.to_str().unwrap()),
        ("AWS_CONFIG_FILE", folder.to_str().unwrap()),
    ]);
    let error = error_of(as_config).await;
    assert!(
        error.to_string().starts_with(&format!(
            "credential source profile: reading {}: ",
            folder.display()
        )),
        "{error}"
    );
}
