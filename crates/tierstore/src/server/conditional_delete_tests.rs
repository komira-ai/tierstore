//! The `conditional_delete` setting of a tier: its default for each kind of store, its refusal
//! for the kinds that cannot take `false`, and that it decides whether a conditional delete is
//! sent as a DELETE with `If-Match` (the fence) or as a read of the tag before a plain DELETE.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use bytes::Bytes;
use objstore_s3::s3::fake::FakeS3;
use tierstore_testkit::dir::TestDir;

use super::cli::{S3Flags, open_tier};
use super::config::from_text;
use super::config_testing::{DEFAULTS, GOLDEN, MINIMAL};
use crate::Error;

/// The message of the refusal of this file text.
fn refusal(text: &str) -> String {
    from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// `MINIMAL` with `from` replaced by `to`.
fn minimal_with(from: &str, to: &str) -> String {
    assert!(MINIMAL.contains(from), "{from}");
    MINIMAL.replacen(from, to, 1)
}

/// The refusal of `false` for a tier that is not an S3 bucket, as the file words it for `key`.
fn off_refused(key: &str) -> String {
    format!(
        "{key}.conditional_delete: false is only for kind \"s3\": a memory or dir tier deletes conditionally in one atomic step"
    )
}

/// Asserts that a file that leaves the key out makes settings with no value for either tier (the
/// default of the kind is taken where the tier is opened), and that the key is read as given for
/// the S3 tiers of the golden file: `true` for both, and `false` for the hot tier alone.
#[test]
fn a_file_without_the_key_leaves_the_default_to_the_kind_and_a_file_with_it_gives_it() {
    let least = from_text(MINIMAL, DEFAULTS).unwrap();
    assert_eq!(least.hot_conditional_delete, None);
    assert_eq!(least.cold_conditional_delete, None);

    let both = from_text(GOLDEN, DEFAULTS).unwrap();
    assert_eq!(both.hot_conditional_delete, Some(true));
    assert_eq!(both.cold_conditional_delete, Some(true));

    let off = GOLDEN.replacen("conditional_delete = true", "conditional_delete = false", 1);
    let off = from_text(&off, DEFAULTS).unwrap();
    assert_eq!(off.hot_conditional_delete, Some(false));
    assert_eq!(off.cold_conditional_delete, Some(true));
}

/// Asserts that `conditional_delete = true` is accepted for a memory tier and a dir tier (it is
/// their default), and that `false` is refused for each of them, in either table, with the key
/// named and the reason said, in full.
#[test]
fn true_is_the_default_of_memory_and_dir_and_false_is_refused_for_them() {
    let kinds = ["kind = \"memory\"", "kind = \"dir\"\npath = \"/tiers/x\""];
    for kind in kinds {
        let on = format!("{kind}\nconditional_delete = true");
        let hot = minimal_with("kind   = \"memory\"", &on);
        assert_eq!(
            from_text(&hot, DEFAULTS).unwrap().hot_conditional_delete,
            Some(true)
        );
        let cold = minimal_with(
            "[cold_store]\nkind = \"memory\"",
            &format!("[cold_store]\n{on}"),
        );
        assert_eq!(
            from_text(&cold, DEFAULTS).unwrap().cold_conditional_delete,
            Some(true)
        );

        let off = format!("{kind}\nconditional_delete = false");
        let hot = minimal_with("kind   = \"memory\"", &off);
        assert_eq!(refusal(&hot), off_refused("hot_store"), "{kind}");
        let cold = minimal_with(
            "[cold_store]\nkind = \"memory\"",
            &format!("[cold_store]\n{off}"),
        );
        assert_eq!(refusal(&cold), off_refused("cold_store"), "{kind}");
    }
}

/// Asserts the same refusal for the flags, in full: `false` for a memory tier or a dir tier is
/// refused naming the flag, `true` and no value are accepted for them, and an S3 tier is not
/// refused for either value.
#[test]
fn the_flag_false_is_refused_for_memory_and_dir_and_true_is_not() {
    let timeout = Duration::from_secs(5);
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-delete-flag");
    let spec = format!("dir:{}", dir.path().join("tier").display());
    let flags = |conditional_delete| S3Flags {
        endpoint: None,
        region: None,
        credentials: None,
        conditional_delete,
    };
    for spec in ["memory", spec.as_str()] {
        let refused = open_tier("--hot", spec, &flags(Some(false)), timeout);
        let want = format!(
            "--hot {spec:?}: --hot-conditional-delete false is only for an `s3:<bucket>` tier: a `memory` or `dir:<path>` tier deletes conditionally in one atomic step"
        );
        assert_eq!(refused.err(), Some(want));
        assert!(open_tier("--hot", spec, &flags(Some(true)), timeout).is_ok());
        assert!(open_tier("--hot", spec, &flags(None), timeout).is_ok());
    }
}

/// The methods of the requests the service took after the first `from`.
fn methods(s3: &FakeS3, from: usize) -> Vec<String> {
    let lines = s3.requests();
    let sent = lines[from..].iter();
    sent.map(|line| line.split(' ').next().unwrap_or("").to_owned())
        .collect()
}

/// Asserts that the setting reaches the S3 backend an S3 tier opens as: with no value (the
/// default of an S3 store) and with `false`, a conditional delete is a HEAD and a DELETE and a
/// wrong tag only a HEAD; with `true` it is one DELETE and no HEAD, the fence of the store. Every
/// object is deleted on its own tag, and the service is asked nothing at the opening.
#[tokio::test]
async fn the_setting_decides_whether_an_s3_tier_sends_the_delete_with_if_match() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-delete-s3");
    let credentials = dir.path().join("credentials");
    std::fs::write(&credentials, "test-key\ntest-secret\n").unwrap();
    std::fs::set_permissions(&credentials, Permissions::from_mode(0o600)).unwrap();
    let cases = [
        (None, vec!["HEAD", "DELETE"]),
        (Some(false), vec!["HEAD", "DELETE"]),
        (Some(true), vec!["DELETE"]),
    ];
    for (setting, sent) in cases {
        let s3 = FakeS3::start().await;
        let flags = S3Flags {
            endpoint: Some(s3.endpoint()),
            region: None,
            credentials: Some(credentials.as_path()),
            conditional_delete: setting,
        };
        let opened = open_tier("--cold", "s3:tier-test", &flags, Duration::from_secs(30));
        let tier = opened.unwrap();
        assert!(s3.requests().is_empty(), "{setting:?}");
        tier.put("k", Bytes::from_static(b"v")).await.unwrap();
        let tag = tier.head("k").await.unwrap().tag;

        let before = s3.requests().len();
        let wrong = tier.delete_if_match("k", "not-the-tag").await;
        assert!(wrong.is_err(), "{setting:?}");
        assert_eq!(s3.keys(), ["k"], "{setting:?}");
        // A HEAD where the tag is read first, the one refused DELETE where the fence is used.
        assert_eq!(methods(&s3, before).len(), 1, "{setting:?}");

        let before = s3.requests().len();
        tier.delete_if_match("k", &tag).await.unwrap();
        assert_eq!(methods(&s3, before), sent, "{setting:?}");
        assert!(s3.keys().is_empty(), "{setting:?}");
        let gone = tier.delete_if_match("k", &tag).await;
        assert!(matches!(gone, Err(Error::NotFound(_))), "{setting:?}");
    }
}
