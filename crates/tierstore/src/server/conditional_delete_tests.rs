//! The `conditional_delete` setting of a tier: its default for each kind of store, its refusal
//! for the kinds that cannot take `false`, and that it decides whether a conditional delete is
//! sent as a DELETE with `If-Match` (the fence) or as a read of the tag before a plain DELETE.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use bytes::Bytes;
use clap::Parser;
use objstore_s3::s3::fake::FakeS3;
use tierstore_testkit::dir::TestDir;

use super::cli::{Cli, S3Flags, build, exit_status, open_tier};
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

/// The flags of a start with the key file `key` and these two tiers, then `more`.
fn flags(key: &str, hot: &str, cold: &str, more: &[String]) -> Vec<String> {
    let mut args: Vec<String> = [
        "tierstore",
        "--listen=127.0.0.1:0",
        "--proxy-id=p1",
        "--hot-bytes=1000",
        "--bucket=tier-bucket",
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend([
        format!("--hot={hot}"),
        format!("--cold={cold}"),
        format!("--access-key-file={key}"),
    ]);
    args.extend(more.iter().cloned());
    args
}

/// Asserts the same refusal for the flags, in full and as flags that cannot go together (exit
/// status 2, which a restart cannot mend): `false` for a memory tier or a dir tier, either tier,
/// is refused naming the flag; `true` and no value start them.
#[test]
fn the_flag_false_is_refused_for_memory_and_dir_with_status_2_and_true_is_not() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-delete-flag");
    let key = dir.path().join("key");
    std::fs::write(&key, "AKID\nsecret\n").unwrap();
    std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
    let key = key.to_str().unwrap();
    let on_disk = format!("dir:{}", dir.path().join("tier").display());
    for spec in ["memory", on_disk.as_str()] {
        for flag in ["--hot", "--cold"] {
            let (hot, cold) = if flag == "--hot" {
                (spec, "memory")
            } else {
                ("memory", spec)
            };
            let off = vec![format!("{flag}-conditional-delete=false")];
            let cli = Cli::try_parse_from(flags(key, hot, cold, &off)).unwrap();
            let why = build(&cli).err().expect("false is refused");
            let want = format!(
                "{flag} {spec:?}: {flag}-conditional-delete false is only for an `s3:<bucket>` tier: a `memory` or `dir:<path>` tier deletes conditionally in one atomic step"
            );
            assert_eq!(why.to_string(), want);
            assert_eq!(exit_status(&why), 2);
            let on = vec![format!("{flag}-conditional-delete=true")];
            let cli = Cli::try_parse_from(flags(key, hot, cold, &on)).unwrap();
            assert!(build(&cli).is_ok(), "{flag} {spec} true");
            let cli = Cli::try_parse_from(flags(key, hot, cold, &[])).unwrap();
            assert!(build(&cli).is_ok(), "{flag} {spec}");
        }
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
