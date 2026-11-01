# CI and releases

## The workflows

| file | when | what |
|---|---|---|
| `ci.yml` | pull request, push to `main` | `lint`: format, cargo-deny (licences, bans, sources), clippy, the workflow lint. `test`: every test under cargo-llvm-cov with branch coverage, and the 99% gate on lines and branches of every crate (`crates/coverage-gate`). |
| `advisories.yml` | daily, push to `main` | `cargo deny check advisories`; the result is kept in one issue, and the run is green whatever it finds. |
| `artifacts.yml` | pull request, push to `main` | builds `tierstore` (`--features server`) for linux-x86_64 and linux-arm64, packages it with its SBOM, makes `SHA256SUMS`. On a push to `main` it also signs (build provenance and SBOM attestations) and checks what it signed. |
| `paper.yml` | pull request that changes the paper check, its copy of the paper or its results; by hand | runs `crates/xtask/tests/paper.rs`: its tests block, and the ignored completion check runs as its own step, printing the open places and passing until the file `crates/xtask/tests/paper_fixture/FINAL` is committed, after which it fails on any open place. It reads a copy of the paper kept in the repository, so a run speaks for the copy and not for the published page; refreshing the copy is the only guard. Not part of `ci.yml`, so it does not count for a release. |
| `release.yml` | when `ci` or `artifacts` finishes on a push to `main` (automatic, see Releases); by hand | `plan` decides whether the push is released and as which tag; `verify` and `publish` attach the files that push built and signed to a GitHub release, after checking them again; `gamma` then sets that release as the farm coordinator's desired version for the service `tierstore` and waits for convergence (see The gamma stage). Builds nothing of tierstore. |
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
finishes last go on, because it looks for a successful push run of both.

Release actions are made one at a time and none is ever cancelled. Every workflow that tags,
publishes or deploys is in one concurrency group, `release-main` for a run that succeeded for a
push to this repository and a group of its own (the run id) for any other run, with
`cancel-in-progress: false`. kbf's `release.yml` uses the same group with a shorter condition
(`conclusion == 'success'` alone); tierstore's also requires the event to be a push and the head
repository to be this one, because a pull request from a fork's branch named `main` matches the
`branches: [main]` of `workflow_run` and, if it counted, could take the one waiting place and
displace a real release. A newer push waits behind a running release. GitHub keeps only the newest
waiting run, so a push that is replaced while it waits is not released by itself; that loses
nothing, because the plan measures from the highest tag and the next release carries the skipped
commits. A manual run is in a group of its own (publish refuses a tag or a release that exists).
The farm deploy job, `gamma`, is in the same group (the group is the workflow's, so every job of `release.yml` is).

The workflow lint (`crates/xtask/tests/workflows`) finds these workflows by what they do, not by a
list of names: any workflow with `contents: write`, `gh release`, a `git tag` or tag push, or a
farm deploy step must be in the group with `cancel-in-progress: false`, or be listed in
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

### The gamma stage

The last job of `release.yml`, `gamma`, runs after `publish`, under the same `if:` as `verify` and
`publish` (the plan said release, on a run of `main`), in the `release` environment, with
`contents: read` and `id-token: write` and no other scope. It checks out `komira-ai/kbf` at `main`
(the deploy verb is kbf's; the coordinator does not report its build commit without a token, so
the job cannot read it beforehand), builds kbf's `xtask`, and runs

```
cargo xtask deploy-farm --service tierstore --url <coordinator> --ca-pem <farm CA> --version <X.Y.Z>+<12 hex>
```

The version is the one tierstore's hosts report (`tierstore --version`, the `version` label of
`tierstore_build_info`): the tag without its `v`, then the first 12 digits of the commit, which
`verify` checked the published binary says. The verb sets that as the desired version of the
service `tierstore` and waits, up to 60 minutes, until the coordinator says the farm converged on
it; a host that fails its update, a farm that stalls or runs out of time, or a refused token fails
the job. The release stays published whatever the job says. No `--min-up-*` flag is passed: for a
service other than `kbf`, the coordinator's services file holds the minimums.

The job needs these, set once by an operator (nothing in this repository does it):

- Repository (or `release` environment) variables: `KBF_COORDINATOR_URL`, the coordinator's control
  address, and `KBF_COORDINATOR_CA`, the farm CA certificate (public) its certificate chains to.
  When the coordinator is reachable only on a Tailscale network, also `TS_CLIENT_ID` and
  `TS_AUDIENCE`: the job joins that network as an ephemeral node with this run's OIDC token, so it
  holds no secret.
- On the coordinator, an entry in the file its `--oidc-config` names, one `[[remote_operator]]`
  for this repository (several entries are read together; never mixed with the single table):

  ```toml
  [[remote_operator]]
  issuer = "https://token.actions.githubusercontent.com"
  audience = "<the coordinator's control address, as written in KBF_COORDINATOR_URL>"
  repository_id = "<the numeric id of komira-ai/tierstore>"
  repository = "komira-ai/tierstore"
  job_workflow_ref = ["komira-ai/tierstore/.github/workflows/release.yml@refs/heads/main"]
  refs = ["refs/heads/main"]
  environments = ["release"]
  event_names = ["workflow_run", "workflow_dispatch"]
  services = ["tierstore"]
  ```

  `event_names` is a list and must name both `workflow_run` and `workflow_dispatch`: an automatic
  release is started by the completion of `ci` and `artifacts` (token event `workflow_run`), and
  a manual release (Releasing by hand) runs the same `gamma` job on a run whose token says
  `workflow_dispatch`. With only `workflow_run`, a manual release would be refused with a 403
  after it is published, and the job would fail. Both runs are of this workflow on `main`, in
  the `release` environment, so the rest of the entry admits them alike.

- On the coordinator, the service `tierstore` defined in the file its `--services-file` names
  (its roles and minimums), and the tierstore hosts enrolled so that their updaters heartbeat to it.

Keep `KBF_COORDINATOR_URL` unset until the farm can answer this job. Two things must exist first:
the host updater's service registration, and tierstore registering its release version
(`X.Y.Z+<12 hex>`, the text above) on it, so that each host's heartbeat for the service
`tierstore` carries that version. While no host reports one, the coordinator has nothing to
converge on and the wait can only run out its 60 minutes and fail the job (after the release is
published).

Until `KBF_COORDINATOR_URL` is set the job prints `::notice::the gamma stage is off` and passes
without fetching or building anything, so a fork, or a repository whose coordinator does not know
tierstore yet, releases as before. Set but not configured on the coordinator, it fails (a 403
`forbidden`) and changes nothing there. `deploy-farm --service` needs kbf's support for services
other than `kbf`; until that is on kbf's `main` a configured job fails at the verb's arguments.

### The version the binary reports

A release binary reports exactly its tag: `tierstore --version` prints `tierstore X.Y.Z+<12 hex>`
for the release `vX.Y.Z` of the commit whose first 12 digits follow the `+`, and the build gauge
`tierstore_build_info` carries the same `version` label. The version is not the crate's version
in `Cargo.toml`. `artifacts.yml` computes it before it builds (`cargo xtask release version`: the
patch after the highest `vX.Y.Z` tag, or the `VERSION` file when that is higher, the same rule as
the plan above) and gives it to the build as `TIERSTORE_BUILD_VERSION_OVERRIDE` (strict
`X.Y.Z`, or the build fails); a build without it, a developer's for one, keeps the crate's
version. The release plan computes the version again and names the tag with it. The `verify` job
then unpacks the linux-x86_64 tarball it will publish, runs the binary with `--version`
(`cargo xtask release check-binary`) and refuses to go on, so that nothing is tagged or published,
unless the binary says the tag and the commit. The two computations differ only when a tag
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

## One-time setup for the site (a maintainer)

The site publish job is off until these exist:

1. The environment `prod-site-tierstore`, restricted to `main`.
2. Its variables `TIERSTORE_SITE_BUCKET`, `TIERSTORE_SITE_DISTRIBUTION_ID` and
   `TIERSTORE_SITE_PUBLISH_ROLE_ARN`.
3. A cloud role the job can assume through OIDC, whose trust names this repository (by id), the
   workflow file `tierstore-site-publish.yml` at `refs/heads/main` and that environment, and that
   can only write the site's bucket and invalidate its distribution.
4. The repository variable `TIERSTORE_SITE_PUBLISH` set to `true`.
