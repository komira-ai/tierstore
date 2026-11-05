# The update hook

`tierstore update-hook` is the part of the binary that hostkeeper runs, as root, to put a new release of tierstore in place. hostkeeper owns the release directories and the link `current` that names the release in use; the hook owns the service: which instance runs, and the proof that the new release serves. This page is for the person who writes the package manifest for tierstore or reads what a step said.

On Linux (systemd) an update is a hand-over without a gap when both releases can do it, and a restart with a gap when they cannot. On macOS (launchd) an update is always a restart with a gap. Both arms are in every build; the params file chooses between them.

## The command line

```text
tierstore update-hook prepare <staged release> --params <file>
tierstore update-hook <cutover|verify|rollback|cleanup|uninstall|restore> --params <file>
```

hostkeeper runs one step per process. `prepare` is given the staged release directory twice, as the argument and as `updater.staged_release` in the params file; the two must agree, or the step is refused (`bad_args`). The verb is answered before the server's flags are parsed, and no value comes from the environment.

A step reports in two ways:

- the exit status: `0` success, `1` failure, `75` deferred (not now: hostkeeper runs the step again after `retry_after_secs`);
- the last line of standard output, one JSON object: `class` (`release` or `environment`: whom a failure is blamed on), `reversible` (whether the hook can still undo what it did; `false` is the commit), `promotion_pending`, and when set `reason` (a token from [the reasons](update-hook.md#the-reasons)), `retry_after_secs` and `detail` (a sentence for the log).

The steps are `prepare`, `cutover`, `verify`, `rollback`, `cleanup`, `uninstall` and `restore`. `restore` is the boot step.

## What hostkeeper does and what the hook does

hostkeeper switches the link `current` to the new release before `cutover`, and back to the previous release before `rollback`; it relinks the units the package ships (`units/`) with it. The hook never moves `current`. Both service managers start `current/bin/tierstore`, so starting an instance after the switch starts the release the link names: that is how `cutover` runs the new release and `rollback` the old.

The hook keeps one record of where an update stands, reads it at the start of every step and writes it as the step's acts happen, so a step that was killed is run again and ends where it would have. A step sets state rather than does things once.

## The params file

hostkeeper writes one TOML table of scalars for each run. A dotted key is quoted. A key the hook does not know is ignored.

| Key | Meaning |
| --- | --- |
| `"updater.staged_release"` | required: the unpacked version directory of the release being installed |
| `"updater.target_version"` | required: the version its binary must report, such as `0.5.0+0123456789ab` |
| `"updater.previous_release"` and `"updater.previous_version"` | both or neither: the release the host ran before. Neither means a first install |
| `"updater.promotion_ceiling_secs"` | read and not acted on: hostkeeper applies the ceiling itself |
| `service_manager` | `"systemd"` (the default when the key is absent) or `"launchd"`; any other value is refused by every step, `prepare` included (`service_manager_unknown`) |
| `launchd_label` | required with `service_manager = "launchd"`: the label of the one job |
| `config_file` | the path of the node's configuration file; `prepare` reads it |
| `mode` | `"restart"` is the only value: the update is a restart even when both releases could hand over (see [the hand-over socket](update-hook.md#the-hand-over-socket)) |

The boot step and `uninstall` carry the manifest's keys, the current release as `updater.staged_release` and `updater.target_version`, and no previous release. Every step after `prepare` refuses params whose staged release or target version differ from the update the record holds (`bad_params`).

A version is read back from the build gauge `tierstore_build_info{version="0.5.0",commit="0123456789ab"}` of `/metricz`; it matches a target version that is the version alone or the version, `+` and the commit.

## The shape of an update

### On Linux

The package ships one unit template, `tierstore@.service`, and the hook alternates between its two instances, `tierstore@a` and `tierstore@b`. Both run `current/bin/tierstore` and serve the public port 9300. Clients keep calling 9300; there is no redirect and no second port.

An instance starts only when its start marker exists: the template has `ConditionPathExists=/var/lib/tierstore/run-%i`, so `/var/lib/tierstore/run-a` starts `tierstore@a` and `/var/lib/tierstore/run-b` starts `tierstore@b`. A start that finds its marker missing is skipped, not failed: the unit stays inactive. Both instances are enabled at boot, and only the one with a marker runs.

The record names `live`, the instance that serves at rest, and, during an update, `next`, the other one. In `cutover` the hook sets the marker of `next` and starts it.

- **Hand-over.** `next` asks `live` for its listening sockets over the hand-over socket and takes them: no connection the kernel holds is reset. `live` then drains and exits with status 0, which the template does not restart (`Restart=on-failure`). `cutover` does not wait for the drain; `cleanup` does.
- **Restart.** The hook stops `live` first (the stop waits for its drain), then starts `next`: new connections are refused in between.
- **First install.** No instance serves, so `next` (`a`) starts and binds the port normally.

The next update starts the other letter: a, b, a, and so on. A cache node serves read-only while another instance holds the cache lock, and takes the lock, and becomes read-write, by itself once the old instance has exited; the hook only watches for it.

### On a Mac

One job, whose label is `launchd_label`, loaded from the plist `/Library/LaunchDaemons/tierstore.plist` (hostkeeper links it from the package's `units/`). An update is a restart with a gap: `cutover` boots the job out, then bootstraps the plist, which starts the release `current` now names. A first install bootstraps only, and boots nothing out. The record has no `live`, `next` or `mode`, and `prepare` does not ask for a hand-over socket. `prepare` does not run a release either: it checks that the `config_file` param is there and names a file, and no more, so a configuration that the live or the staged release refuses is found when the job starts. A Mac never hands over, and its configuration has no `handoff_socket` key; a configuration with `[server] handoff_socket` and `[local_cache]` still updates by a restart, whatever the releases list. The hook touches the one label and no other.

## The steps

### prepare

Nothing a client sees changes, and nothing is started.

- A record whose update is past `cutover` is refused (`out_of_order`): finish or roll back the update in flight first. A record at rest, committed or idle, starts a fresh update.
- Both arms: the `config_file` param must be there (`bad_params`) and name a file that exists (`config_missing`).
- systemd, an update, no `mode = "restart"`: the configuration must have the `handoff_socket` key of the `[server]` table, a non-empty string; else `handoff_not_configured`. The key is needed in every shape of node, a build node and a storage node with `[local_cache]` included. A configuration that is not TOML is refused the same way, with the parse message.
- systemd: the hook runs `<staged release>/bin/tierstore --capabilities` (a staged release with no binary there, or a binary that does not run, is `host_failed`), and for an update the previous release's too. The mode is a hand-over when `mode` is not `"restart"` and both list `handoff-socket-v1` (and, for a configuration with a top-level `[local_cache]` table, both also list `handoff-cache-v1`), and a restart otherwise. A first install records no mode.
- systemd, an update or a first install, whenever the params name a `config_file` (with `mode = "restart"` too): before anything is recorded or stopped, the hook has the staged release and then, for an update, the previous release check the configuration, by running `<release>/bin/tierstore --config <config_file> --check-config`. Exit 0 is a configuration the release accepts. Exit 2 is one it refuses: `prepare` fails with `config_refused`, and the detail says whether the staged or the previous release refused, its path, and the refusal line it printed. Any other exit, a signal, a binary that cannot be started and a check that does not end within 10 seconds are `host_failed`. Nothing is recorded and no unit is touched in either case. A first install has no previous release, so only the staged one checks. `mode = "restart"` without a `config_file` checks nothing. When the hook runs as root, the check runs as the account `tierstore` (the template's `User=`, found in `/etc/passwd`; an account that is not there is `host_failed`), because the check refuses key files that the user it runs as does not own, and the unit does not run as root. A hook that is not root runs the check as itself. The check is the loader's only: what a start finds of the disk, the tiers or the heartbeat is not checked.
- systemd: it looks at the units. Both running defers the step (75, `host_not_ready`, retry after 30 seconds). One running is `live`. None running is the record's `live` on an update, and no instance on a first install. `next` is the other one of `live`, or `a` when there is none.
- It saves the record: updating, from the previous release, to the staged release, the version, and on systemd `live`, `next` and the mode.

### cutover

Needs an update in flight that has not committed (`out_of_order`) with params that match it. As above, the mode decides how `next` starts. After the start the hook waits for the service to answer `/readyz` on 9300 and then for the build gauge to name the target version three times in a row (`not_ready`, `probe_failed`). The record gets `swapped` after that. A failure is reported and nothing is undone: hostkeeper switches `current` back and runs `rollback`. A re-run after `swapped` looks again and, if the new instance serves the target, succeeds without starting anything.

### verify

An observation. After the commit it looks at the service again (`observed`) and says whether the promotion is pending. Before it, it needs `swapped` (`out_of_order`), the new instance (on launchd, the job) running (`not_ready`) and the build gauge on 9300 naming the target version (`wrong_build`, exit 1). It records `verified` and exits 0. A build node that still serves read-only says `promotion_pending: true`: it has not taken the cache lock yet, and hostkeeper runs `verify` again until it does, within its promotion ceiling.

### rollback

Runs with `current` already back at the previous release, so starting an instance starts the previous binary; the hook selects no binary. Not updating: nothing to roll back, exit 0. After the commit: `committed`, exit 1.

- **First install.** The instance `cutover` started is stopped and its marker removed (on launchd the job is booted out), and the step fails with `no_previous_release`: there is nothing to go back to. The record keeps the update in flight with no markers, so a re-run says the same and a later `prepare` starts afresh.
- **Update, restart mode** (and launchd): the new instance is stopped, the old one started, and the hook waits for the previous version to serve; the record is idle again.
- **Update, hand-over mode:** if the new instance runs it holds the sockets. If the old instance still runs (it is draining after it handed over), the step defers with `draining` (75, retry after 10 seconds). Otherwise the old instance is started, takes the sockets back from the new one, which exits, and the hook waits for the previous version to serve. If the new instance is not running, an old instance that still serves means `cutover` never took over, and an old instance that does not serve is a drain that will end; with none running the old one is started cold. A new instance that is still draining after the sockets went back defers the step again (`draining`); the run after it finishes.

### cleanup

The commit. It needs a `verify` that succeeded (`not_verified`). Before the commit it confirms again, freshly, that the new instance runs and serves the target (`not_confirmed`; nothing is committed), then records `stopping`. From then on `reversible` is `false`: `rollback` is refused. On systemd, if the old instance still runs, the step defers with `draining` (75, retry after 10 seconds) and the next run finishes: it removes the old instance's marker and records the new instance as `live`, with no update in flight. On launchd there is no instance to retire. A re-run after `cleanup` finished says there is nothing left to clean.

### uninstall

Stops both instances (the job, on launchd) and removes the markers, and records no instance. It leaves the data, the cache and the configuration (`/var/lib/tierstore` apart from the markers, `/etc/tierstore`; on a Mac, `/usr/local/tierstore`) and the record directory as they are.

### restore

The boot step. It starts the instance the record names, and only it: the recorded `live`, or `next` when an update is in flight and `cutover` finished (`swapped`). Any other instance is stopped if it runs and its marker removed; the marker of the one that starts is set again if it was missing, which repairs a marker that disagrees with the record. No record, or no instance in it, is success with nothing started. On launchd it bootstraps the plist when the job is not loaded and does nothing when it is.

## The record

`state.json` in `/var/lib/tierstore-hook` (the state directory, `STATE_DIR`), which only root can use; the hook makes it when it is absent, and neither hostkeeper nor `uninstall` removes it. It is replaced whole (a temporary file, synced, renamed over) under an exclusive lock, so two steps never run at once: a step that finds the lock held fails with `locked`. The record is JSON with `schema` 1, `kind` `single`, `updating`, `from`, `to`, `version`, `markers` (`swapped`, `verified`, `stopping`, in that order of need), and on systemd `live`, `next` and `mode`. `swapped` and `verified` follow their acts; `stopping` is written before its act. A record that does not decode, names an instance that is not `a` or `b`, or has markers out of order is `state_invalid`, and nothing else is touched. A key the hook does not know is ignored.

## The start markers

`/var/lib/tierstore` (`MARKER_DIR`) is the template's `StateDirectory`: it holds the markers `run-a` and `run-b`, and the hand-over socket. The hook makes it with mode 0700 when it is absent, writes a marker as a regular file and never follows a link. A marker that is a directory or a link is an error (`state_io`).

## The hand-over socket

A hand-over needs the `handoff_socket` key in the `[server]` table of the node's configuration, the path of a Unix socket in a directory the service owns, for example `/var/lib/tierstore/handoff.sock`. The new instance connects to it, takes the listening sockets and then binds the path itself. It also needs both releases to list the capability `handoff-socket-v1` in `tierstore --capabilities`. For a configuration with a top-level `[local_cache]` table (a build node, or a storage node with a cache), both releases must also list `handoff-cache-v1`. When any of these is missing, the update is a restart, which `prepare` records as the mode. A configuration without `handoff_socket` is refused with `handoff_not_configured`, whatever the shape, unless the params force a restart.

This binary has the hand-over socket and lists `handoff-socket-v1`: the new instance takes the sockets of the running one over `server.handoff_socket`, waiting at most `server.handoff_wait` (10s) for it to hand them over (see [Live update by hand-off of the sockets](operations.md#live-update-by-hand-off-of-the-sockets)). An update from a release that does not list it is still a restart. A build node and a storage node with `[local_cache]` hand over too, and this binary lists `handoff-cache-v1` for it. The update to this release from v0.3.47 or an earlier one, of a node with `[local_cache]`, is a restart: that release lists `handoff-socket-v1` but its cache shapes refuse the key, so its instance has no control socket. Later updates of such a node hand over, once the key is in the configuration (see [The order of releases](update-hook.md#the-order-of-releases)).

`mode = "restart"` in the manifest's params forces a restart, and then the configuration needs no `handoff_socket` key. Set it on a storage node whose hot tier is a `dir:` tier: the lease's compare-and-swap there is atomic within one process only, so two instances must never run side by side. The hook does not detect such a tier; a host with one that omits `mode = "restart"` is a documented gap.

### The order of releases

A release that refuses `server.handoff_socket` on a build node or a storage node with `[local_cache]` (v0.3.47 and earlier) exits 2 at start when the key is there, and the unit's `RestartPreventExitStatus=2 3` then keeps it from being started again. So the key goes in by this order:

1. The first release that accepts the key on every shape of node (it lists `handoff-cache-v1`) goes in with `mode = "restart"` in the manifest's params. A restart needs no `handoff_socket` key, and the configuration still has none.
2. Only when that release is live is `server.handoff_socket` added to the node's configuration. While the live release still refuses the key, `prepare` of any update refuses the edited configuration with `config_refused`, naming the previous release, before anything is stopped.
3. The next release, whose manifest has no `mode = "restart"`, is the first hand-over: both releases list the capabilities, and the configuration has the key.

## The reasons

`reason` is a token of lowercase letters, digits and `_`. The class decides whether the fleet rolls back: `release` blames the new version, `environment` the host or the hook's inputs.

| Reason | Class | Meaning |
| --- | --- | --- |
| `bad_args` | environment | the command line was refused, or the artifact is not the staged release |
| `bad_params` | environment | the params file is unreadable or wrong, or names another update than the record's |
| `locked` | environment | another step holds the lock of the state directory |
| `state_invalid` | environment | the record is not as the hook requires |
| `out_of_order` | release | the step does not follow from the record: `cutover` before `prepare`, a `prepare` past `cutover`, and so on |
| `not_verified` | release | `cleanup` before a `verify` succeeded |
| `committed` | release | `rollback` after `cleanup` began |
| `not_confirmed` | release | `cleanup` could not confirm, freshly, that the new release serves; nothing was committed |
| `host_not_ready` | environment | both instances run; try again later (75) |
| `draining` | environment | an instance that gave its sockets away is still draining (75) |
| `not_ready` | release | `/readyz` on 9300 was not answered with 200, or the new instance is not running |
| `probe_failed` | release | 9300 was never answered by the release with the target version |
| `wrong_build` | release | the build gauge on 9300 is not the target version |
| `handoff_not_configured` | environment | the configuration has no `handoff_socket` key in `[server]`, or cannot be read as TOML |
| `config_missing` | environment | there is no file at `config_file` |
| `config_refused` | environment | the staged or the previous release refuses the configuration at `config_file` when it checks it (`--check-config` exits 2); the detail names the release and the line |
| `service_manager_unknown` | release | `service_manager` names a manager the hook does not drive |
| `no_previous_release` | release | `rollback` of a first install: there is nothing to go back to; the instance `cutover` started is stopped |
| `unit_failed` | environment | a unit or the launchd job could not be started, stopped or asked |
| `state_io` | environment | the record or a start marker could not be read or written |
| `host_failed` | environment | the host could not be asked: the configuration, or `--capabilities` or `--check-config` of a release (cannot be run, exits any status but its answer, or does not end), or the account `tierstore` is not in `/etc/passwd` |

## First install

A `prepare` whose params have no previous release is a first install. The hook starts `tierstore@a` (or bootstraps the job) with a normal bind. If it fails and hostkeeper rolls back, the instance is stopped, the step exits 1 with `no_previous_release`, and no instance is left running.

## What the hook leaves to hostkeeper

hostkeeper's hookless Force starts every `run` unit of the package and waits for every one of them to be active. A standby instance is inactive by design (it has no marker), so a Force reports a failure while tierstore serves. hostkeeper's observer treats one active unit as the service being live. Nothing in the hook changes that: a Force is not a hook step.

## Limits

- Hand-over needs both releases to list `handoff-socket-v1`; the first update from a release that does not is a restart.
- A host with a `dir:` hot tier must set `mode = "restart"`; the hook does not detect the tier.
- On macOS an update has a gap in serving while the job restarts.
