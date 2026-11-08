# CI and releases

## The workflows

| file | when | what |
|---|---|---|
| `ci.yml` | pull request, push to `main` | `lint`: format, cargo-deny (licences, bans, sources), clippy, the workflow lint. `test`: every test under cargo-llvm-cov with branch coverage, and the 99% gate on lines and branches of every crate (`crates/coverage-gate`). |
| `advisories.yml` | daily, push to `main` | `cargo deny check advisories`; the result is kept in one issue, and the run is green whatever it finds. |
| `artifacts.yml` | pull request, push to `main`; by hand | builds `tierstore` (`--features server`) for linux-x86_64, linux-arm64 and macos-arm64, each on a GitHub-hosted runner of its platform, packages it with its SBOM and as a hostkeeper package (and, for the Homebrew tap and apt, as a tap-named tarball and, on Linux, a Debian package and an RPM package), writes the release index of the hostkeeper packages, makes `SHA256SUMS`. The macos-arm64 job also runs the tests of `tierstore`, `objstore-s3` and `tierstore-testkit` on macOS, the update hook's among them (both of its arms are built and tested on every system, each against fakes of its service manager, and the macOS packaging). This workflow runs no macOS job for a pull request (`macos.yml` does, for every pull request that changes a path the macOS build reads): for a pull request its macos-arm64 job is skipped and the sums cover the Linux files; a push to `main` builds every platform, and a run by hand (`gh workflow run artifacts.yml --ref <branch>`) builds and sums every platform of a branch and signs nothing. On a push to `main` it also signs (build provenance and SBOM attestations) and checks what it signed. |
| `macos.yml` | pull request | `changes` (Linux) lists the paths the pull request changes (its merge commit against the base, a rename as its old and new path) and judges them with `cargo xtask macos-paths`: the macOS job is not needed when every one is documentation (`docs/`, a top-level `*.md`), `LICENSE`, the lint configuration (`clippy.toml`, `deny.toml`, `rustfmt.toml`), an issue or pull request template, or a workflow that puts nothing on macOS (the list is `SKIPPABLE_DIRS` and `SKIPPABLE_FILES` in `crates/xtask/src/macos_paths.rs`); any other path, a path that is not UTF-8, or a list that is empty or cannot be read runs it. `version` (Linux) computes the version the build is given, as artifacts.yml's `version` job does. `macos` runs on `macos-15` unless `changes` passed and answered that it is not needed: every test of the workspace (`cargo test --workspace --features tierstore/s3`), then the commands of artifacts.yml's macos-arm64 job (build, SBOM, licence notice, tarball, hostkeeper package unpacked with hostkeeper's unpacker, code signature), uploading and signing nothing, with `contents: read` alone and no secret or token. It restores artifacts.yml's macOS cache and saves none. It is not named `macos-arm64`: a pull request's run of artifacts.yml skips its job of that name, and a skipped job passes as a check. If `version` fails, `macos` is skipped and the run fails on `version`. `macos-checks` (Linux) runs after the three whatever they did, a cancelled run included, and passes only when `version` and `macos` passed, or `macos` was skipped on that answer of a `changes` that passed. A newer push to a pull request cancels its older run. |
| `release.yml` | when `ci` or `artifacts` finishes on a push to `main` (automatic, see Releases); by hand | `plan` decides which commit of `main` is released and as which tag (the newest whose build recorded that version); `verify` and `publish` attach the files that commit's push built and signed to a GitHub release, after checking them again; `channel-upload` and `tap` then put the release on tierstore.dev and in the Homebrew tap (see The public channels). Builds nothing of tierstore. |
| `tierstore-site.yml` | pull request, push to `main`, when the site changes | typecheck, tests and build of tierstore.dev. |
| `tierstore-site-publish.yml` | push to `main` / by hand | builds the site again; uploads it only when the repository variable `TIERSTORE_SITE_PUBLISH` is `true`. |
| `hostkeeper-desired.yml` | by hand, from `main` | sets the desired version of tierstore on the hostkeeper coordinator to a published release and waits for the fleet; skipped while the repository variable `HOSTKEEPER_COORDINATOR_URL` is empty. See [Setting the desired version on the hostkeeper coordinator](#setting-the-desired-version-on-the-hostkeeper-coordinator). |

Every workflow is held to the same rules by `crates/xtask/tests/workflows`: every `uses:` is a
40-digit commit SHA with a version comment, `permissions: {}` at the top, no
`pull_request_target`, and `id-token`, `attestations`, `contents` and `issues` written only by the
one job that needs each, under the exact `if:` the lint lists for it (`attestations`, and the
`id-token` of the signing job, only on a push to `main`).

`main` has no branch protection rule, so GitHub requires no check by name. A pull request is
merged only when every check of its head commit has passed. The macOS checks are made to fit that:
every pull request starts `macos.yml`, whose macOS job is skipped only for a pull request
`changes` found it is not needed, and `macos-checks`, never skipped, fails when the macOS job was
needed and did not pass. A rule requiring checks by name needs `macos-checks` alone of this
workflow (a skipped `macos` passes as a check). The macOS job is not named `macos-arm64`, the
name of artifacts.yml's macOS job, which a pull request's run of artifacts.yml skips (a check of
that name would pass with no macOS run behind it).

## The release files

For a commit `<commit>` (the full SHA) and each platform (`linux-x86_64`, `linux-arm64`, `macos-arm64`):

- `tierstore-<commit>-<platform>.tar.gz`: the binary `tierstore` and `THIRD-PARTY-LICENSES.txt`, the
  same bytes on every build of one commit. The notice lists every crate the binary links that is
  not of this repository, with the licence it declares and the text of each licence file in its top directory
  (`LICENSE*`, `LICENCE*`, `COPYING*`, `NOTICE*`, `UNLICENSE*`) and of its declared `license_file`
  (no subdirectory is searched otherwise)
  (`cargo xtask licenses`, from `cargo metadata` of the build's crate, `server` feature and
  target; `package --licenses` puts it in the tarball). Procedural macros (not linked) are left out; a crate that ships no
  licence file is listed with its declared licence and says so; a crate that declares no licence
  fails the build job. A host extracts the binary by its name
  (`tar -xzf <tarball> tierstore`), which the notice does not change;
- `tierstore-<commit>-<platform>.cdx.json`: its CycloneDX SBOM;
- `tierstore-<commit>-<platform>.tar.gz.sigstore.json`: the bundle of the tarball's own provenance
  attestation, for an offline check;
- `tierstore-<version>-<platform>.tar.zst`: the hostkeeper package of the same binary (see "The
  hostkeeper package" below), where `<version>` is what `tierstore --version` prints after the
  name, `X.Y.Z+<12 hex>`;
- `tierstore-<version>-<platform>.tar.zst.sigstore.json`: the bundle of the package's own
  provenance attestation, which hostkeeper verifies the package with, offline;
- `index.json`: the release index of the three hostkeeper packages, hostkeeper's format: for each
  platform its file, size and SHA-256;
- `tierstore-<version>-<target>.tar.gz`, where `<version>` is the release's `X.Y.Z` and `<target>` is
  `macos-arm64`, `linux-aarch64` or `linux-x86_64`: the tarball the Homebrew tap's formula
  downloads (see "The tap tarball, the Debian package and the RPM package" below). It is a copy of the platform's
  commit-named tarball, so it holds the same bytes, and has its own
  `tierstore-<version>-<target>.tar.gz.sigstore.json`;
- `tierstore_<version>_<arch>.deb`, with `<arch>` `amd64` (linux-x86_64) or `arm64` (linux-arm64):
  the Debian package of the binary, and its `.deb.sigstore.json`; there is none for macOS;
- `tierstore-<version>-1.<arch>.rpm`, with `<arch>` `x86_64` (linux-x86_64) or `aarch64`
  (linux-arm64): the RPM package of the binary, and its `.rpm.sigstore.json`; there is none for
  macOS;
- `SHA256SUMS`: the SHA-256 of the tarballs, SBOMs, hostkeeper packages, `index.json`, tap tarballs,
  Debian packages and RPM packages.

The release assets are what an operator copies into hostkeeper's release channel, as
`tierstore/<version>/index.json` and the packages and bundles beside it.

The signer of every attestation is `.github/workflows/artifacts.yml` on `refs/heads/main`: do not
rename or move that file. Verify a file with:

```
gh attestation verify <file> --repo komira-ai/tierstore \
  --signer-workflow komira-ai/tierstore/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main --source-digest <commit> --deny-self-hosted-runners
```

## Releases

Every push to `main` that changes code and passes `ci.yml` and `artifacts.yml` is released, by
itself or in the release of a newer push; nobody runs anything. `release.yml` starts when a run of
`ci` or of `artifacts` on `main` finishes, and it acts only on a run that succeeded for a push to
`main` of this repository. Both workflows finish on every push, so it starts twice for a commit.
The plan job does not plan the commit that run built: it walks the first-parent history of the
tip of `main` down to the highest release tag, newest first, and releases the newest commit that
is ready (both its push runs succeeded and its build recorded the version of the next tag; see
below). A run that finds nothing ready ends green with nothing to do.

Release actions are made one at a time and none is ever cancelled. Every workflow that tags
or publishes is in one concurrency group, `release-main` for a run that succeeded for a
push to this repository and a group of its own (the run id) for any other run, with
`cancel-in-progress: false`. The condition requires the event to be a push and the head
repository to be this one, because a pull request from a fork's branch named `main` matches the
`branches: [main]` of `workflow_run` and, if it counted, could take the one waiting place and
displace a real release. A newer push waits behind a running release. GitHub keeps only the newest
waiting run, so a push that is replaced while it waits is not released by itself; that loses
nothing, because every plan walks from the tip of `main` and measures from the highest tag, and
the next release carries the skipped commits. A manual run is in a group of its own (publish refuses a tag or a release that exists).

The workflow lint (`crates/xtask/tests/workflows`) finds these workflows by what they do, not by a
list of names: any workflow with `contents: write`, `gh release`, or a `git tag` or tag push must be in the group with `cancel-in-progress: false`, or be listed in
`RELEASE_EXEMPT` with a reason. `artifacts.yml` is the one exempt workflow: it only builds and
attests a commit and never tags, publishes or deploys (signing alone, `id-token` with the attest
actions, does not make a workflow release path). It has a group of its own, one per commit
(`artifacts-<sha>`, or per ref for a pull request, whose newer run replaces its older); a push run
is never cancelled, so a commit that is signed is never left half checked, and the build of one
push does not wait for the release of another.

Pushes made before an earlier push has been released all build with the same computed version (the
next number after the tag that exists when they build). Whichever is released first takes it; the
binaries of the others say a version that is no longer the next tag. Each build run records the
version it built under as the name of an artifact, `version-<X.Y.Z>` (`cargo xtask release
version --record`), so the plan knows this before anything is downloaded: it passes such a commit
over with the reason `built as X.Y.Z, not vX.Y.Z+1; a newer commit's release carries it`, and
never plans it again. The next push builds under the new number and is released, and its release
carries the changes of the commits passed over; a re-run of `artifacts.yml` for the newest passed
over commit records the new number and has it released without waiting for a push. A build run
from before the record existed has no `version-` artifact and is passed over the same way.

The `plan` job (read-only, `cargo xtask release plan`) decides, in this order:

1. The repository variable `RELEASE_FREEZE` is set to anything except empty or `false` (any
   case): no release. This is the stop switch; set it to `true` to hold releases and clear it (or
   set it to `false`) to resume. Any other value, a typo included, also freezes: the switch fails
   closed. A push made while it was set is released with the next one, since a plan measures from
   the highest tag.
2. The highest release tag already covers the tip of `main` (this is how the second of two runs
   for one commit stops): no release.
3. The tag is the patch after the highest `vX.Y.Z` tag (or the `VERSION` file, item 6). The plan
   walks the first-parent history of the tip, newest first, down to the highest tag and at most
   twenty commits, and picks the first commit that is ready: `ci.yml` and `artifacts.yml` each
   have a successful push run for it, and the `artifacts.yml` run recorded the version of the tag
   (`version-<X.Y.Z>`). A commit that a merge brought in was never the tip of `main` and is not
   judged. When no commit is ready: no release, and the reason names each commit judged and why
   (a run not finished or failed, no version record, or built under another version).
4. Everything changed from the highest release tag up to the commit picked is documentation: no
   release. The paths are
   `docs/`, `**/*.md`, `crates/*/docs/` and `crates/*/site/` (the list is `DOCS_PATHS` in
   `crates/xtask/src/release/plan.rs`, and a test holds this list to the code). The site is not
   in the release files, which are the `tierstore` binary and its SBOM. A change of any other
   path, `.github/workflows` included, is released. A renamed file counts under both its old and
   its new path, so moving a source file into `docs/` is a code change.
5. Otherwise the commit picked is released under the tag.
6. To release a minor or major
   version, commit a file `VERSION` at the root holding `X.Y.Z`: when it is above the highest tag
   the next release is that tag. A `VERSION` at or below the highest tag is ignored (the patch
   bump stands), so the file never needs to be kept up to date; one that is not `X.Y.Z` fails the
   plan.

The release notes list the titles of the commits since the previous tag (the first twenty), then
how to verify the files. The `verify` and `publish` jobs are those of the manual release below, run
with the plan's tag and commit.

### The version the binary reports

A release binary reports exactly its tag: `tierstore --version` prints `tierstore X.Y.Z+<12 hex>`
for the release `vX.Y.Z` of the commit whose first 12 digits follow the `+`, and the build gauge
`tierstore_build_info` carries the same `version` label. The binary reports the version in
`crates/tierstore/Cargo.toml`, and `build.rs` reads no environment variable for it (nor for the
commit, which is git's, `unknown` without it). `artifacts.yml` computes the release version before
it builds (`cargo xtask release version`: the patch after the highest `vX.Y.Z` tag, or the `VERSION`
file when that is higher, the same rule as the plan above), and each build job writes it into the
crate with `cargo xtask release set-version --version X.Y.Z` (strict `X.Y.Z`, or the verb fails): it
rewrites `version` in that manifest and the `tierstore` entry of `Cargo.lock`, so the `--locked`
build accepts it, and the SBOM step must leave the lock file as the verb wrote it. A build that
does not run the verb, a developer's for one, reports the version committed in `Cargo.toml`. The
release plan computes the version again, names the tag with it, and picks a commit whose build
recorded that version (above). The `verify` job
then unpacks the linux-x86_64 tarball it will publish, runs the binary with `--version`
(`cargo xtask release check-binary`) and refuses to go on, so that nothing is tagged or published,
unless the binary says the tag and the commit. The release runner cannot run the linux-arm64 or
the macos-arm64 binary; each is run with `--version` and checked against the same version and
commit (`cargo xtask build-commit --version`) by the job that builds it, on a runner of its own
platform, and a release needs that run of `artifacts.yml` to be green. That build-time check is
the version proof of the macOS binary. The two computations differ when a tag
appears between the build and the plan (another release published in that time); the plan then
passes the commit over by its record, and the next push is built and released under the new number.
`check-binary` still refuses the release if a tag appears between the plan and the check. The names of the
tarballs and SBOMs carry the commit, not the version, and are unchanged; the hostkeeper packages
are named by the version the binary reports, which holds both.

`v0.3.1` predates this: its binary reports `0.3.0+e7fd86844000`, the crate's version at the time.

### The hostkeeper package

`cargo xtask release hostkeeper-package --binary <path> --version <X.Y.Z+12 hex> --min-hostkeeper
<x.y.z> --platform <platform> --out <dir>` assembles the package hostkeeper installs on a host,
`tierstore-<version>-<platform>.tar.zst`, for linux-x86_64, linux-arm64 or macos-arm64, from a built
binary: the manifest and the units of `packaging/hostkeeper/` (its README says what each holds) and
the binary as `bin/tierstore`, byte for byte, so the macOS binary keeps the signature its build
gave it. The verb never runs the binary, so it packages any platform's on any runner, but it
refuses a `--version` that is not what `tierstore --version` prints after the name or that the
binary does not hold. The same inputs give the same bytes. It appends the package's line to
`<out>/SHA256SUMS.<platform>`, beside the tarball's and the SBOM's.

In `artifacts.yml` each build job packages its binary after the tarball, with `--min-hostkeeper
0.1.0`, and unpacks the package with hostkeeper's own unpacker into a scratch directory (`cargo
xtask verify-assets hostkeeper-package`): the archive keeps every rule hostkeeper's unpacker holds
it to, carries no extended attribute (no pax record of one, no AppleDouble `._` entry), is at most
the `max_package_bytes` of hostkeeper's sample daemon config, and its manifest names tierstore, the
version and the platform. The macOS job then checks the code signature of the binary it unpacked
(`codesign --verify --strict`; the linker's ad-hoc signature is enough). Then:

- the `sums` job writes `index.json` (`cargo xtask release hostkeeper-index`) with hostkeeper's
  own writer: schema 1, keys sorted, packages sorted by platform, at most one per platform, the
  same bytes for the same packages. A release's index lists all three platforms, because
  hostkeeper refuses a host whose platform its release lacks (`no_package_for_platform`): on a push
  or a run by hand the job refuses a run without the macOS package. For a pull request, which has
  no macOS job in this workflow, it checks the two Linux packages are there and writes no index;
- the `attest` job signs each package as its own subject, as it does each tarball, and names the
  bundle `<package>.sigstore.json`; the provenance of the sums covers the packages and the index;
- the `verify` job (and `release.yml`'s `verify` and `publish`) makes the index again from the
  packages, which must give the bytes the sums list, verifies each package's bundle offline as it
  does each tarball's, and takes each package as a host does (`cargo xtask verify-assets
  hostkeeper-index`): the index is read with hostkeeper's strict reader, lists a package for every
  platform, each with the size and SHA-256 of its file, and each unpacks as above.

A commit built before the hostkeeper packages were added has none, and `release check` refuses to
release it.

### The tap tarball, the Debian package and the RPM package

The tap and apt channels take their files from the release; their uploads are not in
`artifacts.yml`. The names are fixed by the channels: the tap's guard accepts a formula whose urls
are `tierstore-<version>-<target>.tar.gz` of the release `v<version>` for `macos-arm64`,
`linux-aarch64` and `linux-x86_64`, and the formula installs the file `tierstore` at the top of the
archive.

- `cargo xtask release tap-tarball <dir> --commit <sha> --version <X.Y.Z> --platform <platform>`
  copies `<dir>/tierstore-<commit>-<platform>.tar.gz` to `tierstore-<version>-<target>.tar.gz`,
  refusing a source whose first entry is not the regular file `tierstore`, and appends the copy's
  line to `<dir>/SHA256SUMS.<platform>`. The copy has the same bytes, so the binary (and the
  licence notice beside it) is the commit-named tarball's, byte for byte; the SBOM attestation of
  that digest covers it. Each build job runs it after the hostkeeper package, the macOS job too.
- `cargo xtask release deb --binary <path> --version <X.Y.Z> --platform <platform> --licenses
  <file> --out <dir>` writes `tierstore_<version>_<arch>.deb` for a Linux platform and its line in
  `<out>/SHA256SUMS.<platform>`. The package is an `ar` archive of `debian-binary`,
  `control.tar.gz` and `data.tar.gz`, the same bytes for the same inputs (every owner `root`, every
  time 2026-09-01, sorted entries, no gzip file name). It installs
  `/usr/libexec/tierstore/tierstore` (0755), `/usr/lib/systemd/system/tierstore.service` and
  `/usr/lib/sysusers.d/tierstore.conf` (from `packaging/`), the example
  `/etc/tierstore/tierstore.toml` (0640, from `packaging/tierstore.toml`; the one conffile, so an
  upgrade keeps a host's edits), and `/usr/share/doc/tierstore/copyright` (the repository's
  `LICENSE`) and `THIRD-PARTY-LICENSES.txt`. Its `postinst` runs `systemd-sysusers`, gives
  `/etc/tierstore` (0750) and the example file (0640) to the `tierstore` group, and runs `systemctl
  daemon-reload`; it never starts or enables the unit, which also waits for
  `/etc/tierstore/access.key`. Removing the package leaves the account and `/etc/tierstore`, which
  holds the keys. The build job then has `dpkg-deb --info` and `dpkg-deb --contents` read the file.
- `cargo xtask release rpm --binary <path> --version <X.Y.Z> --platform <platform> --licenses
  <file> --out <dir>` writes `tierstore-<version>-1.<arch>.rpm` (`x86_64` or `aarch64`) and its line
  in `<out>/SHA256SUMS.<platform>`. It has the files of the Debian package at the same paths and
  modes, every owner `root`, with two differences the RPM conventions ask for: the licence is the
  `%license` file `/usr/share/licenses/tierstore/LICENSE`, and the example configuration is
  `%config(noreplace)`. The package requires `systemd`. Its `%post` (every install and upgrade)
  does what the `postinst` does, each step only where its tool or the `tierstore` group is there,
  so a container without systemd installs; its `%postun` runs `systemctl daemon-reload` when the
  last version is removed. Nothing starts or enables the unit, and removing the package leaves the
  account and `/etc/tierstore`. The file is the same bytes for the same inputs (build time and file
  times 2026-09-01, build host `localhost`, gzip level 9, no signature: the signature header holds
  the digests only). It is built with the `rpm` crate, without its signing and compression
  features. The build job then has `cargo xtask release rpm-info <file>` read it back: the digests,
  the payload, and the header and file list `rpm -qip` and `rpm -qlv` would print (the runner has
  no `rpm`). Nothing yet installs the package with `dnf`.

In `artifacts.yml` the `attest` job signs each of these files as its own subject and names its bundle
`<file>.sigstore.json`, with the version of the `version` job. `release check` (which runs on a
push, never on a pull request) requires the three tap tarballs, the macOS one included as it
requires the macOS hostkeeper package, and the two Debian packages and the two RPM packages, each
with its bundle; `verify-assets attestations` verifies a Debian or an RPM package for provenance
and its bundle, as it does a hostkeeper package; and the release attaches them all. A pull request builds the Linux ones and
sums them; the tap tarball of macOS is made by the macOS job, which `artifacts.yml` runs on a push
and by hand and `macos.yml` runs for every pull request.

### The public channels: tierstore.dev and the Homebrew tap

After `publish`, `release.yml` has two more jobs, `channel-upload` and `tap`. Nothing waits for them
and a failed one un-releases nothing: re-run the failed job. Each starts with `cargo xtask channels
gate`, which is given the job's settings and writes `enabled=true` or `enabled=false`, and every
later step runs only when it said `true`. None of the settings set: the job ends green with a
notice and writes nothing (merging the workflow changes no release). All set: it runs. Some set:
it ends red naming the missing ones.

| job | environment | settings | does |
|---|---|---|---|
| `channel-upload` | `prod-releases-tierstore` | variables `TIERSTORE_RELEASES_BUCKET`, `TIERSTORE_RELEASES_ROLE_ARN`; optional `TIERSTORE_RELEASES_DISTRIBUTION_ID` | assumes the role by OIDC, puts `releases/<version>/` (the three tap tarballs, the two Debian packages, their bundles and a `SHA256SUMS` of exactly these) once each, then replaces `releases/latest.json` |
| `tap` | `release` | variable `RELEASE_BOT_APP_ID`, secret `RELEASE_BOT_PRIVATE_KEY` | renders `Formula/tierstore.rb` from the checked `SHA256SUMS` and opens the pull request `bump/tierstore-v<version>` in `<owner>/homebrew-tap` as the release bot |

- `cargo xtask channels upload` puts each file with one `aws s3api put-object --if-none-match '*'`,
  the only kind of upload the role allows. A file that exists answers `412`; the stored file is then
  read back and the answer is a success only if its digest is the local file's, so a re-run passes
  and a different file at a version's key fails and is never written over. `latest.json`
  (`{"version", "tag", "commit", "files": {name: {"sha256", "size"}}}`) is written last, with
  `--if-match` on the ETag that was read, only when its version is lower than the release's, so a
  late or re-run older release writes nothing; with a distribution id given, `/releases/latest.json`
  is then invalidated.
- `cargo xtask channels formula` writes the formula the tap's guard accepts, line for line, with each
  `sha256` from the release's `SHA256SUMS` and nothing hashed or downloaded; a sums file that lacks
  one of the three tarballs or lists one twice is refused.
- `cargo xtask channels tap` uses the GitHub API only: a branch at the tap's tip, one commit of the
  file, the pull request, and auto-merge (squash) when the tap allows it. It refuses a tap that is
  ahead of the release, does nothing when the tap is at the release or a pull request for it is
  open, and closes older open bump pull requests. The bot's token (the app's installation for the
  tap repository alone) is minted in the second to last step and read by the last one only.

The design is `docs/design/distribution.md`; the workflow lint (`crates/xtask/tests/workflows`)
holds the shape of both jobs.

### The apt repository

After `publish`, `release.yml` also puts the two Debian packages in `packages.komira.ai`, the apt
repository the organisation's products share, in three jobs that never share a secret: no job
holds both the signing key and the cloud role. The release is listed in the suite `testing`
(`apt/dists/testing/`, every verb takes `--suite testing`); the suite `stable`, the one users
install from, is written only by the promotion of the same entries after the install checks
(`cargo xtask channels apt promote`, then `apt write --suite stable --no-pool`: the index only,
the pool holds the packages already). Both suites list the files of one pool.

| job | environment | settings | does |
|---|---|---|---|
| `testing-apt-read` | `apt-publish` | variables `APT_PUBLISH_ROLE_ARN`, `APT_BUCKET`, `APT_DISTRIBUTION_ID`, `APT_HOSTNAME` | assumes the role by OIDC and reads the current index and the ETag of each of its objects from S3 (`cargo xtask channels apt read`) |
| `testing-apt-index` | `apt-signing` | variable `APT_GPG_FINGERPRINT`, secrets `APT_GPG_PRIVATE_KEY`, `APT_GPG_PASSPHRASE` | merges `tierstore_<version>_{amd64,arm64}.deb` into that index, keeping every other product's entries (`apt index`), and signs `InRelease` and `Release.gpg` (`apt sign`); no cloud role |
| `testing-apt-write` | `apt-publish` | as the read job | uploads the packages once each, then the index with `If-Match` on the ETags that were read, then invalidates `/apt/dists/*` (`apt write`) |

A job without its settings ends green with a notice and writes nothing; the index and write jobs
are also off when the read job was (`channels gate --require`). The index is the one object other
products' releases write too. When the write finds it changed in between (`412`), it reports
`conflict=true` and ends green, and the jobs with the suffix `-2` read, merge, sign and write
again; the write of the third attempt (`-3`) fails instead of asking for a fourth. A package that
is in the pool already passes only with the same bytes. A version older than the newest listed is
refused. A release whose apt jobs all failed is still a release: re-run the failed jobs.

### Releasing by hand

Run the `release` workflow from `main` ("Run workflow"), giving the tag (`vX.Y.Z`), the full SHA
of a commit on `main`, and optionally a paragraph for the notes. The plan does not apply (it is
neither held by `RELEASE_FREEZE` nor limited to code changes); the rest is the same. The `verify`
job refuses a commit that is not on `main`, that lacks a successful push run of `artifacts.yml`
and of `ci.yml`, or whose files or attestations do not check. The `publish` job creates the
release as a draft at the commit, reads its assets back and publishes it, which creates the tag.
A tag or release that exists is refused, and so is a commit that the highest release tag already
covers (no second tag is made for a released commit). The binary must say the tag you give, so a release by hand
is made from a commit whose `artifacts.yml` run computed that same version.

## Setting the desired version on the hostkeeper coordinator

Hosts take tierstore through hostkeeper: the coordinator holds the version each service should
run, and every host's daemon fetches that version from the release channel
(`{release_channel}/tierstore/<version>/`) and installs the package of its own platform.
`hostkeeper-desired.yml` sets that version for a published release. It is run by hand, from
`main` ("Run workflow"), with the release's version `X.Y.Z`; nothing else starts it, release.yml
included.

Before you run it, the release must be in the release channel: stage it with hostkeeper's
`cargo xtask release channel-stage` (komira-ai/hostkeeper, `docs/deploy/release-channel.md`).
Nothing in CI publishes into the channel, and a version the channel does not hold leaves every host
failing its fetch.

Its `check` job (`cargo xtask release desired`, no token that writes) refuses the run unless the
release `vX.Y.Z` is published, its `index.json` is the one its `SHA256SUMS` lists, hostkeeper's own
reader reads it as the index of tierstore `X.Y.Z+<the first 12 digits of the release's commit>`,
and it lists a package for each of linux-x86_64, linux-arm64 and macos-arm64 that is an asset of
the release at the size the index gives. The coordinator orders a service and a version and never
a platform, so a host whose platform the index lacks is refused the version
(`no_package_for_platform`) while the others take it; a release made before the hostkeeper
packages has no index and is refused too. Only then does the `set-desired` job call hostkeeper's
reusable workflow (`set-desired.yml` of komira-ai/hostkeeper, pinned to a commit with the
hostkeeper release whose program makes the calls), which proves that release, sets the version
with the job's OIDC token, and reads the convergence every 10 seconds. Each change is printed as
one line with the hosts of each role at the version, updating, failed and down, and the job fails
unless every live host reaches the version. The version the coordinator is given is the packages'
(`X.Y.Z+<12 hex>`), and the digest is the SHA-256 of `index.json`, which pins every platform's
package.

The workflow is off until a maintainer sets it up once:

1. The coordinator's allow-list entry for this repository: its id, `refs/heads/main`, the event
   `workflow_dispatch`, the audience `tierstore-hostkeeper-desired`, the service `tierstore`, and the
   `job_workflow_ref` `komira-ai/hostkeeper/.github/workflows/set-desired.yml@<the pinned commit>`.
   A new pin in the workflow needs a new `job_workflow_ref` in the entry.
2. The repository variable `HOSTKEEPER_COORDINATOR_CA`: the PEM certificates of the CA the
   coordinator's private listener chains to.
3. The repository variable `HOSTKEEPER_COORDINATOR_URL`: that listener, `https://<host>[:<port>]`.
   While it is empty both jobs are skipped.

## The compatibility check against the release before

`crates/compat` holds tests that run two releases of the `tierstore` library side by side: this
branch's, and the previous release's, which is the dev-dependency `tierstore_prev`, taken from this
repository's public URL at the commit of the previous tag (`rev` in the root `Cargo.toml`, in `[workspace.dependencies]`;
`allow-git` in `deny.toml` names that one URL). Over one directory and one in-process S3 service,
each release reads and replaces what the other wrote (`tests/dir.rs`, `tests/s3.rs`), which is how
a change to the formats stays additive. The tests need no network: only the build fetches the
previous release's source. When a release is made, move `rev` to its commit, so that the next
change is held to it; a tag can be moved and a commit cannot, which is why `rev` is the pin.

## One-time setup for the site (a maintainer)

The site publish job is off until these exist:

1. The environment `prod-site-tierstore`, restricted to `main`.
2. Its variables `TIERSTORE_SITE_BUCKET`, `TIERSTORE_SITE_DISTRIBUTION_ID` and
   `TIERSTORE_SITE_PUBLISH_ROLE_ARN`.
3. A cloud role the job can assume through OIDC, whose trust names this repository (by id), the
   workflow file `tierstore-site-publish.yml` at `refs/heads/main` and that environment, and that
   can only write the site's bucket and invalidate its distribution.
4. The repository variable `TIERSTORE_SITE_PUBLISH` set to `true`.
