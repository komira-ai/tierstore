# CI and releases

## The workflows

| file | when | what |
|---|---|---|
| `ci.yml` | pull request, push to `main` | `lint`: format, cargo-deny (licences, bans, sources), clippy, the workflow lint. `test`: every test under cargo-llvm-cov with branch coverage, and the 99% gate on lines and branches of every crate (`crates/coverage-gate`). |
| `advisories.yml` | daily, push to `main` | `cargo deny check advisories`; the result is kept in one issue, and the run is green whatever it finds. |
| `artifacts.yml` | pull request, push to `main` | builds `tierstore` (`--features server`) for linux-x86_64 and linux-arm64, packages it with its SBOM, makes `SHA256SUMS`. On a push to `main` it also signs (build provenance and SBOM attestations) and checks what it signed. |
| `release.yml` | by hand | attaches the files a push to `main` built and signed to a GitHub release, after checking them again. Builds nothing. |
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
  --source-ref refs/heads/main --source-digest <commit>
```

## Cutting a release

Run the `release` workflow from `main` ("Run workflow"), giving the tag (`vX.Y.Z`), the full SHA
of a commit on `main`, and optionally a paragraph for the notes. The `verify` job refuses a commit
that is not on `main`, that lacks a successful push run of `artifacts.yml` and of `ci.yml`, or
whose files or attestations do not check. The `publish` job creates the release as a draft at the
commit, reads its assets back and publishes it, which creates the tag. A tag or release that exists
is refused.

## One-time setup for the site (a maintainer)

The site publish job is off until these exist:

1. The environment `prod-site-tierstore`, restricted to `main`.
2. Its variables `TIERSTORE_SITE_BUCKET`, `TIERSTORE_SITE_DISTRIBUTION_ID` and
   `TIERSTORE_SITE_PUBLISH_ROLE_ARN`.
3. A cloud role the job can assume through OIDC, whose trust names this repository (by id), the
   workflow file `tierstore-site-publish.yml` at `refs/heads/main` and that environment, and that
   can only write the site's bucket and invalidate its distribution.
4. The repository variable `TIERSTORE_SITE_PUBLISH` set to `true`.
