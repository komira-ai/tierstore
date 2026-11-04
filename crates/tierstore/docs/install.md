# Install

tierstore ships as a tarball per platform (Linux on x86_64 and arm64, macOS on Apple silicon) attached to every release of [this repository](https://github.com/komira-ai/tierstore/releases), with its SBOM and a provenance bundle. Versions up to 0.2.0 were built in another repository; from 0.3.0 on, releases are built and signed here. There is no package and no container image, and nothing updates the binary afterwards: to move to a new release, place the new binary the same way and restart the unit (on Linux; the sections from "Place the binary" on are for a Linux host).

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

The unit restarts a failed start after 30 seconds. It does not restart exit status 2 (a command line or file the process refused, which a restart cannot mend) or 3 (a tier answered the start-up self-test wrongly). After 5 failed starts in an hour systemd stops trying; `systemctl reset-failed tierstore` and a new start try again. [Troubleshooting](troubleshooting.md) lists the exit statuses.

`TimeoutStopSec=` in the unit is above the sum of `--drain-lb-wait`, `--drain-deadline` and the reconciler's lease wait, so systemd does not kill a drain in progress. If you raise `--drain-deadline`, raise it too.

## Updates through the host updater

A host can instead run tierstore as two slot units that the host updater updates through `tierstore update-hook` ([The update hook](update-hook.md)), by a hand-over without a gap once both releases of an update can hand over (below). The files are in `packaging` of the repository, each placed owned by root with mode 0644:

| file | placed as |
|---|---|
| `updater.d/tierstore.toml`, the declaration | `/etc/updater.d/tierstore.toml` |
| `tierstore@.service`, the slot unit template | `/etc/systemd/system/tierstore@.service` |
| `tierstore-9301.toml` and `tierstore-9302.toml`, the configuration of each slot | `/etc/tierstore/tierstore-9301.toml` and `/etc/tierstore/tierstore-9302.toml` |
| `tierstore-update.conf`, the hook's account | `/etc/sysusers.d/tierstore-update.conf` |
| `tmpfiles.d/tierstore-update.conf`, the hook's state directory | `/etc/tmpfiles.d/tierstore-update.conf` |
| `polkit/50-tierstore-update.rules`, the hook's right to start and stop the two slots | `/etc/polkit-1/rules.d/50-tierstore-update.rules` |

The hook runs as `tierstore-update`, not as root and not as `tierstore`. Its state directory, `/var/lib/updater/hooks/tierstore`, is mode 0750 with owner and group `tierstore-update`; `tierstore` is a member of the group, so the service can run the binary a slot link points at and can change nothing there. The slot units have no `[Install]` section: at boot the updater runs `tierstore update-hook restore`, which starts the one slot the hook's record names.

### First install

A host that runs `tierstore.service` moves to the slot units once, in this order ([packaging/README.md](https://github.com/komira-ai/tierstore/blob/main/packaging/README.md#first-install) has each step in full):

1. Create the account and the state directory: place the sysusers.d and tmpfiles.d entries and have `systemd-sysusers` and `systemd-tmpfiles` create what they list.
2. Place the declaration, the slot unit and the two slot files (with the edits of the host's `tierstore.service` and its drop-ins and of its configuration file; the two slot files differ only in the port, the proxy id and `server.slot`) and the polkit rule, and run `systemctl daemon-reload`.
3. Place the binary as slot 9301: the release's `tierstore` in a directory that root owns, `slots/9301` a link to it, and the record `{"schema":1,"serving":9301,"mode":"handover","markers":[]}` as `state.json`, owned by `tierstore-update`, mode 0600, so that the redirect points at 9301.
4. Load the hook's guard table, `tierstore_guard`, as root, with the text the README gives (exactly what `restore` loads): it drops every connection to 9301 and 9302, IPv4 and IPv6, that neither comes over loopback nor was redirected from 9300. The slot unit listens on the wildcard of both families, so the guard comes before any slot runs.
5. Start the slot unit `tierstore@9301` by hand and wait until `/readyz` on `127.0.0.1:9301` answers 200 `ready`. Port 9300 is still the old unit's.
6. Have the updater run `tierstore update-hook restore`, its boot entry: it points the redirect of port 9300 at 9301 and starts `tierstore@9301`, which already runs. `restore` moves the redirect before it starts the unit and does not wait for the slot to answer, which is why step 5 comes first.
7. Check that port 9300 is answered by slot 9301 (the `slot` label of `tierstore_build_info` on `/metricz`). If it is not, or `restore` failed, `nft delete table inet tierstore_forward` hands port 9300 back to the old unit, which never stopped; then stop slot 9301, mend it and go back to step 5.
8. Last, stop and disable `tierstore.service`.

The old unit and slot 9301 run side by side between steps 5 and 8, which is safe over S3 tiers. A host with a `dir:` hot tier, or a build node, stops the old unit before step 5 and accepts the gap.

### What these files do not do yet

- An update is a hand-over only when both releases list `handover-v1` in `tierstore --capabilities`; otherwise it is a plain restart, with a gap. A release built before the capability was added does not list it, so the first update of a host from such a release is a restart, and only the updates after it hand over.
- The host updater reads the declaration placed under `/etc/updater.d` and does not yet take it from a release: it is placed, and replaced, by hand.
- A slot stops in at most 975 s (the 70 s `--drain-lb-wait`, the 900 s drain and 5 s for the lease), but the hook waits at most 300 s for `systemctl stop`, and `rollback` has 600 s. A drain past 300 s makes `cleanup` fail while systemd goes on stopping the unit; `cleanup` run again waits for that stop and finishes. Until the hook waits as long as the unit allows, a host that needs every step to succeed the first time keeps `--drain-deadline` at most 220s.
