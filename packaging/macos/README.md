# tierstore on a Mac

A Mac runs one instance as a LaunchDaemon, a build node (a read cache in front of the cold store) that only the processes of that Mac reach, on `127.0.0.1:9300`. Its job is `../hostkeeper/units/tierstore.plist`, which hostkeeper's package of tierstore installs as `/Library/LaunchDaemons/tierstore.plist`; hostkeeper updates it through `tierstore update-hook` by a plain restart, with a gap in serving ([The update hook](../../crates/tierstore/docs/update-hook.md#on-a-mac)). This directory holds the one file the package does not: the node's configuration, `tierstore.toml`.

The updater declaration and the plist this directory held before (`updater.d/tierstore.toml`, `com.komira.tierstore.plist`) belonged to the earlier update path, whose hook is gone; they are removed, and nothing here installs a declaration.

As root, from this directory, once the package is installed:

```sh
dscl . -read /Users/_tierstore >/dev/null   # the account exists (made with dscl or sysadminctl)
install -d -m 0755 -o root -g wheel /usr/local/tierstore
install -d -m 0750 -o root -g _tierstore /usr/local/tierstore/etc
install -d -m 0700 -o _tierstore -g _tierstore /usr/local/tierstore/cache /usr/local/tierstore/log
install -m 0640 -o root -g _tierstore tierstore.toml /usr/local/tierstore/etc/tierstore.toml
```

Then the key files in `/usr/local/tierstore/etc` (each owned by `_tierstore`, mode 0600), and hostkeeper starts the job. `crates/tierstore/tests/packaging_macos.rs` checks that the binary accepts the plist's command line and this file, and that the file serves `[local_cache]` on loopback only, on the port the hook probes.
