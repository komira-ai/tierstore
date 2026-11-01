# CI and releases

## The workflows

| file | when | what |
|---|---|---|
| `ci.yml` | pull request, push to `main` | `lint`: format, cargo-deny (licences, bans, sources), clippy, the workflow lint. `test`: every test under cargo-llvm-cov with branch coverage, and the 99% gate on lines and branches of every crate (`crates/coverage-gate`). |
| `advisories.yml` | daily, push to `main` | `cargo deny check advisories`; the result is kept in one issue, and the run is green whatever it finds. |
| `artifacts.yml` | pull request, push to `main` | builds `tierstore` (`--features server`) for linux-x86_64 and linux-arm64, packages it with its SBOM, makes `SHA256SUMS`. On a push to `main` it also signs (build provenance and SBOM attestations) and checks what it signed. |
| `paper.yml` | pull request that changes the paper check, its copy of the paper or its results; by hand | runs `crates/xtask/tests/paper.rs`: its tests block, and the ignored completion check runs as its own step, printing the open places and passing until the file `crates/xtask/tests/paper_fixture/FINAL` is committed, after which it fails on any open place. It reads a copy of the paper kept in the repository, so a run speaks for the copy and not for the published page; refreshing the copy is the only guard. Not part of `ci.yml`, so it does not count for a release. |
| `release.yml` | when `ci` or `artifacts` finishes on a push to `main` (automatic, see Releases); by hand | `plan` decides whether the push is released and as which tag; `verify` and `publish` attach the files that push built and signed to a GitHub release, after checking them again. Builds nothing. |
| `tierstore-site.yml` | pull request, push to `main`, when the site changes | typecheck, tests and build of tierstore.dev. |
| `tierstore-site-publish.yml` | push to `main` / by hand | builds the site again; uploads it only when the repository variable `TIERSTORE_SITE_PUBLISH` is `true`. |

Every workflow is held to the same rules by `crates/xtask/tests/workflows`: every `uses:` is a
40-digit commit SHA with a version comment, `permissions: {}` at the top, no
`pull_request_target`, and `id-token`, `attestations`, `contents` and `issues` written only by the
one job that needs each (and for `id-token` and `attestations`, only on a push to `main`).

## The release files

For a commit `<commit>` (the full SHA) and each platform (`linux-x86_64`, `linux-arm64`):

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
finishes last go on, because it looks for a successful push run of both. Releases are made one at
a time and none is cancelled.

The `plan` job (read-only, `cargo xtask release plan`) decides, in this order:

1. The repository variable `RELEASE_FREEZE` is `true`: no release. This is the stop switch; set
   it to `true` to hold releases and clear it (or set anything else) to resume. A push made while
   it was set is released with the next one, since a plan measures from the highest tag.
2. `ci.yml` or `artifacts.yml` has no successful push run for the commit yet: no release from
   this run.
3. The highest release tag already covers the commit (this is how the second of two runs for one
   commit stops): no release.
4. Everything changed since the highest release tag is documentation: no release. The paths are
   `docs/`, `**/*.md`, `crates/*/docs/` and `crates/*/site/` (the list is `DOCS_PATHS` in
   `crates/xtask/src/release/plan.rs`, and a test holds this list to the code). The site is not
   in the release files, which are the `tierstore` binary and its SBOM. A change of any other
   path, `.github/workflows` included, is released.
5. Otherwise the tag is the patch after the highest `vX.Y.Z` tag. To release a minor or major
   version, commit a file `VERSION` at the root holding `X.Y.Z`: when it is above the highest tag
   the next release is that tag. A `VERSION` at or below the highest tag is ignored (the patch
   bump stands), so the file never needs to be kept up to date; one that is not `X.Y.Z` fails the
   plan.

The release notes list the titles of the commits since the previous tag (the first twenty), then
how to verify the files. The `verify` and `publish` jobs are those of the manual release below, run
with the plan's tag and commit.

### Releasing by hand

Run the `release` workflow from `main` ("Run workflow"), giving the tag (`vX.Y.Z`), the full SHA
of a commit on `main`, and optionally a paragraph for the notes. The plan does not apply (it is
neither held by `RELEASE_FREEZE` nor limited to code changes); the rest is the same. The `verify`
job refuses a commit that is not on `main`, that lacks a successful push run of `artifacts.yml`
and of `ci.yml`, or whose files or attestations do not check. The `publish` job creates the
release as a draft at the commit, reads its assets back and publishes it, which creates the tag.
A tag or release that exists is refused.
## One-time setup for the site (a maintainer)

The site publish job is off until these exist:

1. The environment `prod-site-tierstore`, restricted to `main`.
2. Its variables `TIERSTORE_SITE_BUCKET`, `TIERSTORE_SITE_DISTRIBUTION_ID` and
   `TIERSTORE_SITE_PUBLISH_ROLE_ARN`.
3. A cloud role the job can assume through OIDC, whose trust names this repository (by id), the
   workflow file `tierstore-site-publish.yml` at `refs/heads/main` and that environment, and that
   can only write the site's bucket and invalidate its distribution.
4. The repository variable `TIERSTORE_SITE_PUBLISH` set to `true`.
