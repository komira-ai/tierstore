# The macOS release files on a Mac of the build farm

The macOS release files of a commit (artifacts.yml's `macos-arm64`: the binary's tarball and
SBOM, its hostkeeper package, the tap tarball, `SHA256SUMS.macos-arm64` and the unsigned
installer) can be built on a Mac of the build farm instead of a GitHub-hosted macOS runner, by
the same steps. This page says how, what it needs, and what stays on hosted runners. The
mechanism is the one of komira-ai/kbf's `docs/design/mac-ci-on-the-farm.md` (a REAPI action
from a Linux runner, `cargo xtask mac-farm`), with one addition: the action's outputs are brought
back.

## Turning it on

It is off until the variable `MACOS_PACKAGE_ON_FARM` (of the repository or the organisation) is
`true`, exactly. Then, on a push to `main`:

- `macos-arm64-farm` runs (it calls `.github/workflows/macos-farm.yml`), and the hosted
  `macos-arm64` is skipped: their conditions are each other's negation for a push to `main`, so
  exactly one of them runs (the workflow lint holds both conditions);
- `sign-pkg` takes the unsigned installer of whichever ran (`pkg-unsigned-macos-arm64`, the same
  artifact), and `sums` takes `assets-macos-arm64` from it, as before.

A run by hand (`workflow_dispatch`, how a branch's macOS build is tried before it merges) and a
pull request are unchanged: a run by hand builds on the hosted runner, and a pull request builds
no macOS file. Unset the variable to go back.

It also needs the organisation variables the farm's other workflows read: `TS_CLIENT_ID` and
`TS_AUDIENCE` (the tailnet's federated identity, which must trust a token of this repository for
`refs/heads/main`, issued in `macos-farm.yml`), and `FARM_ADDRESS`, `FARM_INSTANCE`, `FARM_TLS`
and `FARM_DARWIN_ARM64_PROPERTIES` (the farm and its Mac platform). This repository holds no
endpoint and no property set.

## The job

`macos-farm.yml`'s one job runs on `ubuntu-24.04`:

1. it builds xtask, before the join, so no build script runs on the farm's network;
2. it joins the farm's Tailscale network with the run's OIDC token (`tag:ci`; no stored secret);
3. it runs one command, `cargo xtask mac-farm` (crates/xtask/src/mac_farm.rs), which gathers the
   action's inputs, runs it on a farm Mac, and writes its outputs into the checkout;
4. it uploads `pkg/` as `pkg-unsigned-macos-arm64` and `dist/` as `assets-macos-arm64`, each
   refused when empty, as the hosted job does.

The inputs of the action, since a farm Mac has the Command Line Tools and no Rust, and an action
has no network:

- the checkout's tracked files, as they are on disk;
- the crates of `Cargo.lock` for every platform (`cargo fetch` on the runner): `cargo metadata`,
  which the SBOM and the licence notice run, reads the manifest of every package of the lock,
  even with `--filter-platform`, so the Mac's crates alone are not enough. The crates are sent as
  their `.crate` files (`registry/cache`) and the index; Cargo unpacks them again on the Mac,
  offline, so the unpacked copies (`registry/src`, four times the size) are not sent;
- the Rust toolchain for `aarch64-apple-darwin`, pinned in crates/xtask/src/mac_farm/toolchain.toml
  by URL and SHA-256 (the release `rust-toolchain.toml` names; a test holds the two equal);
- cargo-cyclonedx for `aarch64-apple-darwin` (`--tool-url`, `--tool-sha256`, `--tool-member`:
  artifacts.yml's pin, which the lint holds equal), in the Cargo home's `bin/`, where `cargo
  cyclonedx` finds it.

The Command is `cargo xtask release macos-assets --version <X.Y.Z> --commit <sha> --scratch
target/macos-assets` (crates/xtask/src/release/macos_assets.rs), with the toolchain's Cargo,
offline, and the Mac's `/usr/bin` on the `PATH` (its linker, `tar`, `codesign`, `pkgbuild`,
`productbuild`, `pkgutil`). An action is one command, so the steps of the hosted job are one
verb: the version, the build, the binary's commit and version, the SBOM and the check that it left
`Cargo.lock` as it was, the licence notice, the package, the binary unpacked from it, the
hostkeeper package and its unpack by hostkeeper's rules, the tap tarball, the code signature of
the unpacked binary, the installer, and its read back. They are the hosted job's commands in the
same order with the same arguments (`$RUNNER_TEMP` is the scratch directory): the workflow lint
holds the two lists equal (crates/xtask/tests/workflows/macos_farm.rs), so a change to one that
is not made to the other is named before it merges. The first step that fails ends the run.

## The outputs

The Command declares `dist` and `pkg` as its `output_paths`, and the farm returns each as an
output directory (a `Tree`). `mac-farm` writes them into the checkout only when the command
exited 0, and fails instead when:

- a path of the result is not a plain relative path, or is neither a declared output nor below
  one;
- a declared output is not in the result (the command did not make it);
- an output is a symlink, or a tree names a directory it lacks or a name that is not one plain
  component;
- a file's bytes, read from the farm's CAS or sent inline, are not its digest (the file is
  removed, not left for the upload);
- something is at an output's path already.

So the files the upload publishes, and that `sign-pkg` and `attest` sign, are exactly the bytes
the action made.

## The token and the signer identity

The job holds `id-token: write`, for the tailnet join, while it runs repository code (xtask, and
the build scripts of its crates). A release's attestations are verified as signed by
`artifacts.yml` (`gh attestation verify --signer-workflow .../artifacts.yml`: the certificate's
`job_workflow_ref`). A token issued in a called workflow names the called file as its
`job_workflow_ref`, so a token this job could ask for names `macos-farm.yml` and cannot make an
attestation that passes as `artifacts.yml`'s; `attest` stays the one job that can. That is why the
job is a workflow of its own and not a job of artifacts.yml. The workflow lint lets
`id-token: write` be held by the calling job and by this job only, each under its exact
condition (a push to `main`; the calling job also needs the variable), and lets artifacts.yml call
this one local workflow by its path.

## Failures and limits

- No Mac free: the action waits in the farm's queue, and the log says it is queued. No Mac able to
  run it: the farm refuses the Execute at once, and the job fails naming the refusal.
- The farm cannot finish the job (lost, killed, over its time): the job fails with the farm's
  status and the end of the job's output, if the farm attached it.
- The action's timeout is 40 minutes (`mac-farm`'s default, under the farm's limit); the step's
  `timeout-minutes` (75) also bounds the time in the queue.
- A cut stream is followed with WaitExecution, and executed again when the farm has forgotten the
  operation. A result the farm has cached is used for the same action (the same commit, version
  and inputs), and the log says it was cached.
- `FARM_ADDRESS` unset: `mac-farm` runs nothing, and the uploads of the files it did not make fail
  the job, so a release never goes on without its macOS files.

## What stays on hosted runners

- `sign-pkg` and `check-pkg` (the installer signed with the Developer ID Installer identity and its
  signature checked): signing is not done on the farm.
- `beta.yml`'s `macos-arm64`, the tests of the workspace on macOS: they bind loopback sockets and
  serve NFS on loopback, which a farm Mac's action may not (its sandbox denies every network
  operation), and the tests that need `sudo` skip without it, so on a farm Mac that coverage would
  be lost unseen.
- `release.yml`'s `check-brew` and the macOS leg of `check-install-sh`: they install the released
  files over the network from a clean machine, which is their purpose.
