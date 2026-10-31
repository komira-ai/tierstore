//! `verify-assets attestations` against a stand-in `gh` (src/bin/gh-fake.rs) whose ledger lists
//! the attestations that exist. Each test builds the ledger of a good release, then spoils one
//! thing, and the verb must refuse exactly that.

use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;
use sha2::{Digest, Sha256};
use xtask::Error;
use xtask::sums::{Attestations, verify_attestations};

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
    /// Two tarballs with their SBOMs and bundles, the sums, and the ledger of a release that
    /// was signed as it should be.
    fn good(name: &str) -> Release {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("attestations-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let mut ledger = Vec::new();
        let mut sums = String::new();
        let stems = [
            format!("tierstore-{COMMIT}-linux-arm64"),
            format!("tierstore-{COMMIT}-linux-x86_64"),
        ];
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
            trusted_root: None,
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
    assert_eq!(printed.len(), 4 + 1, "{printed:?}");
    assert_eq!(
        printed
            .iter()
            .filter(|l| l.contains("(provenance, SBOM, bundle)"))
            .count(),
        2
    );
    assert_eq!(
        printed
            .iter()
            .filter(|l| l.ends_with("(provenance)"))
            .count(),
        2
    );
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
