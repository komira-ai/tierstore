# Install

tierstore ships as a tarball per platform (Linux on x86_64 and arm64, macOS on Apple silicon) attached to every release of [this repository](https://github.com/komira-ai/tierstore/releases), with its SBOM and a provenance bundle. Versions up to 0.2.0 were built in another repository; from 0.3.0 on, releases are built and signed here. There are five ways to take it, all available today: [the install script](install.md#install-script), [Homebrew](install.md#homebrew), [apt](install.md#apt), [dnf](install.md#dnf) and [the tarball](install.md#take-the-tarball). The script, Homebrew and the tarball give you the binary alone; apt and dnf also add the account, the unit and an example configuration. There is no container image yet. hostkeeper packages exist and update tierstore through `tierstore update-hook` (see [Updates through hostkeeper](install.md#updates-through-hostkeeper)); without hostkeeper nothing updates the binary: to move to a new release, place the new binary the same way and restart the unit (on Linux; the sections from "Place the binary" on are for a Linux host).

## Platforms

| platform | tarball | minimum |
|---|---|---|
| Linux on x86_64 | `linux-x86_64` | glibc 2.28 |
| Linux on arm64 (aarch64) | `linux-arm64` | glibc 2.28 |
| macOS on Apple silicon | `macos-arm64` | macOS 15 (the release is built and signed on it) |

The Linux binaries are dynamically linked against glibc and are built for glibc 2.28 (`cargo zigbuild` with the target `<triple>.2.28`), not for the glibc of the machine that builds them: they start on any distribution with glibc 2.28 or newer, which is Debian 10, Ubuntu 18.10 and newer, RHEL, AlmaLinux and Rocky Linux 8 and newer, Amazon Linux 2023, Fedora, and SUSE Linux Enterprise 15. A host with an older glibc cannot run them: RHEL and CentOS 7, Amazon Linux 2 (glibc 2.26) and Ubuntu 18.04 (glibc 2.27) fail at start with ``version `GLIBC_2.xx' not found``. The Debian package depends on `libc6 (>= 2.28)` and the RPM package requires `glibc >= 2.28`, so `apt` and `dnf` refuse the install on such a host instead of installing a service that cannot start. There is no musl (Alpine) build. `ldd --version` prints the host's glibc.

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
install -m 0640 -o root -g tierstore config.toml /etc/tierstore/config.toml
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

The values in the example file are examples: `192.0.2.10` is a documentation address, the endpoints are `s3-hot.example` and `s3-cold.example`. Edit the address of the `s3` interface, the proxy id, the bucket names, the tiers and `hot_store.budget` in `/etc/tierstore/config.toml`. Give each process over the same tiers a proxy id of its own. [Configuration](configuration.md) lists every key. The unit gives only `--config` and the two flags of the drain.

The unit has no `[Install]` section, so installing it starts nothing and `systemctl enable` has nothing to link; it is also skipped while `/etc/tierstore/access.key` does not exist. If systemd already loaded the unit (the `daemon-reload` above does that), it keeps that copy until the next `daemon-reload` and warns that the unit changed on disk, so run it again after the edit to make the edit take effect. Start it by hand once the files are in place:

```sh
systemctl daemon-reload
systemctl start tierstore
```

A storage node over a new `dir` or `s3` hot tier (one with no format object) writes the format itself at its first start and takes writes at once; there is no step to run. An `s3` hot tier needs its marks bucket to exist first ([Configure](configure.md#the-marks-bucket-and-the-first-start)).

To start it at boot, add the section in a drop-in (`systemctl edit tierstore`, with `[Install]` and `WantedBy=multi-user.target`) and run `systemctl enable tierstore`.

## Restarts and the unit's exit rules

The unit restarts a failed start after 30 seconds. It does not restart exit status 2 (a command line or file the process refused, which a restart cannot mend) or 3 (the file system of a cache directory answered the start-up check of the cache wrongly). After 5 failed starts in an hour systemd stops trying; `systemctl reset-failed tierstore` and a new start try again. [Troubleshooting](troubleshooting.md) lists the exit statuses.

`TimeoutStopSec=` in the unit is above the sum of `server.drain_lb_wait`, `server.drain_deadline` (10s and 15m unless the file sets them) and the reconciler's lease wait, so systemd does not kill a drain in progress. If you raise `server.drain_deadline`, raise it too.

## Updates through hostkeeper

A host can instead let hostkeeper update tierstore through `tierstore update-hook` ([The update hook](update-hook.md)). hostkeeper's package of tierstore places the files that are in `packaging/hostkeeper/units` of the repository, and the release is reached through the link `current` of the package, which hostkeeper switches and the hook never moves:

| file | for |
|---|---|
| `tierstore@.service`, the instance template | Linux: the units `tierstore@a` and `tierstore@b`, both run `current/bin/tierstore` and both serve port 9300 |
| `tierstore.plist` | macOS: the one LaunchDaemon `/Library/LaunchDaemons/tierstore.plist` |

On Linux an instance starts only when its marker file, `/var/lib/tierstore/run-a` or `run-b`, exists: the hook writes the marker before it starts the instance and removes it when it retires it, and at boot only the instance the hook's record names starts. The hook's record and its lock are in `/var/lib/tierstore-hook`. With the archive package both units are enabled; with the Debian or RPM package hostkeeper starts the run units at its own start and the boot step `restore` starts the recorded one. A standby instance is inactive by design. On a Mac the update is a restart with a gap.

A release of the Debian or RPM package is updated the same way: its units are the template of the package (see "What the packages place"), which runs `/usr/libexec/tierstore/tierstore`; the package manager replaces that binary before `cutover`, and hostkeeper re-installs the previous package before a rollback.

### What this does not do yet

- An update is a hand-over only when both releases list `handoff-socket-v1` in `tierstore --capabilities` (and `handoff-cache-v1` too, for a configuration with a `[local_cache]` table) and the hot tier is not a directory; otherwise it is a restart, with a gap. The configuration need not name a `handoff_socket`: a release that lists `handoff-state-dir-v1` defaults it to `handoff.sock` in the unit's state directory, and an update from one that does not is a restart. This binary lists `handoff-socket-v1`, so the first update that hands over is the one from this release to a later one; the update to this release from one that does not list the words is a restart, which the hook chooses itself ([the hand-over socket](update-hook.md#the-hand-over-socket)).
- An instance stops in at most 975 s (the 70 s `server.drain_lb_wait`, the 900 s drain and 5 s for the lease); the unit's `TimeoutStopSec=` is above it, so a `cleanup` that finds the old instance still draining is deferred and run again.

## Other ways to take it

The tarball above is the base: the other ways install the same release. None of them starts the service.

### Install script

`curl -fsSL https://tierstore.dev/install.sh | sh` reads the newest release from the copy of the release files on tierstore.dev (below), downloads the tarball of the machine and `SHA256SUMS`, refuses unless the SHA-256 matches, and places `tierstore` in `~/.local/bin`. It starts nothing. `~/.local/bin` is on the `PATH` of many but not all hosts; the version check below names the file so that it works either way.

```sh
curl -fsSL https://tierstore.dev/install.sh | sh
~/.local/bin/tierstore --version
```

### Homebrew

A formula in the tap `komira-ai/tap`, for macOS on Apple silicon and for Linux. Homebrew places `tierstore` on the `PATH`:

```sh
brew install komira-ai/tap/tierstore
tierstore --version
```

### apt

For Debian 10 and newer and Ubuntu 20.04 and newer. The package `tierstore` is in the repository `packages.komira.ai`, shared with other Komira tools. Fetch the repository's key over HTTPS into `/usr/share/keyrings`, then write a deb822 source that trusts that file for this source only:

```sh
sudo install -d -m 0755 /usr/share/keyrings
sudo curl -fsSL https://packages.komira.ai/apt/komira-archive-keyring.gpg -o /usr/share/keyrings/komira-archive-keyring.gpg
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
/usr/libexec/tierstore/tierstore --version
```

apt checks the package against the key named in `Signed-By`. The first fetch of the key trusts TLS once; `gpg` can print the fingerprint of the file to compare with the one the repository publishes.

### dnf

For AlmaLinux, Rocky Linux and RHEL 8 and newer, Fedora and Amazon Linux 2023. The repository file names the repository's key; dnf checks the signature of the repository's metadata and of the package against it:

```sh
sudo curl -fsSLo /etc/yum.repos.d/komira.repo https://packages.komira.ai/rpm/komira.repo
sudo dnf install tierstore
/usr/libexec/tierstore/tierstore --version
```

### What the packages place

The deb and the rpm place the binary at `/usr/libexec/tierstore/tierstore`, as "Place the binary" above does by hand, and nothing on the `PATH`: run it by that path, or link it from a directory that is on the `PATH` yourself. They also place the unit, the `sysusers.d` entry and the example configuration, create the `tierstore` account, and run `systemctl daemon-reload`:

| path | what |
|---|---|
| `/usr/lib/systemd/system/tierstore@.service` | the template of the update hook's two instances, `tierstore@a` and `tierstore@b`; it runs `/usr/libexec/tierstore/tierstore --config /etc/tierstore/config.toml`, starts only when `/var/lib/tierstore/run-<instance>` exists, and is wanted by `multi-user.target` once enabled |
| `/usr/lib/sysusers.d/tierstore.conf` | the `tierstore` account |
| `/usr/share/doc/tierstore/examples/config.toml` | the example configuration; nothing reads it (the rpm's licence is under `/usr/share/licenses/tierstore`) |

The package places no file in `/etc`: there is no conffile, and an upgrade or a purge touches nothing of yours. The instances read `/etc/tierstore/config.toml`, which you write. The package makes `/etc/tierstore` (mode 0750, group `tierstore`) only when nothing is at that path; an existing directory and the files in it are left as they are. It starts and enables nothing, and writes no start marker, so a host with no configuration starts no server. The key files and the configuration are yours to write, as in the sections above.

A host without the update hook starts an instance by hand. Write `/etc/tierstore/config.toml` (start from the example) and the key files, then:

```sh
sudo install -d -m 0700 -o tierstore -g tierstore /var/lib/tierstore   # systemd makes it at start if you skip this
sudo touch /var/lib/tierstore/run-a
sudo systemctl enable --now tierstore@a
```

The unit runs with `ProtectSystem=strict` and `StateDirectory=tierstore`. A configuration that writes outside `/var/lib/tierstore` (a `dir` hot tier, a `[local_cache]` path, `hot_store.lock_file`) needs its paths in a drop-in of your own, `/etc/systemd/system/tierstore@.service.d/*.conf`, with `ReadWritePaths=` (and `RequiresMountsFor=` for a path on a mount of its own). The package never writes there.

The package manager replaces the binary on `apt upgrade` or `dnf upgrade`; the running instance keeps the old one until `sudo systemctl restart tierstore@a` (a restart, with a gap in serving). Under hostkeeper the update hook hands over instead (see [Updates through hostkeeper](#updates-through-hostkeeper)).

An earlier package placed `tierstore.service` and the conffile `/etc/tierstore/tierstore.toml`. An upgrade from it removes the unit file and leaves the old configuration file on disk (dnf keeps an edited one as `.rpmsave`); a `tierstore.service` that is running keeps running until stopped. Move the file to `/etc/tierstore/config.toml`, then `sudo systemctl stop tierstore.service`, create the marker and `sudo systemctl enable --now tierstore@a` as above.

### Release files on tierstore.dev

Each release is copied to `https://tierstore.dev/releases/<version>/`, written once and never changed, with `https://tierstore.dev/releases/latest.json` naming the newest. The releases of this repository on GitHub stay the place the files are built and signed; the checks under "Verify" above are the same for a file taken from either.
