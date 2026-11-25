//! Where the credentials of an `s3` tier come from, as a file says it: the key of
//! `credentials_file` (the default), or a source of AWS credentials (`credential_source`, with
//! `credential_profile`), what the loader refuses of the two keys, and that the profile of the
//! file is the one the source reads.

use bytes::Bytes;
use objstore_s3::ObjectKey;
use objstore_s3::ObjectStore as _;
use objstore_s3::s3::S3Store;
use objstore_s3::s3::fake::FakeS3;
use tierstore_testkit::dir::TestDir;

use super::{SourceName, SourceSetting};
use crate::s3::{AwsEnv, Capabilities, CredentialSource};
use crate::server::config::{from_text, settings_from_text};
use crate::server::config_testing::{DEFAULTS, GOLDEN, MINIMAL};

/// The line of the golden file's hot tier that names its key file.
const HOT_FILE: &str = "credentials_file   = \"/etc/tierstore/hot.credentials\"\n";

/// The line of the golden file's cold tier that names its key file.
const COLD_FILE: &str = "credentials_file = \"/etc/tierstore/cold.credentials\"\n";

/// What a start of `text` is refused with.
fn refusal(text: &str) -> String {
    settings_from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// The golden file with the key file line of the hot tier replaced by `hot`.
fn hot(hot: &str) -> String {
    GOLDEN.replacen(HOT_FILE, hot, 1)
}

/// Asserts that each value of `hot_store.credential_source` that names a source gives that
/// source with no key file, that `file` and no value at all keep the key file, and that
/// `credential_profile` reaches the setting with `profile` and with `auto`.
#[test]
fn each_source_of_the_file_is_the_source_of_the_tier() {
    let sources = [
        ("auto", CredentialSource::Auto),
        ("environment", CredentialSource::Environment),
        ("profile", CredentialSource::Profile),
        ("web-identity", CredentialSource::WebIdentity),
        ("container", CredentialSource::Container),
        ("instance-metadata", CredentialSource::InstanceMetadata),
    ];
    for (value, source) in sources {
        let cli = from_text(
            &hot(&format!("credential_source = \"{value}\"\n")),
            DEFAULTS,
        )
        .unwrap();
        let setting = SourceSetting {
            source,
            profile: None,
        };
        assert_eq!(cli.hot_credential_source, Some(setting), "{value}");
        assert_eq!(cli.hot_credentials_file, None, "{value}");
        assert_eq!(cli.cold_credential_source, None, "{value}");
    }
    let file = hot(&format!("{HOT_FILE}credential_source = \"file\"\n"));
    let cli = from_text(&file, DEFAULTS).unwrap();
    assert_eq!(cli.hot_credential_source, None);
    assert!(cli.hot_credentials_file.is_some());
    let cli = from_text(GOLDEN, DEFAULTS).unwrap();
    assert_eq!(cli.hot_credential_source, None);
    for (value, source) in [
        ("profile", CredentialSource::Profile),
        ("auto", CredentialSource::Auto),
    ] {
        let line = format!("credential_source = \"{value}\"\ncredential_profile = \"work\"\n");
        let cli = from_text(&hot(&line), DEFAULTS).unwrap();
        let setting = SourceSetting {
            source,
            profile: Some("work".to_owned()),
        };
        assert_eq!(cli.hot_credential_source, Some(setting), "{value}");
    }
}

/// Asserts that the cold tier takes a source as the hot tier does, each tier its own.
#[test]
fn the_cold_tier_has_a_source_of_its_own() {
    let text = GOLDEN.replacen(COLD_FILE, "credential_source = \"web-identity\"\n", 1);
    let cli = from_text(&text, DEFAULTS).unwrap();
    let setting = SourceSetting {
        source: CredentialSource::WebIdentity,
        profile: None,
    };
    assert_eq!(cli.cold_credential_source, Some(setting));
    assert_eq!(cli.cold_credentials_file, None);
    assert_eq!(cli.hot_credential_source, None);
    assert!(cli.hot_credentials_file.is_some());
}

/// Asserts the refusals, each naming the key, with the message exactly: a key file beside a
/// source, a profile with a source that reads none (`environment`, `file`, and no source), no
/// key file and no source, and either key in a `memory` or a `dir` tier.
#[test]
fn the_two_keys_are_refused_where_they_mean_nothing() {
    let both = hot(&format!("{HOT_FILE}credential_source = \"container\"\n"));
    assert_eq!(
        refusal(&both),
        "hot_store.credentials_file: not with credential_source = \"container\": the \
         credentials come from the one or the other"
    );
    let profile_only = "hot_store.credential_profile: only for the credential sources \
                        \"profile\" and \"auto\"";
    let environment = hot("credential_source = \"environment\"\ncredential_profile = \"work\"\n");
    assert_eq!(refusal(&environment), profile_only);
    let file = hot(&format!(
        "{HOT_FILE}credential_source = \"file\"\ncredential_profile = \"work\"\n"
    ));
    assert_eq!(refusal(&file), profile_only);
    let unnamed = hot(&format!("{HOT_FILE}credential_profile = \"work\"\n"));
    assert_eq!(refusal(&unnamed), profile_only);
    assert_eq!(
        refusal(&hot("")),
        "hot_store.credentials_file: it is needed"
    );
    let file_alone = hot("credential_source = \"file\"\n");
    assert_eq!(
        refusal(&file_alone),
        "hot_store.credentials_file: it is needed"
    );
    let memory = "kind   = \"memory\"\nbudget = 1000";
    for (key, line) in [
        ("credential_source", "credential_source = \"auto\""),
        ("credential_profile", "credential_profile = \"work\""),
    ] {
        let text = MINIMAL.replacen(memory, &format!("{memory}\n{line}"), 1);
        let why = format!("hot_store.{key}: not for kind \"memory\"");
        assert_eq!(refusal(&text), why);
        let dir = format!("kind = \"dir\"\npath = \"/var/hot\"\nbudget = 1000\n{line}");
        let text = MINIMAL.replacen(memory, &dir, 1);
        assert_eq!(
            refusal(&text),
            format!("hot_store.{key}: not for kind \"dir\"")
        );
    }
}

/// Asserts that a value that is not a source is refused, naming the key and not the value.
#[test]
fn a_value_that_is_not_a_source_is_refused() {
    let why = refusal(&hot("credential_source = \"imdsv1\"\n"));
    assert!(why.contains("credential_source"), "{why}");
}

/// Asserts the source of each name, and that `file` is none.
#[test]
fn each_name_is_its_source() {
    assert_eq!(SourceName::File.source(), None);
    assert_eq!(SourceName::Auto.source(), Some(CredentialSource::Auto));
    assert_eq!(
        SourceName::Environment.source(),
        Some(CredentialSource::Environment)
    );
    assert_eq!(
        SourceName::Profile.source(),
        Some(CredentialSource::Profile)
    );
    assert_eq!(
        SourceName::WebIdentity.source(),
        Some(CredentialSource::WebIdentity)
    );
    assert_eq!(
        SourceName::Container.source(),
        Some(CredentialSource::Container)
    );
    assert_eq!(
        SourceName::InstanceMetadata.source(),
        Some(CredentialSource::InstanceMetadata)
    );
}

/// Asserts that the credentials of a setting are those of its source (its `Debug` names it), from
/// the environment of the process.
#[test]
fn the_credentials_of_a_setting_are_of_its_source() {
    let setting = SourceSetting {
        source: CredentialSource::InstanceMetadata,
        profile: None,
    };
    assert_eq!(
        format!("{:?}", setting.credentials()),
        "Credentials { source: InstanceMetadata }"
    );
}

/// Asserts that the profile of the setting is the one the source reads, in place of the one the
/// environment names: a store whose setting names the profile `work` signs with the keys of
/// `work`, which the store requires, and not with those of the profile `AWS_PROFILE` names.
#[tokio::test]
async fn the_profile_of_the_setting_is_the_one_read() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-credential-profile");
    let credentials = dir.path().join("credentials");
    std::fs::write(
        &credentials,
        "[other]\naws_access_key_id = OTHERID\naws_secret_access_key = other-secret\n\
         [work]\naws_access_key_id = WORKID\naws_secret_access_key = work-secret\n",
    )
    .unwrap();
    let config = dir.path().join("config");
    let vars = [
        ("AWS_PROFILE", "other".to_owned()),
        (
            "AWS_SHARED_CREDENTIALS_FILE",
            credentials.display().to_string(),
        ),
        ("AWS_CONFIG_FILE", config.display().to_string()),
    ];
    let env = || AwsEnv::from_vars(vars.iter().map(|(k, v)| ((*k).to_owned(), v.clone())));
    let s3 = FakeS3::start().await;
    s3.require_credentials("WORKID", "work-secret");
    let key = ObjectKey::new("k").unwrap();
    for (profile, stored) in [(Some("work"), true), (None, false)] {
        let setting = SourceSetting {
            source: CredentialSource::Profile,
            profile: profile.map(str::to_owned),
        };
        let mut store = s3.config(Capabilities::default());
        store.credentials = setting.credentials_in(env());
        let store = S3Store::new(store).unwrap();
        let put = store.put_new(&key, Bytes::from_static(b"body"), None).await;
        assert_eq!(put.is_ok(), stored, "{profile:?}: {put:?}");
    }
}
