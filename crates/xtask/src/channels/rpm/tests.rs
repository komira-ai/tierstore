use super::testing::*;
use super::*;

const NOWHERE: &str = "/nonexistent/program";

#[test]
fn the_package_files_of_a_version_are_one_per_architecture_and_must_all_be_there() {
    // Catches: an index or upload for one architecture only, or for another version's files.
    let dir = scratch("package-files");
    release_files(&dir, "1.2.3");
    let files = package_files(&dir, "1.2.3").expect("files");
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "tierstore-1.2.3-1.aarch64.rpm",
            "tierstore-1.2.3-1.x86_64.rpm"
        ]
    );
    assert_eq!(files[0].1, dir.join("tierstore-1.2.3-1.aarch64.rpm"));
    std::fs::remove_file(dir.join("tierstore-1.2.3-1.x86_64.rpm")).expect("remove");
    let e = package_files(&dir, "1.2.3").unwrap_err().to_string();
    assert!(e.contains("tierstore-1.2.3-1.x86_64.rpm is not in"), "{e}");
    assert!(package_files(&dir, "1.2.4").is_err());
    assert!(matches!(
        package_files(&dir, "v1.2.3"),
        Err(Error::Usage(_))
    ));
}

fn write_verb(rpms: Option<&str>, version: Option<&str>, no_pool: bool) -> Verb {
    Verb::Repo {
        verb: RepoVerb::Write {
            suite: Suite::Stable,
            bucket: "rpm-bucket".to_owned(),
            distribution_id: "E1".to_owned(),
            hostname: "packages.example".to_owned(),
            next: PathBuf::from("next"),
            rpms: rpms.map(PathBuf::from),
            version: version.map(str::to_owned),
            no_pool,
            last: true,
            output: None,
            aws: PathBuf::from(NOWHERE),
            curl: PathBuf::from(NOWHERE),
        },
    }
}

#[test]
fn a_write_verb_with_a_pool_half_given_is_a_usage_error() {
    // Catches: a pool of one half (rpms without a version) read as a pool or as none; clap holds
    // the flags together on the command line, and the verb holds them again.
    for v in [
        write_verb(Some("d"), None, false),
        write_verb(None, None, false),
        write_verb(Some("d"), Some("1.2.3"), true),
    ] {
        let e = run(v).unwrap_err();
        assert!(matches!(e, Error::Usage(_)), "{e}");
    }
}

#[test]
fn every_verb_is_wired_to_its_implementation_and_reports_what_it_meets_first() {
    // Catches: a verb that is not dispatched, or dispatched to another one. The programs are not
    // there, so each ends where it first needs one; the others end at the first file they read.
    let dir = scratch("dispatch");
    let p = |name: &str| dir.join(name);
    let spawn = |v: Verb| run(v).unwrap_err().to_string();
    let e = spawn(Verb::Sign {
        rpms: p("dist"),
        out: p("out"),
        version: "1.2.3".to_owned(),
        sums: p("SHA256SUMS"),
        work: p("work"),
        fingerprint: FPR.to_owned(),
        key_file: p("key"),
        passphrase_file: p("pass"),
        signed_at: 1,
        gpg: PathBuf::from(NOWHERE),
        output: None,
    });
    assert!(e.contains("SHA256SUMS"), "{e}");
    release_files(&dir, "1.2.3");
    std::fs::write(p("key"), "k").expect("key");
    std::fs::write(p("pass"), "p").expect("pass");
    let e = spawn(Verb::Sign {
        rpms: dir.to_path_buf(),
        out: p("out"),
        version: "1.2.3".to_owned(),
        sums: p("SHA256SUMS"),
        work: p("work"),
        fingerprint: FPR.to_owned(),
        key_file: p("key"),
        passphrase_file: p("pass"),
        signed_at: 1,
        gpg: PathBuf::from(NOWHERE),
        output: None,
    });
    assert!(e.contains(NOWHERE), "{e}");
    assert!(!p("key").exists());
    let e = spawn(Verb::CheckSigned {
        dir: dir.to_path_buf(),
        version: "1.2.3".to_owned(),
        sums: p("SHA256SUMS"),
        signed_sums: None,
        fingerprint: None,
    });
    assert!(e.contains("it is not signed"), "{e}");
    let e = spawn(Verb::Promote {
        testing: p("t"),
        stable: p("s"),
        out: p("o"),
        version: "1.2.3".to_owned(),
        sums: p("SHA256SUMS"),
        signed_sums: p("none"),
        date: 1,
        keep: 25,
    });
    assert!(e.contains("etags.txt is missing"), "{e}");
    let e = spawn(Verb::Repo {
        verb: RepoVerb::Read {
            suite: Suite::Testing,
            bucket: "rpm-bucket".to_owned(),
            out: p("read"),
            aws: PathBuf::from(NOWHERE),
        },
    });
    assert!(e.contains(NOWHERE), "{e}");
    let e = spawn(Verb::Repo {
        verb: RepoVerb::Index {
            suite: Suite::Testing,
            current: p("read"),
            out: p("next"),
            rpms: p("dist"),
            version: "1.2.3".to_owned(),
            date: 1,
            keep: 25,
        },
    });
    assert!(e.contains("rpm repo read"), "{e}");
    std::fs::create_dir_all(p("next/testing")).expect("dir");
    std::fs::write(p("key"), "k").expect("key");
    let e = spawn(Verb::Repo {
        verb: RepoVerb::Sign {
            dir: p("next/testing"),
            work: p("work"),
            fingerprint: FPR.to_owned(),
            key_file: p("key"),
            passphrase_file: p("pass"),
            gpg: PathBuf::from(NOWHERE),
            output: None,
        },
    });
    assert!(e.contains("nothing to sign"), "{e}");
    let e = spawn(write_verb(Some("dist"), Some("1.2.3"), false));
    assert!(e.contains("is not in"), "{e}");
    let e = spawn(write_verb(None, None, true));
    assert!(e.contains("is missing from the new repodata"), "{e}");
}
