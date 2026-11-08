# Install

tierstore ships as a tarball per platform (Linux on x86_64 and arm64, macOS on Apple silicon) attached to every release of [this repository](https://github.com/komira-ai/tierstore/releases), with its SBOM and a provenance bundle. Versions up to 0.2.0 were built in another repository; from 0.3.0 on, releases are built and signed here. There is no Homebrew formula, apt package or container image yet; Homebrew, apt and a copy of the release files on tierstore.dev are planned, and [Not in a release yet](install.md#not-in-a-release-yet) says how they are meant to be used. hostkeeper packages exist and update tierstore through `tierstore update-hook` (see [Updates through hostkeeper](install.md#updates-through-hostkeeper)); without hostkeeper nothing updates the binary: to move to a new release, place the new binary the same way and restart the unit (on Linux; the sections from "Place the binary" on are for a Linux host).

## Take the tarball

A release lists `tierstore-<commit>-linux-x86_64.tar.gz`, `tierstore-<commit>-linux-arm64.tar.gz` and `tierstore-<commit>-macos-arm64.tar.gz`, each with a `.sigstore.json` bundle, and all of them in `SHA256SUMS`. Take the tarball of the host's architecture by its digest, with its bundle and `SHA256SUMS` from the same release. `<commit>` is the full SHA in the tarball's name; below, `<tarball>` is the tarball's file name, `tierstore-<commit>-linux-x86_64.tar.gz` for instance.

## Verify

Three checks, in this order: the first proves the download is intact, the second or the third that this repository built and signed it. The second needs `gh`; the third needs only cosign, for a host whose `gh` has no `gh attestation` command.

### Integrity

```sh
sha256sum -c --ignore-missing SHA256SUMS
```

`SHA256SUMS` lists the tarballs and SBOMs of every platform, so `--ignore-missing` skips the ones you did not take. `SHA256SUMS` is not itself an attestation subject, so this check proves integrity, not authenticity: who built the file is what the next check proves.

### Authenticity with gh

This needs a `gh` that has the `gh attestation` command (Ubuntu 24.04's packaged `gh`, 2.46.0, does not: use the check without `gh` there).

```sh
gh attestation verify tierstore-<commit>-linux-x86_64.tar.gz --repo komira-ai/tierstore \
  --signer-workflow komira-ai/tierstore/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main --source-digest <commit> --deny-self-hosted-runners
```

`--source-digest` makes the check prove the build came from that commit, not only from the main workflow.

### Authenticity without gh

This needs only cosign, and checks the tarball against its `.sigstore.json` bundle from the same release; it prints `Verified OK`.

```sh
cosign verify-blob-attestation \
  --bundle <tarball>.sigstore.json \
  --type slsaprovenance1 \
  --certificate-identity-regexp '^https://github.com/komira-ai/tierstore/\.github/workflows/artifacts\.yml@refs/heads/main$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  <tarball>
```

`--type slsaprovenance1` is required: the bundle holds SLSA provenance v1, and with cosign's default type the check fails with "invalid predicate type".

With a `gh` that has `gh attestation` on a host with no network, verify against the bundle and a trusted root fetched beforehand on a host that has one, and copied over with the tarball:

```sh
gh attestation trusted-root > trusted_root.jsonl
```

```sh
gh attestation verify <tarball> --repo komira-ai/tierstore \
  --bundle <tarball>.sigstore.json --custom-trusted-root trusted_root.jsonl \
  --signer-workflow komira-ai/tierstore/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main --source-digest <commit> --deny-self-hosted-runners
```

### The signer

Every check above names one signer: the workflow file `.github/workflows/artifacts.yml` of this repository, run on `refs/heads/main`. That identity is part of what a release promises: if the file were renamed or moved, every command on this page would fail. [docs/ci-and-release.md](https://github.com/komira-ai/tierstore/blob/main/docs/ci-and-release.md) lists the files of a release and what each attests.

## macOS (Apple silicon)

The macOS binary is `tierstore-<commit>-macos-arm64.tar.gz`, built and signed by the same workflow as the Linux ones and checked the same way. It is built on a macOS 15 runner on Apple silicon, which runs it before it is packaged and refuses it unless it reports the version and the commit of the release. This section covers taking and verifying it. A Mac runs it as a LaunchDaemon, one build node on loopback that the host updater updates by a plain restart through `tierstore update-hook`: the files and the layout are in [The update hook](update-hook.md#on-a-mac).

Take the tarball, its bundle and `SHA256SUMS` from one release, with `gh` (Homebrew's `gh` has `gh attestation`) or with `curl` from the release page:

```sh
gh release download v<X.Y.Z> --repo komira-ai/tierstore \
  --pattern 'tierstore-*-macos-arm64.tar.gz*' --pattern SHA256SUMS
```

Check its digest against `SHA256SUMS` with `shasum`, which every Mac has; it prints `OK`:

```sh
grep -- '-macos-arm64\.tar\.gz$' SHA256SUMS | shasum -a 256 -c
```

Then check that this repository built and signed it, offline against its bundle: run the offline check of "Authenticity without gh" above (`gh attestation trusted-root` on a host with a network, then `gh attestation verify <tarball> --bundle <tarball>.sigstore.json --custom-trusted-root trusted_root.jsonl ...`), with `<tarball>` set to `tierstore-<commit>-macos-arm64.tar.gz`. cosign's check without `gh` works the same way.

The binary is not notarized by Apple. It is ad-hoc signed by the linker, as every binary for Apple silicon must be to run. `gh release download` and `curl` save it without the quarantine attribute, so macOS runs it as it runs any program installed from the command line, as a Linux host runs a binary fetched with `curl`; a file saved by a web browser carries the attribute, and macOS then refuses to open it because it is not notarized. Take it with `gh` or `curl`. Its authenticity is what the checks above prove, not an Apple signature.

```sh
tar -xzf tierstore-<commit>-macos-arm64.tar.gz tierstore
./tierstore --version
```

On a Mac this is the last step of this page: the sections after it (the placement under `/usr/libexec`, the account, the systemd unit and its start) are for a Linux host. The release goes in its own directory, `/usr/local/tierstore/current` is a link to it, and the job is placed as [On a Mac](update-hook.md#on-a-mac) lists.

## Place the binary

The tarball holds one member, the binary under its own name.

```sh
tar -xzf tierstore-<commit>-linux-x86_64.tar.gz tierstore
install -d -m 0755 /usr/libexec/tierstore
install -m 0755 -o root -g root tierstore /usr/libexec/tierstore/tierstore
```

## Create the account and the directories

The example unit runs as a `tierstore` account with no login and no home. The sysusers entry, the unit and the example configuration file are in `packaging` of the repository.

```sh
install -m 0644 tierstore.conf /etc/sysusers.d/tierstore.conf
systemd-sysusers /etc/sysusers.d/tierstore.conf
install -d -m 0750 -o root -g tierstore /etc/tierstore
install -m 0640 -o root -g tierstore tierstore.toml /etc/tierstore/tierstore.toml
install -m 0644 tierstore.service /etc/systemd/system/tierstore.service
systemctl daemon-reload
```

`/etc/tierstore` holds the key files and the configuration file. If a tier is a directory (`kind = "dir"`), create it, give it to the `tierstore` account, and add its path to `ReadWritePaths=` in the unit: the unit runs with `ProtectSystem=strict`, so the process cannot write anywhere else. Put the hot tier's directory on the SSD and the cold tier's on the spinning disks.

```sh
install -d -m 0700 -o tierstore -g tierstore /srv/tierstore/hot
install -d -m 0700 -o tierstore -g tierstore /srv/tierstore/cold
```

## Write the key files

Write each key file owned by `tierstore` with mode 0600 or 0400. [Keys and roles](keys.md) says which files you need and what each may do. A start with a key file that its group or others can access is refused.

## Edit the configuration file

The values in the example file are examples: `192.0.2.10` is a documentation address, the endpoints are `s3-hot.example` and `s3-cold.example`. Edit the address of the `s3` interface, the proxy id, the bucket names, the tiers and `hot_store.budget` in `/etc/tierstore/tierstore.toml`. Give each process over the same tiers a proxy id of its own. [Configuration](configuration.md) lists every key. The unit gives only `--config` and the two flags of the drain.

The unit has no `[Install]` section, so installing it starts nothing and `systemctl enable` has nothing to link; it is also skipped while `/etc/tierstore/access.key` does not exist. If systemd already loaded the unit (the `daemon-reload` above does that), it keeps that copy until the next `daemon-reload` and warns that the unit changed on disk, so run it again after the edit to make the edit take effect. Start it by hand once the files are in place:

```sh
systemctl daemon-reload
systemctl start tierstore
```

A storage node over a new `dir` or `s3` hot tier serves reads and answers writes 503 until the store is activated, which is done once, with the same tier flags the unit has:

```sh
tierstore format-epoch --hot s3:tierstore-hot --hot-endpoint http://s3-hot.example:9000 --hot-credentials-file /etc/tierstore/hot.credentials --activate --apply
```

Within seconds every process over the tier takes writes. [Operations](operations.md#rolling-out-the-entry-protocol) says what this does on a store that an earlier release wrote.

To start it at boot, add the section in a drop-in (`systemctl edit tierstore`, with `[Install]` and `WantedBy=multi-user.target`) and run `systemctl enable tierstore`.

## Restarts and the unit's exit rules

The unit restarts a failed start after 30 seconds. It does not restart exit status 2 (a command line or file the process refused, which a restart cannot mend) or 3 (the file system of a cache directory answered the start-up check of the cache wrongly). After 5 failed starts in an hour systemd stops trying; `systemctl reset-failed tierstore` and a new start try again. [Troubleshooting](troubleshooting.md) lists the exit statuses.

`TimeoutStopSec=` in the unit is above the sum of `--drain-lb-wait`, `--drain-deadline` and the reconciler's lease wait, so systemd does not kill a drain in progress. If you raise `--drain-deadline`, raise it too.

## Updates through hostkeeper

A host can instead let hostkeeper update tierstore through `tierstore update-hook` ([The update hook](update-hook.md)). hostkeeper's package of tierstore places the files that are in `packaging/hostkeeper/units` of the repository, and the release is reached through the link `current` of the package, which hostkeeper switches and the hook never moves:

| file | for |
|---|---|
| `tierstore@.service`, the instance template | Linux: the units `tierstore@a` and `tierstore@b`, both run `current/bin/tierstore` and both serve port 9300 |
| `tierstore.plist` | macOS: the one LaunchDaemon `/Library/LaunchDaemons/tierstore.plist` |

On Linux an instance starts only when its marker file, `/var/lib/tierstore/run-a` or `run-b`, exists: the hook writes the marker before it starts the instance and removes it when it retires it, and at boot only the instance the hook's record names starts. The hook's record and its lock are in `/var/lib/tierstore-hook`. Both units are enabled; a standby instance is inactive by design. On a Mac the update is a restart with a gap.

### What this does not do yet

- An update is a hand-over only when both releases list `handoff-socket-v1` in `tierstore --capabilities` (and `handoff-cache-v1` too, for a configuration with a `[local_cache]` table) and the configuration names a `handoff_socket`; otherwise it is a restart, with a gap. This binary lists `handoff-socket-v1`, so the first update that hands over is the one from this release to a later one; the update to this release of a node with `[local_cache]` needs `mode = "restart"` in its params, since its configuration cannot have the key while the earlier release runs ([the hand-over socket](update-hook.md#the-hand-over-socket)).
- An instance stops in at most 975 s (the 70 s `--drain-lb-wait`, the 900 s drain and 5 s for the lease); the unit's `TimeoutStopSec=` is above it, so a `cleanup` that finds the old instance still draining is deferred and run again.

## Not in a release yet

Three more ways to take tierstore are planned. None of them exists yet: the commands below fail today (there is no formula, no `tierstore` package in the apt repository, and nothing at the address), so take the tarball above. This section describes the intended use and is not held to the code until they exist.

### Homebrew

A formula in the tap `komira-ai/tap`, for macOS on Apple silicon and for Linux:

```sh
brew install komira-ai/tap/tierstore
```

### apt

The package `tierstore` in the repository `packages.komira.ai`, shared with other Komira tools. Fetch the repository's key over HTTPS into `/usr/share/keyrings`, compare its fingerprint with the one the repository publishes, then write a deb822 source that trusts that file for this source only:

```sh
sudo install -d -m 0755 /usr/share/keyrings
sudo curl -fsSL https://packages.komira.ai/apt/komira-archive-keyring.gpg -o /usr/share/keyrings/komira-archive-keyring.gpg
gpg --show-keys --with-fingerprint /usr/share/keyrings/komira-archive-keyring.gpg
```

```sh
sudo tee /etc/apt/sources.list.d/komira.sources >/dev/null <<'EOF'
Types: deb
URIs: https://packages.komira.ai/apt
Suites: stable
Components: main
Signed-By: /usr/share/keyrings/komira-archive-keyring.gpg
EOF
sudo apt update
sudo apt install tierstore
```

The first fetch of the key trusts TLS once; the fingerprint is the check against it. There is no rpm package.

### Release files on tierstore.dev

Each release is meant to be copied to `https://tierstore.dev/releases/<version>/`, written once and never changed, with `https://tierstore.dev/releases/latest.json` naming the newest. The releases of this repository on GitHub stay the place the files are built and signed; the checks under "Verify" above are the same for a file taken from either.
