//! `verify-assets attestations` against a stand-in `gh` (src/bin/gh-fake.rs) whose ledger lists
//! the attestations that exist. Each test builds the ledger of a good release, then spoils one
//! thing, and the verb must refuse exactly that.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;
use sha2::{Digest, Sha256};
use xtask::Error;
use xtask::bundles::{DEAD_PROXY, ISSUER};
use xtask::sums::{Attestations, verify_attestations};

/// The reader of the install pages' commands, shared with the unit tests of the crate.
#[path = "../src/install_pages.rs"]
mod install_pages;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPO: &str = "example/tierstore";
const SIGNER: &str = "example/tierstore/.github/workflows/artifacts.yml";
const PROVENANCE: &str = "https://slsa.dev/provenance/v1";
const SBOM: &str = "https://cyclonedx.org/bom";

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn bundle(subject: &str) -> String {
    let statement = json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": "asset", "digest": { "sha256": subject } }],
        "predicateType": PROVENANCE,
        "predicate": {},
    });
    let bundle = json!({
        "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
        "verificationMaterial": {},
        "dsseEnvelope": {
            "payloadType": "application/vnd.in-toto+json",
            "payload": STANDARD.encode(statement.to_string()),
            "signatures": [],
        },
    });
    format!("{bundle}\n")
}

/// A release directory and the ledger of its attestations.
struct Release {
    dir: PathBuf,
    ledger: Vec<String>,
}

impl Release {
    /// The tarball of every release platform with its SBOM and bundle, the sums, and the
    /// ledger of a release that was signed as it should be.
    fn good(name: &str) -> Release {
        Release::good_for(name, &["linux-arm64", "linux-x86_64", "macos-arm64"])
    }

    /// The same for the platforms `platforms` alone.
    fn good_for(name: &str, platforms: &[&str]) -> Release {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("attestations-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let mut ledger = Vec::new();
        let mut sums = String::new();
        let stems: Vec<String> = platforms
            .iter()
            .map(|p| format!("tierstore-{COMMIT}-{p}"))
            .collect();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for stem in &stems {
            files.push((
                format!("{stem}.cdx.json"),
                format!("sbom of {stem}").into_bytes(),
            ));
            files.push((
                format!("{stem}.tar.gz"),
                format!("tarball of {stem}").into_bytes(),
            ));
        }
        for (file, bytes) in &files {
            let digest = sha(bytes);
            std::fs::write(dir.join(file), bytes).expect("file");
            sums.push_str(&format!("{digest}  {file}\n"));
            ledger.push(format!("{digest}|{SIGNER}|{PROVENANCE}|-|-"));
            if file.ends_with(".tar.gz") {
                let own = bundle(&digest);
                std::fs::write(dir.join(format!("{file}.sigstore.json")), &own).expect("bundle");
                ledger.push(format!("{digest}|{SIGNER}|{SBOM}|-|-"));
                // The same provenance, as the bundle of that tarball: bound to its commit.
                ledger.push(format!(
                    "{digest}|{SIGNER}|{PROVENANCE}|{}|{COMMIT}",
                    sha(own.as_bytes())
                ));
            }
        }
        std::fs::write(dir.join("SHA256SUMS"), sums).expect("sums");
        Release { dir, ledger }
    }

    /// The release with `keep` applied to its ledger.
    fn filtered(&self, keep: impl Fn(&String) -> bool) -> Release {
        Release {
            dir: self.dir.clone(),
            ledger: self.ledger.iter().filter(|l| keep(l)).cloned().collect(),
        }
    }

    /// The release with `change` applied to each line of its ledger.
    fn changed(&self, change: impl Fn(&str) -> String) -> Release {
        Release {
            dir: self.dir.clone(),
            ledger: self.ledger.iter().map(|l| change(l)).collect(),
        }
    }

    /// The SHA-256 of the file `name` of the release.
    fn digest_of(&self, name: &str) -> String {
        sha(&std::fs::read(self.dir.join(name)).expect("read"))
    }

    /// Runs the verb with the ledger as it is now.
    fn verify(&self) -> Result<Vec<String>, Error> {
        self.verify_with(None)
    }

    /// Runs the verb with the ledger as it is now, and `trusted_root` for the bundles.
    fn verify_with(&self, trusted_root: Option<&Path>) -> Result<Vec<String>, Error> {
        // What the verb is to pass `gh`, held by the stand-in: the repository, the issuer, the
        // branch, and the trusted root with the dead proxies around it, or no root at all.
        let root = match trusted_root {
            Some(root) => format!("root={}\nproxy={DEAD_PROXY}\n", root.display()),
            None => "root=-\n".to_owned(),
        };
        let expect = format!("repo={REPO}\nissuer={ISSUER}\nsource-ref=refs/heads/main\n{root}");
        self.verify_expecting(trusted_root, &expect)
    }

    /// Runs the verb with `trusted_root`, the stand-in holding the command line to `expect`.
    fn verify_expecting(
        &self,
        trusted_root: Option<&Path>,
        expect: &str,
    ) -> Result<Vec<String>, Error> {
        std::fs::write(self.dir.join("expect.txt"), expect).expect("expect");
        std::fs::write(self.dir.join("ledger.txt"), self.ledger.join("\n")).expect("ledger");
        let scratch = self.dir.join("scratch");
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch");
        let gh = PathBuf::from(env!("CARGO_BIN_EXE_gh-fake"));
        let a = Attestations {
            repo: REPO,
            workflow_ref: &format!("{SIGNER}@refs/heads/main"),
            source_ref: "refs/heads/main",
            commit: COMMIT,
            trusted_root,
            gh: &gh,
            scratch: &scratch,
        };
        verify_attestations(&self.dir, &a)
    }
}

fn failure(r: Result<Vec<String>, Error>) -> String {
    match r {
        Err(e) => e.to_string(),
        Ok(lines) => panic!("expected a refusal, got {lines:?}"),
    }
}

#[test]
fn a_release_signed_as_it_should_be_verifies() {
    // Catches: the verb refusing the good set, and a verb that checks nothing: the printed
    // lines name every file and every kind of attestation it verified.
    let release = Release::good("good");
    let printed = release.verify().expect("verify");
    assert_eq!(printed.len(), 6 + 1, "{printed:?}");
    assert_eq!(
        printed
            .iter()
            .filter(|l| l.contains("(provenance, SBOM, bundle)"))
            .count(),
        3
    );
    assert_eq!(
        printed
            .iter()
            .filter(|l| l.ends_with("(provenance)"))
            .count(),
        3
    );
    let mac = format!("verified: tierstore-{COMMIT}-macos-arm64.tar.gz (provenance, SBOM, bundle)");
    assert!(printed.contains(&mac), "{printed:?}");
    assert!(
        printed
            .last()
            .is_some_and(|l| l.starts_with("attestations: every asset verifies"))
    );
}

#[test]
fn a_missing_attestation_of_any_kind_is_refused() {
    // Catches: each kind of attestation going unchecked: the SBOM attestation of a tarball,
    // the provenance of an SBOM, and the tarball's own bundle.
    let good = Release::good("missing");
    let failed = failure(good.filtered(|l| !l.contains(SBOM)).verify());
    assert!(
        failed.contains("no https://cyclonedx.org/bom attestation"),
        "{failed}"
    );

    let sbom_file = format!("tierstore-{COMMIT}-linux-arm64.cdx.json");
    let digest = good.digest_of(&sbom_file);
    let failed = failure(good.filtered(|l| !l.starts_with(&digest)).verify());
    assert!(
        failed.contains(&sbom_file) && failed.contains("no build provenance attestation"),
        "{failed}"
    );

    let failed = failure(good.filtered(|l| l.ends_with("|-|-")).verify());
    assert!(failed.contains("its bundle"), "{failed}");
}

#[test]
fn an_attestation_by_another_workflow_is_refused() {
    // Catches: the signer not being checked: every attestation is by ci.yml, not the
    // workflow the installers pin.
    let good = Release::good("other-signer");
    let other = "example/tierstore/.github/workflows/ci.yml";
    let failed = failure(good.changed(|l| l.replace(SIGNER, other)).verify());
    assert!(failed.contains(&format!("signed by {SIGNER}")), "{failed}");
}

#[test]
fn the_controls_refuse_attestations_that_are_too_loose() {
    // Catches: a verification that accepts too much passing the good set (each control must
    // fail it): an attestation that another workflow's check satisfies, one that fits any
    // file, a bundle that is not bound to its commit, and a bundle that fits any file.
    let good = Release::good("controls");
    let arm = format!("tierstore-{COMMIT}-linux-arm64.tar.gz");
    let first = good.digest_of(&arm);

    let mut loose = good.changed(str::to_owned);
    loose.ledger.push(format!(
        "{first}|example/tierstore/.github/workflows/ci.yml|{PROVENANCE}|-|-"
    ));
    let failed = failure(loose.verify());
    assert!(
        failed.contains("verified as signed by example/tierstore/.github/workflows/ci.yml"),
        "{failed}"
    );

    let mut loose = good.changed(str::to_owned);
    loose.ledger.push(format!("*|{SIGNER}|{PROVENANCE}|-|-"));
    let failed = failure(loose.verify());
    assert!(
        failed.contains("a flipped byte"),
        "any file verifies: {failed}"
    );

    let loose = good.changed(|l| match l.strip_suffix(&format!("|{COMMIT}")) {
        Some(head) => format!("{head}|*"),
        None => l.to_owned(),
    });
    let failed = failure(loose.verify());
    assert!(
        failed.contains("verified for commit"),
        "not bound to a commit: {failed}"
    );

    let loose = good.changed(|l| {
        if l.ends_with(COMMIT) {
            let mut fields: Vec<&str> = l.split('|').collect();
            fields[3] = "*";
            fields.join("|")
        } else {
            l.to_owned()
        }
    });
    let failed = failure(loose.verify());
    assert!(
        failed.contains("bundle of"),
        "a bundle fits any file: {failed}"
    );
}

#[test]
fn a_stray_swapped_or_missing_bundle_is_refused_before_gh_is_run() {
    // Catches: a bundle under another tarball's name, a tarball with no bundle and a bundle for
    // nothing: found by the pairing, with no call to gh (the ledger is empty).
    let arm = format!("tierstore-{COMMIT}-linux-arm64.tar.gz");
    let x86 = format!("tierstore-{COMMIT}-linux-x86_64.tar.gz");

    let mut release = Release::good("pairing-swapped");
    release.ledger.clear();
    std::fs::copy(
        release.dir.join(format!("{x86}.sigstore.json")),
        release.dir.join(format!("{arm}.sigstore.json")),
    )
    .expect("swap");
    assert!(failure(release.verify()).contains("subject digest"));

    let mut release = Release::good("pairing-missing");
    release.ledger.clear();
    std::fs::remove_file(release.dir.join(format!("{arm}.sigstore.json"))).expect("rm");
    assert!(failure(release.verify()).contains("no provenance bundle"));

    let mut release = Release::good("pairing-stray");
    release.ledger.clear();
    std::fs::write(
        release.dir.join("tierstore-other.tar.gz.sigstore.json"),
        b"x",
    )
    .expect("stray");
    assert!(failure(release.verify()).contains("not the bundle of a listed tarball"));
}

#[test]
fn a_workflow_or_branch_that_does_not_fit_is_a_usage_error() {
    // Catches: a signer of another repository, a tag as the source ref, and a short commit,
    // accepted as the thing to verify against.
    let release = Release::good("usage");
    let gh = PathBuf::from(env!("CARGO_BIN_EXE_gh-fake"));
    let attest = |workflow_ref: &str, source_ref: &str, commit: &str| {
        let a = Attestations {
            repo: REPO,
            workflow_ref,
            source_ref,
            commit,
            trusted_root: None,
            gh: &gh,
            scratch: &release.dir,
        };
        verify_attestations(&release.dir, &a)
    };
    let own = format!("{SIGNER}@refs/heads/main");
    assert!(matches!(
        attest(&own, "refs/tags/v1", COMMIT),
        Err(Error::Usage(_))
    ));
    assert!(matches!(
        attest(&own, "refs/heads/", COMMIT),
        Err(Error::Usage(_))
    ));
    assert!(matches!(
        attest(
            "other/repo/.github/workflows/artifacts.yml@refs/heads/main",
            "refs/heads/main",
            COMMIT
        ),
        Err(Error::Usage(_))
    ));
    assert!(matches!(
        attest(&own, "refs/heads/main", "0123456789ab"),
        Err(Error::Usage(_))
    ));
}

#[test]
fn a_trusted_root_and_the_dead_proxies_reach_every_bundle_check() {
    // Catches: the offline check dropping its trusted root or its proxies (the call would then
    // go to the network). The stand-in refuses any bundle verification whose command line or
    // environment departs from `expect.txt`, so the release verifies only when every one of
    // them carried both; it also refuses a root where none is expected, and the other way.
    let release = Release::good("trusted-root");
    let root = release.dir.join("trusted_root.jsonl");
    std::fs::write(&root, "{}").expect("root");
    let printed = release.verify_with(Some(&root)).expect("verify");
    assert_eq!(printed.len(), 7, "{printed:?}");

    let base = format!("repo={REPO}\nissuer={ISSUER}\nsource-ref=refs/heads/main\n");
    let proxy = format!("proxy={DEAD_PROXY}\n");
    let failed = failure(release.verify_expecting(Some(&root), &format!("{base}root=-\n")));
    assert!(
        failed.contains("its bundle"),
        "a root where none is expected: {failed}"
    );
    let failed =
        failure(release.verify_expecting(None, &format!("{base}root={}\n", root.display())));
    assert!(
        failed.contains("its bundle"),
        "no root where one is expected: {failed}"
    );
    let failed = failure(release.verify_expecting(
        Some(&root),
        &format!("{base}root={}\nproxy=http://127.0.0.1:1\n", root.display()),
    ));
    assert!(failed.contains("its bundle"), "other proxies: {failed}");
    let failed = failure(release.verify_expecting(None, &format!("{base}root=-\n{proxy}")));
    assert!(
        failed.contains("its bundle"),
        "proxies without a root: {failed}"
    );
}

#[test]
fn the_repository_the_issuer_and_the_branch_are_passed_as_given() {
    // Catches: `--repo`, `--cert-oidc-issuer` or `--source-ref` dropped or changed. The
    // stand-in refuses a command line that departs from `expect.txt`, so each wrong
    // expectation below fails the release as a dropped or altered flag would.
    let release = Release::good("expectations");
    let root = "root=-\n";
    for (wrong, what) in [
        (format!("repo=example/other\n{root}"), "the repository"),
        (
            format!("issuer=https://issuer.example\n{root}"),
            "the issuer",
        ),
        (format!("source-ref=refs/heads/other\n{root}"), "the branch"),
    ] {
        let failed = failure(release.verify_expecting(None, &wrong));
        assert!(
            failed.contains("verifies") || failed.contains("its bundle"),
            "{what}: {failed}"
        );
    }
}

#[test]
fn a_release_of_one_platform_has_no_other_bundle_to_swap_in() {
    // Catches: the swapped-bundle control assuming a second tarball, which fails the release
    // of a single platform.
    let release = Release::good_for("one-platform", &["linux-x86_64"]);
    let printed = release.verify().expect("verify");
    assert_eq!(printed.len(), 2 + 1, "{printed:?}");
}

#[test]
fn a_bundle_that_attests_another_tarball_or_a_flipped_one_fails_its_control() {
    // Catches: the two bundle controls that no other test trips: the bundle of the second
    // tarball accepted for the first, and the first tarball with a byte flipped accepted
    // against its own bundle.
    let good = Release::good("bundle-controls");
    let arm = format!("tierstore-{COMMIT}-linux-arm64.tar.gz");
    let x86 = format!("tierstore-{COMMIT}-linux-x86_64.tar.gz");

    let any_subject = |tarball: &str| {
        let digest = good.digest_of(tarball);
        good.changed(|l| {
            if l.starts_with(&digest) && l.ends_with(COMMIT) {
                l.replacen(&digest, "*", 1)
            } else {
                l.to_owned()
            }
        })
    };
    let failed = failure(any_subject(&x86).verify());
    assert!(
        failed.contains("a bundle is to attest its own tarball only"),
        "{failed}"
    );
    let failed = failure(any_subject(&arm).verify());
    assert!(failed.contains("a flipped byte"), "{failed}");
}

#[test]
fn a_sums_file_that_lists_neither_a_tarball_nor_an_sbom_is_refused() {
    // Catches: a file of another kind listed in SHA256SUMS passing as verified.
    let release = Release::good("other-kind");
    std::fs::write(release.dir.join("NOTES"), b"notes").expect("notes");
    let sums = release.dir.join("SHA256SUMS");
    let mut text = std::fs::read_to_string(&sums).expect("sums");
    text.push_str(&format!("{}  NOTES\n", release.digest_of("NOTES")));
    std::fs::write(&sums, text).expect("sums");
    let failed = failure(release.verify());
    assert!(failed.contains("neither a tarball nor an SBOM"), "{failed}");
}

#[test]
fn the_offline_gh_check_of_the_install_pages_gives_only_flags_the_verifier_passes() {
    // Catches: an install page whose offline command gives a flag that the bundle check of a
    // release does not pass to `gh`, or lacks `--bundle` or `--custom-trusted-root` (a reader's
    // offline check would then not be the one every release passed). The stand-in records the
    // arguments of each verification, so what is compared is what the verb passed.
    let release = Release::good("install-pages-offline");
    let root = release.dir.join("trusted_root.jsonl");
    std::fs::write(&root, "{}").expect("root");
    let record = release.dir.join("args.txt");
    let expect = format!(
        "repo={REPO}\nissuer={ISSUER}\nsource-ref=refs/heads/main\nroot={}\nproxy={DEAD_PROXY}\nrecord={}\n",
        root.display(),
        record.display()
    );
    release
        .verify_expecting(Some(&root), &expect)
        .expect("verify");
    let recorded = std::fs::read_to_string(&record).expect("the record");
    let bundled: Vec<Vec<&str>> = recorded
        .split("\n\n")
        .map(|block| block.lines().collect::<Vec<_>>())
        .filter(|args| args.contains(&"--bundle"))
        .collect();
    assert!(!bundled.is_empty(), "no bundle verification: {recorded}");
    let passed: BTreeSet<&str> = bundled
        .iter()
        .flatten()
        .copied()
        .filter(|w| w.starts_with("--"))
        .collect();
    assert!(passed.contains("--custom-trusted-root"), "{recorded}");
    for (name, page) in install_pages::pages() {
        let lines =
            install_pages::commands(&page, "gh attestation verify", "--custom-trusted-root");
        assert_eq!(lines.len(), 1, "{name}: one offline gh command");
        let flags = install_pages::flags(&lines[0]);
        assert!(flags.contains(&"--bundle"), "{name}");
        assert!(flags.contains(&"--custom-trusted-root"), "{name}");
        let bundle = install_pages::value_of(&lines[0], "--bundle");
        assert_eq!(bundle, "<tarball>.sigstore.json", "{name}");
        for flag in flags {
            assert!(
                passed.contains(flag),
                "{name}: the verifier does not pass {flag}"
            );
        }
    }
}

#[test]
fn every_tarball_needs_its_own_bundle_the_macos_arm64_one_included() {
    // Catches: a release whose macos-arm64 tarball has no bundle passing (the bundle copy of the
    // attest job left out for that platform), refused before gh is run; and a bundle that pairs
    // with its tarball but whose attestation was signed by another workflow than artifacts.yml
    // passing because the tarball's other attestations are good.
    let mac = format!("tierstore-{COMMIT}-macos-arm64.tar.gz");

    let mut release = Release::good("macos-no-bundle");
    release.ledger.clear();
    std::fs::remove_file(release.dir.join(format!("{mac}.sigstore.json"))).expect("rm");
    assert_eq!(
        failure(release.verify()),
        format!("{mac}: no provenance bundle {mac}.sigstore.json")
    );

    let good = Release::good("macos-bundle-other-signer");
    let digest = good.digest_of(&mac);
    let other = "example/tierstore/.github/workflows/ci.yml";
    let failed = failure(
        good.changed(|l| {
            if l.starts_with(&digest) && l.ends_with(COMMIT) {
                l.replace(SIGNER, other)
            } else {
                l.to_owned()
            }
        })
        .verify(),
    );
    assert!(
        failed.contains(&format!(
            "{mac}: its bundle {mac}.sigstore.json does not verify"
        )),
        "{failed}"
    );
}
