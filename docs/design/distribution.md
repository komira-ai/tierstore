# Distribution: the farm first through hostkeeper, then tierstore.dev, Homebrew, apt and public release files

Status: **proposal, for review. No code comes with this document.** It is a living document: each pull request that builds a part of it changes the part's section from "will" to what the code does (AGENTS.md P10), and a decision that changes is changed here first. What it says about the repository was read in `.github/workflows/release.yml`, `.github/workflows/tierstore-site-publish.yml`, `crates/xtask/` and `packaging/` on main at the time of writing. What it says about the sibling repositories (buildrules, homebrew-tap, komira-internal, hostkeeper) and the sibling site's CDK app was read in checkouts of them at the time of writing and is cited by file. Revision of 2026-11-07: the owner decided that a release reaches the farm first, through hostkeeper, and the public channels follow after the farm has been healthy for N hours (section 5.3).

## 1. The problem

A tierstore release today is a GitHub release (`release.yml`): the commit-named tarballs, SBOMs, hostkeeper packages, provenance bundles, `index.json` and `SHA256SUMS`. That is what a host operator with hostkeeper takes. Everybody else has nothing to install from: no `brew install`, no `apt install`, no stable URL that does not need a GitHub login on a browser, and no site. tierstore.dev is a domain the project has; the site's source is in `crates/tierstore/site` and `tierstore-site-publish.yml` publishes it, but the cloud side (bucket, CDN, certificate, the roles the workflows assume) does not exist: neither the `tierstore-dns-zone` nor the `tierstore-prod-site` CloudFormation stack is deployed, and its CDK app is in the sibling repository (`infra/site`), which lists `tierstore.dev` as a second site.

What the user wants:

- `brew install komira-ai/tap/tierstore` on macOS (Apple silicon) and Linux (x86_64, aarch64).
- `sudo apt install tierstore` on Debian and Ubuntu (amd64, arm64) from `packages.komira.ai`, the apt repository the organisation's products share.
- A file anyone can `curl` from `https://tierstore.dev/releases/<version>/`, and `https://tierstore.dev/releases/latest.json` that says which version is the newest.
- All of it from the release that already happens by itself on every push to main that changes code: no manual tag, no human verdict (AGENTS.md B12). Every release the farm has taken and kept healthy goes to every public channel.
- Our own farm runs a release before anybody outside does: it reaches the farm through hostkeeper, and the public channels (tierstore.dev, Homebrew, apt) wait until the farm has been healthy on that version for N hours. N is a configuration value, `0` for now, raised once the farm is stable.
- Nothing runs on the project's side until the owner has made the one-time settings, and the merge of the pull requests that carry this design breaks no release.

## 2. The design

| | Before | After |
|---|---|---|
| Release files | GitHub release: commit-named tarballs, SBOMs, hostkeeper packages, bundles, `index.json`, `SHA256SUMS` | the same, plus `tierstore-<V>-{macos-arm64,linux-aarch64,linux-x86_64}.tar.gz` (and their bundles) beside the commit-named ones |
| tierstore.dev | nothing deployed | CDK app in `infra/site` of this repository: a bucket behind a CDN, the zone, the certificate and two roles |
| `https://tierstore.dev/releases/<V>/` | nothing | the tap-named tarballs, their bundles, `tierstore_<V>_amd64.deb`, `tierstore_<V>_arm64.deb` and the directory's `SHA256SUMS`, written once |
| `https://tierstore.dev/releases/latest.json` | nothing | the newest version and its files, rewritten by each release, never to an older version |
| Homebrew | nothing | `komira-ai/homebrew-tap` gets `Formula/tierstore.rb` by a pull request from the release bot, per release |
| apt | nothing | `packages.komira.ai` lists `tierstore` for amd64 and arm64 |
| the farm | hand-installed; a release reaches it when somebody runs `hostkeeper-desired.yml` | every release is staged for hostkeeper and moved onto the farm by the coordinator, by `release.yml` (section 5.3) |
| `release.yml` | `plan`, `verify`, `publish` | the same three, then `farm` and `soak` (section 5.3), then `channel-upload`, `tap` and the three-job apt publish, which wait for them |

What does not change (section 9): the existing release files and their names, the hostkeeper packages and the update hook, the signer of every attestation (`artifacts.yml` on `refs/heads/main`), and the key the shared apt repository is signed with.

Decisions, from the owner:

1. **tierstore.dev's CDK moves into this repository** at `infra/site`. The repository is the one the OIDC trust names; the site and the release files belong to the project that produces them.
2. **apt only**; no rpm yet. The shared repository keeps `apt/` as a prefix so a later `rpm/` fits beside it.
3. **Every release the farm keeps healthy goes to every public channel**: there is no "stable" or "latest only" subset, and the three public channels do not wait for one another. They wait for the farm (decision 6).
4. **Packages are public artifacts of the site**: `https://tierstore.dev/releases/<V>/` (write-once) and `releases/latest.json`. The tap and apt do not depend on it (section 4 says where each reads from); it is the stable URL.
5. **Homebrew** through the organisation's tap, `komira-ai/homebrew-tap`; **apt** through `packages.komira.ai`, package name `tierstore`.
6. **The farm first, through hostkeeper** (2026-11-07). A release reaches the farm through the farm's hostkeeper channel and coordinator, not through apt. The public channels (`releases/<V>/` and `latest.json` on tierstore.dev, the Homebrew pull request, the apt publish) run only after every farm host has reported the version healthy and N hours have passed since. N is the repository variable `FARM_SOAK_HOURS`, read by `release.yml` and passed to the tool as a flag (AGENTS.md S2); it is `0` for now and raised once the farm is stable. The soak is a timer, automatic; no person approves it (B12).

## 3. The files of a release

For a version `V` (`X.Y.Z`, the tag without the `v`) and a commit `C`.

**Tap-named tarballs.** The tap's guard accepts a formula only if it equals its template line for line (`.github/guard/guard.js` in homebrew-tap, `template`), and the template fixes where the files are and what they are called: `https://github.com/komira-ai/tierstore/releases/download/v<V>/tierstore-<V>-<target>.tar.gz`, for the targets `macos-arm64`, `linux-aarch64` and `linux-x86_64`, with `bin.install "tierstore"` and a test that runs `tierstore --version` and wants `tierstore <V>` in it. So the Homebrew files must be assets of the GitHub release, under these names. They cannot be the files on tierstore.dev, and the platform name for arm64 Linux is `aarch64` there, where the commit-named file says `linux-arm64`.

Each is a **copy of the commit-named tarball with the same bytes**, under the tap's name: `tierstore-<V>-macos-arm64.tar.gz` is `tierstore-<C>-macos-arm64.tar.gz`, `tierstore-<V>-linux-aarch64.tar.gz` is `tierstore-<C>-linux-arm64.tar.gz`, and `tierstore-<V>-linux-x86_64.tar.gz` is `tierstore-<C>-linux-x86_64.tar.gz`. Nothing is repacked, so:

- the SHA-256 in the formula is the one `SHA256SUMS` already lists for the commit-named file, and the formula is rendered from that one file (nothing is downloaded or hashed to make it);
- the attestation of the commit-named tarball is an attestation of this file too (it names a digest), so `gh attestation verify` on either passes, and no new signing step and no new signer is needed. Each tap-named tarball is published with a copy of its tarball's bundle, `<name>.sigstore.json`;
- the tarball holds `tierstore` and `THIRD-PARTY-LICENSES.txt`, and `bin.install "tierstore"` finds the binary at the top, as the tap expects (`docs/ci-and-release.md` says how a host extracts it by name).

`SHA256SUMS` is not changed: it is produced and checked by `artifacts.yml`, the signer's workflow, and the copies need no line of their own. `cargo xtask release check` learns the copies as a rule instead: a file named `tierstore-<V>-<platform>.tar.gz` must exist for each commit-named tarball and have its digest. A tap-named file with other bytes, or one missing, fails the `verify` job before anything is published.

**Debian packages.** `tierstore_<V>_amd64.deb` and `tierstore_<V>_arm64.deb`, made in the `verify` job (no token) from the verified linux tarballs by `cargo xtask`, byte for byte the same for the same inputs: members and entries sorted, owner root, one modification time (the commit's committer time, passed as a flag), no time in the gzip header. That matters twice: the package is never attested (the attestation is the tarball's), so anyone can rebuild it from the tarball and compare; and a `.deb` is written to the pool once and never replaced, so a re-run must produce the same bytes (section 8).

| path in the package | from |
|---|---|
| `/usr/libexec/tierstore/tierstore` | the tarball's binary (the path `packaging/tierstore.service` runs) |
| `/usr/lib/systemd/system/tierstore.service` | `packaging/tierstore.service` |
| `/usr/lib/sysusers.d/tierstore.conf` | `packaging/tierstore.conf` |
| `/usr/share/doc/tierstore/examples/tierstore.toml` | `packaging/tierstore.toml` (an example, not read by anything; `/etc/tierstore` is the operator's) |
| `/usr/share/doc/tierstore/copyright`, `THIRD-PARTY-LICENSES.txt` | the project licence and the tarball's notice |

- **Not started on install.** The unit has no `[Install]` section and the package does not enable or start it; the operator copies the example to `/etc/tierstore`, edits it and starts the unit (`packaging/README.md`). The only maintainer script is a `postinst` that creates the account from the sysusers entry (`systemd-sysusers`) and runs `systemctl daemon-reload` when systemd is running. Removal leaves the account and `/etc/tierstore`; the package says so.
- `Architecture:` `amd64` or `arm64`; `Version:` is `V`; `Depends:` nothing the static binary does not need (the control file is listed in the pull request that writes it).
- The hostkeeper packages (`.tar.zst`) are not in the `.deb` and not on tierstore.dev: hostkeeper's channel is unchanged.

**On tierstore.dev.** `releases/<V>/` holds the three tap-named tarballs and their bundles, the two `.deb`s, and `SHA256SUMS`, which lists exactly the files of that directory (it is not the release's `SHA256SUMS`, which lists the commit-named files; a tap-named tarball's line here equals the commit-named line there). `releases/latest.json`:

```json
{"version": "X.Y.Z", "tag": "vX.Y.Z", "commit": "<40 hex>",
 "files": {"tierstore-X.Y.Z-linux-x86_64.tar.gz": {"sha256": "<64 hex>", "size": 0}}}
```

`files` lists every file of `releases/<V>/` except `SHA256SUMS`. The format is additive from the start (AGENTS.md B8): a reader ignores a key it does not know, and a later release may add keys.

## 4. The cloud side: `infra/site`

A CDK app (TypeScript, us-east-1), copied from the sibling repository's `infra/site` (`README.md` there is the description of what it builds) and cut down to the one site; the held steps (`held/main.ts`) come with it. What the stack builds is the the sibling site design as it stands, so a reviewer who knows it reads only the differences:

- a **private bucket** behind a CDN with origin access control, a Route 53 zone, a certificate, a headers policy (one Content-Security-Policy) and a router function;
- the site at the root of the bucket; `releases/<V>/` and `releases/latest.json` beside it. Pages are served `public, max-age=60, s-maxage=300`; `latest.json` is written `public, max-age=60`, so a release is visible within a minute with no invalidation and the release role needs no CDN rights; a file of `releases/<V>/` is immutable (`max-age=31536000, immutable`).
- stack names `tierstore-dns-zone` and `tierstore-prod-site`, bucket `tierstore-prod-site-<account>`. Neither stack exists, so nothing is replaced when the names are written; the account is a CDK context value and a flag of the held steps, never in a file (AGENTS.md S1).

**Two roles, assumed by GitHub OIDC; no stored key.** The repository's subject template is the default with the immutable subject, so a role trusts `sub = repo:komira-ai@<owner id>/tierstore@<repo id>:environment:<env>` (the ids are read by the held step from `gh api`, which refuses a repository whose name differs from the one given, as in the sibling app; they are not written in a file), and the `job_workflow_ref` and `environment` claims as well:

| | `tierstore-prod-publish` (site) | `tierstore-prod-releases` (release files) |
|---|---|---|
| workflow it trusts | `.github/workflows/tierstore-site-publish.yml` at `refs/heads/main` | `.github/workflows/release.yml` at `refs/heads/main` |
| GitHub environment | `prod-site-tierstore` | `prod-releases-tierstore` |
| may write | the site's objects at the root, and delete them in the sync; **no write under `releases/`** | `releases/<V>/*` only with `If-None-Match` (write-once); `releases/latest.json` only with `If-Match` or `If-None-Match` |
| may read | the bucket's listing (for the sync) | `GetObject` on `releases/*` (the files are public; this is for the re-run check, section 8) |
| may invalidate | the one distribution | nothing |
| delete | the site's own objects | **never**; the bucket policy also denies `DeleteObject` and `DeleteObjectVersion` on `releases/*` to every principal |

Today's `publish.ts` already excludes `releases/` from every upload and from `--delete`; the role's policy turns that convention into a rule the program cannot break. The policy is pinned by a TypeScript test of the synthesized template (the language of the component, AGENTS.md T11); the conditions (`s3:if-none-match`, `s3:if-match`, `s3:ObjectCreationOperation` for multipart parts) are policy text and are not exercised against AWS by any test: the first release's upload is the proof, and a refusal there is a red `channel-upload` job and nothing else (section 8). The bucket is versioned, so a wrongly overwritten `latest.json` is restored by copying the previous version back, an administrator step.

**The workflow `tierstore-site-publish.yml` is not changed** by this design: it already names the three variables (`TIERSTORE_SITE_BUCKET`, `TIERSTORE_SITE_DISTRIBUTION_ID`, `TIERSTORE_SITE_PUBLISH_ROLE_ARN`) of the environment `prod-site-tierstore` and is off until `TIERSTORE_SITE_PUBLISH` is `true`.

**Supersession (AGENTS.md B22).** Copying is not moving: the sibling repository keeps its own `infra/site` (it serves the sibling site, with golden tests that pin every the sibling site name) and its `SITES` entry for tierstore.dev stays valid until the owner has deployed from here. Removing that entry, and the note in its README that the trust is "not moved yet", is a separate pull request in that repository, after the deploy of section 7 step 6. Two apps must not deploy the same stack, so the owner deploys from one of them only: from here (section 7), and the sibling repository's entry is removed once that has happened.

## 5. The new jobs of `release.yml`

`release.yml` keeps its shape: `plan` (read), `verify` (read; builds nothing that holds a token), `publish` (the one `contents: write`). `verify` gains the steps that make the copies and the packages (`cargo xtask release channel-files`) and checks them; the GitHub release gains the tap-named tarballs and their bundles as assets (they are in `release-assets`); the packages go in a second artifact, `channel-files`, that no `release check` lists, because they are not assets of the GitHub release.

After `publish` come two jobs that gate the rest, `farm` and `soak` (section 5.3), and then three independent branches that need `soak` (`channel-upload`, `tap` and the apt jobs). Nothing waits for the three and a failed one un-releases nothing: the release is already a GitHub release and its tag exists (as in buildrules, whose `release.yml` these jobs follow).

| job | environment | holds | does |
|---|---|---|---|
| `channel-upload` (needs `soak`) | `prod-releases-tierstore` | `id-token: write`, no repository write | assumes `tierstore-prod-releases`, uploads `releases/<V>/*` with `If-None-Match`, then `releases/latest.json` |
| `tap` (needs `soak`) | `release` | `contents: read`, the release bot's token for `komira-ai/homebrew-tap` alone | renders `Formula/tierstore.rb` from the verified `SHA256SUMS`, opens the pull request `bump/tierstore-v<V>` in the tap as the bot, with auto-merge when the tap allows it |
| `apt-deploy-read`, `apt-index`, `apt-deploy-write` (the first needs `soak`) | `apt-publish`, `apt-signing`, `apt-publish` | read job and write job: the cloud role (`id-token: write`) and no key; index job: the signing key and no role | read the current index, merge the `.deb`s and sign it, upload the `.deb`s and write the index; a lost write starts attempt 2, then 3 (`apt-*-2`, `apt-*-3`) |

The three apt jobs and their split are copied from buildrules' `release.yml` (the `apt-*` jobs; `RELEASING.md`, "The apt repository"): no job holds both the signing key and the cloud role, so a compromise of one step cannot sign and publish. The shared repository's own role for the product (`komira-apt-publish-tierstore`) is created by the apt repository's stack in komira-internal (section 7), trusts `.github/workflows/release.yml` of this repository at main in the environment `apt-publish`, may write `apt/pool/tierstore/*` with `If-None-Match` only and the shared `apt/dists/*` with `If-Match` only, and may delete nothing.

**The workflow lint** (`crates/xtask/tests/workflows`, AGENTS.md B10): the new jobs go in the existing workflow, not in a new one (B9); the lint lists, for `release.yml`, which jobs may hold `id-token: write` (`farm`, `soak`, the upload job and the apt read and write jobs), under which `if:`, and that `contents: write` stays with `publish` alone. The `if:` is the one `publish` has (`needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main'`). Actions are pinned by commit SHA; the release bot's token comes from `actions/create-github-app-token` pinned the same way, in the last two steps of `tap`, given to the one step that opens the pull request.

**Where each channel reads from.** The tap and the formula read the GitHub release (the template fixes it), which exists before the farm job runs; the tap is held back by `soak` all the same, so no public channel names a version before the farm has kept it. The upload reads `release-assets` and `channel-files`. The apt jobs read `channel-files`; they do not download from tierstore.dev, so a failed upload does not hold apt. The site is a copy for people; no other channel depends on it. The farm reads none of them: it takes the release from the hostkeeper channel (section 5.3).

### 5.1 A job is a green no-op until its settings exist

Every channel job starts with `cargo xtask channels gate`, which reads the job's settings from its flags (the workflow passes variables and secrets of the environment as environment variables of the step; the verb reads them as flags, AGENTS.md S2: the allowed environment is secret material and platform markers, and the gate is given each as a flag by the step's `run:` line) and writes `enabled=true` or `enabled=false` to `$GITHUB_OUTPUT`. Every later step has `if: steps.gate.outputs.enabled == 'true'`.

| settings of the job | result |
|---|---|
| none of them set | `enabled=false`, a notice naming the job and the settings, the job ends **green**, writes nothing |
| all of them set | `enabled=true`, the job runs |
| some of them set | the job ends **red** naming the missing ones, writes nothing |

The same three rows judge `farm` (its settings are in the table below). The third row is the half-made settings of section 8: a typo in a variable name must not leave a channel silently off. Before the owner has begun, all the jobs are green no-ops, so merging the workflow changes no release. One difference: with `farm` a green no-op the public jobs do not run either (section 5.3, "No farm configured"); they are no-ops anyway until their own settings exist.

| job | settings |
|---|---|
| `farm` | variables of `release`: `HOSTKEEPER_COORDINATOR_URL`, `HOSTKEEPER_COORDINATOR_CA`; and, only if the coordinator is reached over the tailnet, `HOSTKEEPER_TAILNET_CLIENT_ID`, `HOSTKEEPER_TAILNET_AUDIENCE` and `HOSTKEEPER_TAILNET_COORDINATOR` (the inputs of `set-desired.yml`'s `join_tailnet`; buildrules' gamma stage keeps the same two values as `TS_CLIENT_ID` and `TS_AUDIENCE`). `FARM_SOAK_HOURS` is not a setting of the gate: empty is `0` |
| `soak` | none: it runs after `farm` ended green with `reached=true` |
| `channel-upload` | variables of `prod-releases-tierstore`: `TIERSTORE_RELEASES_BUCKET`, `TIERSTORE_RELEASES_ROLE_ARN` |
| `tap` | variable `RELEASE_BOT_APP_ID` and secret `RELEASE_BOT_PRIVATE_KEY` of `release` |
| `apt-deploy-read`, `apt-deploy-write` | variables of `apt-publish`: `APT_PUBLISH_ROLE_ARN`, `APT_BUCKET`, `APT_DISTRIBUTION_ID`, `APT_HOSTNAME` |
| `apt-index` | variable `APT_GPG_FINGERPRINT`, secrets `APT_GPG_PRIVATE_KEY`, `APT_GPG_PASSPHRASE` of `apt-signing`, and the outputs of the read job |

No secret is written in a file, and no account id, endpoint or role ARN is (S1): they are the values of the environments. The upload and write steps run `aws` from a flag-given program (`--aws`, default `aws`, as `verify-assets attestations` takes `--gh`) with the role assumed by `aws sts assume-role-with-web-identity` from the job's OIDC token, as `tierstore-site-publish.yml` does today.

### 5.2 One release through every channel

```mermaid
sequenceDiagram
  participant P as plan / verify
  participant G as GitHub release
  participant U as channel-upload
  participant S as tierstore.dev bucket
  participant T as tap
  participant H as komira-ai/homebrew-tap
  participant R as apt-deploy-read
  participant I as apt-index
  participant W as apt-deploy-write
  participant A as packages.komira.ai bucket
  P->>P: plan: tag vX.Y.Z and commit C from the tip of main
  P->>P: verify: check files, attestations; make tap-named copies and the two .debs
  P->>G: publish: release with commit-named files, tap-named copies, bundles
  Note over U,A: after soak, independent of one another, each a green no-op until its settings exist
  par site files
    U->>S: assume role by OIDC, PUT releases/V/* (If-None-Match)
    U->>S: GET latest.json and ETag, then PUT it (If-Match) if V is newer
  and Homebrew
    T->>T: render Formula/tierstore.rb from the verified SHA256SUMS
    T->>H: pull request bump/tierstore-vV as the release bot (auto-merge)
    H->>H: guard accepts only the template; the formula points at G
  and apt
    R->>A: assume role by OIDC, GET index and ETag (from S3, not the CDN)
    R->>I: current index
    I->>I: merge the .debs, sign with the shared key (no cloud role)
    I->>W: signed index
    W->>A: PUT .debs (If-None-Match), PUT index (If-Match on the ETag)
    alt 412 on the index
      W-->>R: conflict: attempt 2 reads again (then attempt 3)
    end
  end
```

### 5.3 The farm first, then the public channels

**What exists.** `hostkeeper-desired.yml` (by hand) already does the farm half: its `check` job (`cargo xtask release desired`) refuses a release whose hostkeeper index lacks a platform, and its `set-desired` job calls hostkeeper's reusable workflow (`set-desired.yml`, pinned to a hostkeeper release by commit), which proves that release, sets the desired version of the service `tierstore` with the job's OIDC token and reads the convergence every 10 seconds (`docs/ci-and-release.md`, "Setting the desired version on the hostkeeper coordinator"; `docs/deploy/ci-client.md` in hostkeeper). It is not changed. The release channel is filled by `channel-sync` on the channel's host, which pulls the GitHub release by a timer (`docs/deploy/release-channel.md` in hostkeeper): no workflow pushes into it. buildrules' `release.yml` has the same pattern for its own farm: the `gamma` job joins the farm's network by Tailscale workload identity (an OIDC token exchanged for a short-lived node, no stored key) and fails the release when the farm is not configured.

**The new jobs**, in `release.yml`, after `publish`:

| job | holds | does |
|---|---|---|
| `farm` | `contents: read`, `id-token: write` (the call of `set-desired.yml`, and the tailnet join when it is used) | runs the `check` of `hostkeeper-desired.yml` for the version just published, then calls `set-desired.yml` with `wait: true` for the service `tierstore`; the coordinator refuses the version with `not_in_channel` until `channel-sync` has staged it, so the job asks again (bounded, a flag with a default) rather than racing the timer; ends green with `reached=true` when the coordinator reports the service converged on the version |
| `soak` | `contents: read`, `id-token: write` | `cargo xtask channels soak --hours "$FARM_SOAK_HOURS" ...`: with `0` it ends at once; with N it keeps reading the convergence until N hours have passed since `farm` reached the version, and fails the moment a read shows the farm not healthy |

Both are green no-ops until the settings exist (5.1). Nothing is deployed to the farm by anything but the coordinator: the workflow only says which version is desired.

**Healthy, concretely.** The word means what hostkeeper reports, and the gate adds nothing of its own:

1. A host is healthy for a version when its daemon reports the service `live`, at that version, and its health check passes (a `[health]` URL answering 2xx, or no `[health]` and the version not contradicted); `stopped`, `transitioning`, `unobserved` and a `version_mismatch` are unhealthy (`docs/service-observation.md` in hostkeeper).
2. The coordinator counts a host done when it is up at the desired version and has baked `bake_beats` healthy heartbeats (`docs/operations/rollout-plans.md`). The farm's plan for `tierstore` is the one host per role at a time kind: `max_updating` of 1 for each role and `stages = [100]`; that is hostkeeper configuration of the farm, not of this repository.
3. **The farm is healthy on V** when the coordinator's convergence answer reports the service converged on V: every live host at V, none updating, none failed (`set-desired.yml`'s output `converged` is `true` and `reason` is `converged`). A host that is silent is not live and the coordinator does not wait for it; the gate does: `farm` and `soak` also fail when the per-role line of the answer shows a host `down`, because a farm host that went quiet during an update is not a healthy host. A deliberately retired host is excluded in hostkeeper (`Hosts.Exclude`) and then is not counted.
4. A halt, or the version marked bad, is not healthy (`reason` `halted` or `held_bad`), and neither is an automatic rollback that set the desired version back to the last good one: the desired version is then no longer V, so the read does not report V converged (the `reason` token is read in the pull request that writes the job).

**Release order**

```mermaid
sequenceDiagram
  participant P as plan / verify
  participant G as GitHub release
  participant F as farm job
  participant C as hostkeeper coordinator
  participant CH as hostkeeper channel (channel-sync)
  participant H as farm hosts
  participant K as soak job
  participant PUB as channel-upload, tap, apt
  P->>G: publish: tag vX.Y.Z, files (the release, candidate for the farm)
  CH->>G: channel-sync (timer) stages tierstore/X.Y.Z+C into the channel
  F->>F: check: index lists every platform
  F->>C: join tailnet by OIDC, set-desired tierstore X.Y.Z+C
  alt not in the channel yet
    C-->>F: not_in_channel, ask again (bounded)
  end
  C->>H: order the update by the plan (max_updating, bake)
  H->>CH: fetch, verify the signer, install, report live and healthy
  F->>C: read convergence every 10 s
  alt converged, no host down
    C-->>F: converged
    F->>K: reached=true
    K->>C: read convergence until N hours have passed (N = FARM_SOAK_HOURS)
    alt still healthy at N hours (N = 0: at once)
      K->>PUB: soak passed: run the public channels (5.2)
    else unhealthy, halted, rolled back or timed out
      K-->>PUB: public channels skipped; the release stays a GitHub release
    end
  else failed host, halt, timeout
    C-->>F: not converged
    F-->>PUB: skipped
  end
```

**The order**: build -> GitHub release (the candidate; the tag exists and nothing public names it) -> the channel holds it -> the coordinator moves the farm -> every farm host reports the version healthy (`farm`) -> the soak of N hours (`soak`) -> `channel-upload`, `tap`, apt. The release is a release the moment `publish` ends; that the farm has taken it decides only whether the public channels follow.

**The soak is a timer, not a verdict (B12).** Nobody approves it and no job waits for a person. The time is measured from the moment `farm` read converged, passed to `soak` as a flag, and the tool takes the clock as an injected value so a test reads no real time (AGENTS.md T10). A hosted job lives at most 6 hours, so one run can soak up to 5 hours; the tool refuses a larger N rather than ending the job silently, and a soak of more hours needs a scheduled continuation (open question, section 11). While `soak` waits it holds the workflow's `release` group (the next release waits for it, as for any job); a newer push is planned from the tip of main afterwards, so nothing is lost.

**Settings and N.** `FARM_SOAK_HOURS` is a repository variable (a tunable is a configuration key with a default: here empty means `0`). `release.yml` reads it into the step's environment and gives it to the tool as `--hours`; the tool reads no environment (S2). Raising N later is a change to the variable alone, and applies from the next release.

**No farm configured.** While the settings of `farm` are empty it is a green no-op that writes `reached=false`, and `soak` and the public jobs are skipped: no farm, no public release. That is on purpose and differs from the other channel jobs: a public release that skipped the farm because the farm was forgotten is what the order exists to prevent. It breaks nothing today, since the public jobs have no settings yet either. The owner turns the farm on first (section 7).

**Rolling the farm back.** The coordinator's own automatic rollback (`rollback = "automatic"` in the rollout file, at the failure threshold in `docs/operations/rollout-plans.md`) sets the desired version back to the last good one and marks the failed one bad. By hand, with the operator token: `hostkeeper ctl releases mark-bad` for the version, then `hostkeeper ctl releases set-desired` for the previous good version (`docs/ctl.md` in hostkeeper); or `releases halt` to stop everything while somebody looks. The next release is a new version and clears the way. Nothing in this repository rolls a host back.

## 6. The xtask verbs

Rust, in `crates/xtask`, with Rust tests (AGENTS.md T11; there is no shell test). Small verbs, one subject each, each its own pull request (P7). A verb that calls `aws` or `gh` takes the program as a flag, so a test hands it a stand-in as `tests/attestations.rs` does with `gh-fake`. Whether a second stand-in can be a `[[bin]]` the coverage run reaches rather than one marked `test = false` (T4 bars hiding code from measurement) is settled in the first verb's pull request, with the choice written here.

| verb | what it does |
|---|---|
| `release channel-files dist --commit C --version V --time T --out channel-files` | makes the tap-named copies in `dist` (same bytes, with their bundle copies) and the two `.deb`s and `SHA256SUMS` of `releases/<V>/` in `--out` |
| `release check dist --commit C` (extended) | adds the rule of section 3: each commit-named tarball has its tap-named copy with the same digest, and nothing else of that name |
| `channels gate --label L --out FILE --env NAME ...` | the three-way result of 5.1 |
| `channels soak --hours N --reached-at T --now T ... --coordinator-url U` | reads the convergence until N hours have passed since `--reached-at`; ends non-zero on the first read that is not converged with no host down; refuses N above 5; takes the clock as a value, so a test passes it; N of 0 ends at once. `farm` reuses `release desired` and `set-desired.yml` and needs no verb of its own beyond `channels gate` |
| `channels upload --dir D --version V --bucket B --aws P` | puts each file of `D` with `If-None-Match`; on `412` reads the stored file's digest and succeeds only if it equals the local one; then `latest.json` (section 8) |
| `channels formula --sums SUMS --version V --out FILE` | renders `Formula/tierstore.rb` as the tap's template has it, from `SHA256SUMS` alone; refuses a sums file that lacks one of the three tarballs or lists one twice |
| `channels tap --version V --formula-file F --tap-repo R --workdir W --gh P` | opens the pull request `bump/tierstore-v<V>` as the bot, or finishes quietly when a pull request for it is open or the formula at `V` is already in the tap |
| `channels apt read`, `index`, `sign`, `write` | the four steps of the three jobs: read the index and its ETag from S3 (never through the CDN, which caches the index for 60 seconds and would return a stale ETag), merge the `.deb`s into it (keeping the newest few versions per architecture, refusing a version older than the newest listed), sign it with `gpg` (a program given by flag), upload the `.deb`s with `If-None-Match` and write the index with `If-Match` on the ETag read, and report `conflict=true` on a `412` |

The merge and the index writer follow buildrules' `tools/channels/apt.zig` and `tools/deploy/apt_publish.zig`, ported to Rust, not called from there: this repository builds with `cargo` and has no Zig. A golden test pins the index text against what the Zig tool writes for the same inputs, so the two repositories feed one repository the same format. The `.deb` writer follows `deb.zig` (an `ar` of `debian-binary`, `control.tar.gz` and `data.tar.gz`); `flate2` and `tar` are already dependencies of `xtask`; a dependency added for the rest comes from crates.io under a permissive licence (R6) and each verb's pull request names it.

Coverage is the repository's: every crate at 99% of lines and branches by `coverage-gate` (T1). The tests assert behaviour, listed by name in each pull request with what each proves (T8): a re-run of an upload with the same bytes passes, with other bytes at the same key fails, a gate with some settings fails and names the missing, a `.deb` built twice is byte-identical, an index merged twice equals the index merged once.

## 7. What the owner sets up, and in which order

Every step before the tag is a held step (AGENTS.md S3): it is run by the owner with their own AWS credentials and `gh` login, is a dry run unless `--apply` is given, never forces, and refuses to run unless the profile's account equals the `--account` given. No pull request and no workflow deploys anything.

0. **Put the farm under hostkeeper first.** The farm's tierstore is hand-installed today. Before any release can go through the farm, each farm host is enrolled in hostkeeper, the service `tierstore` is assigned to its role (`Services.Assign`; `docs/operations/assignments.md` in hostkeeper), and the host runs the hostkeeper-installed package (units `tierstore@a/@b`, the update hook) instead of the hand-installed one. A service left installed and unmanaged is never ordered by the coordinator, so a version set for it moves nothing. The moving of each host from the hand-installed to the managed tierstore is its own runbook with a rollback (the hand-installed copy kept until the managed one has run) and is not part of this design's pull requests. Also: the farm's plan in the rollout file (`stages = [100]`, `max_updating` of 1 per role), the allow-list entry of the coordinator for `release.yml` (below), and `channel-sync` running on the channel's host with `tierstore` in its trust list (it is in the committed trust list).
1. **Merge the pull requests** of section 10. Nothing happens: the farm, soak, upload, tap and apt jobs are green no-ops (5.1) and `infra/site` is a directory.
2. **OIDC provider.** One per account for GitHub (`token.actions.githubusercontent.com`). If the account has one already (the owner checks with `aws iam list-open-id-connect-providers`), nothing to do; if not, the held `oidc-provider` step creates it.
3. **DNS zone.** `npm run held -- dns --domain tierstore.dev ...` deploys `tierstore-dns-zone` and prints the four name servers of the zone. The zone must exist before the registrar can point at it.
4. **GoDaddy.** In the registrar, replace the name servers of `tierstore.dev` with those four. Wait until the zone answers for the domain (`dig NS tierstore.dev`): the certificate in the next stack is validated through DNS, and its creation waits (and times out) until the registrar's change is live.
5. **Site stack.** `site --domain tierstore.dev ...` deploys `tierstore-prod-site`: the bucket, the CDN, the certificate (for `tierstore.dev` and `www.tierstore.dev`; `www` answers 301 to the apex), the headers policy, the router and both roles. It prints the bucket name, the distribution id and the two role ARNs. Until DNS is live it can run `--no-domain`, serving on the CDN's own name, and be run again after step 4.
6. **Variables.** `vars --domain tierstore.dev ...` sets, in `prod-site-tierstore`, the three `TIERSTORE_SITE_*` variables and, in `prod-releases-tierstore`, `TIERSTORE_RELEASES_BUCKET` and `TIERSTORE_RELEASES_ROLE_ARN`. A dry run prints them. The environments exist already and are limited to `main`. Then set the repository variable `TIERSTORE_SITE_PUBLISH` to `true`: the site publishes on the next push that changes it. Then, if the sibling repository still lists tierstore.dev, its owner removes it there (section 4).
7. **Release bot.** The app (`RELEASE_BOT_APP_ID` is already a variable of `release`): add the secret `RELEASE_BOT_PRIVATE_KEY` to the environment `release`, and make sure the app is installed on `komira-ai/homebrew-tap` with the rights to push a branch and open a pull request there. If the app is not installed on the tap, the mint step of `tap` fails and `tap` is red; nothing else is affected. The tap needs no change for tierstore (its guard derives the name from the branch); if its auto-merge is off, the pull request waits for a maintainer.
8. **apt.** In komira-internal (`infra/apt_repo`): add `{ name: 'tierstore', repo: 'tierstore' }` to `PRODUCTS` and tierstore's repository id to the `repoIds` context, and redeploy that stack: it adds `komira-apt-publish-tierstore` and its policy. Then set in `apt-publish` the four variables `APT_PUBLISH_ROLE_ARN`, `APT_BUCKET`, `APT_DISTRIBUTION_ID`, `APT_HOSTNAME` (the role is tierstore's own; the others are those of the shared repository, host `packages.komira.ai`), and in `apt-signing` the fingerprint variable and the two secrets of the shared signing key. That change is in komira-internal and is not made by this repository's pull requests; it is listed here because the channel does nothing without it.
9. **The farm gate.** In the environment `release`: the variables `HOSTKEEPER_COORDINATOR_URL`, `HOSTKEEPER_COORDINATOR_CA` (and the tailnet ones if the coordinator is reached over it) and, in the coordinator's configuration, an allow-list entry for this repository that admits the event `workflow_run` (what starts `release.yml`) and `workflow_dispatch`, `refs/heads/main`, the audience the workflow passes, the service `tierstore` and the `job_workflow_ref` of the pinned `set-desired.yml`; the coordinator reads it at start, so it is restarted. Then set `FARM_SOAK_HOURS` to `0`. Until `HOSTKEEPER_COORDINATOR_URL` is set no public channel runs (5.3). Raise `FARM_SOAK_HOURS` when the farm has been stable; a change of the variable needs no pull request.
10. **The first release after the settings** is the proof of every step. Watch it: the upload (a `403` is a role condition that AWS reads differently from the test), the tap's pull request, the apt index. A channel not yet ready stays a green no-op, so the steps can be taken one channel at a time, in any order after 0 and 1, except that the public channels run only once step 9 is made; the order above is the one that has each step's inputs.

Step 0 and step 9 are the farm; steps 3 to 6 are the site and the release files, 7 the tap, 8 apt: the three public groups are independent of one another and all of them follow the farm.

## 8. Failure modes

**The farm is unhealthy.** `farm` is red (a failed host, a halt, a version marked bad, a timeout of the bounded wait, a host down) or `soak` is red (the farm stopped being healthy inside the N hours, or the coordinator was not reachable). The public jobs are skipped; the release stays a GitHub release. tierstore.dev's `latest.json`, the tap and apt stay at the last release that passed. Fixing the farm or rolling it back (5.3) and re-running the failed jobs of the run continues from where it stopped, and the public channels then run; or the next push makes the next release, and a release the farm never took is never published publicly (the tap's guard and the apt merge only go up, so the next release's channels simply carry the newer number). A coordinator that cannot be reached (the tailnet join or the listener) is the same: red, nothing public.

**The farm moved but the soak never ran.** Possible only if `soak` is skipped by a bug of the workflow; the workflow lint holds that every public job lists `soak` in `needs` and runs only when it succeeded, and a test of the lint with a public job lacking it must fail (T15).

**Half-made settings.** Some of a job's settings set, others not: the job is red and names the missing (5.1), writes nothing, and the release is unaffected. A variable that is set but wrong (a role ARN of another role, a bucket of another name) is not a half-made setting; the first write is refused by AWS (the role's trust or policy), the job is red with AWS's answer, and nothing is written. The release was made, the other channels ran (if the farm gate let them). The fix is the variable, then a re-run of the failed job.

**A role's trust does not match.** The first `AssumeRoleWithWebIdentity` is refused (`Not authorized to perform sts:AssumeRoleWithWebIdentity`) when the `sub` the job sends is not the one the role trusts: a different environment name, a workflow file moved, a repository renamed (the immutable subject survives a rename; a delete and re-create does not). The workflow file names in section 4 are part of the contract: renaming `release.yml` or `tierstore-site-publish.yml` is a change to the stack.

**A re-run.** Re-run the failed jobs of a run, not the whole run (as `release.yml` says today). Every channel job is made to be run again:

- *upload*: a file that exists answers `412` to `If-None-Match`; the verb then reads the stored file's SHA-256 (`GetObject`, permitted) and passes when it equals the local one and fails (red, naming the file) when it does not. A different file at a version's key is never overwritten: it is a defect in what the release made, and the owner decides. `latest.json` is written last, so a run that failed halfway leaves a version without a pointer, never a pointer to missing files.
- *`latest.json` and an older release*: the verb reads `latest.json` and its ETag, and writes only when the version in it is lower than this release's, with `If-Match` on that ETag. A re-run of an older release, or an older release that finishes late, writes nothing. A `412` (another release wrote between the read and the write) makes the verb read again and decide again.
- *tap*: the pull request exists, or the formula at this version is in the tap already: finish with success. A pull request for a version lower than the tap's is refused by the tap's guard ("not higher than ... on the base"), and the job is red, which is correct: the tap has a newer formula.
- *apt*: a `.deb` already in the pool answers `412`; the verb compares its digest, which is why the writer is deterministic (section 3) and the modification time is the commit's. An index already listing this version is not written again.

**A lost apt index race.** Two writers (another product's release, or a second tierstore release) read the same index; one writes first; the other's `If-Match` is refused (`412`). The write job reports `conflict=true` and the next attempt (`-2`, then `-3`) reads the index again, merges again and signs again. The `.deb` it uploaded is in the pool already and is found there. The third attempt's write fails the job instead of starting a fourth. The index is the only shared mutable object; the pool is write-once, so a lost race loses no package. A release whose apt jobs all failed is still a release: run the apt jobs again.

**A release made between plan and publish.** The plan walks the first-parent history of main from its tip and names a tag and a commit. A newer commit can be released by a manual run (own concurrency group) in between, or a tag can appear. `publish` refuses an existing tag or release, so no channel job runs for the lost plan (they need `publish`). When both releases do publish, they publish in the order they finish, and:

- `latest.json` never goes down (above);
- the apt merge refuses a version older than the newest listed, and the tap's guard refuses a version not higher than the base;
- a file of `releases/<V>/` belongs to its own version, so the two releases do not meet in the bucket.

The automatic path cannot reach this state: the workflow's group serialises runs triggered by pushes, never cancels one, and holds the next until the first is done.

**A `.deb` or a tarball with wrong bytes.** Found before publishing, by `release check` (the copies are the commit-named tarballs, the packages are rebuilt from them by the same verb in `verify` and again in the channel job, which compares them). After publishing, nothing here deletes a file: the role cannot, and a bad file under `releases/<V>/` is dealt with by a new release; the administrator can remove it from the bucket with their own credentials, and from the apt pool the same way.

**The site is down or the CDN is stale.** `latest.json` is cached for a minute and is the only mutable file; the files of a version are immutable. A user who sees an old `latest.json` sees an older complete version, never a mix.

**Partial deploys of this design's pull requests.** The jobs are no-ops until the settings exist (5.1); the workflow lint refuses a job with a stray write scope; the tap needs the bot's secret. A release made while only some pull requests have landed publishes the files that exist: if `release check` learns the copies in one pull request and the copies are made in another, the pull requests land in the order of section 10 so no state fails the check.

## 9. What does not change

- **The existing release files**: names, bytes and `SHA256SUMS`. The tap-named tarballs are additional files beside them; the commit-named ones stay, so every installer, pin and doc that names them keeps working (`docs/ci-and-release.md`, `packaging/README.md`, the install guide).
- **The hostkeeper packages and the update hook**: the `.tar.zst`, `index.json`, the `tierstore@a/@b` units and the plist. A host under hostkeeper takes the release as before; a host that installs the `.deb` is not under hostkeeper and does not update through it (apt does).
- **The signing identity.** The signer of every attestation is `artifacts.yml` on main; nothing here signs a build. The `.deb`s are not attested: they are rebuilt from the attested tarball and compared. The apt index is signed with the key the shared repository already uses (the secrets of `apt-signing`), not a new key and not a tierstore key; a user who has `packages.komira.ai` configured for another komira product gets tierstore without another key.
- **The formats of tierstore itself**: nothing here touches a stored format, the wire, the configuration of the server or a data path (AGENTS.md B16 does not apply: no job here handles object data). The only new formats are `latest.json` (section 3, additive) and the index of the shared apt repository (the existing format).
- **The PR checks.** No job is added to a pull request's checks (B9); the jobs are in `release.yml`, which runs after a push to main. Nothing darwin-specific is added to a pull request.
- **Release automation (B12).** Releases stay automatic from main: no manual tag, and nothing that holds a release for a person's verdict. The farm check and the soak are automatic and read what hostkeeper reports; the soak is a timer. The held steps of section 7 are one-time setup, not a gate on any release.
- **`hostkeeper-desired.yml`** stays as the by-hand way to set a version (for a rollback to an older release, say); `release.yml` does not call it, it runs the same `check` and the same reusable workflow.

## 10. The pull requests, in order

Each is one subject (P7), lands alone and leaves main releasing. Interfaces land before the code that uses them.

1. **This document.**
2. **`infra/site`**: the CDK app and its held steps and TypeScript tests (the two roles, their conditions, the bucket policy, the names). Nothing is deployed by it. `.github/workflows/tierstore-site.yml` runs its tests if a step is added to that existing workflow (B9).
3. **`release channel-files` and the extended `release check`**, with the `verify` steps and the tap-named assets of the GitHub release. The first release after it carries the extra files, which are inert until the tap uses them.
4. **`channels gate` and `channels soak`** and the `farm` and `soak` jobs, with the lint's list of jobs that may hold `id-token: write` and a lint rule that the public jobs need `soak`. Interfaces first (P7): this lands before any public job, and with no farm configured it makes `release.yml` skip nothing that exists today.
5. **`channels upload`** and the `channel-upload` job (needs `soak`).
6. **`channels formula` and `channels tap`** and the `tap` job (needs `soak`).
7. **`channels apt ...`** and the three apt jobs (and their `-2` and `-3` attempts; the first needs `soak`).
8. **Docs**: `docs/ci-and-release.md`, `packaging/README.md` and the install guide say how to install by brew and apt, in the pull request that makes each channel work (T19), not before.

Outside this repository: hostkeeper's farm (step 0 and the coordinator's entry of step 9, section 7), komira-internal (step 8 of section 7), and the sibling repository (removing its tierstore.dev entry, after the first deploy from here).

## 11. Open questions

- **Version in the apt index and `+`.** `tierstore --version` prints `X.Y.Z+<12 hex>` after the name, and the tap's test line matches `tierstore X.Y.Z` as a prefix of it. The `.deb` uses `X.Y.Z` as its version; the 12 hex digits go in a `Description` line or a control field of their own, not in the version, so apt's ordering is the tag's. To be confirmed by the pull request that writes the control file.
- **Number of versions kept in the apt index** per architecture: buildrules' tool has a `keep`; the value and whether it is a flag with a default (every tunable is a config key with a default) are chosen in the apt pull request.
- **A stand-in for `aws`** that the coverage run reaches (section 6).
- **A soak of more than 5 hours.** One hosted job is limited to 6 hours. If N should be larger, the soak needs a continuation that is not a job waiting: a `schedule` trigger on release.yml (the workflow lint pins its two triggers today) or a separate scheduled workflow that publishes the newest release whose N hours have passed. Not designed until N is raised above 5; `0` is the value now.
- **Reading the convergence again in `soak`.** `set-desired.yml` sets and then waits; whether it can be run again for the same version as a read (or whether hostkeeper's ci client has a read-only mode) is to be confirmed against hostkeeper's `docs/deploy/ci-client.md` in the pull request that writes `soak`; if it cannot, the pull request asks hostkeeper for one rather than setting a version twice.
- **`down` hosts.** The gate fails on a host the coordinator counts `down` (5.3). Whether the answer of `releases converge` carries that count in a form the tool can read, or the tool must read it from `releases status`, is settled in the pull request of `soak`.
