# The update hook

`tierstore update-hook` is the part of the binary that a host updater runs to put a new release in place without a gap. Any updater that speaks the protocol (the steps as arguments, a params file, an exit status and one JSON line on standard output) can run it. This page is for the person who writes the updater's declaration for tierstore or reads what a step said. The verb runs on Linux, where an update is the hand-over below, and on macOS, where one instance runs under launchd and an update is a plain restart with a brief gap in serving (see [On a Mac](update-hook.md#on-a-mac)). The steps run on the machine through its programs and files (see [On the machine](update-hook.md#on-the-machine)); no release carries the hook yet (see [Not in a release yet](update-hook.md#not-in-a-release-yet)).

## The shape of an update

Two instances run side by side during an update. Each has its own private port and its own unit: `tierstore@9301` and `tierstore@9302`, one unit template whose `%i` is the port. Clients call the public port 9300, and one nftables redirect sends it to the instance that serves. The new instance starts beside the old one and, finding the cache lock held, serves read-only. The hook moves the redirect to it. When the old instance has exited the new one takes the lock by itself and becomes read-write; nothing commands that, the hook only watches for it.

A release that lacks the capability `handover-v1` (see `tierstore --capabilities`), on either side of the update, is installed by a plain restart: the redirect is pointed at the new instance's port, the old instance is stopped, and then the new one is started, with a gap: new connections are refused from the moment the redirect moves, for the old instance's settle (`--drain-lb-wait`), its exit and the new one's start, while the old instance serves the connections it already has to the end of its drain. So is every update of a host whose params file says `mode = "restart"` (see [The params file](update-hook.md#the-params-file)).

## The command line

```text
tierstore update-hook prepare <staged release> --params <file>
tierstore update-hook cutover --params <file>
tierstore update-hook verify --params <file>
tierstore update-hook rollback --params <file>
tierstore update-hook cleanup --params <file>
tierstore update-hook restore
```

The verb is answered before the server's flags are read. `--params` is the file the updater writes from the declaration's `[hook.params]` table and its own reserved keys. `restore` is the boot entry: it takes no file, because no update is running when it is called. The staged release given to `prepare` must be the one the params file names.

A release takes no other flag. A build with the `hook-conformance` feature, which only the crate's own tests turn on, takes six more, before or after the step, for every step and `restore`, so that a test can run the real binary against a scratch host, fake programs and ports of its own. In a release each value is the default below, and the words are refused as unexpected arguments:

| flag | default | what it sets |
|---|---|---|
| `--root <dir>` | `/` | the root the declaration, `etc/group`, `etc/passwd`, `proc/sys` and the state directory are read under |
| `--root-uid <uid>` | `0` | the user that must own the declaration and the binary of a staged release |
| `--nft <program>` | `/usr/sbin/nft` | the `nft` program |
| `--systemctl <program>` | `/usr/bin/systemctl` | the `systemctl` program |
| `--public-port <port>` | `9300` | the public port the redirect and the guard name and the probe asks |
| `--poll-interval-ms <ms>` | `1000` | what a step waits between two looks at an instance |

Each path must be absolute UTF-8, with no `.` or `..` and no doubled or trailing slash; each number must be in its range (a port is not 0); each flag may be given once. A public port that is one of the declared slots is refused (`state_invalid`).

```message
prepare needs the staged release directory
--params <file> is required
unexpected argument <word>
unknown step <word>
<flag> needs a value
<flag> must be an absolute UTF-8 path of plain components
<flag> must be a whole number in its range
```

A command line or a params file that is refused runs no step; the words go to stderr, prefixed with the verb, and the step's last line is a failure.

## The params file

A flat TOML table of numbers and strings. The keys the hook reads: `promotion_bound_secs` (required, 1 to 86400: how long `verify` waits, after the old instance has been stopped, for the new one to say `read-write`), `defer_retry_secs` (default 300: what a deferred `prepare` asks the updater to wait), `conntrack_percent` (default 80, 1 to 100: the share of the connection-tracking table `prepare` accepts), `mode` (optional, see below), and from the updater `updater.staged_release`, `updater.target_version` and, if it sets one, `updater.promotion_ceiling_secs`. Other scalar keys are ignored. The hook never fails because of the ceiling: the updater applies it.

`mode = "restart"` forces a plain restart: `prepare` chooses `restart` even when both releases list `handover-v1`. `"restart"` is the only value; any other string, a number, a boolean or an array is refused, and the refusal names the key (`bad_params`). Without the key the capabilities choose, as above. Packaging sets it in `[hook.params]` on a storage node whose hot tier is a `dir:` tier: there the lease's compare-and-swap is atomic only among the calls of one process, so two instances must never run over that tier side by side.

```text
[hook.params]
promotion_bound_secs = 900
mode = "restart"
```

```message
params key <key> must be the string "restart", the one mode it can force
```

## The record

The hook keeps one file, `state.json`, in the state directory of the declaration, and reads it through a validator before it makes any call to `nft`, `systemctl` or the slot links. It holds `schema`, `serving`, `previous`, `mode`, `markers`, `boot_id` and `guard_placed`:

```json
{"schema":1,"serving":9302,"previous":9301,"mode":"handover","markers":["swapped"]}
```

- `serving` is the private port that serves once the update is through. `prepare` sets it to the idle port, so while an update is in flight it names the new instance. With no update in flight it names the one instance.
- `previous` is the port of the old instance while an update is in flight, and absent otherwise.
- `mode` is `handover` or `restart`. Why `prepare` chose it is in that step's detail, not in the record (see `prepare` below), so that a hook of an earlier release, which refuses a field it does not know, still reads every record this one writes.
- `boot_id` is the boot of the host at the last successful `verify`, and `guard_placed` says that this update loaded the guard table (it was not there before). A record written without them reads with `boot_id` absent and `guard_placed` false.
- `markers` are `swapped` (the redirect names the new instance and it answers through the public port), `verified` (a `verify` succeeded) and `stopping` (`cleanup` has begun to retire the old instance). Each needs the one before it. `swapped` and `verified` are written after what they name has been done; `stopping` is written before `cleanup` removes a link or stops a unit, so a crash at any later point leaves the commit on record. After `cleanup` the three stay, with `previous` gone, so that a later `verify` knows it is watching a promotion; the next `prepare` starts afresh.

A record whose ports are not the two declared ones, whose `serving` and `previous` are equal, whose mode or marker is unknown, whose markers do not follow from one another, whose schema is not 1, or that is not JSON, is refused: exit 1, reason `state_invalid`, and no call to the machine. So is a record that is missing; the first install lays it down. The file is written whole and atomically, and only while the hook holds the exclusive lock of the state directory; a second step that finds it held exits 1 with the reason `locked`.

## The steps

Each step reads the record first and sets state; running it again after a crash at any point ends in the same state.

- `prepare`: reads the capabilities of the staged release and of the release the serving slot links to, and chooses the mode: `restart` when the params say `mode = "restart"`, `handover` when both releases list `handover-v1`, `restart` otherwise; its detail names the mode and the reason (`prepared slot 9302 beside 9301, mode restart (declared_restart)`); loads the guard table before anything else is applied; defers (exit 75) if the guard table cannot be loaded, if the connection-tracking established timeout is not longer than 120 seconds, or if its table is filled to the share the params allow, and a deferral removes the guard table this update loaded (one that was there before stays; if the removal fails the exit is 1); records the update; links the idle slot to the staged release; and, in handover mode, starts the idle unit and waits for `/readyz` to say `ready read-only`. An instance that never answers is exit 1; one that says `ready read-write` beside a running one is exit 1 too: two writers.
- `cutover`: makes sure the new instance answers `/readyz`; applies the redirect to it in one `nft` transaction; then asks through the public port until the new instance, by its build gauge, answers three times in a row with the staged version. A probe that never succeeds puts the old rule back and exits 1. It then records `swapped`. In restart mode the redirect is first pointed at the new instance's port, where nothing listens yet, so that a new connection is refused rather than reset by the stopping instance; then the old unit is stopped and the new one started. A probe that fails points the redirect back at the old port, then stops the new unit before it starts the old one again.
- `verify`: only observes, and records `verified`. Before `cleanup` it checks that the new instance is ready, the redirect names it, the public port is answered by it with the staged version, and the old instance still answers, and exits 0 with `promotion_pending`, since the old instance holds the lock. After `cleanup` has begun it waits up to `promotion_bound_secs` for `ready read-write`: exit 0, with `promotion_pending` if the bound passes. The detail then says that the promotion is pending; it does not name who holds the cache lock. It exits 1 only when the new instance answers wrongly or not at all.
- `rollback`: valid only before `cleanup` writes `stopping`. It starts the old unit if it is not running, points the redirect at it, stops the new unit, removes the new slot's link, removes the guard table if this update loaded it, and clears the record. In restart mode the redirect is pointed at the old port first, and the new unit is stopped before the old one is started, since the release that runs has no lock. After `stopping` it changes nothing and exits 1 with `"reversible":false`.
- `cleanup`: the commit. Unless the record says `verified` it does nothing and exits 1. It then looks again, in the same step, at what `verify` saw: the boot of the host is the one `verify` recorded (a reboot sends `restore` to the previous slot, which voids `verified`), the guard table is loaded, the redirect names the new instance, the new instance answers `/readyz`, and the public port is answered by it with the staged version; if any of these fails it exits 1 with `not_confirmed` and `"reversible":true`, and commits nothing. Then it writes `stopping`, removes the old slot's link, stops the old unit, which drains, and clears `previous`. The link goes first, so that the unit's condition on its link keeps a restart from bringing the old slot back. The hook never masks a unit. Run again, it goes forward.
- `restore`: at boot, loads the guard, points the redirect at one slot and starts that slot's unit; the other slot is not started. With no update in flight, or after `stopping`, that is `serving`; with an update in flight and no `stopping`, the old slot, so a reboot before the commit is an abort.

After `stopping` is written, every report says `"reversible":false`.

## What a step prints today

The last line of standard output is one JSON object, which the updater reads. `class`, `reversible` and `promotion_pending` are always there:

```json
{"class":"environment","reason":"host_not_ready","retry_after_secs":300,"reversible":true,"promotion_pending":false,"detail":"the conntrack table is nearly full"}
```

The exit status is 0 for success, 1 for failure and 75 for deferred (nothing was done, try later). The hook never exits 76. `class` is `environment` when the host or the hook's own inputs are at fault and `release` when the new release is; a deferral for the host is `environment`, a new instance that never becomes ready is `release`.

| `reason` | when |
|---|---|
| `bad_args`, `bad_params` | the command line or the params file was refused |
| `locked` | another step holds the lock of the state directory |
| `state_invalid` | the declaration, the record or the params are not as required |
| `out_of_order` | the step does not follow from the record |
| `not_verified` | `cleanup` before a `verify` succeeded |
| `committed` | `rollback` after `stopping` |
| `not_confirmed` | `cleanup` could not confirm, freshly, that the new instance serves alone |
| `host_not_ready` | exit 75: the guard or the connection-tracking bounds |
| `not_ready`, `unexpected_mode` | an instance does not answer `/readyz`, or is read-write beside a running one |
| `probe_failed`, `wrong_build`, `redirect_wrong`, `previous_down` | the public port, the build or the redirect is not what the step needs |
| `no_slot` | the slot to start has no link |
| `nft_failed`, `unit_failed`, `link_failed`, `state_io`, `host_failed` | the machine failed |

```message
this step needs the params file
the slot <port> has no link
no verify has succeeded
there is no record
```

## On the machine

This section is the Linux host; a Mac is [below](update-hook.md#on-a-mac). A step reads `/etc/updater.d/tierstore.toml` first. Its `state_dir` is where the lock, `state.json` and the slot links are; a declaration that is missing, cannot be read or is not trusted (each directory above it under the root, `/etc` and `/etc/updater.d`, is opened without following a link and must be owned by root with no group or other write bit, and the file, opened without following a link, must be a regular file owned by root with no group or other write bit) is `host_failed`, one that is refused is `state_invalid`, and in both nothing else has been touched. The step then takes the lock of that directory, reads the declaration again, and goes on as described above. Each port of the steps has an implementation in `update_hook::machine`: The declaration's `uid` names the service account (required); `state_dir` is refused when it has whitespace or a control character anywhere, is not absolute, plain and without a trailing slash, or has a component that starts with `-`, `+` or `!` (which systemd reads as a prefix), and when it is the root, `/var`, `/var/lib`, the updater's own `/var/lib/updater` or `/var/lib/updater/hooks`, or `/etc`, `/usr`, `/proc`, `/sys`, `/dev`, `/run`, `/boot` or another system directory, the service's `/var/lib/tierstore` or a cache under `/var/cache`, the places of people and temporary files and logs (`/root`, `/home`, `/tmp`, `/var/tmp`, `/var/log`, `/opt`), or anything under them. The state directory is read under the host's root, as the other files are.

- `nft` is run with an argument vector and no shell, never `sh -c`. The program is `/usr/sbin/nft`. `nft -f -` is fed the rule text on standard input. A table is absent when `nft list table inet <name>` or `nft delete table inet <name>` exits non-zero with `No such file or directory` on standard error; absence is `None` for the listing and fine for the delete, and any other failure is an error. A call is killed after 10 seconds.
- `/usr/bin/systemctl start -- <unit>` (killed after 60 seconds) and `/usr/bin/systemctl stop -- <unit>` (after 300 seconds, since it returns when the drain is over); a non-zero exit, a signal or a timeout is an error. systemd.unit(5) says that a failing `ConditionPathExists=` skips the start and does not fail the unit; it does not state the exit status of `systemctl start` then, which is 0 on the systemd versions it was written against. A start skipped that way is therefore not taken for a running instance: `prepare` and a restart-mode `cutover` wait for `/readyz` to answer, and fail with `not_ready` when it never does.
- The slot links are `<state_dir>/slots/<port>`, replaced by renaming a new link over the old one. A path there that is not a link is an error.
- The instances are asked with a blocking `GET` on `127.0.0.1`, with a connect timeout of 2 seconds and then 5 seconds for the whole request: its write and every read of the answer together, so an answer that comes a piece at a time cannot keep a look going longer. A status other than 200 is an answer. A chunked answer is refused.
- The boot id is read from `/proc/sys/kernel/random/boot_id` and the connection-tracking values from `/proc/sys/net/netfilter/nf_conntrack_*`. The capabilities of a staged release are the lines that `<release>/tierstore --capabilities` prints, asked for at most 10 seconds; the binary is refused unless it is a regular file, not a link, owned by root (the updater's verified tree) and not writable by a group or others. A program the hook runs gets an empty environment but for a fixed `PATH` and `LC_ALL=C`. A binary that exits with status 2, the status of an argument it does not know, has none; any other failure to ask is an error.
- The state directory is opened once, without following a link, and checked on that descriptor: a directory owned by the user and group the hook runs as (its own account, `tierstore-update`), with mode exactly 0750, and the declaration's `uid` (the service account) must be in that group, since the service reaches the slot links through the group: either its primary group (the `gid` field of its line in `/etc/passwd`) is that group, or it is listed among the group's members in `/etc/group`. Membership is read from those two files only, not through the name service switch (NSS, sssd, LDAP): the packaging's `sysusers` must create the account, and the group, in `/etc/passwd` and `/etc/group`. Every later open (the lock file, `state.json`, its temporary file, the `slots` directory, the renames and the syncs) is made relative to that descriptor. `state.json` is opened without following a link and must be a regular file owned by the hook's user that no group or others can write (0600 or 0640), on load and before a save replaces it. The lock is `flock` on the `lock` file, taken without waiting; `state.json` is written to a temporary file, synced, renamed over, and the directory synced. A mode is never repaired: what is not as required is refused (`state_io`).

None of these values is set by the declaration or the params file; they are fixed in a release (a conformance build's flags may name the root, the programs, the public port and the poll). Nothing is read from the environment.

## On a Mac

A Mac (macOS on Apple silicon) runs one instance of tierstore, a build node: a read cache on local disk (`[local_cache]`) in front of the cold store, on loopback only (`127.0.0.1:9300`), for the processes of that Mac. It runs as a LaunchDaemon, not as two slots, and there is no redirect: an update is a plain restart, and the instance does not serve from the moment launchd stops it until the new release answers `/readyz`. Expect that gap, a few seconds plus the old instance's drain, on every update; a client on the Mac sees connections refused for its length. The files are in `packaging/macos` of the repository (its README has the commands that place them):

| file | placed as | owner, mode |
|---|---|---|
| `com.komira.tierstore.plist`, the job (label `com.komira.tierstore`) | `/Library/LaunchDaemons/com.komira.tierstore.plist` | root:wheel, 0644 |
| `tierstore.toml`, the build node's file | `/usr/local/tierstore/etc/tierstore.toml` | root:_tierstore, 0640 |
| `updater.d/tierstore.toml`, the declaration | `/usr/local/etc/updater.d/tierstore.toml` | root:wheel, 0644 |

and the paths the job and the hook use:

| path | what | owner, mode |
|---|---|---|
| `/usr/local/tierstore/current`, `/usr/local/tierstore/previous` | the release links: the job's program is `current/tierstore`; `previous` exists while an update is in flight | root, in a directory of root's, 0755 |
| `/usr/local/tierstore/etc/` | the file and the key files (each `_tierstore`, 0600) | root:_tierstore, 0750 |
| `/usr/local/tierstore/cache`, `/usr/local/tierstore/log` | the cache, and the job's standard output and error | _tierstore, 0700 |
| `/var/db/updater/hooks/tierstore` | the hook's `state_dir`: the lock and `state.json` | root, 0700 |

The job runs as the account `_tierstore` with `RunAtLoad` and `KeepAlive`, so launchd starts it at boot and again whenever it exits while it is loaded. Its command line is `--config` and the two flags of the drain (`--drain-lb-wait 1s`, `--drain-deadline 60s`; on loopback there is no load balancer to wait for), and it sets no environment variable; its `ExitTimeOut` (90 s) covers the drain. The file pins an `s3` interface at `127.0.0.1:9300` and `server.slot` at `9300`: the hook probes that port and reads that slot label of the build gauge.

The hook runs as root, since only root may bootstrap and boot out a job of the system domain. The declaration is `/usr/local/etc/updater.d/tierstore.toml` (`/etc` is a link on macOS, and no directory above the declaration may be one); every directory above it and the file must be owned by root with no group or other write bit, as on Linux. It is read by its own rules: `uid` (the account the job runs as) is required, `health_ports` is one port, `[hook] service` must be `com.komira.tierstore` and `[hook] units` must not be there, and `state_dir` follows the Linux rules of spelling and is refused when it is the root, `/usr`, `/usr/local`, `/var`, `/var/db`, `/var/db/updater` or `/var/db/updater/hooks`, or lies in a system tree (`/System`, `/Library`, `/Applications`, `/Users`, `/Volumes`, `/private` and the links into it: `/etc`, `/tmp`, `/var/log`, `/var/tmp`, `/var/folders`, `/var/root`, `/var/run`, `/var/vm`), in `/bin`, `/sbin`, `/cores`, `/dev`, `/opt`, `/usr/bin`, `/usr/sbin`, `/usr/lib`, `/usr/libexec`, `/usr/share`, in the programs and settings under `/usr/local` (`bin`, `etc`, `lib`, `libexec`), or in `/usr/local/tierstore`. The state directory must be owned by the hook's user (root) with mode exactly 0700, and no group is checked: the job never reads it. The links directory, `/usr/local/tierstore`, must be owned by that user with no group or other write bit, since a link there chooses what the job runs.

The steps keep the contract of the Linux hook: the same command line, params file, report line and exit statuses, a lock for the whole step, and steps that set state, so that a re-run after a crash at any point ends where an uninterrupted run would. The record has a kind of its own, which neither hook takes for the other's:

```json
{"schema":1,"kind":"single","updating":false,"markers":[]}
```

`updating` says an update is in flight. While it is, the record also names `from`, the release `current` named when `prepare` recorded the update and the one a rollback goes back to; `to`, the staged release `prepare` checked; and `version`, the version `to` must report. After the commit `to` and `version` stay and `from` goes. `markers` are `swapped`, `verified` and `stopping`, as on Linux. A key the hook does not know is ignored, so that a later release may add one. The first install places the record above as `state.json` (root, 0600), with `current` a link to the first release. The link `previous` mirrors `from` for a person; the steps go by the record.

- `prepare` checks the staged release, and the release `current` names, on disk; it runs neither. The path is walked from the root one directory at a time, each opened without following a link. Every directory must be owned by root with no group or other write bit, and so must `tierstore` in it, which must be a regular file. So a path through a link (such as one through `current` or `previous`), a relative, dangling or looping one, and a release that another account could change are refused with `host_failed`, before anything is written. A release staged under `/var` or `/tmp` is reached through a link on macOS; stage it on a path of real directories, such as `/usr/local/tierstore/releases/<commit>`. `prepare` then records the update (`from`, `to`, `version`) and points `previous` at `from`. Nothing is started or stopped. A second `prepare` before the cutover keeps the recorded `from`, whatever a crash left in `previous`. Its detail says `mode restart (single_instance)` and what it skips: the guard table, the conntrack check and a second instance, none of which a single instance has.
- Every step after `prepare` refuses, with `bad_params` and before it acts, params whose staged release or version is not the record's `to` and `version`.
- `cutover` stops the job, points `current` at the staged release, starts the job, waits for `/readyz` on the port (120 looks a second apart) and for three answers in a row from the staged version (60 looks), and writes `swapped`. A second `cutover` after `swapped` looks again: if `current` names the staged release and it serves, it only makes sure the job runs; otherwise it swaps again. If any of that fails, it points `current` back at `from`, restarts the job, waits for it to answer `/readyz`, clears `swapped` if it was written, and fails with the reason of what failed (`not_ready`, `probe_failed`, ...). Its detail ends `rolled back: the service runs <from> again`, or says what failed in the rollback. The update stays on record for the updater's `rollback`. It moves no redirect.
- `verify` checks that `current` names the staged release, that the port answers `/readyz` and the build gauge says the staged version, and waits for `read-write` as a restart-mode `verify` does; then it writes `verified`. No boot is recorded: a reboot starts the release `current` names, which is the one that ran before it.
- `rollback`, before the commit, points `current` back at `from` if it names another release and restarts the job (otherwise it only makes sure the job runs), and waits for it to answer `/readyz`. Only then does it remove `previous` and clear the update; a rollback whose release does not answer fails with `not_ready` and keeps both, so it can be run again.
- `cleanup` confirms afresh that `current` names the staged release and the port answers with it, writes `stopping` (the commit: every later report says `"reversible":false`), removes `previous` and clears the update. There is no previous instance to stop. A `cleanup` that fails reads the record back, so a write of `stopping` that reached the disk before it reported a failure is reported as the commit it is.
- `restore`, the boot entry, starts the job if it is not running and changes nothing else.

The job is stopped and started through `/bin/launchctl`, with an argument vector and no shell, as the Linux programs are:

- stop: `launchctl print system/com.komira.tierstore`; if the system domain has the job, `launchctl bootout system/com.komira.tierstore` (killed after 120 seconds: launchd sends SIGTERM and the instance drains, within the plist's `ExitTimeOut` of 90), then `print` again, a second apart, at most 30 times, until the domain no longer has it; only then is `current` moved;
- start: `print`; if the domain does not have the job, `launchctl bootstrap system /Library/LaunchDaemons/com.komira.tierstore.plist` (`RunAtLoad` starts it), and otherwise `launchctl kickstart system/com.komira.tierstore`, which starts it if it is not running.

`print` exits 0 for a job the domain has and 113 for one it does not; every other status of every call, a signal and a timeout (10 seconds, `bootout` aside) are `unit_failed`. A link that cannot be read, made or removed is `link_failed`. The hook runs nothing else: no `nft`, no `systemctl`, no release. The step also refuses a declaration that, read again under the lock, names another `state_dir` than the one whose lock it holds. The declaration's `[hook.timeout_secs]` covers the longest each step may take within these bounds: every call to its time limit, and every look at the instance to its connect timeout (2 s), its whole-request deadline (5 s) and its pause.

A hand-over on a Mac (two instances behind a `pf` redirect) is not built: every update of a Mac is a restart.

## Not in a release yet

No release carries the hook as a step the updater calls: the declarations, `packaging/updater.d/tierstore.toml` for Linux and `packaging/macos/updater.d/tierstore.toml` for a Mac, are in the repository and placed by hand under `/etc/updater.d` and `/usr/local/etc/updater.d` (see [Install](install.md) and [On a Mac](update-hook.md#on-a-mac)), not taken from a release. `tierstore --capabilities` lists `handover-v1`, since the binary has the cache lock with its read-only mode, the wildcard listeners and the lingering close in the drain of both fronts; an update from a release that does not list it is still a plain restart (see [The shape of an update](update-hook.md#the-shape-of-an-update)). The probe through the public port reads the `slot` label of the build gauge of `/metricz`, which an instance has when it is started with `server.slot <port>` (the slot unit passes its own port); an instance started without it names no slot, and the probe takes it for no instance. `/readyz` of a storage node says no mode.
