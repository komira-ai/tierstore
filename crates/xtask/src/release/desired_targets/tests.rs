use super::*;

const RELEASE: Release<'static> = Release {
    repo: "example/tierstore",
    tag: "v0.4.0",
    version: "0.4.0+0123456789ab",
};

fn archive(platform: Platform, digit: char) -> Archive {
    Archive {
        platform,
        digest: digit.to_string().repeat(64),
        bytes: 7_413_041,
        bundle_digest: "f".repeat(64),
        bundle_bytes: 10_732,
        min_hostkeeper: "0.1.0".to_owned(),
    }
}

#[test]
fn one_platform_is_one_hostkeeper_archive_entry_in_proto3_json() {
    // Catches: a field named as in Rust or the proto rather than proto3 JSON (min_hostkeeper,
    // which the coordinator would not read), a size written as a number, the channel location
    // not first, a bundle at a path other than the archive's with `.sigstore.json`, and a GitHub
    // location of another release than the tag.
    let x86 = "tierstore-0.4.0+0123456789ab-linux-x86_64.tar.zst";
    let channel = format!("tierstore/0.4.0+0123456789ab/{x86}");
    let github = format!("example/tierstore/releases/download/v0.4.0/{x86}");
    let a = "a".repeat(64);
    let f = "f".repeat(64);
    assert_eq!(
        targets(RELEASE, &[archive(Platform::LinuxX86_64, 'a')]).expect("targets"),
        format!(
            "{{\"linux-x86_64\":{{\"entries\":[{{\"attestation\":{{\"bytes\":\"10732\",\"digest\":\"{f}\",\"locations\":[{{\"locator\":\"{channel}.sigstore.json\",\"store\":\"channel\"}},{{\"locator\":\"{github}.sigstore.json\",\"store\":\"github\"}}]}},\"bytes\":\"7413041\",\"digest\":\"{a}\",\"installer\":\"hostkeeper-archive\",\"locations\":[{{\"locator\":\"{channel}\",\"store\":\"channel\"}},{{\"locator\":\"{github}\",\"store\":\"github\"}}],\"manifest\":{{\"minHostkeeper\":\"0.1.0\"}}}}]}}}}"
        )
    );
}

#[test]
fn every_platform_has_a_list_of_its_own_archive_and_no_other() {
    // Catches: one platform's archive in another's list (a Mac ordered a Linux archive), a
    // platform dropped, and a list of more than the one entry there is today.
    let all: Vec<Archive> = Platform::ALL
        .iter()
        .zip(['1', '2', '3'])
        .map(|(p, d)| archive(*p, d))
        .collect();
    let json: serde_json::Value =
        serde_json::from_str(&targets(RELEASE, &all).expect("targets")).expect("JSON");
    let keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
    assert_eq!(keys, ["linux-arm64", "linux-x86_64", "macos-arm64"]);
    for (platform, digit) in Platform::ALL.iter().zip(['1', '2', '3']) {
        let entries = json[platform.name()]["entries"]
            .as_array()
            .expect("entries");
        assert_eq!(entries.len(), 1, "{platform:?}");
        assert_eq!(entries[0]["digest"], digit.to_string().repeat(64));
        assert_eq!(entries[0]["installer"], ARCHIVE_INSTALLER);
        let locator = entries[0]["locations"][0]["locator"]
            .as_str()
            .expect("a locator");
        assert!(locator.ends_with(&format!("-{}.tar.zst", platform.name())));
    }
    assert_eq!(targets(RELEASE, &[]).expect("targets"), "{}");
}

#[test]
fn a_version_hostkeeper_does_not_take_is_refused() {
    // Catches: locators written for a version no host could be ordered.
    let bad = Release {
        version: "0.4.0 +x",
        ..RELEASE
    };
    assert!(targets(bad, &[archive(Platform::LinuxArm64, 'a')]).is_err());
}
