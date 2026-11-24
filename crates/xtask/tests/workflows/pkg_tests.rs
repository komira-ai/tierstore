//! The path of the signed macOS installer through `artifacts.yml` and `release.yml`
//! ([`super::pkg`]): the file passes, and each way of building, signing, checking or releasing
//! it wrongly is named.

use super::platforms::pull_request_runner_problems;
use super::platforms_tests::{
    MAC, artifacts_problems_of, each_named, in_job, release_problems_of, runner_line,
};
use super::text_of;

const IDENT_LINE: &str =
    "      IDENTITY: \"Developer ID Installer: Komira AI, Inc. (U7RTX4XD3X)\"\n";
const CHECKOUT: &str =
    "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n";
const PUSH_ONLY: &str =
    "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n";
const SIGN_PKG_LINE: &str = "    if: ${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' && (needs.macos-arm64.result == 'success' || needs.macos-arm64-farm.result == 'success') }}\n";

/// Changes to the macOS installer's path through `artifacts.yml`: its build, the one job that
/// holds the identity (`sign-pkg`), the job that checks it (`check-pkg`) and the files that take
/// it into the sums and the signing.
fn pkg_defects() -> Vec<(&'static str, String, String, &'static str)> {
    let d = |job: &'static str, replace: &str, with: &str, says: &'static str| {
        (job, replace.to_owned(), with.to_owned(), says)
    };
    vec![
        d(
            MAC,
            "      - name: Expand the pkg\n        run: pkgutil --expand \"pkg/tierstore-$RELEASE_VERSION-macos-arm64.pkg\" \"$RUNNER_TEMP/pkg-expanded\"\n",
            "",
            "macos-arm64: does not run `pkgutil --expand",
        ),
        d(
            MAC,
            "      - name: The pkg installs exactly what its builder writes\n        run: cargo xtask release check-macos-pkg \"$RUNNER_TEMP/pkg-expanded\" --version \"$RELEASE_VERSION\" --binary \"target/$TARGET/release/tierstore\"\n",
            "",
            "macos-arm64: does not run `cargo xtask release check-macos-pkg",
        ),
        d(
            MAC,
            "--binary \"target/$TARGET/release/tierstore\" --stage",
            "--binary \"target/release/tierstore\" --stage",
            "macos-arm64: does not run `cargo xtask release macos-pkg",
        ),
        d(
            MAC,
            "--out pkg\n",
            "--out dist\n",
            "macos-arm64: does not run `cargo xtask release macos-pkg",
        ),
        d(
            MAC,
            "name: pkg-unsigned-macos-arm64",
            "name: pkg-unsigned",
            "macos-arm64: uploads",
        ),
        d(MAC, "path: pkg/\n", "path: dist/\n", "macos-arm64: uploads"),
        d(
            "sign-pkg",
            IDENT_LINE,
            "      IDENTITY: \"Developer ID Installer: Someone Else (AAAAAAAAAA)\"\n",
            "sign-pkg: IDENTITY is",
        ),
        d(
            "sign-pkg",
            "    environment: pkg-signing\n",
            "",
            "sign-pkg: is not in the environment pkg-signing",
        ),
        d(
            "sign-pkg",
            "    steps:\n",
            &format!("    steps:\n{CHECKOUT}"),
            "sign-pkg: uses actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1, which is not one of its actions",
        ),
        d(
            "sign-pkg",
            "    steps:\n",
            "    steps:\n      - run: cargo xtask --help\n",
            "sign-pkg: runs `cargo xtask --help`, which is not one of its commands",
        ),
        d(
            "sign-pkg",
            "      - run: mkdir dist\n",
            "      - run: mkdir dist\n      - run: cargo build -p tierstore\n",
            "sign-pkg: runs `cargo build -p tierstore`, which is not one of its commands",
        ),
        d(
            "sign-pkg",
            "      contents: read\n",
            "      contents: write\n",
            "sign-pkg: permissions are",
        ),
        d(
            "sign-pkg",
            "      contents: read\n",
            "      contents: read\n      id-token: write\n",
            "sign-pkg: id-token: write is held",
        ),
        d(
            "sign-pkg",
            SIGN_PKG_LINE,
            "    if: ${{ !cancelled() && github.event_name == 'push' && (needs.macos-arm64.result == 'success' || needs.macos-arm64-farm.result == 'success') }}\n",
            "sign-pkg: runs under",
        ),
        d(
            "sign-pkg",
            SIGN_PKG_LINE,
            "    if: ${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n",
            "sign-pkg: runs under",
        ),
        d(
            "sign-pkg",
            "needs: [version, macos-arm64, macos-arm64-farm]",
            "needs: [version, macos-arm64]",
            "sign-pkg: does not need exactly",
        ),
        d(
            "sign-pkg",
            "runs-on: macos-15",
            "runs-on: ubuntu-24.04",
            "sign-pkg: does not run on macos-15",
        ),
        d(
            "sign-pkg",
            " --timestamp",
            "",
            "sign-pkg: does not run `productsign",
        ),
        d(
            "sign-pkg",
            "        if: ${{ always() }}\n        run: security delete-keychain",
            "        run: security delete-keychain",
            "sign-pkg: the steps under `if: ${{ always() }}` run",
        ),
        d(
            "sign-pkg",
            "      - run: mkdir dist\n",
            "      - if: false\n        run: mkdir dist\n",
            "sign-pkg: a step has an `if:` other than `${{ always() }}`",
        ),
        d(
            "sign-pkg",
            "          name: pkg-unsigned-macos-arm64\n          path: unsigned",
            "          name: pkg-unsigned-macos-arm64\n          path: dist",
            "sign-pkg: downloads",
        ),
        d(
            "sign-pkg",
            "name: pkg-signed-macos-arm64",
            "name: assets-pkg-macos-arm64",
            "sign-pkg: uploads",
        ),
        d(
            "check-pkg",
            "    runs-on: macos-15\n",
            "    environment: pkg-signing\n    runs-on: macos-15\n",
            "check-pkg: is in an environment",
        ),
        d(
            "check-pkg",
            "      - run: rustup toolchain install\n",
            "      - run: rustup toolchain install\n        env:\n          P12: ${{ secrets.MACOS_INSTALLER_P12 }}\n",
            "check-pkg: reads a secret; sign-pkg alone does",
        ),
        d(
            "check-pkg",
            "      - name: Signing changed no file of the pkg\n        run: diff -r \"$RUNNER_TEMP/unsigned-expanded\" \"$RUNNER_TEMP/signed-expanded\"\n",
            "",
            "check-pkg: does not run `diff -r",
        ),
        d(
            "check-pkg",
            "--identity \"$IDENTITY\"",
            "--identity x",
            "check-pkg: does not run `cargo xtask release check-pkg-signature",
        ),
        d(
            "check-pkg",
            "      - name: The signature is the identity's, under Apple's chain, with a trusted timestamp\n        run: cargo xtask release check-pkg-signature \"dist/tierstore-$RELEASE_VERSION-macos-arm64.pkg\" --identity \"$IDENTITY\"\n",
            "",
            "check-pkg: does not run `cargo xtask release check-pkg-signature",
        ),
        d(
            "check-pkg",
            "name: assets-pkg-macos-arm64",
            "name: assets-pkg",
            "check-pkg: uploads",
        ),
        d(
            "check-pkg",
            PUSH_ONLY,
            "",
            "check-pkg: runs under `if: None`",
        ),
        d(
            "check-pkg",
            "          name: pkg-signed-macos-arm64\n          path: dist",
            "          name: pkg-signed-macos-arm64\n          path: signed",
            "check-pkg: downloads",
        ),
        d("check-pkg", IDENT_LINE, "", "check-pkg: IDENTITY is None"),
        d(
            "sums",
            "      contents: read\n    # The SHA-256",
            "      contents: read\n    env:\n      P12: ${{ secrets.MACOS_INSTALLER_P12 }}\n    # The SHA-256",
            "sums: reads a secret; sign-pkg alone does",
        ),
        d(
            "sums",
            ", macos-arm64, macos-arm64-farm, check-pkg]",
            ", macos-arm64]",
            "sums: does not need check-pkg",
        ),
        d(
            "sums",
            " && (needs.check-pkg.result == 'success' || (github.event_name != 'push' && needs.check-pkg.result == 'skipped'))",
            "",
            "sums: runs under",
        ),
        d(
            "sums",
            "needs.check-pkg.result == 'success' ||",
            "needs.check-pkg.result != 'failure' ||",
            "sums: runs under",
        ),
        d(
            "sums",
            "pattern: assets-*",
            "pattern: assets-linux-*",
            "sums: no download takes assets-pkg-macos-arm64",
        ),
        d(
            "attest",
            "pattern: assets-*",
            "pattern: assets-linux-*",
            "attest: no download takes assets-pkg-macos-arm64",
        ),
        d(
            "attest",
            "id: provenance-pkg-macos-arm64",
            "id: provenance-pkg",
            "attest: no provenance attestation `provenance-pkg-macos-arm64`",
        ),
        d(
            "attest",
            "subject-path: dist/tierstore-${{ needs.version.outputs.version }}-macos-arm64.pkg",
            "subject-path: dist/tierstore-${{ needs.version.outputs.version }}-linux-arm64.tar.gz",
            "attest: no provenance attestation `provenance-pkg-macos-arm64`",
        ),
        d(
            "attest",
            "-macos-arm64.pkg.sigstore.json\"",
            "-macos-arm64.tar.gz.sigstore.json\"",
            "attest: the bundle of `provenance-pkg-macos-arm64` is not copied",
        ),
    ]
}

#[test]
fn each_way_the_macos_installer_could_be_unsigned_unchecked_or_left_out_of_the_release_is_named() {
    // Asserts: the identity is held by `sign-pkg` alone, which runs no repository code, only on a
    // push to main, with `contents: read`; the job that checks the signature holds no secret and
    // no environment and runs exactly its checks; the installer is built, read back, summed,
    // attested with its bundle and taken by the sums and attest downloads; and each
    // change that breaks one of those is named by the lint.
    let text = text_of("artifacts.yml");
    for (job, replace, with, says) in pkg_defects() {
        let problems = artifacts_problems_of(&in_job(&text, job, &replace, &with));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{job}: {replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn a_release_yml_that_does_not_take_the_signed_installer_where_it_checks_it_is_named() {
    // Asserts: release.yml's verify job downloads the artifact of the signed installer with the
    // other platforms' assets.
    each_named(
        "release.yml",
        release_problems_of,
        &[(
            "verify",
            "pattern: assets-*",
            "pattern: assets-linux-*",
            "release.yml verify: no download takes assets-pkg-macos-arm64",
        )],
    );
}

#[test]
fn a_macos_job_of_a_pull_request_is_accepted_under_a_push_to_main_only_and_named_without_it() {
    // Asserts: sign-pkg and check-pkg run on macOS in a workflow a pull request starts, which
    // the rule allows because their condition is never true for a pull request; weakening it to
    // anything else is named as a macOS job of a pull request.
    let text = text_of("artifacts.yml");
    assert_eq!(
        pull_request_runner_problems("artifacts.yml", &text),
        Vec::<String>::new()
    );
    for (job, line) in [("sign-pkg", SIGN_PKG_LINE), ("check-pkg", PUSH_ONLY)] {
        let weaker = in_job(
            &text,
            job,
            line,
            "    if: ${{ github.ref == 'refs/heads/main' }}\n",
        );
        assert_eq!(
            pull_request_runner_problems("artifacts.yml", &weaker),
            [runner_line("artifacts.yml", job, "[\"macos-15\"]")],
            "{job}"
        );
    }
}
