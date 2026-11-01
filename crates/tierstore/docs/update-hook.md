# The update hook

`tierstore update-hook` is the part of the binary that a host updater runs to put a new release in place without a gap. This page is for the person who writes the updater's declaration for tierstore or reads what a step said. The verb is Linux only. The steps run on the machine through its programs and files (see [On the machine](update-hook.md#on-the-machine)); no release carries the hook yet (see [Not in a release yet](update-hook.md#not-in-a-release-yet)).

## The shape of an update

Two instances run side by side during an update. Each has its own private port and its own unit: `tierstore@9301` and `tierstore@9302`, one unit template whose `%i` is the port. Clients call the public port 9300, and one nftables redirect sends it to the instance that serves. The new instance starts beside the old one and, finding the cache lock held, serves read-only. The hook moves the redirect to it. When the old instance has exited the new one takes the lock by itself and becomes read-write; nothing commands that, the hook only watches for it.

A release that lacks the capability `handover-v1` (see `tierstore --capabilities`), on either side of the update, is installed by a plain restart: the old instance is stopped before the new one is started, with a gap.

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

```message
prepare needs the staged release directory
--params <file> is required
unexpected argument <word>
unknown step <word>
```

A command line or a params file that is refused runs no step; the words go to stderr, prefixed with the verb, and the step's last line is a failure.

## The params file

A flat TOML table of numbers and strings. The keys the hook reads: `promotion_bound_secs` (required, 1 to 86400: how long `verify` waits, after the old instance has been stopped, for the new one to say `read-write`), `defer_retry_secs` (default 300: what a deferred `prepare` asks the updater to wait), `conntrack_percent` (default 80, 1 to 100: the share of the connection-tracking table `prepare` accepts), and from the updater `updater.staged_release`, `updater.target_version` and, if it sets one, `updater.promotion_ceiling_secs`. Other scalar keys are ignored. The hook never fails because of the ceiling: the updater applies it.

## The record

The hook keeps one file, `state.json`, in the state directory of the declaration, and reads it through a validator before it makes any call to `nft`, `systemctl` or the slot links. It holds `schema`, `serving`, `previous`, `mode`, `markers`, `boot_id` and `guard_placed`:

```json
{"schema":1,"serving":9302,"previous":9301,"mode":"handover","markers":["swapped"]}
```

- `serving` is the private port that serves once the update is through. `prepare` sets it to the idle port, so while an update is in flight it names the new instance. With no update in flight it names the one instance.
- `previous` is the port of the old instance while an update is in flight, and absent otherwise.
- `mode` is `handover` or `restart`.
- `boot_id` is the boot of the host at the last successful `verify`, and `guard_placed` says that this update loaded the guard table (it was not there before). A record written without them reads with `boot_id` absent and `guard_placed` false.
- `markers` are `swapped` (the redirect names the new instance and it answers through the public port), `verified` (a `verify` succeeded) and `stopping` (`cleanup` has begun to retire the old instance). Each needs the one before it. `swapped` and `verified` are written after what they name has been done; `stopping` is written before `cleanup` removes a link or stops a unit, so a crash at any later point leaves the commit on record. After `cleanup` the three stay, with `previous` gone, so that a later `verify` knows it is watching a promotion; the next `prepare` starts afresh.

A record whose ports are not the two declared ones, whose `serving` and `previous` are equal, whose mode or marker is unknown, whose markers do not follow from one another, whose schema is not 1, or that is not JSON, is refused: exit 1, reason `state_invalid`, and no call to the machine. So is a record that is missing; the first install lays it down. The file is written whole and atomically, and only while the hook holds the exclusive lock of the state directory; a second step that finds it held exits 1 with the reason `locked`.

## The steps

Each step reads the record first and sets state; running it again after a crash at any point ends in the same state.

- `prepare`: reads the capabilities of the staged release and of the release the serving slot links to; loads the guard table before anything else is applied; defers (exit 75) if the guard table cannot be loaded, if the connection-tracking established timeout is not longer than 120 seconds, or if its table is filled to the share the params allow, and a deferral removes the guard table this update loaded (one that was there before stays; if the removal fails the exit is 1); records the update; links the idle slot to the staged release; and, in handover mode, starts the idle unit and waits for `/readyz` to say `ready read-only`. An instance that never answers is exit 1; one that says `ready read-write` beside a running one is exit 1 too: two writers.
- `cutover`: makes sure the new instance answers `/readyz`; applies the redirect to it in one `nft` transaction; then asks through the public port until the new instance, by its build gauge, answers three times in a row with the staged version. A probe that never succeeds puts the old rule back and exits 1. It then records `swapped`. In restart mode the old unit is stopped and the new one started first; a probe that fails stops the new unit before it starts the old one again.
- `verify`: only observes, and records `verified`. Before `cleanup` it checks that the new instance is ready, the redirect names it, the public port is answered by it with the staged version, and the old instance still answers, and exits 0 with `promotion_pending`, since the old instance holds the lock. After `cleanup` has begun it waits up to `promotion_bound_secs` for `ready read-write`: exit 0, with `promotion_pending` if the bound passes. It exits 1 only when the new instance answers wrongly or not at all.
- `rollback`: valid only before `cleanup` writes `stopping`. It starts the old unit if it is not running, points the redirect at it, stops the new unit, removes the new slot's link, removes the guard table if this update loaded it, and clears the record. In restart mode the new unit is stopped before the old one is started, since the release that runs has no lock. After `stopping` it changes nothing and exits 1 with `"reversible":false`.
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

A step reads `/etc/updater.d/tierstore.toml` first. Its `state_dir` is where the lock, `state.json` and the slot links are; a declaration that is missing or cannot be read is `host_failed`, one that is refused is `state_invalid`, and in both nothing else has been touched. The step then takes the lock of that directory, reads the declaration again, and goes on as described above. Each port of the steps has an implementation in `update_hook::machine`:

- `nft` is run with an argument vector and no shell, never `sh -c`. The program is `/usr/sbin/nft`. `nft -f -` is fed the rule text on standard input. A table is absent when `nft list table inet <name>` or `nft delete table inet <name>` exits non-zero with `No such file or directory` on standard error; absence is `None` for the listing and fine for the delete, and any other failure is an error. A call is killed after 10 seconds.
- `/usr/bin/systemctl start -- <unit>` (killed after 60 seconds) and `/usr/bin/systemctl stop -- <unit>` (after 300 seconds, since it returns when the drain is over); a non-zero exit, a signal or a timeout is an error. systemd.unit(5) says that a failing `ConditionPathExists=` skips the start and does not fail the unit; it does not state the exit status of `systemctl start` then, which is 0 on the systemd versions it was written against.
- The slot links are `<state_dir>/slots/<port>`, replaced by renaming a new link over the old one. A path there that is not a link is an error.
- The instances are asked with a blocking `GET` on `127.0.0.1`, with a connect timeout of 2 seconds and a read timeout of 5. A status other than 200 is an answer. A chunked answer is refused.
- The boot id is read from `/proc/sys/kernel/random/boot_id` and the connection-tracking values from `/proc/sys/net/netfilter/nf_conntrack_*`. The capabilities of a staged release are the lines that `<release>/tierstore --capabilities` prints, asked for at most 10 seconds; the binary is refused unless it is a regular file, not a link, owned by root and not writable by a group or others. A program the hook runs gets an empty environment but for a fixed `PATH` and `LC_ALL=C`. A binary that exits with status 2, the status of an argument it does not know, has none; any other failure to ask is an error.
- The state directory must be a directory (not a link) owned by root, the updater's user, with no group or other write bit. The lock is `flock` on its `lock` file, taken without waiting; `state.json` is written to a temporary file, synced, renamed over, and the directory synced.

None of these values is set by the declaration or the params file; they are fixed in the hook. Nothing is read from the environment.

## Not in a release yet

No release carries the hook as a step the updater calls: `tierstore --capabilities` does not yet list `handover-v1`, so an updater that asks does not hand over. The build gauge of `/metricz` does not yet carry the slot label the probe through the public port needs, and `/readyz` of a storage node says no mode. The holder of the cache lock is not yet named when `verify` reports a pending promotion.
