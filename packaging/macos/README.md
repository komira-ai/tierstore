# tierstore on a Mac

A Mac runs one instance as a LaunchDaemon, and on a Mac tierstore is only a local read cache: a build node (a read cache in front of the cold store) that only the processes of that Mac reach, on `127.0.0.1:9300`. It has no hot tier, no server role, no hand-off socket and no firewall guard; nothing on the network connects to it. Its job is `/Library/LaunchDaemons/tierstore.plist`, which the signed installer package installs (the file is `../macos-pkg/tierstore.plist`, the plist of `../hostkeeper/units/tierstore.plist` with the program `/Library/Application Support/tierstore/bin/tierstore`); hostkeeper updates it through `tierstore update-hook` by a plain restart, with a gap in serving ([The update hook](../../crates/tierstore/docs/update-hook.md#on-a-mac)). The package also installs the job of the file service, `tierstore fs`, `/Library/LaunchDaemons/tierstore-fs.plist` (`../macos-pkg/tierstore-fs.plist`, the plist of `../hostkeeper/units/tierstore-fs.plist` with the same program). This directory holds the two files the package does not: the node's configuration, `tierstore.toml`, and the file service's, `fs.toml`.

## How a Mac gets tierstore

The package is `tierstore-<version>-macos-arm64.pkg`, built by `cargo xtask release macos-pkg` and signed with a Developer ID Installer identity (see "The macOS installer package" in [`docs/ci-and-release.md`](../../docs/ci-and-release.md)). It installs exactly three files, owned by root: the binary `/Library/Application Support/tierstore/bin/tierstore` and the jobs `/Library/LaunchDaemons/tierstore.plist` and `/Library/LaunchDaemons/tierstore-fs.plist`. Its one script, the `postinstall` (`../macos-pkg/postinstall`), makes the account the jobs run as and their directories (below); it loads no job: nothing loads the file service's job but hostkeeper's update hook (`tierstore update-hook`, the manifest's param `fs_launchd_label`), which also bootstraps the other. The Mac's hostkeeper installs it with its `macos-pkg` installer:

- The release channel holds the package as `pkg/tierstore/<version>/tierstore-<version>-macos-arm64.pkg`, and the desired version names the package by that path and its digest, with the manifest `xtask::release::macos_pkg::manifest_json` writes.
- The Mac's signers file lists the Developer ID Installer identity the package is signed with (hostkeeper's `docs/design/packages-repositories.md`, section 4.7, says where the file is and what it holds); hostkeeper refuses a package signed by any other identity, and checks the package's shape before it installs it.
- The package's version is the release's `<X.Y.Z>`, the identifier `com.komira.tierstore`.

The updater declaration and the plist this directory held before (`updater.d/tierstore.toml`, `com.komira.tierstore.plist`) belonged to the earlier update path, whose hook is gone; they are removed, and nothing here installs a declaration.

The `postinstall` runs as root each time the package is installed, and makes:

- the account `_tierstore`, the plist's `UserName` and `GroupName`: a group and a user with no password, the shell `/usr/bin/false` and the home `/var/empty`, with the same id, the highest from 399 down to 300 that no user and no group has (with none free the install fails and makes nothing). An account that is there is kept as it is;
- `/usr/local/tierstore` (0755 `root:wheel`), its `etc` (0750 `root:_tierstore`), its `cache`, `log` and `run` (0700 `_tierstore:_tierstore`) and its `put` (0755 `_tierstore:_tierstore`, the root of the put ingest), made when missing and set to these owners and modes on every install; what they hold is never touched;
- `/usr/local/tierstore/run/fs` (0700 `_tierstore:_tierstore`), the runtime directory of `tierstore fs`, and its mount point `/private/var/tierstore/fs` (0755 `_tierstore:_tierstore`) with its parent `/private/var/tierstore` (0755 `root:wheel`). On an upgrade the file service's NFS mount may be on the mount point, a read-only file system on which `chown` and `chmod` fail, so the `postinstall` sets the mount point's owner and mode only when the listing of `mount` does not show it mounted.

The configuration and the key files are not the package's. The update hook's `prepare`, which runs before the install and so before the `postinstall`, fails (`config_missing`) while `/usr/local/tierstore/etc/tierstore.toml` is missing, so the file is placed before the package's first install, owned by root and readable by every user (a config package that the host group lists before tierstore does this); the `postinstall` then gives `etc` its group. Once the account is there, as root, from this directory, the file is placed so:

```sh
install -m 0640 -o root -g _tierstore tierstore.toml /usr/local/tierstore/etc/tierstore.toml
```

What the file holds that is the Mac's own:

- **The proxy id** is not written in the file. `server.proxy_id_from_facts` makes it of the prefix `tierstore-cache-` and the `host_id` of the facts file that hostkeeper writes (`/var/lib/hostkeeper/facts.json`, readable by every user), so each Mac has an id of its own. The file is read at each start and each `--check-config`; a start is refused (exit 2) if it is missing or has no `schema` of 1 or no `host_id`. A change of the Mac's facts takes a restart of the job.
- **The upstream** is the tierstore S3 servers of the six storage nodes, port 9300, in `cold_store.endpoints`. The file ships the documentation addresses `192.0.2.1` to `192.0.2.6`: replace them with the storage nodes' addresses before use. A read goes to the next address when a connect fails ([multi-endpoint store](../../docs/design/multi-endpoint-store.md)).
- **The cache** is under `/usr/local/tierstore/cache`, with a budget to set for the disk, `whole_fill_limit = "128MiB"` (at least the largest pack), and one policy, `cas/`, cached.
- **The file system** (an `fs` interface) serves the jobs' trees in this process, over NFSv3 on `127.0.0.1:12049` (loopback only, a fixed port so that a restart finds its mount), and mounts the export itself at `/private/var/tierstore/fs`. Its group and owner are the account `_tierstore`, by name (the numeric id differs from Mac to Mac), and its runtime directory is `/usr/local/tierstore/run/fs`, which the package makes and which the file names because launchd gives the job no runtime directory. The file has no `local_cache.fill_socket`: no fill socket is served, the file system calls the cache in the process (a file that still names one is read, the key ignored).
- **The mount provider** (a `mount-provider` interface) answers jobscheduler's agent on the socket `/usr/local/tierstore/run/provider.sock`, mode 0600 and owned by `_tierstore` (the agent runs as root), in the directory made above; the trees it makes appear under `/private/var/tierstore/fs`, the mount of the file's `fs` interface. A file with a `mount-provider` entry and no `fs` entry (the two-process layout this file had before, where `tierstore fs` served the mount) is refused at load, so a node that cannot serve a job its tree does not start. The agent is given `--mount-provider tierstore=/usr/local/tierstore/run/provider.sock` ([operations](../../crates/tierstore/docs/operations.md#the-mount-provider) says what it does).

The file service's configuration, `fs.toml`, is placed the same way (it holds no secret):

```sh
install -m 0640 -o root -g _tierstore fs.toml /usr/local/tierstore/etc/fs.toml
```

It names the account `_tierstore` by name (the numeric id differs from Mac to Mac), and repeats what `tierstore.toml` says: `fs.cache_path` (the node's `local_cache.path`), `fs.mountpoint` (the `mountpoint` of the node's `fs` interface and of its mount provider), the runtime directory, the account and the NFS address of that interface; its `fs.fill_socket` is read and ignored, as `tierstore.toml` has none. `crates/tierstore/tests/packaging_macos.rs` holds them together and checks that the binary accepts the file.

Then the key files in `/usr/local/tierstore/etc` (each owned by `_tierstore`, mode 0600), and hostkeeper starts the job. `crates/tierstore/tests/packaging_macos.rs` checks that the binary accepts the plist's command line and this file, and that the file serves `[local_cache]` on loopback only, on the port the hook probes.
