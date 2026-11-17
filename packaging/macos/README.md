# tierstore on a Mac

A Mac runs one instance as a LaunchDaemon, and on a Mac tierstore is only a local read cache: a build node (a read cache in front of the cold store) that only the processes of that Mac reach, on `127.0.0.1:9300`. It has no hot tier, no server role, no hand-off socket and no firewall guard; nothing on the network connects to it. Its job is `/Library/LaunchDaemons/tierstore.plist`, which the signed installer package installs (the file is `../macos-pkg/tierstore.plist`, the plist of `../hostkeeper/units/tierstore.plist` with the program `/Library/Application Support/tierstore/bin/tierstore`); hostkeeper updates it through `tierstore update-hook` by a plain restart, with a gap in serving ([The update hook](../../crates/tierstore/docs/update-hook.md#on-a-mac)). This directory holds the one file the package does not: the node's configuration, `tierstore.toml`.

## How a Mac gets tierstore

The package is `tierstore-<version>-macos-arm64.pkg`, built by `cargo xtask release macos-pkg` and signed with a Developer ID Installer identity (see "The macOS installer package" in [`docs/ci-and-release.md`](../../docs/ci-and-release.md)). It installs exactly two files, owned by root: the binary `/Library/Application Support/tierstore/bin/tierstore` and the job `/Library/LaunchDaemons/tierstore.plist`. It has no script; hostkeeper's update hook bootstraps the job. The Mac's hostkeeper installs it with its `macos-pkg` installer:

- The release channel holds the package as `pkg/tierstore/<version>/tierstore-<version>-macos-arm64.pkg`, and the desired version names the package by that path and its digest, with the manifest `xtask::release::macos_pkg::manifest_json` writes.
- The Mac's signers file lists the Developer ID Installer identity the package is signed with (hostkeeper's `docs/design/packages-repositories.md`, section 4.7, says where the file is and what it holds); hostkeeper refuses a package signed by any other identity, and checks the package's shape before it installs it.
- The package's version is the release's `<X.Y.Z>`, the identifier `com.komira.tierstore`.

The updater declaration and the plist this directory held before (`updater.d/tierstore.toml`, `com.komira.tierstore.plist`) belonged to the earlier update path, whose hook is gone; they are removed, and nothing here installs a declaration.

As root, from this directory, once the package is installed:

```sh
dscl . -read /Users/_tierstore >/dev/null   # the account exists (made with dscl or sysadminctl)
install -d -m 0755 -o root -g wheel /usr/local/tierstore
install -d -m 0750 -o root -g _tierstore /usr/local/tierstore/etc
install -d -m 0700 -o _tierstore -g _tierstore /usr/local/tierstore/cache /usr/local/tierstore/log /usr/local/tierstore/run
install -m 0640 -o root -g _tierstore tierstore.toml /usr/local/tierstore/etc/tierstore.toml
```

What the file holds that is the Mac's own:

- **The proxy id** is not written in the file. `server.proxy_id_from_facts` makes it of the prefix `tierstore-cache-` and the `host_id` of the facts file that hostkeeper writes (`/var/lib/hostkeeper/facts.json`, readable by every user), so each Mac has an id of its own. The file is read at each start and each `--check-config`; a start is refused (exit 2) if it is missing or has no `schema` of 1 or no `host_id`. A change of the Mac's facts takes a restart of the job.
- **The upstream** is the tierstore S3 servers of the six storage nodes, port 9300, in `cold_store.endpoints`. The file ships the documentation addresses `192.0.2.1` to `192.0.2.6`: replace them with the storage nodes' addresses before use. A read goes to the next address when a connect fails ([multi-endpoint store](../../docs/design/multi-endpoint-store.md)).
- **The cache** is under `/usr/local/tierstore/cache`, with a budget to set for the disk, `whole_fill_limit = "128MiB"` (at least the largest pack), and one policy, `cas/`, cached.
- **The mount provider** (a `mount-provider` interface) answers jobscheduler's agent on the socket `/usr/local/tierstore/run/provider.sock`, mode 0600 and owned by `_tierstore` (the agent runs as root), in the directory made above; the trees it makes appear under `/private/var/tierstore/fs`, the Mac's `fs.mountpoint` of `tierstore fs`. The agent is given `--mount-provider tierstore=/usr/local/tierstore/run/provider.sock` ([operations](../../crates/tierstore/docs/operations.md#the-mount-provider) says what it does).

Then the key files in `/usr/local/tierstore/etc` (each owned by `_tierstore`, mode 0600), and hostkeeper starts the job. `crates/tierstore/tests/packaging_macos.rs` checks that the binary accepts the plist's command line and this file, and that the file serves `[local_cache]` on loopback only, on the port the hook probes.
