//! The signature check of the installer pkg: a Developer ID report passes, each way a report
//! differs from it is refused with its own line, and `pkgutil` is run as the check needs.

use std::path::Path;

use super::*;
use crate::release::macos_pkg::tests::FakeTools;
use crate::release::macos_pkg::{BuildPkg, PkgTools};

const IDENTITY: &str = "Developer ID Installer: Example Org (TEAM000000)";

/// What `pkgutil --check-signature` prints of a pkg signed by [`IDENTITY`] with a timestamp:
/// the fingerprint lines start with digits, as real ones do, and are not certificates.
const REPORT: &str = "\
Package \"tierstore-1.2.3-macos-arm64.pkg\":
   Status: signed by a developer certificate issued by Apple for distribution
   Signed with a trusted timestamp on: TIME
   Certificate Chain:
    1. Developer ID Installer: Example Org (TEAM000000)
       Expires: LATER
       SHA256 Fingerprint:
           05 14 2C 49 F1 29 5E 6E DA 6B CA ED 7E 2C 68 C5 BE 91 B5 A1 10 01
           F0 24 B0 B1 73 0E CB C7 FF 45
       ------------------------------------------------------------------------
    2. Developer ID Certification Authority
       Expires: LATER
       SHA256 Fingerprint:
           12 34 56 78 9A BC DE F0 12 34 56 78 9A BC DE F0 12 34 56 78 9A BC
       ------------------------------------------------------------------------
    3. Apple Root CA
       Expires: LATER
       SHA256 Fingerprint:
           B0 B1 73 0E CB C7 FF 45 05 14 2C 49 F1 29 5E 6E DA 6B CA ED 7E 2C

";

/// [`REPORT`] with the one occurrence of `from` replaced by `to`.
fn report_with(from: &str, to: &str) -> String {
    assert_eq!(REPORT.matches(from).count(), 1, "{from:?}");
    REPORT.replacen(from, to, 1)
}

const CHAIN_WANT: &str = "[\"Developer ID Installer: Example Org (TEAM000000)\", \
                          \"Developer ID Certification Authority\", \"Apple Root CA\"]";

fn chain_problem(got: &str) -> String {
    format!("the certificate chain is {got}, not {CHAIN_WANT}")
}

#[test]
fn a_timestamped_developer_id_signature_by_the_identity_passes_under_every_trusted_wording() {
    // Catches: the parser refusing the report a release's pkg gets, a fingerprint line read as a
    // certificate, or a trusted status of an earlier macOS refused.
    assert_eq!(problems(REPORT, IDENTITY), Vec::<String>::new());
    for status in TRUSTED_STATUSES {
        let report = report_with(TRUSTED_STATUSES[0], status);
        assert_eq!(
            problems(&report, IDENTITY),
            Vec::<String>::new(),
            "{status}"
        );
    }
}

#[test]
fn a_status_that_is_not_a_trusted_signature_is_refused_by_name() {
    // Catches: an untrusted, expired or missing signature passed because the line merely starts
    // with "signed".
    for status in [
        "no signature",
        "signed by untrusted certificate",
        "signed by a developer certificate issued by Apple for distribution (revoked)",
    ] {
        let report = report_with(TRUSTED_STATUSES[0], status);
        assert_eq!(
            problems(&report, IDENTITY),
            vec![format!(
                "the status is {status:?}, not a trusted signature ({TRUSTED_STATUSES:?})"
            )],
            "{status}"
        );
    }
}

#[test]
fn no_status_line_or_two_are_refused() {
    // Catches: a report with no status passed, or one that says two things judged by its first.
    let line = format!("   Status: {}\n", TRUSTED_STATUSES[0]);
    assert_eq!(
        problems(&report_with(&line, ""), IDENTITY),
        vec!["pkgutil printed 0 status lines, not one".to_owned()]
    );
    let two = report_with(&line, &format!("{line}   Status: no signature\n"));
    assert_eq!(
        problems(&two, IDENTITY),
        vec!["pkgutil printed 2 status lines, not one".to_owned()]
    );
}

#[test]
fn a_signature_without_a_trusted_timestamp_is_refused() {
    // Catches: a pkg signed with `--timestamp=none`, whose signature lapses with the certificate.
    let report = report_with("   Signed with a trusted timestamp on: TIME\n", "");
    assert_eq!(
        problems(&report, IDENTITY),
        vec![format!(
            "the signature has no trusted timestamp ({TIMESTAMP:?})"
        )]
    );
}

#[test]
fn a_chain_of_another_identity_or_shape_is_refused_with_the_chain_it_read() {
    // Catches: a pkg signed by another team's or another kind of identity, a chain with the
    // root missing, an extra certificate, the order changed, or a report with no chain at all.
    let other = report_with("(TEAM000000)\n", "(TEAM999999)\n");
    assert_eq!(
        problems(&other, IDENTITY),
        vec![chain_problem(
            "[\"Developer ID Installer: Example Org (TEAM999999)\", \
             \"Developer ID Certification Authority\", \"Apple Root CA\"]"
        )]
    );
    let application = report_with("1. Developer ID Installer:", "1. Developer ID Application:");
    assert_eq!(
        problems(&application, IDENTITY),
        vec![chain_problem(
            "[\"Developer ID Application: Example Org (TEAM000000)\", \
             \"Developer ID Certification Authority\", \"Apple Root CA\"]"
        )]
    );
    let no_root = report_with("3. Apple Root CA", "Apple Root CA");
    assert_eq!(
        problems(&no_root, IDENTITY),
        vec![chain_problem(
            "[\"Developer ID Installer: Example Org (TEAM000000)\", \
             \"Developer ID Certification Authority\"]"
        )]
    );
    let extra = format!("{REPORT}    4. Another CA\n");
    assert_eq!(
        problems(&extra, IDENTITY),
        vec![chain_problem(
            "[\"Developer ID Installer: Example Org (TEAM000000)\", \
             \"Developer ID Certification Authority\", \"Apple Root CA\", \"Another CA\"]"
        )]
    );
    let swapped = report_with(
        "2. Developer ID Certification Authority",
        "2. Apple Root CA",
    )
    .replacen(
        "3. Apple Root CA",
        "3. Developer ID Certification Authority",
        1,
    );
    assert_eq!(
        problems(&swapped, IDENTITY),
        vec![chain_problem(
            "[\"Developer ID Installer: Example Org (TEAM000000)\", \
             \"Apple Root CA\", \"Developer ID Certification Authority\"]"
        )]
    );
    let unnumbered = report_with("Certificate Chain:", "Chain:");
    assert_eq!(problems(&unnumbered, IDENTITY), vec![chain_problem("[]")]);
    let dotted = report_with("    3. Apple Root CA", "    . Apple Root CA");
    assert_eq!(
        problems(&dotted, IDENTITY),
        vec![chain_problem(
            "[\"Developer ID Installer: Example Org (TEAM000000)\", \
             \"Developer ID Certification Authority\"]"
        )]
    );
}

#[test]
fn an_unsigned_pkg_is_refused_for_every_reason_at_once() {
    // Catches: the check stopping at its first reason, so a fix of one hides the next.
    let report = "Package \"x.pkg\":\n   Status: no signature\n";
    assert_eq!(
        problems(report, IDENTITY),
        vec![
            format!(
                "the status is \"no signature\", not a trusted signature ({TRUSTED_STATUSES:?})"
            ),
            format!("the signature has no trusted timestamp ({TIMESTAMP:?})"),
            chain_problem("[]"),
        ]
    );
}

#[test]
fn check_reads_the_pkg_through_pkgutil_and_names_it_in_every_line() {
    // Catches: the check reading another file than the one named, a refusal that does not say
    // which pkg, or a pass that does not say by whom.
    let pkg = Path::new("dist/tierstore-1.2.3-macos-arm64.pkg");
    let fake = FakeTools::signing(Ok(REPORT.to_owned()));
    assert_eq!(
        check(&fake, pkg, IDENTITY),
        Ok(vec![format!(
            "dist/tierstore-1.2.3-macos-arm64.pkg: signed by {IDENTITY} under \
             Developer ID Certification Authority and Apple Root CA, with a trusted timestamp"
        )])
    );
    assert_eq!(fake.signature_calls.borrow().as_slice(), [pkg.to_owned()]);

    let untimed = FakeTools::signing(Ok(report_with(
        "   Signed with a trusted timestamp on: TIME\n",
        "",
    )));
    assert_eq!(
        check(&untimed, pkg, IDENTITY),
        Err(Error::Refused(vec![format!(
            "dist/tierstore-1.2.3-macos-arm64.pkg: the signature has no trusted timestamp \
             ({TIMESTAMP:?})"
        )]))
    );
}

#[test]
fn check_refuses_a_pkgutil_failure() {
    // Catches: a pkg pkgutil refuses (exit 1, as for an unsigned one) passed because its output
    // went unread.
    let failed = || Error::Failed("pkgutil --check-signature x.pkg: no signature".to_owned());
    let fake = FakeTools::signing(Err(failed()));
    assert_eq!(check(&fake, Path::new("x.pkg"), IDENTITY), Err(failed()));
}

#[test]
fn an_identity_that_is_not_developer_id_installer_is_refused_before_pkgutil_runs() {
    // Catches: the release checking against an application identity, an Apple Distribution one
    // or an empty name, which a pkg signed by anything Apple issues would then match.
    for identity in [
        "Developer ID Application: Example Org (TEAM000000)",
        "3rd Party Mac Developer Installer: Example Org (TEAM000000)",
        "Developer ID Installer: ",
        "",
    ] {
        let fake = FakeTools::signing(Ok(REPORT.to_owned()));
        assert_eq!(
            check(&fake, Path::new("x.pkg"), identity),
            Err(Error::Failed(format!(
                "--identity {identity:?} is not a Developer ID Installer identity \
                 ({IDENTITY_PREFIX}<name> (<team>))"
            ))),
            "{identity}"
        );
        assert!(fake.signature_calls.borrow().is_empty(), "{identity}");
    }
}

#[test]
fn the_program_runs_pkgutil_check_signature_and_keeps_what_it_printed() {
    // Catches: another pkgutil verb, the pkg's path lost, the report of a pkgutil that exited
    // other than 0 passed, or a pkgutil that could not start passed. The stand-ins are programs
    // every system has: `echo` prints its arguments, `false` fails.
    let tools = |program: &str| PkgTools {
        pkgutil: program.into(),
        ..PkgTools::system()
    };
    assert_eq!(
        tools("echo").check_signature(Path::new("dist/a.pkg")),
        Ok("--check-signature dist/a.pkg\n".to_owned())
    );
    assert_eq!(
        tools("false").check_signature(Path::new("dist/a.pkg")),
        Err(Error::Failed(
            "false --check-signature dist/a.pkg: \n".to_owned()
        ))
    );
    assert!(matches!(
        tools("no-such-pkgutil").check_signature(Path::new("dist/a.pkg")),
        Err(Error::Spawn { what, .. }) if what == "no-such-pkgutil --check-signature dist/a.pkg"
    ));
}

#[test]
fn a_path_that_is_not_utf8_is_refused_before_pkgutil_runs() {
    // Catches: a lossy path handed to pkgutil, which would check another file.
    use std::os::unix::ffi::OsStrExt;
    let path = Path::new(std::ffi::OsStr::from_bytes(b"dist/\xff.pkg"));
    assert!(matches!(
        PkgTools::system().check_signature(path),
        Err(Error::Failed(why)) if why.ends_with("is not UTF-8")
    ));
}
