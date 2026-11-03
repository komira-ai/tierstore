# tierstore on a Mac

A Mac runs one instance as a LaunchDaemon, a build node (a read cache in front of the cold store) that only the processes of that Mac reach, on `127.0.0.1:9300`; the host updater updates it through `tierstore update-hook` by a plain restart, with a gap in serving while the job restarts. [The update hook](../../crates/tierstore/docs/update-hook.md#on-a-mac) lists where each file goes, who owns it, and what each step does. In short, as root, from this directory:

```sh
dscl . -read /Users/_tierstore >/dev/null   # the account exists (made with dscl or sysadminctl)
install -d -m 0755 -o root -g wheel /usr/local/tierstore /usr/local/tierstore/releases
install -d -m 0750 -o root -g _tierstore /usr/local/tierstore/etc
install -d -m 0700 -o _tierstore -g _tierstore /usr/local/tierstore/cache /usr/local/tierstore/log
install -d -m 0700 -o root -g wheel /var/db/updater/hooks/tierstore
install -d -m 0755 -o root -g wheel /usr/local/tierstore/releases/<commit>
install -m 0755 -o root -g wheel tierstore /usr/local/tierstore/releases/<commit>/tierstore
ln -s /usr/local/tierstore/releases/<commit> /usr/local/tierstore/current
install -m 0640 -o root -g _tierstore tierstore.toml /usr/local/tierstore/etc/tierstore.toml
install -d -m 0755 -o root -g wheel /usr/local/etc /usr/local/etc/updater.d
install -m 0644 -o root -g wheel updater.d/tierstore.toml /usr/local/etc/updater.d/tierstore.toml
install -m 0644 -o root -g wheel com.komira.tierstore.plist /Library/LaunchDaemons/com.komira.tierstore.plist
```

Every release, the first and those the updater stages, must sit on a path of real directories owned by root that no group or others can write (`/usr/local/tierstore/releases/<commit>` is one; `/var` and `/tmp` are links on macOS), with its `tierstore` a file of root's: the hook checks that before it points the job at a release, and refuses anything else. Then the key files in `/usr/local/tierstore/etc` (each `_tierstore`, mode 0600), the hook's first record as `/var/db/updater/hooks/tierstore/state.json` (root, mode 0600):

```json
{"schema":1,"kind":"single","updating":false,"markers":[]}
```

and `launchctl bootstrap system /Library/LaunchDaemons/com.komira.tierstore.plist`, which starts the job. The hook refuses a declaration in a directory a group or others can write, so `/usr/local/etc` and `/usr/local/etc/updater.d` must be root's with mode 0755 (the `install -d` above leaves an existing directory's owner as it is: check it). `crates/tierstore/tests/packaging_macos.rs` reads the three files and checks that the plist runs the binary of `current` as `_tierstore` with `KeepAlive` and `RunAtLoad`, that the binary accepts its command line and the file, that the file serves `[local_cache]` on loopback only, that `ExitTimeOut` covers the drain, and that the plist, the declaration, the file and the hook name the same job, program, account and port.
