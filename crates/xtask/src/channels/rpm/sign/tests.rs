use super::*;
use crate::channels::rpm::testing::*;
use sha2::Digest;

const OTHER: &str = "FEDCBA9876543210FEDCBA9876543210FEDCBA98";

fn names() -> [String; 2] {
    [
        "tierstore-1.2.3-1.aarch64.rpm".to_owned(),
        "tierstore-1.2.3-1.x86_64.rpm".to_owned(),
    ]
}

#[test]
fn each_package_is_signed_in_its_header_only_and_listed_with_its_new_digest() {
    // Catches: a package published unsigned, signed by another key than the subkey, one whose
    // bytes changed beyond the signature (it would no longer be the attested package), and a
    // signature made over anything but the main header.
    let s = Signing::new("sign-ok", "1.2.3");
    let gpg = FakeGpg::default();
    let out = s.root.join("out");
    let said = sign(&gpg, &s.request("1.2.3", FPR), Some(&out)).expect("sign");
    assert_eq!(std::fs::read_to_string(&out).expect("out"), "signed=true\n");
    assert_eq!(said[0], format!("signed {} with {KEY_ID}", names()[0]));
    assert_eq!(said[2], "SHA256SUMS.signed: 2 packages");
    let attested = sums::read_sums(&s.root.join("dist/SHA256SUMS")).expect("sums");
    let signed_sums =
        sums::read_sums(&s.root.join("signed").join(SIGNED_SUMS)).expect("signed sums");
    for name in names() {
        let signed = std::fs::read(s.root.join("signed").join(&name)).expect("signed");
        let unsigned = std::fs::read(s.root.join("dist").join(&name)).expect("unsigned");
        assert_eq!(
            layout::without_signatures(&signed).expect("stripped"),
            unsigned
        );
        let parts = layout::split(&signed).expect("parts");
        let (tag, sig) = parts.signature().expect("signed");
        assert_eq!(tag, layout::TAG_RSA);
        assert_eq!(signer(sig).expect("signer").key_id, KEY_ID);
        assert_eq!(
            sig[sig.len() - 32..],
            <[u8; 32]>::from(sha2::Sha256::digest(parts.header))
        );
        assert_eq!(
            signed_sums
                .iter()
                .find(|l| l.name == name)
                .expect("line")
                .sha256,
            Sha256::of(&signed)
        );
        assert_ne!(
            attested
                .iter()
                .find(|l| l.name == name)
                .expect("line")
                .sha256,
            Sha256::of(&signed)
        );
    }
    let signs = gpg.calls_with("--detach-sign");
    assert_eq!(signs.len(), 2);
    let sign_call = signs[0].join(" ");
    for part in [
        "--pinentry-mode loopback",
        &format!("--passphrase-file {}", s.root.join("pass").display()),
        "--digest-algo SHA256",
        "--faked-system-time 1794200000!",
        &format!("--local-user {FPR}!"),
        "--no-armor --detach-sign",
    ] {
        assert!(sign_call.contains(part), "{part}: {sign_call}");
    }
    assert!(!sign_call.contains("--armor "), "{sign_call}");
    assert_eq!(gpg.calls_with("--verify").len(), 2);
}

#[test]
fn signing_twice_gives_the_same_bytes() {
    // Catches: a re-run, or the promotion's own run, listing a package the testing suite does not
    // (the digests would differ) because something that varies went into the signature.
    let a = Signing::new("sign-twice-a", "1.2.3");
    let b = Signing::new("sign-twice-b", "1.2.3");
    a.sign("1.2.3");
    b.sign("1.2.3");
    for name in names() {
        assert_eq!(
            std::fs::read(a.root.join("signed").join(&name)).expect("a"),
            std::fs::read(b.root.join("signed").join(&name)).expect("b")
        );
    }
}

#[test]
fn the_key_and_the_passphrase_are_gone_whatever_the_verb_came_to() {
    // Catches: a key left on the runner after a failed step.
    let s = Signing::new("sign-cleanup", "1.2.3");
    let gpg = FakeGpg::default();
    gpg.refuse_when("--import", "gpg: no valid OpenPGP data found");
    let e = sign(&gpg, &s.request("1.2.3", FPR), None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("could not import the key"), "{e}");
    assert!(!s.root.join("key.asc").exists() && !s.root.join("pass").exists());
    assert!(!s.root.join("work/gnupg").exists());
    assert!(!s.root.join("signed").exists());
}

#[test]
fn a_package_that_is_not_the_attested_one_is_not_signed() {
    // Catches: signing bytes the build did not attest, which would give them the archive key's
    // authority.
    let s = Signing::new("sign-unattested", "1.2.3");
    let path = s.root.join("dist").join(&names()[1]);
    let mut bytes = std::fs::read(&path).expect("read");
    bytes.push(0);
    std::fs::write(&path, bytes).expect("write");
    let gpg = FakeGpg::default();
    let e = sign(&gpg, &s.request("1.2.3", FPR), None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("is not the package that was attested"), "{e}");
    assert!(gpg.calls.borrow().is_empty());
    std::fs::write(s.root.join("dist/SHA256SUMS"), "").expect("sums");
    assert!(sign(&gpg, &s.request("1.2.3", FPR), None).is_err());
    std::fs::write(
        s.root.join("dist/SHA256SUMS"),
        format!("{}  other.rpm\n", "0".repeat(64)),
    )
    .expect("sums");
    let e = sign(&gpg, &s.request("1.2.3", FPR), None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("lists no tierstore-1.2.3-1.aarch64.rpm"), "{e}");
}

#[test]
fn flags_out_of_their_form_and_missing_packages_fail_before_gpg_runs() {
    // Catches: a signing run that starts the key before it knows what it signs.
    let s = Signing::new("sign-flags", "1.2.3");
    let gpg = FakeGpg::default();
    assert!(matches!(
        sign(&gpg, &s.request("1.2.3", "nonsense"), None),
        Err(Error::Usage(_))
    ));
    assert!(matches!(
        sign(&gpg, &s.request("v1.2.3", FPR), None),
        Err(Error::Usage(_))
    ));
    let e = sign(&gpg, &s.request("1.2.4", FPR), None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("is not in"), "{e}");
    assert!(gpg.calls.borrow().is_empty());
}

#[test]
fn a_failed_gpg_step_is_told_by_what_it_says() {
    // Catches: a signing key that is not the one asked for, a signature that fails to be made or
    // to verify, and a signature by another key than the one asked for, all ending green.
    for (refusal, expected) in [
        ("--list-secret-keys", "not among the secret keys"),
        (
            "--detach-sign",
            "could not sign tierstore-1.2.3-1.aarch64.rpm",
        ),
        ("--verify", "does not verify"),
    ] {
        let s = Signing::new("sign-gpg", "1.2.3");
        let gpg = FakeGpg::default();
        gpg.refuse_when(refusal, "gpg: failed");
        let e = sign(&gpg, &s.request("1.2.3", FPR), None)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(expected) && e.contains("gpg: failed"),
            "{refusal}: {e}"
        );
    }
    let s = Signing::new("sign-other-key", "1.2.3");
    let e = sign(
        &FakeGpg::default(),
        &s.request("1.2.3", &format!("{FPR},{OTHER}")),
        None,
    )
    .expect("first signs");
    assert!(e[0].contains(KEY_ID));
}

#[test]
fn a_signature_that_gpg_makes_by_another_key_or_in_another_form_is_refused() {
    // Catches: a signature the fake (or a misconfigured gpg) made with a key other than the one
    // asked for, and bytes that are not a signature at all, put into a package.
    let s = Signing::new("sign-wrong-key", "1.2.3");
    let gpg = FakeGpg::default();
    *gpg.signs_as.borrow_mut() = Some(OTHER.to_owned());
    let e = sign(&gpg, &s.request("1.2.3", FPR), None)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("was signed by the key") && e.contains(FPR),
        "{e}"
    );
    let s = Signing::new("sign-garbage", "1.2.3");
    *gpg.garbage.borrow_mut() = Some(b"-----BEGIN PGP SIGNATURE-----".to_vec());
    let e = sign(&gpg, &s.request("1.2.3", FPR), None)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("not an OpenPGP signature packet") || e.contains("does not verify"),
        "{e}"
    );
    let s = Signing::new("sign-elgamal", "1.2.3");
    *gpg.garbage.borrow_mut() = Some(packet(16, FPR, b"x"));
    assert!(sign(&gpg, &s.request("1.2.3", FPR), None).is_err());
}

#[test]
fn a_package_that_is_signed_already_is_not_signed_again() {
    // Catches: a second signature stacked by a re-run over a signed directory.
    let s = Signing::new("sign-again", "1.2.3");
    s.sign("1.2.3");
    let e = sign(&FakeGpg::default(), &s.request("1.2.3", FPR), None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("write") || e.contains("exist"), "{e}");
}

fn check<'a>(s: &'a Signing, fingerprint: Option<&'a str>, signed_sums: bool) -> Check<'a> {
    Check {
        dir: Box::leak(s.root.join("signed").into_boxed_path()),
        version: "1.2.3",
        sums: Box::leak(s.root.join("dist/SHA256SUMS").into_boxed_path()),
        signed_sums: signed_sums
            .then(|| &*Box::leak(s.root.join("signed").join(SIGNED_SUMS).into_boxed_path())),
        fingerprint,
    }
}

#[test]
fn check_signed_proves_each_package_is_the_attested_one_plus_a_signature() {
    // Catches: a published package that is not the attested bytes (a rebuild, a swap) or that was
    // never signed, and a signed one by a key that is not ours.
    let s = Signing::new("check-ok", "1.2.3");
    s.sign("1.2.3");
    let said = check_signed(&check(&s, Some(FPR), true)).expect("check");
    assert_eq!(said.len(), 2);
    assert!(said[0].contains(&format!(
        "{}: signed by {KEY_ID}; without the signature it is the attested package",
        names()[0]
    )));
    assert_eq!(
        check_signed(&check(&s, None, false)).expect("plain").len(),
        2
    );
    let e = check_signed(&check(&s, Some(OTHER), false))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("it is signed by the key") && e.contains(OTHER),
        "{e}"
    );
}

#[test]
fn check_signed_refuses_a_package_that_is_unsigned_changed_or_not_listed() {
    // Catches: each way the bytes about to be published differ from what was verified.
    let s = Signing::new("check-bad", "1.2.3");
    let e = check_signed(&Check {
        dir: &s.root.join("dist"),
        ..check(&s, None, false)
    })
    .unwrap_err()
    .to_string();
    assert!(e.contains("it is not signed"), "{e}");
    s.sign("1.2.3");
    let path = s.root.join("signed").join(&names()[0]);
    let good = std::fs::read(&path).expect("read");
    // The payload altered: the digests of the package no longer hold.
    let mut payload = good.clone();
    let last = payload.len() - 1;
    payload[last] ^= 1;
    std::fs::write(&path, &payload).expect("write");
    assert!(check_signed(&check(&s, None, false)).is_err());
    // The main header altered: its digest, in the signature header, no longer holds.
    let mut header = good.clone();
    let (_, end) = layout::split(&good).expect("parts").header_range();
    header[end - 5] ^= 1;
    std::fs::write(&path, &header).expect("write");
    assert!(check_signed(&check(&s, None, false)).is_err());
    // A signature under the wrong tag for its algorithm.
    let parts = layout::split(&good).expect("parts");
    let (_, sig) = parts.signature().expect("sig");
    let stripped = layout::without_signatures(&good).expect("stripped");
    let wrong = layout::with_signature(&stripped, layout::TAG_DSA, sig).expect("dsa");
    std::fs::write(&path, wrong).expect("write");
    let e = check_signed(&check(&s, None, false))
        .unwrap_err()
        .to_string();
    assert!(e.contains("filed under the tag 267"), "{e}");
    // The signed package restored, but a SHA256SUMS.signed that lists other bytes, or none.
    std::fs::write(&path, &good).expect("write");
    let signed_sums = s.root.join("signed").join(SIGNED_SUMS);
    std::fs::write(
        &signed_sums,
        format!("{}  {}\n", "1".repeat(64), names()[0]),
    )
    .expect("write");
    let e = check_signed(&check(&s, None, true))
        .unwrap_err()
        .to_string();
    assert!(e.contains("is not the one SHA256SUMS.signed lists"), "{e}");
    // An attested digest that is not the package's once its signature is taken out.
    std::fs::write(
        s.root.join("dist/SHA256SUMS"),
        format!(
            "{}  {}\n{}  {}\n",
            "2".repeat(64),
            names()[0],
            "2".repeat(64),
            names()[1]
        ),
    )
    .expect("write");
    let e = check_signed(&check(&s, None, false))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("with its signature taken out")
            && e.contains("not the package that was attested"),
        "{e}"
    );
    assert!(matches!(
        check_signed(&Check {
            fingerprint: Some("zz"),
            ..check(&s, None, false)
        }),
        Err(Error::Usage(_))
    ));
    let e = check_signed(&Check {
        version: "1.2.4",
        ..check(&s, None, false)
    })
    .unwrap_err()
    .to_string();
    assert!(e.contains("is not in"), "{e}");
}
