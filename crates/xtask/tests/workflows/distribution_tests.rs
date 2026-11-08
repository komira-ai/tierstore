//! The tap tarballs and Debian and RPM packages in `artifacts.yml` ([`super::platforms`]): the files of the
//! repository pass, their names are the ones the Homebrew tap and apt take, and each way of leaving
//! one out, making it for another platform, leaving it unread by `dpkg`, or signing it under
//! another name or as no subject of its own is named.

use super::platforms::{BUILDS, artifacts_platform_problems, attest_commands, build_commands};
use super::platforms_tests::each_named;
use xtask::package::Platform;
use xtask::release::ARTIFACTS;

fn platforms() -> Vec<&'static str> {
    Platform::ALL.iter().map(|p| p.name()).collect()
}

fn artifacts_problems_of(text: &str) -> Vec<String> {
    artifacts_platform_problems(text, &platforms(), &ARTIFACTS)
}

#[test]
fn the_tap_debian_and_rpm_names_are_the_ones_the_tap_apt_and_dnf_take() {
    // Catches: a tap tarball named linux-arm64 (the tap's guard takes linux-aarch64 and refuses
    // the pull request), a Debian package for macOS, a file name whose version runs into the
    // architecture (`$RELEASE_VERSION_amd64` is another variable), and a platform whose Debian
    // bundle copy or tap copy is left out of the signing commands.
    let targets: Vec<(&str, &str, Option<&str>, Option<&str>)> = BUILDS
        .iter()
        .map(|b| (b.platform, b.tap, b.deb, b.rpm))
        .collect();
    assert_eq!(
        targets,
        [
            (
                "linux-x86_64",
                "linux-x86_64",
                Some("amd64"),
                Some("x86_64")
            ),
            (
                "linux-arm64",
                "linux-aarch64",
                Some("arm64"),
                Some("aarch64")
            ),
            ("macos-arm64", "macos-arm64", None, None),
        ]
    );
    let linux = build_commands(&BUILDS[1]);
    assert!(
        linux.contains(
            &"dpkg-deb --info \"dist/tierstore_${RELEASE_VERSION}_arm64.deb\"".to_owned()
        )
    );
    assert!(
        linux.contains(
            &"cargo xtask release rpm-info \"dist/tierstore-${RELEASE_VERSION}-1.aarch64.rpm\""
                .to_owned()
        )
    );
    assert!(
        !build_commands(&BUILDS[2])
            .iter()
            .any(|c| c.contains("deb") || c.contains("rpm"))
    );
    let attest = attest_commands(&platforms());
    assert_eq!(attest.len(), 2 + 3 + 3 + 3 + 2 + 2);
    assert_eq!(
        attest[attest.len() - 4..],
        [
            "cp \"$BUNDLE\" \"dist/tierstore_${RELEASE_VERSION}_amd64.deb.sigstore.json\"",
            "cp \"$BUNDLE\" \"dist/tierstore_${RELEASE_VERSION}_arm64.deb.sigstore.json\"",
            "cp \"$BUNDLE\" \"dist/tierstore-${RELEASE_VERSION}-1.x86_64.rpm.sigstore.json\"",
            "cp \"$BUNDLE\" \"dist/tierstore-${RELEASE_VERSION}-1.aarch64.rpm.sigstore.json\"",
        ]
    );
}

/// Changes to the build jobs and `attest` that leave a tap tarball or a Debian or RPM package out, make
/// it for another platform, leave it unread by `dpkg`, or sign it under another name or as no
/// subject of its own.
const DISTRIBUTION_DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "linux-x86_64",
        "      - name: The tarball named as the Homebrew tap downloads it\n        run: cargo xtask release tap-tarball dist --commit \"$GITHUB_SHA\" --version \"$RELEASE_VERSION\" --platform linux-x86_64\n",
        "",
        "linux-x86_64: does not run `cargo xtask release tap-tarball dist --commit \"$GITHUB_SHA\" --version \"$RELEASE_VERSION\" --platform linux-x86_64`",
    ),
    (
        "macos-arm64",
        "      - name: The tarball named as the Homebrew tap downloads it\n        run: cargo xtask release tap-tarball dist --commit \"$GITHUB_SHA\" --version \"$RELEASE_VERSION\" --platform macos-arm64\n",
        "",
        "macos-arm64: does not run `cargo xtask release tap-tarball dist --commit \"$GITHUB_SHA\" --version \"$RELEASE_VERSION\" --platform macos-arm64`",
    ),
    (
        "linux-arm64",
        "--platform linux-arm64 --licenses",
        "--platform linux-x86_64 --licenses",
        "linux-arm64: does not run `cargo xtask release deb --binary",
    ),
    (
        "linux-arm64",
        "dpkg-deb --contents \"dist/tierstore_${RELEASE_VERSION}_arm64.deb\"",
        "dpkg-deb --contents \"dist/tierstore_${RELEASE_VERSION}_amd64.deb\"",
        "linux-arm64: does not run `dpkg-deb --contents \"dist/tierstore_${RELEASE_VERSION}_arm64.deb\"`",
    ),
    (
        "linux-x86_64",
        "      - name: dpkg reads the control file of the Debian package\n        run: dpkg-deb --info \"dist/tierstore_${RELEASE_VERSION}_amd64.deb\"\n",
        "",
        "linux-x86_64: does not run `dpkg-deb --info \"dist/tierstore_${RELEASE_VERSION}_amd64.deb\"`",
    ),
    (
        "attest",
        "subject-path: dist/tierstore-${{ needs.version.outputs.version }}-linux-aarch64.tar.gz",
        "subject-path: dist/tierstore-${{ needs.sums.outputs.package-version }}-linux-aarch64.tar.gz",
        "attest: no provenance attestation `provenance-tap-linux-aarch64` of dist/tierstore-${{ needs.version.outputs.version }}-linux-aarch64.tar.gz alone",
    ),
    (
        "attest",
        "- id: provenance-deb-amd64\n",
        "- id: provenance-deb-amd64-other\n",
        "attest: no provenance attestation `provenance-deb-amd64` of dist/tierstore_${{ needs.version.outputs.version }}_amd64.deb alone",
    ),
    (
        "attest",
        "run: cp \"$BUNDLE\" \"dist/tierstore_${RELEASE_VERSION}_arm64.deb.sigstore.json\"",
        "run: cp \"$BUNDLE\" \"dist/tierstore_${RELEASE_VERSION}_amd64.deb.sigstore.json\"",
        "attest: the bundle of `provenance-deb-arm64` is not copied as",
    ),
    (
        "attest",
        "          RELEASE_VERSION: ${{ needs.version.outputs.version }}\n",
        "          RELEASE_VERSION: ${{ needs.sums.outputs.package-version }}\n",
        "attest: the bundle of `provenance-tap-linux-x86_64` is not copied as",
    ),
    (
        "linux-x86_64",
        "      - name: The RPM package\n        run: cargo xtask release rpm --binary \"target/$TARGET/release/tierstore\" --version \"$RELEASE_VERSION\" --platform linux-x86_64 --licenses \"$RUNNER_TEMP/THIRD-PARTY-LICENSES.txt\" --out dist\n",
        "",
        "linux-x86_64: does not run `cargo xtask release rpm --binary",
    ),
    (
        "linux-arm64",
        "--platform linux-arm64 --licenses \"$RUNNER_TEMP/THIRD-PARTY-LICENSES.txt\" --out dist\n      - name: The RPM package reads back",
        "--platform linux-x86_64 --licenses \"$RUNNER_TEMP/THIRD-PARTY-LICENSES.txt\" --out dist\n      - name: The RPM package reads back",
        "linux-arm64: does not run `cargo xtask release rpm --binary",
    ),
    (
        "linux-arm64",
        "      - name: The RPM package reads back\n        run: cargo xtask release rpm-info \"dist/tierstore-${RELEASE_VERSION}-1.aarch64.rpm\"\n",
        "",
        "linux-arm64: does not run `cargo xtask release rpm-info \"dist/tierstore-${RELEASE_VERSION}-1.aarch64.rpm\"`",
    ),
    (
        "attest",
        "- id: provenance-rpm-aarch64\n",
        "- id: provenance-rpm-aarch64-other\n",
        "attest: no provenance attestation `provenance-rpm-aarch64` of dist/tierstore-${{ needs.version.outputs.version }}-1.aarch64.rpm alone",
    ),
    (
        "attest",
        "run: cp \"$BUNDLE\" \"dist/tierstore-${RELEASE_VERSION}-1.x86_64.rpm.sigstore.json\"",
        "run: cp \"$BUNDLE\" \"dist/tierstore-${RELEASE_VERSION}-1.aarch64.rpm.sigstore.json\"",
        "attest: the bundle of `provenance-rpm-x86_64` is not copied as",
    ),
    (
        "attest",
        "    needs: [version, sums]\n",
        "    needs: [sums]\n",
        "attest: does not need version",
    ),
];

#[test]
fn each_way_of_leaving_out_or_misnaming_a_tap_tarball_or_a_package_is_named() {
    // Catches: a distribution file that is not made, is made for the wrong platform, is not read
    // by dpkg, is signed with a version that is not the release's, has no provenance attestation
    // of its own, or whose bundle is copied under another file's name.
    each_named("artifacts.yml", artifacts_problems_of, DISTRIBUTION_DEFECTS);
}
