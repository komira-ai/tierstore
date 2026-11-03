# CI and releases

## The workflows

| file | when | what |
|---|---|---|
| `ci.yml` | pull request, push to `main` | `lint`: format, cargo-deny (licences, bans, sources), clippy, the workflow lint. `test`: every test under cargo-llvm-cov with branch coverage, and the 99% gate on lines and branches of every crate (`crates/coverage-gate`). |
| `advisories.yml` | daily, push to `main` | `cargo deny check advisories`; the result is kept in one issue, and the run is green whatever it finds. |
| `artifacts.yml` | pull request, push to `main`; by hand | builds `tierstore` (`--features server`) for linux-x86_64, linux-arm64 and macos-arm64, each on a GitHub-hosted runner of its platform, packages it with its SBOM, makes `SHA256SUMS`. The macos-arm64 job also runs the tests of `tierstore`, `objstore-s3` and `tierstore-testkit` on macOS, the update hook's among them (its single-instance launchd steps, the macOS declaration and the macOS packaging; the two-slot conformance test and the `nft` and `systemctl` ports are Linux only and compiled out there). No macOS runner is part of a pull request's checks: for a pull request the macos-arm64 job is skipped and the sums cover the Linux files; a push to `main` builds every platform, and a run by hand (`gh workflow run artifacts.yml --ref <branch>`) builds and sums every platform of a branch and signs nothing. On a push to `main` it also signs (build provenance and SBOM attestations) and checks what it signed. |
| `release.yml` | when `ci` or `artifacts` finishes on a push to `main` (automatic, see Releases); by hand | `plan` decides whether the push is released and as which tag; `verify` and `publish` attach the files that push built and signed to a GitHub release, after checking them again. Builds nothing of tierstore. |
| `tierstore-site.yml` | pull request, push to `main`, when the site changes | typecheck, tests and build of tierstore.dev. |
| `tierstore-site-publish.yml` | push to `main` / by hand | builds the site again; uploads it only when the repository variable `TIERSTORE_SITE_PUBLISH` is `true`. |

Every workflow is held to the same rules by `crates/xtask/tests/workflows`: every `uses:` is a
40-digit commit SHA with a version comment, `permissions: {}` at the top, no
`pull_request_target`, and `id-token`, `attestations`, `contents` and `issues` written only by the
one job that needs each (and for `id-token` and `attestations`, only on a push to `main`).

## The release files

For a commit `<commit>` (the full SHA) and each platform (`linux-x86_64`, `linux-arm64`, `macos-arm64`):

- `tierstore-<commit>-<platform>.tar.gz`: the binary alone, the same bytes on every build;
- `tierstore-<commit>-<platform>.cdx.json`: its CycloneDX SBOM;
- `tierstore-<commit>-<platform>.tar.gz.sigstore.json`: the bundle of the tarball's own provenance
  attestation, for an offline check;
- `SHA256SUMS`: the SHA-256 of the tarballs and SBOMs.

The signer of every attestation is `.github/workflows/artifacts.yml` on `refs/heads/main`: do not
rename or move that file. Verify a file with:

```
gh attestation verify <file> --repo komira-ai/tierstore \
  --signer-workflow komira-ai/tierstore/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main --source-digest <commit> --deny-self-hosted-runners
```

## Releases

Every push to `main` that changes code and passes `ci.yml` and `artifacts.yml` is released by
itself; nobody runs anything. `release.yml` starts when a run of `ci` or of `artifacts` on `main`
finishes, and it acts only on a run that succeeded for a push to `main` of this repository. Both
workflows finish on every push, so it starts twice for a commit; the plan job lets the run that
finishes last go on, because it looks for a successful push run of both.

Release actions are made one at a time and none is ever cancelled. Every workflow that tags
or publishes is in one concurrency group, `release-main` for a run that succeeded for a
push to this repository and a group of its own (the run id) for any other run, with
`cancel-in-progress: false`. The condition requires the event to be a push and the head
repository to be this one, because a pull request from a fork's branch named `main` matches the
`branches: [main]` of `workflow_run` and, if it counted, could take the one waiting place and
displace a real release. A newer push waits behind a running release. GitHub keeps only the newest
waiting run, so a push that is replaced while it waits is not released by itself; that loses
nothing, because the plan measures from the highest tag and the next release carries the skipped
commits. A manual run is in a group of its own (publish refuses a tag or a release that exists).

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
release runs of the others then stop at `check-binary`, because their binary says a version that
is no longer the tag, and publish nothing. That is the safe direction: nothing wrong is published.
The next push builds under the new number and is released, and a re-run of `artifacts.yml` for a
stopped commit does the same for it.

The `plan` job (read-only, `cargo xtask release plan`) decides, in this order:

1. The repository variable `RELEASE_FREEZE` is set to anything except empty or `false` (any
   case): no release. This is the stop switch; set it to `true` to hold releases and clear it (or
   set it to `false`) to resume. Any other value, a typo included, also freezes: the switch fails
   closed. A push made while it was set is released with the next one, since a plan measures from
   the highest tag.
2. `ci.yml` or `artifacts.yml` has no successful push run for the commit yet: no release from
   this run.
3. The highest release tag already covers the commit (this is how the second of two runs for one
   commit stops): no release.
4. Everything changed since the highest release tag is documentation: no release. The paths are
   `docs/`, `**/*.md`, `crates/*/docs/` and `crates/*/site/` (the list is `DOCS_PATHS` in
   `crates/xtask/src/release/plan.rs`, and a test holds this list to the code). The site is not
   in the release files, which are the `tierstore` binary and its SBOM. A change of any other
   path, `.github/workflows` included, is released. A renamed file counts under both its old and
   its new path, so moving a source file into `docs/` is a code change.
5. Otherwise the tag is the patch after the highest `vX.Y.Z` tag. To release a minor or major
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
release plan computes the version again and names the tag with it. The `verify` job
then unpacks the linux-x86_64 tarball it will publish, runs the binary with `--version`
(`cargo xtask release check-binary`) and refuses to go on, so that nothing is tagged or published,
unless the binary says the tag and the commit. The release runner cannot run the linux-arm64 or
the macos-arm64 binary; each is run with `--version` and checked against the same version and
commit (`cargo xtask build-commit --version`) by the job that builds it, on a runner of its own
platform, and a release needs that run of `artifacts.yml` to be green. That build-time check is
the version proof of the macOS binary. The two computations differ only when a tag
appears between the build and the release (another release published in that time); the release is
then refused, and the next push is built and released under the new number. File names carry the
commit, not the version, and are unchanged.

`v0.3.1` predates this: its binary reports `0.3.0+e7fd86844000`, the crate's version at the time.

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
