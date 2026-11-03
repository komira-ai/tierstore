//! The start-up checks of the front's certificate and key.

use std::fs::Permissions;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

use super::OciError;
use super::s3_acceptor;
use super::test_pki::{ca, dns, email, ip, leaf, leaf_with};
use super::tls::{acceptor, acceptor_for, ip_of, unnamed};
use rcgen::date_time_ymd;

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("an address")
}

/// Why `acceptor` refuses these files for `listen`.
fn refusal(cert: &Path, key: &Path, listen: &str) -> String {
    let refused = acceptor(cert, key, addr(listen)).err();
    refused.expect("refused").to_string()
}

/// Asserts a certificate that names the address is accepted: the address itself, or any IP when
/// the listen address is the unspecified one, and a certificate with several names.
#[test]
fn a_certificate_that_names_the_address_is_accepted() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-tls-named");
    let ca = ca();
    let one = leaf(dir.path(), "one", &ca, vec![ip("127.0.0.1")]);
    acceptor(&one.cert, &one.key, addr("127.0.0.1:443")).expect("named");
    acceptor(&one.cert, &one.key, addr("0.0.0.0:443")).expect("any IP will do for 0.0.0.0");
    let many = vec![dns("registry.test"), ip("192.0.2.7"), ip("::1")];
    let many = leaf(dir.path(), "many", &ca, many);
    for listen in ["192.0.2.7:443", "[::1]:443", "[::]:443"] {
        acceptor(&many.cert, &many.key, addr(listen)).expect(listen);
    }
}

/// Asserts that for a front at several addresses the certificate must name each of them, that
/// the refusal says which one it does not name, and that any IP covers the wildcards.
#[test]
fn a_certificate_must_name_every_address_of_the_front() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-tls-several");
    let ca = ca();
    let both = leaf(dir.path(), "both", &ca, vec![ip("127.0.0.1"), ip("::1")]);
    let v4 = leaf(dir.path(), "v4", &ca, vec![ip("127.0.0.1")]);
    let listen = [addr("127.0.0.1:443"), addr("[::1]:443")];
    acceptor_for(&both.cert, &both.key, &listen).expect("both are named");
    let refused = acceptor_for(&v4.cert, &v4.key, &listen).err();
    let why = refused.expect("::1 is not named").to_string();
    assert!(why.starts_with("--oci-tls-cert "), "{why}");
    assert!(why.contains("no IP subjectAltName for ::1"), "{why}");
    let wildcards = [addr("0.0.0.0:443"), addr("[::]:443")];
    acceptor_for(&v4.cert, &v4.key, &wildcards).expect("any IP will do for a wildcard");
}

/// Asserts a certificate without an IP subjectAltName for the address is refused with the
/// names it does have: none at all, a DNS name only, or another address.
#[test]
fn a_certificate_that_does_not_name_the_address_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-tls-unnamed");
    let ca = ca();
    let bare = leaf(dir.path(), "bare", &ca, vec![]);
    let named = leaf(dir.path(), "dns", &ca, vec![dns("registry.test")]);
    let other = leaf(dir.path(), "other", &ca, vec![ip("192.0.2.8"), ip("::2")]);
    for (files, listen) in [
        (&bare, "127.0.0.1:443"),
        (&bare, "0.0.0.0:443"),
        (&named, "127.0.0.1:443"),
        (&named, "[::]:443"),
        (&other, "192.0.2.7:443"),
    ] {
        let why = refusal(&files.cert, &files.key, listen);
        assert!(why.starts_with("--oci-tls-cert "), "{why}");
        assert!(why.contains("no IP subjectAltName"), "{why}");
        assert!(
            why.contains(&listen.replace(":443", "").replace(['[', ']'], "")),
            "{why}"
        );
    }
    let why = refusal(&other.cert, &other.key, "192.0.2.7:443");
    assert!(why.contains("[192.0.2.8, ::2]"), "{why}");
}

/// Asserts what is wrong with the files is said with the flag that names the file: a file that
/// is not there, text with no certificate, a certificate that is not X.509, a key that is not a
/// key, and a key that is another certificate's.
#[test]
fn files_that_are_not_a_certificate_and_a_key_are_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-tls-files");
    let ca = ca();
    let good = leaf(dir.path(), "good", &ca, vec![ip("127.0.0.1")]);
    let stranger = leaf(dir.path(), "stranger", &ca, vec![ip("127.0.0.1")]);
    let missing: PathBuf = dir.path().join("missing");
    let text = dir.path().join("text");
    std::fs::write(&text, "not a PEM file\n").expect("write");
    std::fs::set_permissions(&text, Permissions::from_mode(0o600)).expect("mode");
    let junk = dir.path().join("junk.pem");
    let block = "-----BEGIN CERTIFICATE-----\nanVuaw==\n-----END CERTIFICATE-----\n";
    std::fs::write(&junk, block).expect("write");
    let broken = dir.path().join("broken.pem");
    let broken_block = "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
    std::fs::write(&broken, broken_block).expect("write");

    let listen = "127.0.0.1:443";
    let why = refusal(&missing, &good.key, listen);
    assert!(why.starts_with("--oci-tls-cert "), "{why}");
    assert!(why.contains("missing"), "{why}");
    let why = refusal(&good.cert, &missing, listen);
    assert!(why.starts_with("--oci-tls-key "), "{why}");
    assert!(why.contains("missing"), "{why}");
    assert!(refusal(&text, &good.key, listen).contains("no certificate"));
    assert!(refusal(&junk, &good.key, listen).contains("not X.509"));
    assert!(refusal(&broken, &good.key, listen).contains("not PEM certificates"));
    let why = refusal(&good.cert, &text, listen);
    assert!(why.starts_with("--oci-tls-key "), "{why}");
    assert!(why.contains("not a PEM private key"), "{why}");
    let why = refusal(&good.cert, &stranger.key, listen);
    assert!(
        why.starts_with("--oci-tls-cert and --oci-tls-key: "),
        "{why}"
    );
}

/// Asserts a certificate that is expired or not yet valid is refused with its period, and that a
/// key file any user can read is refused while one a group can read is not.
#[test]
fn an_expired_certificate_and_an_open_key_are_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-tls-lifetime");
    let ca = ca();
    let names = || vec![ip("127.0.0.1")];
    let past = leaf_with(dir.path(), "past", &ca, names(), |params| {
        params.not_before = date_time_ymd(2020, 1, 1);
        params.not_after = date_time_ymd(2021, 1, 1);
    });
    let future = leaf_with(dir.path(), "future", &ca, names(), |params| {
        params.not_before = date_time_ymd(3000, 1, 1);
        params.not_after = date_time_ymd(3001, 1, 1);
    });
    for files in [&past, &future] {
        let why = refusal(&files.cert, &files.key, "127.0.0.1:443");
        assert!(why.starts_with("--oci-tls-cert "), "{why}");
        assert!(why.contains("not valid now"), "{why}");
        assert!(why.contains("notBefore"), "{why}");
        assert!(why.contains("notAfter"), "{why}");
    }
    assert!(refusal(&past.cert, &past.key, "127.0.0.1:443").contains("2021"));

    let good = leaf(dir.path(), "good", &ca, names());
    let mode = |bits| std::fs::set_permissions(&good.key, Permissions::from_mode(bits));
    mode(0o640).expect("mode");
    acceptor(&good.cert, &good.key, addr("127.0.0.1:443")).expect("a group may read the key");
    for open in [0o644, 0o604, 0o666] {
        mode(open).expect("mode");
        let why = refusal(&good.cert, &good.key, "127.0.0.1:443");
        assert!(why.starts_with("--oci-tls-key "), "{why}");
        assert!(why.contains("any user can read the key"), "{why}");
    }
}

/// Asserts which errors a restart can mend: an unreadable file and an address that cannot be
/// bound; the rest are flags and files that are wrong as they are.
#[test]
fn only_unreadable_files_and_bind_errors_are_not_invalid_flags() {
    let io = || std::io::Error::from(std::io::ErrorKind::NotFound);
    let read = OciError::Read {
        flag: "--oci-tls-cert",
        path: PathBuf::from("c"),
        source: io(),
    };
    let bind = OciError::Bind {
        addr: addr("127.0.0.1:1"),
        source: io(),
    };
    assert!(!read.is_invalid_flags());
    assert!(!bind.is_invalid_flags());
    assert!(bind.to_string().starts_with("binding 127.0.0.1:1: "));
    let pem = OciError::Pem {
        flag: "--oci-tls-key",
        path: PathBuf::from("k"),
        why: "bad".to_owned(),
    };
    for invalid in [
        pem,
        OciError::Config {
            flags: "x",
            why: "z".to_owned(),
        },
        OciError::Missing("y".to_owned()),
    ] {
        assert!(invalid.is_invalid_flags(), "{invalid}");
    }
}

/// Asserts a subjectAltName address is read from 4 bytes or 16 and from no other length, and
/// that an empty list of names names no address.
#[test]
fn an_address_is_four_bytes_or_sixteen() {
    assert_eq!(ip_of(&[192, 0, 2, 7]), Some("192.0.2.7".parse().unwrap()));
    let v6 = "2001:db8::1".parse::<std::net::Ipv6Addr>().unwrap();
    assert_eq!(ip_of(&v6.octets()), Some(v6.into()));
    for length in [0, 3, 5, 15, 17] {
        assert_eq!(ip_of(&vec![1; length]), None, "{length}");
    }
    assert!(unnamed(&[], addr("0.0.0.0:1")).is_some());
    assert!(unnamed(&["192.0.2.7".parse().unwrap()], addr("0.0.0.0:1")).is_none());
}

/// Why `s3_acceptor` refuses these files for `listen`.
fn s3_refusal(cert: &Path, key: &Path, listen: &[&str]) -> String {
    let listen: Vec<SocketAddr> = listen.iter().map(|text| addr(text)).collect();
    let refused = s3_acceptor(cert, key, &listen).err();
    refused.expect("refused").to_string()
}

/// Asserts the S3 front accepts a certificate that names it by a DNS name, by an IP address or by
/// both: a DNS name is enough for any listen address (the wildcards and an address the
/// certificate does not name included), and without one the certificate must name each address.
#[test]
fn the_s3_front_takes_a_dns_name_an_ip_address_or_both() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-names");
    let ca = ca();
    let by_dns = leaf(dir.path(), "dns", &ca, vec![dns("s3.test")]);
    let by_ip = leaf(dir.path(), "ip", &ca, vec![ip("127.0.0.1"), ip("::1")]);
    let by_both = leaf(
        dir.path(),
        "both",
        &ca,
        vec![dns("s3.test"), ip("127.0.0.1")],
    );
    let other = vec![email("ops@s3.test"), dns("s3.test")];
    let other = leaf(dir.path(), "other", &ca, other);
    for listen in [
        "127.0.0.1:9000",
        "0.0.0.0:9000",
        "[::]:9000",
        "192.0.2.7:9000",
    ] {
        for files in [&by_dns, &by_both, &other] {
            s3_acceptor(&files.cert, &files.key, &[addr(listen)]).expect(listen);
        }
    }
    let both = [addr("127.0.0.1:9000"), addr("[::1]:9000")];
    s3_acceptor(&by_ip.cert, &by_ip.key, &both).expect("each address is named");
    s3_acceptor(&by_ip.cert, &by_ip.key, &[addr("0.0.0.0:9000")]).expect("any IP will do");
}

/// Asserts the S3 front refuses a certificate with no name for it: no subjectAltName at all (an
/// e-mail name is not one), or only IP addresses other than its own, with the S3 flag in the
/// message and the names the certificate does have.
#[test]
fn the_s3_front_refuses_a_certificate_that_does_not_name_it() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-unnamed");
    let ca = ca();
    let bare = leaf(dir.path(), "bare", &ca, vec![]);
    let mail = leaf(dir.path(), "mail", &ca, vec![email("ops@s3.test")]);
    let other = leaf(dir.path(), "other", &ca, vec![ip("192.0.2.8")]);
    for (files, listen) in [
        (&bare, "127.0.0.1:9000"),
        (&bare, "0.0.0.0:9000"),
        (&mail, "[::]:9000"),
        (&other, "192.0.2.7:9000"),
    ] {
        let why = s3_refusal(&files.cert, &files.key, &[listen]);
        assert!(why.starts_with("--tls-cert "), "{why}");
        assert!(why.contains("no IP subjectAltName"), "{why}");
    }
    let why = s3_refusal(
        &other.cert,
        &other.key,
        &["192.0.2.8:9000", "192.0.2.7:9000"],
    );
    assert!(
        why.contains("for 192.0.2.7 (it names [192.0.2.8])"),
        "{why}"
    );
}

/// Asserts the S3 front's refusals name its own flags: the file that is missing, a key that is
/// not a key, a key that any user can read, an expired certificate and a key that is another
/// certificate's, whose message is exactly the two flags and what rustls says.
#[test]
fn the_s3_front_names_its_own_flags_in_every_refusal() {
    let dir = TestDir::under(std::env::temp_dir(), "s3-tls-flags");
    let ca = ca();
    let names = || vec![dns("s3.test")];
    let good = leaf(dir.path(), "good", &ca, names());
    let stranger = leaf(dir.path(), "stranger", &ca, names());
    let expired = leaf_with(dir.path(), "expired", &ca, names(), |params| {
        params.not_before = date_time_ymd(2026, 9, 1);
        params.not_after = date_time_ymd(2026, 9, 2);
    });
    let missing = dir.path().join("missing");
    let text = dir.path().join("text");
    std::fs::write(&text, "not a PEM file\n").expect("write");
    std::fs::set_permissions(&text, Permissions::from_mode(0o600)).expect("mode");
    let listen = ["127.0.0.1:9000"];
    assert!(s3_refusal(&missing, &good.key, &listen).starts_with("--tls-cert "));
    assert!(s3_refusal(&good.cert, &missing, &listen).starts_with("--tls-key "));
    assert!(s3_refusal(&good.cert, &text, &listen).starts_with("--tls-key "));
    let why = s3_refusal(&expired.cert, &expired.key, &listen);
    assert!(why.starts_with("--tls-cert "), "{why}");
    assert!(why.contains("not valid now"), "{why}");
    std::fs::set_permissions(&good.key, Permissions::from_mode(0o644)).expect("mode");
    let why = s3_refusal(&good.cert, &good.key, &listen);
    assert!(why.starts_with("--tls-key "), "{why}");
    assert!(why.contains("any user can read the key"), "{why}");
    let why = s3_refusal(&good.cert, &stranger.key, &listen);
    assert!(why.starts_with("--tls-cert and --tls-key: "), "{why}");
}
