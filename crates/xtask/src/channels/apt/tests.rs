use super::testing::*;
use super::*;

#[test]
fn a_failed_aws_call_is_told_by_what_it_says() {
    // Catches: a lost race read as a failure (or the reverse), and a missing key read as a denial.
    assert_eq!(classify(PRECONDITION), Failure::Precondition);
    assert_eq!(
        classify("An error occurred (ConditionalRequestConflict) when calling"),
        Failure::Precondition
    );
    assert_eq!(
        classify("a response (412) came back"),
        Failure::Precondition
    );
    assert_eq!(classify(NO_SUCH_KEY), Failure::NotFound);
    assert_eq!(classify("Not Found"), Failure::NotFound);
    assert_eq!(classify(DENIED), Failure::Denied);
    assert_eq!(classify("Forbidden"), Failure::Denied);
    assert_eq!(classify("could not connect"), Failure::Other);
}

#[test]
fn the_package_files_of_a_version_are_one_per_architecture_and_must_all_be_there() {
    // Catches: an index or upload for one architecture only, or for another version's files.
    let dir = scratch("package-files");
    release_files(&dir, "1.2.3");
    let files = package_files(&dir, "1.2.3").expect("files");
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["tierstore_1.2.3_amd64.deb", "tierstore_1.2.3_arm64.deb"]
    );
    assert_eq!(files[0].1, dir.join("tierstore_1.2.3_amd64.deb"));
    std::fs::remove_file(dir.join("tierstore_1.2.3_arm64.deb")).expect("remove");
    let e = package_files(&dir, "1.2.3").unwrap_err().to_string();
    assert!(e.contains("tierstore_1.2.3_arm64.deb is not in"), "{e}");
    assert!(package_files(&dir, "1.2.4").is_err());
    assert!(matches!(
        package_files(&dir, "v1.2.3"),
        Err(Error::Usage(_))
    ));
}

#[test]
fn a_write_verb_with_a_pool_half_given_is_a_usage_error() {
    // Catches: a pool of one half (debs without a version) read as a pool or as none; clap holds
    // the flags together on the command line, and the verb holds them again.
    let verb = |debs: Option<&str>, version: Option<&str>, no_pool: bool| Verb::Write {
        suite: Suite::Stable,
        bucket: "apt-bucket".to_owned(),
        distribution_id: "E1".to_owned(),
        hostname: "packages.example".to_owned(),
        next: PathBuf::from("next"),
        debs: debs.map(PathBuf::from),
        version: version.map(str::to_owned),
        no_pool,
        last: true,
        output: None,
        aws: PathBuf::from("aws"),
    };
    for v in [
        verb(Some("d"), None, false),
        verb(None, None, false),
        verb(Some("d"), Some("1.2.3"), true),
    ] {
        let e = run(v).unwrap_err();
        assert!(matches!(e, Error::Usage(_)), "{e}");
    }
}
