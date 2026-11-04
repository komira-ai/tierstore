//! The settings, servers, tiers and key files made of the flags.

use super::*;

/// Asserts that percents are shares of a whole.
#[test]
fn a_percent_is_a_share() {
    assert_eq!(share(80), 0.8);
    assert_eq!(share(95), 0.95);
    assert_eq!(share(100), 1.0);
}

/// Asserts that the settings are made from the flags: the key file's key and secret, the
/// read-only keys of their files (one in each form) in the order of the flags, the bucket and
/// region, the limits, the water marks as shares, the id of the proxy as the holder of the
/// lease, and the durations, the lengths of the two parts of the drain (given values that are not
/// the defaults) among them.
#[test]
fn the_settings_are_made_from_the_flags() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-build");
    let mut cli = cli(&dir);
    cli.region = "eu-west-1".to_owned();
    cli.drain_lb_wait = Duration::from_secs(7);
    cli.drain_deadline = Duration::from_secs(11);
    cli.put_permit_wait = Duration::from_secs(4);
    let lines = key_file(&dir, "reader-lines", "READ1\nread-secret-1\n", 0o600);
    let json = key_file(&dir, "reader-json", JSON_KEY, 0o400);
    cli.read_only_key_file = vec![lines, json];
    let built = build(&cli).expect("built");
    let readers = [
        AccessKey::new("READ1", "read-secret-1"),
        AccessKey::new("JSONID", "json-secret"),
    ];
    assert_eq!(built.server.read_only_keys, readers);
    assert_eq!(built.server.bucket, "tier-bucket");
    assert_eq!(built.server.region, "eu-west-1");
    assert_eq!(built.server.access_key_id, "AKID");
    assert_eq!(built.server.secret_access_key, "secret");
    assert_eq!(built.server.max_request_bytes, 256 << 20);
    assert_eq!(built.server.max_concurrent_puts, 32);
    assert_eq!(built.server.put_permit_wait, Duration::from_secs(4));
    assert_eq!(built.server.put_min_bytes_per_sec, 1024);
    assert_eq!(built.server.put_rate_grace, Duration::from_secs(30));
    assert_eq!(built.server.body_idle_timeout, Duration::from_secs(30));
    assert_eq!(built.server.drain_lb_wait, Duration::from_secs(7));
    assert_eq!(built.server.drain_deadline, Duration::from_secs(11));
    assert_eq!(built.server.usage_stale_after, Duration::from_secs(120));
    assert_eq!(built.proxy.proxy_id, "p1");
    assert_eq!(built.proxy.hot_budget, 1000);
    assert_eq!(built.proxy.critical, 0.95);
    assert_eq!(built.proxy.linger, Duration::from_millis(20));
    assert_eq!(built.reconciler.holder, "p1");
    assert_eq!(built.reconciler.hot_budget, 1000);
    assert_eq!(built.reconciler.high_water, 0.8);
    assert_eq!(built.reconciler.low_water, 0.7);
    assert_eq!(built.reconciler.parallel, 16);
    assert_eq!(built.reconciler.lease_ttl, Duration::from_secs(30));
    assert_eq!(built.reconciler.retention, Duration::from_secs(24 * 3600));
}

/// Asserts that a file whose `[server] bucket` is the name of a probe path, `metricz` among them,
/// is refused as the flag is, naming the probe path. It would catch a file that reaches the
/// server with a bucket the probe answers for.
#[test]
fn a_file_with_a_probe_path_as_the_bucket_is_refused() {
    use crate::server::config::from_text;
    use crate::server::config_testing::{DEFAULTS, MINIMAL};
    for name in ["healthz", "readyz", "usagez", "metricz"] {
        let text = MINIMAL.replacen("tier-bucket", name, 1);
        let cli = from_text(&text, DEFAULTS).unwrap();
        assert_eq!(cli.bucket, name);
        let why = build(&cli).err().expect("refused").to_string();
        assert!(why.contains("probe path"), "{name}: {why}");
    }
}

/// Asserts that flags that do not fit together are refused with a message that names them: the
/// water marks out of order, a reconcile interval that is not shorter than the lease, a bucket
/// called like a probe path, an idle timeout of zero, a request timeout of zero for the S3
/// tiers, a tier that cannot be opened, a lock file that is not an absolute path to a file, and a
/// key file, of the full key or of a read-only one, that cannot be used. None of them is `InvalidFlags`, the refusal that exits with status 2.
#[test]
fn flags_that_do_not_fit_together_are_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-refused");
    let refused = |change: &dyn Fn(&mut Cli)| {
        let mut cli = cli(&dir);
        change(&mut cli);
        let why = build(&cli).err().expect("refused");
        assert!(why.downcast_ref::<InvalidFlags>().is_none(), "{why}");
        why.to_string()
    };
    assert!(refused(&|cli| cli.low_water = 80).contains("--low-water"));
    assert!(refused(&|cli| cli.high_water = 96).contains("--high-water"));
    let interval = refused(&|cli| cli.reconcile_interval = cli.lease_ttl);
    assert!(interval.contains("--reconcile-interval"), "{interval}");
    let stale = refused(&|cli| cli.usage_stale_after = cli.lease_ttl);
    assert!(stale.contains("--usage-stale-after"), "{stale}");
    assert!(refused(&|cli| cli.bucket = "readyz".to_owned()).contains("probe path"));
    assert!(refused(&|cli| cli.bucket = "healthz".to_owned()).contains("probe path"));
    assert!(refused(&|cli| cli.bucket = "usagez".to_owned()).contains("probe path"));
    assert!(refused(&|cli| cli.bucket = "metricz".to_owned()).contains("probe path"));
    let idle = refused(&|cli| cli.body_idle_timeout = Duration::ZERO);
    assert!(idle.contains("--body-idle-timeout"), "{idle}");
    let wait = refused(&|cli| cli.s3_request_timeout = Duration::ZERO);
    assert!(wait.contains("--s3-request-timeout"), "{wait}");
    let hot = refused(&|cli| cli.hot = "s3://hot".to_owned());
    assert!(hot.contains("--hot"), "{hot}");
    let cold = refused(&|cli| cli.cold = "/var/cold".to_owned());
    assert!(cold.contains("--cold"), "{cold}");
    let relative = refused(&|cli| cli.lock_file = Some(PathBuf::from("instance.lock")));
    assert!(relative.contains("--lock-file"), "{relative}");
    assert!(relative.contains("must be an absolute path"), "{relative}");
    let folder = refused(&|cli| cli.lock_file = Some(PathBuf::from("/")));
    assert!(folder.contains("--lock-file"), "{folder}");
    assert!(folder.contains("must name a file"), "{folder}");
    let missing = dir.path().join("missing");
    let key = refused(&|cli| cli.access_key_file = missing.clone());
    assert!(key.contains("--access-key-file"), "{key}");
    let reader = refused(&|cli| cli.read_only_key_file = vec![missing.clone()]);
    assert!(reader.contains("--read-only-key-file"), "{reader}");
    assert!(reader.contains("missing"), "{reader}");
}

/// Asserts that two keys with one id are refused with a message that names the id and the two
/// flags and not the secrets, and as flags that cannot go together (exit status 2): a read-only
/// key with the id of the full key, and two read-only keys with one id. The start is refused
/// before a tier is opened: a directory tier is not made.
#[test]
fn two_keys_with_one_id_are_invalid_flags_refused_before_a_tier_is_opened() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-twice");
    let same = key_file(&dir, "same", "AKID\nanother-secret\n", 0o600);
    let reader = key_file(&dir, "reader", "READ1\nread-secret-1\n", 0o600);
    let again = key_file(&dir, "again", JSON_KEY, 0o600);
    let json_twice = key_file(&dir, "twice", JSON_KEY, 0o600);
    let made = dir.path().join("made");
    let cases = [
        vec![same],
        vec![reader.clone(), again, json_twice],
        vec![reader.clone(), reader],
    ];
    for files in cases {
        let mut cli = cli(&dir);
        cli.hot = format!("dir:{}", made.display());
        cli.read_only_key_file = files;
        let why = build(&cli).err().expect("refused");
        assert!(why.downcast_ref::<InvalidFlags>().is_some(), "{why}");
        let text = why.to_string();
        assert!(text.contains("used twice"), "{text}");
        assert!(text.contains("--access-key-file"), "{text}");
        assert!(text.contains("--read-only-key-file"), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert!(!made.exists());
    }
}

/// Asserts that `--scoped-key-file` puts the keys of its files on the server, in the order of the
/// flags; that a file that is not a valid scoped key is refused with a message that names the flag
/// (as a bad key file is, not as invalid flags); and that a scoped key whose id is the full
/// key's, a read-only key's, a GC key's or another scoped key's is invalid flags (status 2).
#[test]
fn scoped_key_files_are_built_into_the_server_and_their_ids_must_differ() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-scoped-flag");
    let text = |id: &str| {
        format!(
            "{{\"access_key_id\": \"{id}\", \"secret_access_key\": \"s3cret\", \
             \"read_prefixes\": [\"cas/\"], \"write_prefixes\": [\"cas/f/a/\"], \
             \"delete_prefixes\": []}}"
        )
    };
    let one = key_file(&dir, "one", &text("SC1"), 0o600);
    let two = key_file(&dir, "two", &text("SC2"), 0o600);
    let mut ok = cli(&dir);
    ok.scoped_key_file = vec![one.clone(), two.clone()];
    let built = build(&ok).expect("built");
    let ids: Vec<&str> = built
        .server
        .scoped_keys
        .iter()
        .map(|key| key.id())
        .collect();
    assert_eq!(ids, ["SC1", "SC2"]);
    assert_eq!(built.server.scoped_keys[0].write_prefixes(), ["cas/f/a/"]);

    let empty = key_file(
        &dir,
        "empty",
        &text("SC3")
            .replace("[\"cas/\"]", "[]")
            .replace("[\"cas/f/a/\"]", "[]"),
        0o600,
    );
    let mut bad = cli(&dir);
    bad.scoped_key_file = vec![empty];
    let why = build(&bad).err().expect("refused");
    assert!(why.to_string().starts_with("--scoped-key-file "), "{why}");
    assert!(why.downcast_ref::<InvalidFlags>().is_none(), "{why}");

    let full = key_file(&dir, "full", &text("AKID"), 0o600);
    let reader = key_file(&dir, "reader", "SC1\nread-secret\n", 0o600);
    let gc = key_file(
        &dir,
        "gc",
        r#"{"access_key_id": "SC1", "secret_access_key": "g", "prefix": "cas/b/"}"#,
        0o600,
    );
    for (scoped, setup) in [
        (vec![full], 0),
        (vec![one.clone()], 1),
        (vec![one.clone()], 2),
        (vec![one.clone(), one], 0),
    ] {
        let mut cli = cli(&dir);
        cli.scoped_key_file = scoped;
        match setup {
            1 => cli.read_only_key_file = vec![reader.clone()],
            2 => cli.gc_key_file = vec![gc.clone()],
            _ => {}
        }
        let why = build(&cli).err().expect("refused");
        assert!(why.downcast_ref::<InvalidFlags>().is_some(), "{why}");
        let text = why.to_string();
        assert!(text.contains("used twice"), "{text}");
        assert!(!text.contains("s3cret"), "{text}");
    }
}

/// Asserts that `--grant-key-file` puts the keys of its files on the server, in the order of the
/// flags; that a bad file is refused with a message that names the flag (not as invalid flags);
/// and that a grant key whose id is the full key's, a read-only key's, a scoped key's or another
/// grant key's is invalid flags (status 2).
#[test]
fn grant_key_files_are_built_into_the_server_and_their_ids_must_differ() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-grant-flag");
    const GRANT_SECRET: &str = "grant-secret-0123456789-abcdefghij";
    let text = |id: &str| {
        format!("{{\"access_key_id\": \"{id}\", \"secret_access_key\": \"{GRANT_SECRET}\"}}")
    };
    let one = key_file(&dir, "one", &text("GR1"), 0o600);
    let two = key_file(&dir, "two", &text("GR2"), 0o600);
    let mut ok = cli(&dir);
    ok.grant_key_file = vec![one.clone(), two];
    ok.grant_max_lifetime = Duration::from_secs(300);
    let built = build(&ok).expect("built");
    assert_eq!(built.server.grant_max_lifetime, Duration::from_secs(300));
    let keys = &built.server.grant_keys;
    let ids: Vec<&str> = keys.iter().map(|key| key.id.as_str()).collect();
    assert_eq!(ids, ["GR1", "GR2"]);

    let empty = key_file(&dir, "empty", "", 0o600);
    let mut bad = cli(&dir);
    bad.grant_key_file = vec![empty];
    let why = build(&bad).err().expect("refused");
    assert!(why.to_string().starts_with("--grant-key-file "), "{why}");
    assert!(why.downcast_ref::<InvalidFlags>().is_none(), "{why}");

    let full = key_file(&dir, "full", &text("AKID"), 0o600);
    let reader = key_file(&dir, "reader", "GR1\nread-secret\n", 0o600);
    let scoped = key_file(
        &dir,
        "scoped",
        r#"{"access_key_id": "GR1", "secret_access_key": "g", "read_prefixes": ["cas/"], "write_prefixes": [], "delete_prefixes": []}"#,
        0o600,
    );
    for (grants, setup) in [
        (vec![full], 0),
        (vec![one.clone()], 1),
        (vec![one.clone()], 2),
        (vec![one.clone(), one], 0),
    ] {
        let mut cli = cli(&dir);
        cli.grant_key_file = grants;
        match setup {
            1 => cli.read_only_key_file = vec![reader.clone()],
            2 => cli.scoped_key_file = vec![scoped.clone()],
            _ => {}
        }
        let why = build(&cli).err().expect("refused");
        assert!(why.downcast_ref::<InvalidFlags>().is_some(), "{why}");
        let text = why.to_string();
        assert!(text.contains("used twice"), "{text}");
        assert!(!text.contains(GRANT_SECRET), "{text}");
    }
}

/// Asserts that a memory tier and a directory tier open, the directory being made, and that any
/// other kind, and a directory that has no name or cannot be made, is refused with the flag's
/// name and the kind it was given.
#[test]
fn only_a_memory_or_a_directory_tier_opens() {
    assert!(tier("--hot", "memory").is_ok());
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-tiers");
    let made = dir.path().join("made/here");
    assert!(tier("--hot", &format!("dir:{}", made.display())).is_ok());
    assert!(made.join(".uploads").is_dir());
    let why = tier("--cold", "s3://bucket").err().unwrap();
    assert!(why.contains("--cold"), "{why}");
    assert!(why.contains("s3://bucket"), "{why}");
    let unnamed = tier("--cold", "dir:").err().unwrap();
    assert!(unnamed.contains("--cold"), "{unnamed}");
    let file = key_file(&dir, "file", "x", 0o600);
    let under = format!("dir:{}/inside", file.display());
    let blocked = tier("--hot", &under).err().unwrap();
    assert!(blocked.contains("--hot"), "{blocked}");
}

/// Asserts that a tier `s3:<bucket>` is built from the flags of its tier, each tier from its
/// own: the settings are made without a request, the region is not needed, and the two tiers
/// have a key file of their own, the hot one in the JSON form and the cold one in the two-line
/// form, and then the other way round.
#[test]
fn an_s3_tier_is_built_from_the_flags_of_its_tier() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-s3-tiers");
    let hot_key = key_file(&dir, "hot-key", JSON_KEY, 0o600);
    let cold_key = key_file(&dir, "cold-key", "COLDID\ncold-secret\n", 0o600);
    let mut cli = cli(&dir);
    cli.hot = "s3:hot-bucket".to_owned();
    cli.hot_endpoint = Some("http://127.0.0.1:9000".to_owned());
    cli.hot_credentials_file = Some(hot_key.clone());
    cli.cold = "s3:cold-bucket".to_owned();
    cli.cold_endpoint = Some("http://127.0.0.1:9001".to_owned());
    cli.cold_region = Some("eu-west-1".to_owned());
    cli.cold_credentials_file = Some(cold_key.clone());
    build(&cli).expect("built");
    cli.hot_credentials_file = Some(cold_key);
    cli.cold_credentials_file = Some(hot_key);
    build(&cli).expect("built the other way round");
}

/// Asserts that an S3 tier that its flags do not describe is refused with a message that names
/// the flag: no endpoint, no credentials file, a file that is missing, that others can read or
/// that lacks a line, an endpoint that is not valid (TLS is not built), and a bucket name that
/// is not one; and that the flags of an S3 tier are refused for a tier of another kind.
#[test]
fn an_s3_tier_that_its_flags_do_not_describe_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-s3-refused");
    let good = key_file(&dir, "good", "ID\nsecret\n", 0o600);
    let open = key_file(&dir, "open", "ID\nsecret\n", 0o644);
    let incomplete = key_file(&dir, "incomplete", "ID\n", 0o600);
    let typo = r#"{"access_key_id": "ID", "secret_acess_key": "secret"}"#;
    let misspelled = key_file(&dir, "misspelled", typo, 0o600);
    let missing = dir.path().join("missing");
    let http = Some("http://127.0.0.1:9000");
    let timeout = Duration::from_secs(5);
    let refused = |spec: &str, endpoint: Option<&str>, credentials: Option<&Path>| {
        let flags = S3Flags {
            endpoint,
            region: None,
            credentials,
            conditional_delete: None,
        };
        let opened = open_tier("--cold", spec, &flags, timeout);
        opened.err().expect("refused")
    };
    let no_endpoint = refused("s3:bucket", None, Some(&good));
    assert!(no_endpoint.contains("--cold-endpoint"), "{no_endpoint}");
    assert!(no_endpoint.contains("s3:bucket"), "{no_endpoint}");
    let no_key = refused("s3:bucket", http, None);
    assert!(no_key.contains("--cold-credentials-file"), "{no_key}");
    let gone = refused("s3:bucket", http, Some(&missing));
    assert!(gone.contains("--cold-credentials-file"), "{gone}");
    assert!(gone.contains("missing"), "{gone}");
    let others = refused("s3:bucket", http, Some(&open));
    assert!(others.contains("others can read it"), "{others}");
    let short = refused("s3:bucket", http, Some(&incomplete));
    assert!(short.contains("on the second"), "{short}");
    let typed = refused("s3:bucket", http, Some(&misspelled));
    assert!(typed.contains("--cold-credentials-file"), "{typed}");
    assert!(typed.contains("access_key_id"), "{typed}");
    let tls = refused("s3:bucket", Some("https://127.0.0.1:9000"), Some(&good));
    assert!(tls.contains("TLS"), "{tls}");
    assert!(tls.contains("--cold"), "{tls}");
    let name = refused("s3:Not_A_Bucket", http, Some(&good));
    assert!(name.contains("bucket name"), "{name}");

    let given = [
        (http, None, None),
        (None, Some("eu-west-1"), None),
        (None, None, Some(good.as_path())),
    ];
    for (endpoint, region, credentials) in given {
        let flags = S3Flags {
            endpoint,
            region,
            credentials,
            conditional_delete: None,
        };
        let opened = open_tier("--hot", "memory", &flags, timeout);
        let why = opened.err().unwrap();
        assert!(why.contains("--hot"), "{why}");
        assert!(why.contains("s3:<bucket>"), "{why}");
    }
    let none = S3Flags {
        endpoint: None,
        region: None,
        credentials: None,
        conditional_delete: None,
    };
    assert!(open_tier("--hot", "memory", &none, timeout).is_ok());
    let unknown = open_tier("--hot", "tape", &none, timeout).err().unwrap();
    assert!(unknown.contains("s3:<bucket>"), "{unknown}");
}

/// Asserts that the key file is read as an id on the first line and a secret on the second,
/// trimmed, with either line ending, or as JSON; that others may not read it; and that a file
/// with a missing or empty line, a missing file and a directory are refused with the flag's
/// name.
#[test]
fn the_key_file_is_an_id_and_a_secret_that_only_its_owner_reads() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-key");
    let want = Ok(("AKID".to_owned(), "secret".to_owned()));
    let spaced = key_file(&dir, "spaced", "  AKID \n secret \n", 0o600);
    assert_eq!(read_key(&spaced), want);
    let json = r#"{"access_key_id": "AKID", "secret_access_key": "secret"}"#;
    let from_json = key_file(&dir, "json", json, 0o600);
    assert_eq!(read_key(&from_json), want);
    let crlf = key_file(&dir, "crlf", "AKID\r\nsecret\r\n", 0o600);
    assert_eq!(read_key(&crlf), want);
    let open = key_file(&dir, "open", "AKID\nsecret\n", 0o644);
    let why = read_key(&open).unwrap_err();
    assert!(why.contains("others can read it"), "{why}");
    assert!(why.contains("open"), "{why}");
    let incomplete = [
        ("one", "AKID\n"),
        ("first", "\nsecret\n"),
        ("second", "AKID\n\n"),
    ];
    for (name, text) in incomplete {
        let path = key_file(&dir, name, text, 0o600);
        let why = read_key(&path).unwrap_err();
        assert!(why.contains("--access-key-file"), "{why}");
        assert!(why.contains("on the second"), "{why}");
    }
    let missing = read_key(&dir.path().join("missing")).unwrap_err();
    assert!(missing.contains("--access-key-file"), "{missing}");
    let folder = dir.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    std::fs::set_permissions(&folder, Permissions::from_mode(0o700)).unwrap();
    let unreadable = read_key(&folder).unwrap_err();
    assert!(unreadable.contains("--access-key-file"), "{unreadable}");
}
