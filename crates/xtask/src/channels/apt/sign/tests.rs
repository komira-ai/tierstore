use super::*;
use crate::channels::apt::testing::*;
use std::os::unix::fs::PermissionsExt;

const FPR: &str = "0123456789ABCDEF0123456789ABCDEF01234567";
const OTHER: &str = "FEDCBA9876543210FEDCBA9876543210FEDCBA98";

struct Setup {
    dir: crate::testdir::TestDir,
}

impl Setup {
    fn new(name: &str) -> Setup {
        let dir = scratch(name);
        std::fs::create_dir_all(dir.join("next/dists/stable")).expect("dir");
        std::fs::write(dir.join("next/dists/stable/Release"), "Suite: stable\n").expect("release");
        std::fs::write(dir.join("key.asc"), "PRIVATE KEY").expect("key");
        std::fs::write(dir.join("pass"), "secret").expect("pass");
        Setup { dir }
    }

    fn request<'a>(&'a self, fingerprints: &'a str) -> Request<'a> {
        Request {
            dir: Box::leak(self.dir.join("next/dists/stable").into_boxed_path()),
            work: Box::leak(self.dir.join("work").into_boxed_path()),
            fingerprints,
            key_file: Box::leak(self.dir.join("key.asc").into_boxed_path()),
            passphrase_file: Box::leak(self.dir.join("pass").into_boxed_path()),
        }
    }
}

#[test]
fn release_is_signed_clear_and_detached_and_both_are_verified() {
    // Catches: an index published unsigned, signed by another key than the subkey, or never
    // checked after signing.
    let s = Setup::new("sign-ok");
    let gpg = FakeGpg::default();
    let out = s.dir.join("out");
    let said = sign(&gpg, &s.request(FPR), Some(&out)).expect("sign");
    assert_eq!(
        said,
        [format!(
            "signed Release with 1 subkey(s), {FPR}: InRelease and Release.gpg verify"
        )]
    );
    assert_eq!(std::fs::read_to_string(&out).expect("out"), "signed=true\n");
    let stable = s.dir.join("next/dists/stable");
    let clear = std::fs::read_to_string(stable.join("InRelease")).expect("InRelease");
    assert!(clear.contains(&format!("{FPR}!")) && clear.ends_with("Suite: stable\n"));
    assert!(
        std::fs::read_to_string(stable.join("Release.gpg"))
            .expect("gpg")
            .contains(&format!("{FPR}!"))
    );
    // The steps, in order: import, list, sign twice, verify twice.
    let calls = gpg.calls.borrow();
    let words: Vec<&str> = calls
        .iter()
        .map(|c| {
            [
                "--import",
                "--list-secret-keys",
                "--clearsign",
                "--detach-sign",
                "--verify",
            ]
            .into_iter()
            .find(|w| c.iter().any(|a| a == w))
            .expect("a step")
        })
        .collect();
    assert_eq!(
        words,
        [
            "--import",
            "--list-secret-keys",
            "--clearsign",
            "--detach-sign",
            "--verify",
            "--verify"
        ]
    );
    // The signing lines: the loopback pinentry, the passphrase as a file, never as an argument.
    for call in calls
        .iter()
        .filter(|c| c.iter().any(|a| a.ends_with("sign")))
    {
        assert_eq!(flag(call, "--pinentry-mode").as_deref(), Some("loopback"));
        assert_eq!(
            flag(call, "--passphrase-file").as_deref(),
            Some(s.dir.join("pass").to_str().expect("utf8"))
        );
        assert_eq!(flag(call, "--digest-algo").as_deref(), Some("SHA512"));
        assert!(
            !call
                .iter()
                .any(|a| a == "--passphrase" || a.contains("secret"))
        );
    }
    // The detached verify names the signature and the file it signs.
    let last = calls.last().expect("last");
    assert_eq!(
        last[last.len() - 2..],
        [
            stable.join("Release.gpg").display().to_string(),
            stable.join("Release").display().to_string()
        ]
    );
}

#[test]
fn the_key_and_the_keyring_are_gone_when_it_ends_and_the_keyring_was_private() {
    // Catches: a private key or passphrase left on the runner's disk, and a keyring other users
    // can read while it exists.
    struct Peek(std::path::PathBuf, std::cell::RefCell<Option<u32>>);
    impl Gpg for Peek {
        fn run(&self, args: &[String]) -> Result<Answer, Error> {
            if args.iter().any(|a| a == "--import") {
                let mode = std::fs::metadata(&self.0)
                    .expect("home")
                    .permissions()
                    .mode();
                *self.1.borrow_mut() = Some(mode & 0o777);
            }
            FakeGpg::default().run(args)
        }
    }
    let s = Setup::new("sign-clean");
    let peek = Peek(s.dir.join("work/gnupg"), std::cell::RefCell::new(None));
    sign(&peek, &s.request(FPR), None).expect("sign");
    assert_eq!(*peek.1.borrow(), Some(0o700));
    assert!(!s.dir.join("work/gnupg").exists());
    assert!(!s.dir.join("key.asc").exists() && !s.dir.join("pass").exists());
}

#[test]
fn a_failing_step_names_what_failed_and_still_removes_the_key() {
    // Catches: a failed signing that leaves the key behind, or an index published with a signature
    // that does not verify.
    for (when, said, why) in [
        (
            "--import",
            "no secret key",
            "gpg could not import the key: no secret key",
        ),
        (
            "--list-secret-keys",
            "not found",
            "the signing key is not among the secret keys that were imported",
        ),
        (
            "--clearsign",
            "bad passphrase",
            "gpg could not sign InRelease: bad passphrase",
        ),
        (
            "--detach-sign",
            "bad passphrase",
            "gpg could not sign Release.gpg: bad passphrase",
        ),
        (
            "InRelease",
            "BAD signature",
            "InRelease does not verify: BAD signature",
        ),
        (
            "Release.gpg",
            "BAD signature",
            "Release.gpg does not verify: BAD signature",
        ),
    ] {
        let s = Setup::new("sign-fail");
        let gpg = FakeGpg::default();
        gpg.refuse_when(
            &if when.starts_with("--") {
                when.to_owned()
            } else {
                format!(
                    "--verify {}",
                    s.dir.join("next/dists/stable").join(when).display()
                )
            },
            said,
        );
        let out = s.dir.join("out");
        let e = sign(&gpg, &s.request(FPR), Some(&out))
            .unwrap_err()
            .to_string();
        assert!(e.contains(why), "{when}: {e}");
        assert!(!out.exists(), "{when}: nothing says it is signed");
        assert!(!s.dir.join("key.asc").exists() && !s.dir.join("work/gnupg").exists());
    }
}

#[test]
fn a_key_rotation_signs_with_each_subkey_and_checks_each_is_there() {
    // Catches: a rotation that signs with one key only, or signs before finding a key missing.
    let s = Setup::new("sign-two");
    let gpg = FakeGpg::default();
    let both = format!("{FPR}, {}", OTHER.to_ascii_lowercase());
    // A comma separates; spaces inside a fingerprint are tolerated.
    let said = sign(&gpg, &s.request(&both.replace(", ", ",")), None).expect("sign");
    assert!(said[0].starts_with("signed Release with 2 subkey(s)"));
    let clear = std::fs::read_to_string(s.dir.join("next/dists/stable/InRelease")).expect("clear");
    assert!(clear.contains(&format!("{FPR}!")) && clear.contains(&format!("{OTHER}!")));
    assert_eq!(
        gpg.calls
            .borrow()
            .iter()
            .filter(|c| c.iter().any(|a| a == "--list-secret-keys"))
            .count(),
        2
    );

    let s = Setup::new("sign-missing");
    let gpg = FakeGpg::default();
    gpg.refuse_when(&format!("--with-colons {OTHER}"), "no such key");
    let e = sign(&gpg, &s.request(&format!("{FPR},{OTHER}")), None)
        .unwrap_err()
        .to_string();
    assert!(e.contains(OTHER) && e.contains("no such key"), "{e}");
    assert!(
        !gpg.calls
            .borrow()
            .iter()
            .any(|c| c.iter().any(|a| a == "--clearsign"))
    );
}

#[test]
fn what_is_not_a_fingerprint_or_has_nothing_to_sign_is_refused_before_gpg_runs() {
    // Catches: signing with a key spec that is a name or a pattern, and gpg started with nothing to do.
    let s = Setup::new("sign-usage");
    let gpg = FakeGpg::default();
    for bad in [
        "",
        "abc",
        "0123456789ABCDEG0123",
        &format!("{FPR},"),
        &format!("{FPR},{FPR}"),
        &format!("{FPR}!"),
    ] {
        assert!(
            matches!(sign(&gpg, &s.request(bad), None), Err(Error::Usage(_))),
            "{bad:?}"
        );
    }
    assert!(sign(&gpg, &s.request("0123 4567 89AB CDEF"), None).is_ok());
    std::fs::remove_file(s.dir.join("next/dists/stable/Release")).expect("remove");
    let e = sign(&gpg, &s.request(FPR), None).unwrap_err().to_string();
    assert!(e.contains("nothing to sign"));
    assert_eq!(
        gpg.calls.borrow().len(),
        6,
        "only the one good run called gpg"
    );
}

#[test]
fn a_work_directory_that_cannot_hold_the_keyring_is_an_error_and_an_old_keyring_is_replaced() {
    // Catches: a keyring of an earlier run reused, and a failure to make the directory ignored.
    let s = Setup::new("sign-work");
    std::fs::create_dir_all(s.dir.join("work/gnupg")).expect("old");
    std::fs::write(s.dir.join("work/gnupg/old-key"), "old").expect("old key");
    let gpg = FakeGpg::default();
    sign(&gpg, &s.request(FPR), None).expect("sign");
    let s = Setup::new("sign-work-blocked");
    std::fs::write(s.dir.join("work"), "a file").expect("file");
    let e = sign(&gpg, &s.request(FPR), None).unwrap_err().to_string();
    assert!(e.starts_with("create "), "{e}");
}

#[test]
fn the_key_files_are_removed_when_the_flags_are_refused_before_any_signing() {
    // Catches: a key and a passphrase left on the runner's disk because the verb stopped on a
    // fingerprint or a missing Release before it reached the cleanup.
    let s = Setup::new("sign-refused-removes-key");
    let gpg = FakeGpg::default();
    assert!(matches!(
        sign(&gpg, &s.request("abc"), None),
        Err(Error::Usage(_))
    ));
    assert!(!s.dir.join("key.asc").exists() && !s.dir.join("pass").exists());
    std::fs::write(s.dir.join("key.asc"), "PRIVATE KEY").expect("key");
    std::fs::write(s.dir.join("pass"), "secret").expect("pass");
    std::fs::remove_file(s.dir.join("next/dists/stable/Release")).expect("remove");
    assert!(sign(&gpg, &s.request(FPR), None).is_err());
    assert!(!s.dir.join("key.asc").exists() && !s.dir.join("pass").exists());
    assert!(gpg.calls.borrow().is_empty());
}
