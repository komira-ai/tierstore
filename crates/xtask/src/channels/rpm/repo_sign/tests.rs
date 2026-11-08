use super::*;
use crate::channels::rpm::testing::*;

const FPR2: &str = "0123456789ABCDEF0123456789ABCDEF01234567";

struct Setup {
    dir: crate::testdir::TestDir,
}

impl Setup {
    fn new(name: &str) -> Setup {
        let dir = scratch(name);
        for arch in ["aarch64", "x86_64"] {
            let repodata = dir.join("next/testing").join(arch).join("repodata");
            std::fs::create_dir_all(&repodata).expect("dir");
            std::fs::write(
                repodata.join("repomd.xml"),
                format!("<repomd>{arch}</repomd>"),
            )
            .expect("repomd");
        }
        std::fs::write(dir.join("key.asc"), "PRIVATE KEY").expect("key");
        std::fs::write(dir.join("pass"), "secret").expect("pass");
        Setup { dir }
    }

    fn request<'a>(&'a self, fingerprints: &'a str) -> Request<'a> {
        Request {
            dir: Box::leak(self.dir.join("next/testing").into_boxed_path()),
            work: Box::leak(self.dir.join("work").into_boxed_path()),
            fingerprints,
            key_file: Box::leak(self.dir.join("key.asc").into_boxed_path()),
            passphrase_file: Box::leak(self.dir.join("pass").into_boxed_path()),
        }
    }
}

#[test]
fn each_repomd_gets_an_armored_detached_signature_that_is_verified() {
    // Catches: a repository published with an architecture unsigned (dnf with `repo_gpgcheck=1`
    // refuses it), signed by another key than the subkey, or never checked after signing.
    let s = Setup::new("repo-sign-ok");
    let gpg = FakeGpg::default();
    let out = s.dir.join("out");
    let said = sign(&gpg, &s.request(&format!("{FPR},{FPR2}")), Some(&out)).expect("sign");
    assert_eq!(
        said,
        [format!(
            "signed repomd.xml of 2 architectures with 2 subkey(s), {FPR}, {FPR2}: repomd.xml.asc verifies"
        )]
    );
    assert_eq!(std::fs::read_to_string(&out).expect("out"), "signed=true\n");
    for arch in ["aarch64", "x86_64"] {
        let asc = std::fs::read_to_string(
            s.dir
                .join("next/testing")
                .join(arch)
                .join("repodata/repomd.xml.asc"),
        )
        .expect("asc");
        assert!(asc.starts_with("-----BEGIN PGP SIGNATURE-----\n"), "{asc}");
    }
    let signs = gpg.calls_with("--detach-sign");
    assert_eq!(signs.len(), 2);
    let call = signs[0].join(" ");
    for part in [
        &format!("--local-user {FPR}! --local-user {FPR2}!"),
        "--digest-algo SHA512",
        "--armor --detach-sign",
        "--pinentry-mode loopback",
    ] {
        assert!(call.contains(part), "{part}: {call}");
    }
    assert_eq!(gpg.calls_with("--verify").len(), 2);
    assert_eq!(gpg.calls_with("--list-secret-keys").len(), 2);
    assert!(
        !s.dir.join("work/gnupg").exists()
            && !s.dir.join("key.asc").exists()
            && !s.dir.join("pass").exists()
    );
}

#[test]
fn nothing_is_signed_for_a_bad_fingerprint_or_a_missing_repomd_and_the_key_goes_anyway() {
    // Catches: a key started for nothing, and a key left behind by a refusal.
    let s = Setup::new("repo-sign-refused");
    let gpg = FakeGpg::default();
    assert!(matches!(
        sign(&gpg, &s.request("nonsense"), None),
        Err(Error::Usage(_))
    ));
    assert!(!s.dir.join("key.asc").exists());
    let s = Setup::new("repo-sign-missing");
    std::fs::remove_file(s.dir.join("next/testing/x86_64/repodata/repomd.xml")).expect("remove");
    let e = sign(&gpg, &s.request(FPR), None).unwrap_err().to_string();
    assert!(
        e.contains("x86_64/repodata/repomd.xml does not exist: nothing to sign"),
        "{e}"
    );
    assert!(gpg.calls.borrow().is_empty());
}

#[test]
fn a_failed_gpg_step_is_told_by_what_it_says() {
    // Catches: an unsigned repository published because a gpg step failed quietly.
    for (refusal, expected) in [
        ("--import", "could not import the key"),
        ("--list-secret-keys", "not among the secret keys"),
        ("--detach-sign", "gpg could not sign"),
        ("--verify", "repomd.xml.asc does not verify"),
    ] {
        let s = Setup::new("repo-sign-gpg");
        let gpg = FakeGpg::default();
        gpg.refuse_when(refusal, "gpg: failed");
        let e = sign(&gpg, &s.request(FPR), None).unwrap_err().to_string();
        assert!(
            e.contains(expected) && e.contains("gpg: failed"),
            "{refusal}: {e}"
        );
        assert!(!s.dir.join("key.asc").exists(), "{refusal}");
    }
}
