# tierstore update hook: hand-over

The host updater this hook plugs into is hostkeeper, a separate program with a protocol of its own: a process protocol (steps as
arguments, a params file, an exit status and one JSON line), so a hook in any language can speak it; this one is written in Rust
because tierstore is.

Status: built. The hook is `crates/tierstore/src/server/update_hook/`, and the operator's page is
`crates/tierstore/docs/update-hook.md`. Section 0 is the design of record of the hook: the new instance takes the listening
sockets of the old one, so no redirect, no second port and no privilege beyond root's are involved. Sections 1 to 4, 9 and 10 describe the
port-forward design (nftables redirect, slot units on private ports, an updater declaration and a hook-only account) that the
hook of section 0 replaces; they are kept as the record of that design and each is marked superseded. Sections 5 to 8 (the cache lock,
entries and lease, the drain, the metrics) are about the server, which both designs share; where they speak of slots or a
redirect, read the two instances `tierstore@a` and `tierstore@b` and the hand-over.

tierstore ships its update as a subcommand of its own binary, `tierstore update-hook <step>`, run by hostkeeper: steps `prepare`,
`cutover`, `verify`, `rollback`, `cleanup`, `uninstall` and the boot step `restore`; exit `0`, `1`, `75` deferred; the last stdout line
is one JSON object; every step idempotent.

## 0. Live update by listening-socket hand-off (server side)

This replaces the port-forward of sections 1 to 4 and the slot ports. The hook uses it (`server/handoff/`, entered through
`run_handing`, with the control socket named by `server.handoff_socket`), on Linux; for `tierstore` on macOS there is no hand-over and it restarts (`tierstore fs` has one for its NFS socket, below).
The binary calls it: `run_serving` passes the hand-off of the settings, `server.handoff_socket` and `server.handoff_wait`
(default 10s, 1ms to 1h), to `run_handing`; without `server.handoff_socket` it binds as before and makes no control socket.
A build node and a storage node with `[local_cache]` hand off through the same `run_handing` machinery: `bind_fronts`
(`server/cli_serve.rs`) takes or binds the sockets, and `serve_front` (`server/cache_cli.rs`) uses it for the build node and
for the combined node. The sockets handed over include the loopback sockets the cache shape adds; a `server.handoff_socket`
inside `local_cache.path` is refused. A build with it lists `handoff-socket-v1` in `tierstore --capabilities`, beside
`handover-v1`, which the hook no longer reads, and `handoff-cache-v1` for the cache shapes. Released v0.3.47 lists
`handoff-socket-v1` but its cache shapes refuse the keys, so `prepare` requires `handoff-cache-v1` of both releases for a
configuration with a `[local_cache]` table: the update to this release from it is a restart.

Problem: an update must not drop a connection, must not need root, and must work the same on Linux and on macOS. A redirect
(`nft`, `pf`) needs root and differs per OS; two instances on two ports need a fixed port pair.

Solution: the new instance asks the old one for its bound listening sockets. They are the same sockets, so no connection
the kernel holds is reset when the old one stops accepting.

Sequence of one update (one control socket, a file in the state directory, mode 0600, its user only):

```
new                                     old (running)
 | connect, "take 1"  ----------------->  |
 |                                        | one taker at a time: another is answered "refused"
 | <---- "offer" + "<front> <addr>"...    |
 |       + the sockets (SCM_RIGHTS)       |
 | addresses match the settings?          |
 | "ack" (else "nak": old goes on) ---->  |
 |                                        | removes the control socket, begins its drain
 | <---- "done"                           | (requests in flight finish, then it exits)
 | binds the control socket itself        |
 | serves the inherited sockets           |
```

Edge cases, each proved by a test in `server/handoff/` or `tests/s3_front/handoff.rs`:

- A put half sent when the new instance takes over is answered by the old one, which waits for it
  (`a_request_in_flight_finishes_and_none_fails_while_the_sockets_change_hands`).
- A second taker is refused while one is at it, and a taker that says `nak` or goes away leaves the old instance able to
  serve the next (`a_second_taker_is_refused_while_the_first_is_at_it`, `an_offer_names_its_sockets_and_an_ack_ends_it`).
- No file, or a file no one listens on (an instance that was killed), means a cold start: the new instance binds its own
  sockets (`with_no_file_there_nothing_answers`, `a_file_no_one_listens_on_is_the_same_as_none`). A running instance that
  accepts and never answers is not a cold start: the start fails after `Handoff::wait`
  (`an_instance_that_never_answers_ends_in_a_timeout`).
- Settings that name other addresses than the running instance serves do not take its sockets, since a live update cannot
  change them (`settings_that_name_other_addresses_take_nothing`).
- A taker of another user is refused (`a_taker_of_another_user_is_refused`).

The same code hands over the NFS socket of `tierstore fs` on a Mac (`fs.nfs.handoff_socket`, `server/treefs/nfs/run.rs`): a
front kind `nfs` is a new line of the offer (`<front> <address>`; a taker that meets a front it does not know skips the line
and closes the descriptor, `an_old_release_ignores_the_new_front_kind_line`, so a front can be added without breaking the
update to or from a release that lacks it). What differs from the S3 front: the taker asks before it opens the table of tree
numbers or binds the control socket of `fs`, since the running process holds both, and it makes no mount, since the mount is
there. The old process stops accepting when `offer.serve` returns, answers the requests it has read, shuts down its side of
each connection and waits for the clients to close theirs (up to `fs.nfs.drain_wait`) so that no reply it wrote is lost to a
reset, and exits without unmounting (`the_old_instance_stops_accepting_at_ack_and_answers_every_owed_reply_before_it_exits`,
`the_write_side_of_each_connection_is_shut_down_before_exit`). A request it had not read is not answered: the client sends it
again on its new connection, and requests on trees are idempotent. The drain is `NFSTcp::handle_until` of the `komira-ai/nfsserve`
fork; a client that never closes is let go when `fs.nfs.drain_wait` has passed
(`a_client_that_never_closes_is_let_go_when_the_drain_wait_has_passed`).

Two processes must not write the table of tree numbers and the attachments file, which the taker reads after `done`. The offer
therefore calls a hold when a taker answers `ack`, before `done` is sent (`the_hold_is_told_before_the_taker_has_its_done`), and
releases it if that taker does not complete (`the_hold_is_released_when_the_taker_does_not_complete`); the hold waits for the attach
or detach in progress, and every later one is refused (`a_held_core_changes_neither_the_table_nor_the_attachments`,
`the_old_instance_writes_no_state_once_the_new_one_has_the_socket`). The old process gives numbers only at an attach, so it gives
none after the ack. And the taker finds what can fail in its start (the directories, the table, the attachments file, the
directory of its control socket) before it asks, since a start that fails after the ack leaves no one listening
(`a_start_that_cannot_work_is_refused_before_the_running_instance_gives_up_its_socket`). What it cannot find before (the
bind of the control socket itself, a full disk) is not checked.

Not built yet: the proxy id that does not derive from a port. No package sets either key, and the macOS package is not to
set `server.handoff_socket`: an update on a Mac is a restart, for every shape, until the launchd jobs `tierstore-fs@a` and
`@b` and the hook's record and rollback for them land (the next change); `fs.nfs.handoff_socket` is for an operator or a hook
that starts the two processes by hand.

### The hook on top of it

The package ships one unit template, `tierstore@.service`, and the hook alternates between its two instances, `tierstore@a` and
`tierstore@b`, both running `current/bin/tierstore` and serving the public port 9300. hostkeeper switches the link `current` before
`cutover` and back before `rollback`; the hook never does. An instance starts only when its marker file `/var/lib/tierstore/run-<i>`
is there (`ConditionPathExists=`), so at boot only the recorded one runs.

- `prepare` reads the node's configuration (`config_file`), requires `server.handoff_socket` for an update, asks both releases for
  `--capabilities`, and records the mode: a hand-over when both list `handoff-socket-v1` (and, for a configuration with a `[local_cache]` table, `handoff-cache-v1`), else a restart. `mode = "restart"` in the
  manifest's params records a restart whatever the capabilities say, for a storage node whose hot tier is a `dir:` tier, where the
  lease's compare-and-swap is not atomic across processes. Nothing is started.
- `cutover` sets the marker of the idle instance and starts it; in a hand-over it takes the sockets and the old instance drains and
  exits 0, which the template does not restart. In a restart the old instance is stopped first. It then waits for `/readyz` and for
  the build gauge of 9300 to name the target version.
- `verify` observes the same and says `promotion_pending` while a build node still serves read-only. `cleanup` is the commit: it
  confirms afresh, records `stopping`, waits for the old instance to exit and removes its marker. `rollback` starts the previous release
  on the old letter (the link is back), the new instance gives the sockets back and exits. `restore` starts only the recorded instance.
- A first install starts `tierstore@a` with a normal bind; its `rollback` stops it and fails with `no_previous_release`.
- On macOS one launchd job is booted out and bootstrapped again: an update has a gap.

The record is `state.json` in `/var/lib/tierstore-hook`, a JSON object whose keys are the previous release's single-instance record and
three optional keys of the systemd arm (`live`, `next`, `mode`), so a release reads the record of the one before it and the other way round.

## 1. Shape (superseded)

> Superseded by section 0 and `crates/tierstore/docs/update-hook.md`: this section describes the nftables redirect and the slot
> units, which the hook no longer uses. It is kept as the record of that design.

Two instances run side by side during an update, each on its own private port (slot units `tierstore@9301` and
`tierstore@9302`); clients call 9300, which one redirect rule sends to the current instance. v-next starts, finds the
cache lock held, and serves read-only. The hook swaps the rule to it. When v-current has exited, the kernel frees the
lock and v-next takes it by itself and enables writes. Nothing commands that: the hook observes it.

`crates/tierstore/src/server/handover.rs` is not a process hand-over: it hands a put's body from the request to the
backend's task (`hand_over`, `carry`, `received`). Two shapes run from one binary (`Settings::Storage`,
`Settings::Cache`; `server/config.rs`; `serve_until_signalled` in `server/cli.rs`, `serve_cache_until_signalled` in
`server/cache_cli.rs`): a **build node** (a `LocalCache` over a directory, no entries or lease, hazard: the shared
directory, section 5) and a **storage node** (stateless proxy plus a reconciler under a lease, hazard: lease and entries,
section 6).

**What tierstore registers at runtime** (liveness only, the updater protocol, 2): name `tierstore`, the version, health
`GET /readyz` on the instance's private port. **Exists**: `/readyz` is 200 until the drain begins, 503 after
(`Server::respond`, `probe`, `server/mod.rs`); `/healthz` is 200 while the process runs; both unsigned. **Gap**: the
version is only in the start line and `--version`, and is `CARGO_PKG_VERSION`, not a commit. New: `tierstore_build_info
{version,slot}` on `/metricz`; `/readyz`'s 200 body saying `ready read-only` or `ready read-write` (status unchanged);
`tierstore --capabilities`, one capability per line, where `handover-v1` means "has the lock, read-only mode, wildcard
listeners and the lingering drain".

## 2. The declaration (`/etc/updater.d/tierstore.toml`, installed with the release) (superseded)

> Superseded by section 0 and `crates/tierstore/docs/update-hook.md`: this section describes the nftables redirect and the slot
> units, which the hook no longer uses. It is kept as the record of that design.

| item | value | note |
|---|---|---|
| hook | `tierstore update-hook <step>`, `hook.user` and `hook.group` `tierstore-update` | the staged release's own binary; Rust, no shell. A **hook-only account** that differs from the service's `uid = "tierstore"` (the updater protocol, 3.4.1): the service must not be able to write what the hook trusts. The polkit rule for the units attaches to this account |
| `CAP_NET_ADMIN` | every step (`prepare`, `cutover`, `verify`, `rollback`, `cleanup`) and the boot entry (`restore`) | the updater's root part launches the step as a transient unit with the capability ambient (the updater protocol, 3.4); the hook is never root. Declared hook-wide, `hook.capabilities = ["CAP_NET_ADMIN"]`, with no `hook.step_capabilities` table: listing nftables rules over netlink needs `CAP_NET_ADMIN` too, so `verify` and `cleanup` need it as well, and the boot entry, which is not one of the five steps, runs with the hook-wide set (the updater protocol, 3.5) |
| units, families | `tierstore@9301.service`, `tierstore@9302.service` (one template, `%i` is the port, below); `AF_UNIX AF_INET AF_INET6 AF_NETLINK` | the only units it may start or stop (never mask or enable: polkit grants the hook only `org.freedesktop.systemd1.manage-units`, for these two units); netlink for nft |
| record directory | `state_dir = "/var/lib/updater/hooks/tierstore"`, created by the updater, owned by `tierstore-update` (the updater protocol, 2.4, 3.4.1) | holds `state.json` (temporary file, fsync, rename), an exclusive lock for the hook's own steps, and `slots/<port>`. **Outside every path the service may write**; never under `/run` |
| slot links | `<state_dir>/slots/<port>` points at a staged release (`/var/lib/updater/releases/tierstore/<digest>/`, root-owned, read-only), or is absent | a retired slot has none; the service executes it and cannot change it. **Conflict to settle**: the service must traverse `state_dir`, which 3.4.1 allows "through group membership" (mode 0750, the service in the group, read only) and the 2.4 example's `0700` does not |
| lock files, nft tables | build node `<cache>/.lock`, storage node `/var/lib/tierstore/instance.lock`; `tierstore_forward`, `tierstore_guard` | the lock is the service's (section 5), no step reads a value from it; tables: the updater protocol, 4.1.1 |
| read / write paths | read `/usr/sbin/nft` and `/etc/updater.d/tierstore.toml` (root-owned, 0644); `write_paths = []`, `state_dir` is implicit | no step touches the cache directory or the service's state. Asks of the 2.4 example: add the declaration to `read_paths` (the validator below reads `health_ports` and `units` from it); `/etc/tierstore` and `/usr/libexec/tierstore` are not read by any step. `read_paths` is a statement of intent only: under `ProtectSystem=strict` it confines nothing. That the hook cannot read the service's secrets rests on file modes and ownership: key files are 0600 and owned by the service user, and the hook account is in neither the service's user nor its group |
| timeouts (s), deadline | `prepare` 600, `cutover` 300, `verify` 1000, `rollback` 600, `cleanup` 1300; `verify_deadline_secs` 1600 | `verify`: `promotion_bound_secs` 900 + margin; `prepare`: self-test of the tiers (`--backend-check-wait`); `cleanup`: settle 70 + `--drain-deadline` 900 + lease 5 + margin, and the unit's `TimeoutStopSec` becomes 1100 (960 in `packaging/tierstore.service`, which assumes a 10 s wait); the deadline is at least `verify` plus `rollback` |
| applied by the updater | `promotion_ceiling_secs` 3600, `cleanup_retries` 3; `[hook.params]` `promotion_bound_secs = 900` | the one ceiling and the forward re-runs of `cleanup`; params passed as `--params <file>` with the reserved key `updater.promotion_ceiling_secs` added (section 3, `verify`) |

Slot unit template (no environment; the data path is the slot's file, as built in the design doc's item d2):
`ExecStart=<state_dir>/slots/%i/tierstore --config /etc/tierstore/tierstore-%i.toml
--drain-lb-wait 70s --drain-deadline 15m` and `ConditionPathExists=` the binary's path, so a slot with no link does not
start. The file of a slot has an `s3` interface at `0.0.0.0:<port>` and `[::]:<port>`, `server.proxy_id = "tierstore-<port>"` and
`server.slot = <port>`. No `[Install]`, as today's unit has none. One template starts both slots, so the settle (`--drain-lb-wait`,
[host-updates.md][hu] Question 13) is the same for either. The two addresses of an interface and the proxy id per slot are new (sections 4, 6).

**What starts the active slot at boot: the declaration's `boot = "restore"`, not an enabled unit** (recommended). The
updater's root part runs `tierstore update-hook restore` at its own start, before any unit, with the hook's rights and
step record (the updater protocol, 2.4, 3.5). `restore` is not one of the five steps: it validates `state.json` (section 3),
applies the redirect and the guard for the slot it names, and runs `systemctl start tierstore@<that slot>`; the other slot
is not started. Which slot it names: with no update in flight, `serving`; with an update in flight **and no `stopping`
marker**, the **previous** slot, so a reboot before the commit is an abort (the new slot is not started and `previous` is
the rule target); with `stopping` set, `serving` (the new slot). The updater then follows 3.2 for the step the record
shows pending: a pending `cutover` is rolled back (the old slot is already running and the rule names it, so `rollback`
only has to stop what is not running), a pending `verify` fails and rolls back, a pending `cleanup` is re-run forward.
Why not `WantedBy=` on the slot units, with `enable` and `disable` done by `cutover` and `cleanup`: it needs the right to
manage unit files (a polkit action beyond start and stop), it makes two sources of truth (the symlinks and the record)
that a crash can split, and a reboot between the two would start both. With `restore` there is one record and one rule
target, and running it again repairs a rule lost to a `flush ruleset`. The units have no `[Install]`.

## 3. The steps (superseded)

> Superseded by section 0 and `crates/tierstore/docs/update-hook.md`: this section describes the nftables redirect and the slot
> units, which the hook no longer uses. It is kept as the record of that design.

Every step reads the record first and sets state, never "does a thing once": that is what makes a re-run after a crash at
any point converge. The record, `state.json` in `state_dir`, has `schema`, `serving` (the port the rule names), `previous`
(the port of v-current while an update is in flight, else absent), `mode`, and markers.

**The record is read through a validator, before any other call** (the updater protocol, 3.4.1; a file forged by the service
cannot even be written there, and the hook still does not trust it). The valid ports are the declaration's `health_ports`
(9301, 9302) and the valid units are its `units`, both read from `/etc/updater.d/tierstore.toml`; `serving` and `previous`
must each be one of the two ports and differ; `mode` is `handover` or `restart`; each marker is one of `swapped`,
`verified`, `stopping`; `schema` is one the binary knows. A unit name is built from a validated port, never read. Anything
else is refused with exit `1`, class `environment`, reason `state_invalid`, **before any `nft`, `systemctl` or file call**.
The same goes for the params file (a flat table of scalars: numbers within sane ranges, the updater's staged release
and target version, and `mode`, whose only value is `"restart"`; a refused file is `bad_params`) and the environment
(empty). The updater then runs
`rollback`, which reads the same record, refuses the same way, and ends in `halt` (the updater protocol, 3.2): a record that
cannot be trusted is an operator's. The idle port is the other one of `serving`, recomputed every time.

**`prepare <artifact>`** (before anything visible). Check `tierstore --capabilities` of the staged release and of the one
the `serving` slot links to: both must have `handover-v1`, else the hook records `mode=restart` and `cutover` does a plain
restart with a gap (the first release with the lock, section 5; the updater protocol, Question 15). The params key
`mode = "restart"` records `mode=restart` whatever the capabilities say (a `dir:` hot tier, section 6). The step's
`detail` names the mode and why (`handover_capable`, `not_handover_capable` or `declared_restart`); the record keeps
the mode only, so that an earlier release's hook can still read it. Then load the guard table, before
anything else is applied (it records whether the table was there before, and `guard_placed` once this update loaded it).
Refuse with `75` (class `environment`: host not ready, nothing done) if the guard table cannot be loaded,
`nf_conntrack_tcp_timeout_established` is not longer than the service's idle bound, or `nf_conntrack_count` is near
`nf_conntrack_max` (the updater protocol, 4.1.3). "Nothing done" holds for the guard too: a deferral removes the guard
table if this update placed it and leaves one that was there; if that removal fails, the exit is `1` (class
`environment`), and `rollback` removes it. `guard_placed` is written after the load, so a crash between the two leaves a guard
this update placed but did not record; neither a deferral nor `rollback` removes it, and it outlives the update. That is
safe (the guard only blocks off-host access to the two private ports), and the next update's `prepare` finds it there. Point the idle slot's link at the artifact,
start `tierstore@<idle>`, and poll `/readyz` on `127.0.0.1:<idle>` until it answers 200 with `read-only` (a counted
number of attempts). It records `previous = serving`. *Idempotent*: the link is
replaced by rename, `systemctl start` of a started unit does nothing, the guard table is loaded by replacing it, the idle
port comes from the validated record. *Fails*: v-next never ready: `1`, class `release`.

**`cutover`** (the visible step). First check that the idle instance still answers `/readyz` with `read-only` (after a
reboot it would not): if not, `1`, nothing changed. Then write intent to the record, apply the swap file of 4.1.2 for the
idle port in one `nft -f -` (the ruleset on standard input, no file written), then probe through 9300 until a request is answered by v-next (its build gauge names the slot
and the version), a counted number of times. A probe that never succeeds swaps back and exits `1`. Records `swapped` and
sets `serving` to the new port. *Idempotent*: the file replaces the table and names the target port, so a second apply
changes nothing; a crash before `swapped` is re-run. *Restart mode*: point the redirect at v-next's port first, where
nothing listens yet, then stop v-current, start v-next, wait for readiness, then the same swap. A new connection in
between is refused (its SYN finds no listener), and none reaches v-current after its stop begins, so its listening
sockets never close on a connection whose client saw its connect succeed (section 7). **The restart's gap is v-current's
settle** (`--drain-lb-wait`, 70 s in the slot template) and its exit, then v-next's start: new connections are refused
for all of it, while the connections v-current already has are served to the end of its drain. Going back in restart
mode (a failed `cutover`'s own compensation, or `rollback`) points the redirect at v-current first, then stops v-next
**before** it starts v-current, because a release without `handover-v1` must never run beside another instance; in
handover mode the old unit is started first. In both modes no unit is stopped while the redirect names it.

**`verify`** (the gate; run again and again). Before `cleanup`: v-next is ready, a counted series of probes through 9300
is answered by v-next with the staged version, the redirect names v-next, and v-current still serves. It exits `0` with
`"promotion_pending":true`: v-current holds the lock, so v-next is read-only by design (the updater protocol, 3.8). After
`cleanup` has stopped v-current (the `stopping` marker): wait for `/readyz` to say `read-write` for
`promotion_bound_secs` (900, from the params file); if it does, `0` with no pending flag; if the bound passes, `0` with
`"promotion_pending":true`, and `detail` says the promotion is pending (it does not name who holds the lock: no instance
reports that). **There is no ceiling in the hook**: the updater keeps
running `verify` as its post-success watcher, outside the step table, and applies `promotion_ceiling_secs` (3600) itself,
ending the watch as `Failed(after_commit)` for a storage node and a build node alike (the updater protocol, 3.2); the hook
receives the value as `updater.promotion_ceiling_secs` in the params file and must not exit `1` for it. After the commit
`verify` exits `1` only for a fault of its own observation (v-next crashed, answers wrongly), class `release`, which the
updater also reports as `Failed(after_commit)` and never rolls back. **Exit `75` is not used**: nothing here is "not
ready, try later with nothing done". *Idempotent*: it only observes, and sets the `verified` marker, which records the boot id read at that moment (a
`verify` in a later boot records the new one).

**`rollback`** (before the stop of v-current only). Apply the swap file (on standard input, as in `cutover`) for the other port (the one `previous` names once
`swapped`, else the one `serving` names), then stop the instance the restored rule does not name and remove that instance's slot link (a later `prepare` makes it
again by rename), so that nobody starts an unvetted release by hand, remove the guard table only if `guard_placed` says this update loaded it, and clear `previous` and the markers. *Idempotent*:
stopping a stopped unit, removing an absent link and re-applying a swap are no-ops. **It is valid only before `cleanup`
has written its `stopping` marker.** After it, v-current is exiting or gone and a swap back would send new connections
to a closed port, so `rollback` exits `1` with `"reversible":false`, which the updater reads as `Failed(after_commit)`,
not `halt` (the updater protocol, 3.2).

**`cleanup`** (the commit point of tierstore). The updater calls it only after a successful `verify`, and the hook
refuses anyway: **unless the record says `verified`, `cleanup` does nothing and exits `1` with `"reversible":true`**
(the updater protocol, 3.2: a `cleanup` must not commit on its own authority, and a step that commits re-observes
first). If `verified` is set and `stopping` is not yet written, `cleanup` first re-observes in its own run: the host's
boot id equals the one the marker holds, the guard table is loaded, the redirect names `serving`, `serving` answers
`/readyz`, and a request through 9300 is answered by `serving` with the staged version (its build gauge names the slot and
version). A failed check, or a stale marker (another boot id), commits nothing and exits `1` with `"reversible":true`;
the updater runs `rollback`. Then write `stopping` and from here print `"reversible":false` on every exit, so the updater
records the commit (a re-run after `stopping` does not re-observe, so the commit goes forward in any boot). Then, in this order: **remove the previous
slot's link first**, then `systemctl stop tierstore@<previous>`, then clear `previous`. The link goes first because it is
the guard (section 2: the slot unit has `ConditionPathExists=` its link and no `[Install]`): with the link gone, nothing
can start that slot again, by hand, by a dependency, or by `Restart=` if the old process crashes during its settle. The
hook does not mask the unit: masking needs polkit's `manage-unit-files`, which systemd turns into `manage-units` for
every unit, so a polkit rule could not hold the hook to its two declared units; and a reboot goes through `restore`, which
starts only the slot the record names. The process sees SIGTERM, keeps accepting for the settle (`--drain-lb-wait`, 70 s,
the last SYN retransmit of a default client, the updater protocol, 4.1.3 and Question 13), applies P1 and P2 (section 7,
Question 14), waits for requests in flight at most `--drain-deadline`, gives up the lease, and exits; the unit's stop
timeout kills a hung one and the kernel frees the lock. *Idempotent*: every part is "set to state": link absent, unit
stopped (stop of a stopped unit is a no-op), `previous` cleared. A `cleanup` that fails after `stopping` is re-run forward
by the updater up to `cleanup_retries` (3) times, then `Failed(after_commit)`; it is never rolled back.

## 4. What tierstore changes in the listener (superseded)

> Superseded by section 0 and `crates/tierstore/docs/update-hook.md`: this section describes the nftables redirect and the slot
> units, which the hook no longer uses. It is kept as the record of that design.

The redirect rewrites a remote packet's destination to the address of the interface it arrived on, so the private ports
listen on the **wildcard of both families**, not loopback (the updater protocol, 4.1.1, Question 6); the guard table keeps a
direct connection to a private port out. Today `--listen` is one `SocketAddr` (`server/cli.rs`, field `listen`; one
`listen` key in `server/config_cache.rs`), bound by one `TcpListener::bind` (`run_serving`, `serve_cache`), which cannot
set `IPV6_V6ONLY`. New: a repeatable `--listen`, two sockets, `IPV6_V6ONLY` explicit; a start that cannot bind both is
refused. `--oci-listen` (`oci::OciListener`) has the same single address (question 4 below).

## 5. Cache lock and read-only mode (build node)

**The lock decides the mode** (new; no `flock` or lock file exists under `crates/tierstore/src`). A start tries
`<path>/.lock` with `flock(LOCK_EX | LOCK_NB)` on a descriptor it keeps open (close-on-exec, no follow, a local file
system): it gets it and is read-write, or it does not and is **read-only until it does**. No flag: a lone instance after a
reboot, or a restarted v-current, is read-write at once; a second instance can never write beside the first.

- **Waiting**: a task retries on an interval on the server's clock (a second by default); it blocks no thread, nothing
  waits on the lock inside a request, and a test counts retries and never sleeps.
- **Taking it**: `layout::clear_stage`, `layout::scan` with deletes, replace the index, enable fills, keeps and eviction
  (a storage node: the reconciler), then publish `read-write`. v-current's last act is `flush_touches` at the end of
  `maintain`, so the rescan sees its recency. Only the holder mutates the directory.
- **v-current crashes or hangs**: the kernel frees the lock; a hung one is killed by the unit's stop timeout, so v-next
  takes it after at most the drain deadline plus margin and serves read-only until then.
- **A v-current that predates the lock** holds nothing, so v-next would get the lock at once while it still writes:
  hence `prepare`'s capability check and `mode=restart`. The first release with the lock goes in by a plain restart.
- **A replaced `.lock`** is a lock on nothing: the holder compares `fstat` of its descriptor with `stat` of the path when
  it takes the lock and periodically, and releases and retries on a mismatch. A storage node's lock file is under
  `/var/lib/tierstore/`, not a `RuntimeDirectory=` (systemd removes it with the unit).

**Read-only behaviour.** Hits come from the existing directory; misses and writes go to the upstream. Nothing like it
exists (`CacheConfig::write_through` is about client writes; a read cache already refuses them at the front,
`Server::respond`, `read_cache`). A hit beside v-current is safe: the data file is a create-only hard link of a complete
file (`fill.rs: publish` renames the sidecar, then links the data file last), and an eviction unlinks a name without
touching bytes a reader has open. v-next's index is a snapshot: an object v-current fills later is a miss, one it evicted
is a failed read and a miss. The five writes to the shared directory today and how each is disabled:

| write today | how read-only disables it |
|---|---|
| `LocalCache::open` runs `layout::clear_stage` (empties `.stage/`, where v-current's fills are) and `layout::scan` (deletes a sidecar with no data file: the middle of v-current's `publish`) | a read-only open reads and skips; deletes and empties nothing |
| `check_local` writes a probe under `.stage/` (`local_cache/selfcheck.rs`) | probe a directory of its own on the same file system, or skip |
| `layout::make_dirs` chmods directories (in `open`, `scan`, `publish`) | not called on the shared tree |
| `Inner::lookup` calls `drop_entry` on `NotFound`, deleting files v-current may just have refilled | a read error is a miss and touches nothing |
| `flush_touches` stamps file times (`utimensat`) in `maintain` and `evict_pass` | `maintain` does not run |

It also starts no fills, keeps, eviction or scrub (`scrub_rate` is accepted and ignored today, `serve_cache`).

## 6. Entries, lease and proxy id (storage node)

The reconciler takes the lease, in `EntryReconciler::step` through `Lease::ensure` (`lease.rs`), on `_tiering/lease` in the hot
tier: create-only, renewed (at the start of a step if less than a third of `lease_ttl` of it is left, and on a timer of its
own while the step works) or taken over by `put_if_match` once the same tag has stood for `lease_ttl` on the taker's own
monotonic clock. The proxy makes its writes on the entry `_entries/<sha256 of the key>`
(`tiered/entry_put.rs`, `tiered/entry_delete.rs`), which names the proxy id as its owner while the write is in flight.

- **Any number of proxies work over the same tiers** (`lib.rs`), but **one proxy id for both is wrong**: an entry
  names its owner by the proxy id, so a second writer under the same id would take the entries of the first as its own. The unit already says to give each process its own id; the slot
  template gives `tierstore-9301` and `tierstore-9302`, stable across updates.
- **One lease holder name is safe**: `ReconcilerConfig::holder` is the proxy id and the body is
  `holder\n<nonce>\n<generation>` (`Lease::body`), the nonce 128 random bits as hex drawn in `Lease::new` and the
  generation counting from 1 in each process; the content tag hashes the body. A restarted process with the same name
  writes other bodies than the process before it, so a stale renew or release with the old tag fails. (Before the
  nonce, the body was `holder\n<generation>` and the two could collide.)
- **A `dir:` hot tier cannot have two holders**: `DirBackend::put_if_match` is atomic among the calls of one backend only
  (`dir.rs`), so the lease's compare-and-swap is not atomic across two processes. It keeps the plain restart
  (`mode=restart`): packaging sets `mode = "restart"` in `[hook.params]` on such a host, and `prepare` then chooses a
  restart even when both releases list `handover-v1`.

**The same lock decides the reconciler.** A storage node takes `--lock-file` (new) with the same try loop. v-next serves
reads and writes through its own proxy at once and does not run the reconcile loop until it holds the lock: v-current
holds the lease through the whole drain (`reconcile` ends only when `serve` returns, `run_serving`); a second runner
takes it only by expiry or makes v-current lose it silently, which `Lease` documents; and v-current gives it up on exit
(`Reconciler::stop`, at most `STOP_WAIT` = 5 s), so v-next's first `ensure` after taking the lock finds it free.

## 7. The drain of v-current: P1 and P2

Today, when told to stop (`Server::serve_from`, `respond`, `finish`): `/readyz` 503 at once; for `drain_lb_wait` (10 s) it
serves everything; then the listener is dropped and every request **on a connection still open** is answered 503; then it
waits for the requests in flight, at most `drain_deadline` (15 min), and closes the connections. After the swap each
established keep-alive connection still reaches v-current and would get a 503 while one long transfer holds the drain
(the updater protocol, Question 14). Two changes, independent of how the port moves:

- **P1** from the moment the drain begins, every answer carries `Connection: close`.
- **P2** a **lingering close** for idle connections at the end of the settle: `shutdown(SHUT_WR)` on each idle keep-alive
  connection (the client reads a clean end of stream), keep reading until the client closes, then close. A plain
  `close()` with unread request bytes sends a reset that can destroy an answer. A request already on the wire is served.
  Bytes arriving after the FIN are read, dropped and counted (`tierstore_drain_late_requests_total`): the remaining race
  is a request within one round trip of the FIN, which no server can answer. The count (of connections, once each) can rise
  from the start of the drain, but reaches its last value only after the S3 listener that serves `/metricz` has closed,
  so a node that counted any says so on stderr as it exits (`late_note`, `server/drain.rs`), for each front.

At the end of the settle the front closes its listening sockets, and the kernel resets every connection it still holds
for them: those set up and not yet accepted, and those whose handshake is in flight, whose client saw its connect
succeed. So the hook never stops a slot while the redirect names it: in a hand-over `cleanup` stops v-current after the
swap, and in a plain restart `cutover` points the redirect at v-next's port before it stops v-current (section 4), so
that a connect in the restart's gap is refused, never reset. What the kernel still holds when the settle ends (a
connection set up just before the redirect moved, and not yet accepted) is taken rather than reset. The idle
connections close first (P2), so that a client waiting for the end of its stream frees what it holds (the OCI front's
slots); then the door is closed in steps (`server/door.rs`): the connections the kernel set up and the handshakes in
flight (`SYN_RECV` in `/proc/self/net/tcp` and `tcp6`) are taken and served, and the sockets close once two looks 10 ms
apart find none, or after 3 s. The looks end the wait early only while the count sees every handshake: once the
namespace has sent any SYN cookie (`SyncookiesSent` in `/proc/self/net/netstat` is not 0; such a handshake has no
entry, and nothing says which listener it was for or when), and on any system but Linux, the count is an error and the
door waits the 3 s. A new connection
that has not sent its first bytes yet is given the same 3 s to send them, and its request is then served whole, rather
than closed as an idle one; a shorter header timeout or drain deadline wins. Left open: a handshake still in flight after
3 s, and a connection that comes as the sockets close, which only a redirect that still names the stopping slot could
bring. The door needs no privilege: a socket filter that dropped SYNs would need `CAP_NET_ADMIN` on some kernels, which
the slot unit does not grant.

The OCI front (an `oci` interface) drains the same way and at the same moments, with the same code (`server/drain.rs`,
`server/linger.rs`, `server/door.rs`): P1 from the start of the drain, the settle, P2 for its idle connections, an answer in flight (a long
blob) sent in full and its connection closed the same way after it, and its late requests counted in
`tierstore_oci_drain_late_requests_total`, so that its clients see no gap either (question 4).

**The client policy is no retry**: a retried create-only put is ambiguous (the retry's 412 cannot tell its own earlier
success from another writer's). A client that follows P1 never meets the race: a connection is idle at the end of the
settle only if its client sent nothing since before the drain began. The settle is `--drain-lb-wait`, 70 s in the slot
template, never `0s`: a connection whose SYN was mapped to v-current just before the swap is served, not reset
(the updater protocol, 4.1.3). A keep-alive connection that outlives `drain_deadline` is closed (`finish_by_deadline`) and
the cut requests are counted in `Drained.abandoned` (`cut_note`).

## 8. Metrics

`/metricz` is one block per source (`Server::metrics_probe`). Read-only and overlapping:

| family (`tierstore_`) | read-only v-next | with two processes |
|---|---|---|
| `cache_hits_total`, `cache_misses_total`, `cache_bytes_served_local_total`, `cache_bytes_served_upstream_total` | count | per process; the sum is right |
| `cache_fills_total`, `cache_fills_skipped_total`, `cache_fills_skipped_busy_total`, `cache_fill_failures_total{reason}`, `cache_evictions_total{reason}`, `cache_pinned_skips_total`, `cache_staging_files` | stay 0 | v-current only |
| `cache_discarded_at_start_total` | 0 (read-only open deletes nothing) | v-current's |
| `cache_bytes`, `cache_objects` (a snapshot in v-next), `cache_fs_free_bytes` | start snapshot / live | **double-count** one directory and one file system |
| `hits_total`, `misses_total`, `read_errors_total`, `reads_total`, `bytes_served_total`, `bytes_read_total` (`tier`, `op`), `promotions_*` (storage), `oci_requests_total`, `oci_bytes_served_total`, `oci_digest_mismatches_total`, `oci_tag_recheck_failures_total`, `oci_front_info` | count | per process; the sum is right |

Counters restart at 0 in a new process; a rate handles it; read the directory gauges from the lock holder. No family names
a build, slot or mode today, and a scrape of 9300 does not say which instance answered. New: `tierstore_build_info
{version,slot}`, `tierstore_cache_mode{mode}`, `tierstore_drain_late_requests_total` and
`tierstore_oci_drain_late_requests_total`. A scraper that wants both instances
scrapes the two private ports from the node (the guard blocks off-host); one is down between updates, so an alert is on
"no instance up", not on `up == 0`.

## 9. Failure cases, per step (superseded)

> Superseded by section 0 and `crates/tierstore/docs/update-hook.md`: this section describes the nftables redirect and the slot
> units, which the hook no longer uses. It is kept as the record of that design.

| step | case | what happens | state after |
|---|---|---|---|
| any | `state.json` invalid or forged (a port outside the declared two, a bad mode or marker, an unknown schema) | `1`, `state_invalid`, **no `nft`, `systemctl` or file call**; the updater's `rollback` refuses the same way and ends in `halt` | host as it was; an operator looks |
| `prepare` | v-next never ready; guard cannot load; conntrack bounds fail | `1` (class `release`) or `75` (class `environment`); the updater runs `rollback`, which stops v-next | v-current serves, rule untouched |
| `cutover` | `nft -f` fails | one transaction: the old rule stays; `1` | v-current serves |
| `cutover` | probe never answered by v-next | swap back inside the step; `1` | v-current serves |
| `verify` (before cleanup) | v-next crashes or answers wrongly | `1`; the updater runs `rollback`: swap back, stop v-next; v-current still accepts | v-current serves |
| `cleanup` | no `verified`, a boot id that changed since it, or v-next not serving through 9300 | commits nothing; `1`, `reversible:true`; the updater runs `rollback` | v-current serves |
| `cleanup` | killed after `stopping`, or fails after it | re-run forward by the updater up to `cleanup_retries` (3): remove the link, stop, clear `previous`; no swap back; then `Failed(after_commit)` | v-next serves; the previous slot link is absent |
| `cleanup` | the old process crashes during its settle, after `stopping` | its link is already absent, so `Restart=` cannot start it (`ConditionPathExists=` fails); the lock is freed by the kernel | v-next takes the lock; one instance |
| `cleanup` | v-next crashes after `stopping` | no swap back (v-current has stopped accepting); systemd restarts v-next, read-only while v-current exits, read-write at once if gone; the coordinator rolls back by a new hand-over | a gap until v-next is up; failed |
| `cleanup` | v-current hangs past the drain deadline | `Drained.abandoned` counts what was cut; the unit is killed at its stop timeout; the kernel frees the lock | v-next takes it |
| `verify` (after the commit) | lock not taken within `promotion_bound_secs` | `0` with `promotion_pending`; the coordinator holds the slot; the updater's watcher keeps running `verify` | at `promotion_ceiling_secs` the updater reports `Failed(after_commit)`; the hook exits `1` for nothing of the kind |
| `rollback` | asked after `stopping` | `1`, `reversible:false` | `Failed(after_commit)`, not `halt` |
| any | v-current crashes during the overlap | the kernel frees the lock; v-next takes it; if the swap has not happened the next `cutover` finds v-next ready and applies it | v-next serves, read-write |
| any | host reboots mid-update | `restore` (boot entry point): before `stopping` it starts the previous slot and points the rule at it (an abort, the pending step is rolled back); with `stopping` it starts the new slot, which finds the lock free | one instance; no overlap |
| any | the disk fills while both run | v-current's fills and eviction handle it as today (`critical`, `free_floor`); v-next writes nothing; stale entries are misses | no corruption |

## 10. Test plan (superseded)

> Superseded by section 0 and `crates/tierstore/docs/update-hook.md`: this section describes the nftables redirect and the slot
> units, which the hook no longer uses. It is kept as the record of that design.

The test T1 below (a hand-over under load behind real nftables in network namespaces) and its CI job `handover-t1` are removed with
the nftables redirect: no pull-request check needs `CAP_NET_ADMIN` now. The hook's steps are tested in the library against fakes of
`systemctl`, `launchctl`, the markers, the host and the instances' answers (`update_hook/systemd_tests`, `launchd_tests`), and the
socket hand-off, with requests in flight, by `server/handoff/` and `tests/s3_front/handoff.rs`. The plan below is the old one.

House rules: nothing asserts timing, no real sleeps, injected clocks and counted events. Each test names the defect it
catches and is written red before the code. **Conformance, one line per step** (the suite of the updater protocol, 9, with the
real hook binary against a fake service and a fake `nft` and `systemctl`):

- **C1 `prepare`**: twice leaves one started idle unit, one link, one guard table; killed at each boundary and re-run
  reaches the same state; no `handover-v1` yields `mode=restart`; a bad conntrack bound is `75`; nothing is applied before the guard; a
  deferral removes a guard it placed and leaves one that was there, and a failed removal is `1`.
- **C2 `cutover`**: twice leaves one rule naming the idle port; killed between apply and `swapped` converges; a failing
  probe swaps back and exits `1`; the swap is one `nft -f` (a recording fake sees one call).
- **C3 `verify`**: twice changes nothing; the marker holds the boot id; with the lock held it exits `0` with `promotion_pending`; with `read-write` it
  clears the flag; a wrong version or slot through 9300 is `1`; with `updater.promotion_ceiling_secs` in the params file
  and the lock never taken it still exits `0` (red: a hook that exits `1` at the ceiling).
- **C4 `rollback`**: after `cutover` it returns the fake to its earlier state, with the idle slot's link absent; after
  `cleanup`'s `stopping` marker it exits `1` with `reversible:false` and changes nothing; in restart mode it points the redirect at
  v-current, then stops v-next before it starts v-current; it removes the guard only when `guard_placed` is set.
- **C5 `cleanup`**: without `verified`, with a `verified` marker holding another boot id, or with v-next not serving
  through 9300, it exits `1` with `reversible:true` and stops nothing (red: a hook that stops v-current); with a fresh one, twice and
  killed at each boundary, it ends with the unit stopped, link absent, `previous` cleared, and every exit
  after `stopping` carries `reversible:false`.
- **C6 `restore`** (boot): before `stopping` it starts only the previous slot and names it in the rule; with `stopping`
  only the new one; twice changes nothing.
- **C7 forged record** (the updater protocol, 3.4.1): a `state.json` forged anywhere the service can write changes nothing;
  one in `state_dir` with a port outside the two, an unknown mode or marker, equal `serving` and `previous`, or a newer
  schema makes every step and `restore` exit `1` with the fake `nft` and `systemctl` recording **no call** (red: a hook
  that takes the port from the record). nft tests: see the updater protocol, 9. The service's own tests follow.
- **T1 hand-over under load (superseded).** It ran the hook against real nftables in network namespaces, with the test-only features `handover-t1` and `test-drain-off`. The test, the features and the CI job are removed; nothing here can be run.
- **T2 lock and self-take.** Two processes on one directory: the second is read-only and its retry count rises while the
  first holds the lock (an injected clock ticks the retries); after the first is killed it takes the lock, runs
  `clear_stage` and `scan` (a half-published sidecar is cleaned), serves a counted fill and says `read-write`; a replaced
  `.lock` is released and retaken; a lone instance is `read-write` before it answers `/readyz`.
- **T3 read-only writes nothing.** Start read-only on a directory with a half-published sidecar and files in `.stage/`;
  serve hits and misses (also an object v-current evicted and refilled: served, nothing deleted); compare the whole tree
  (names, sizes, modes, mtimes) before and after; one case per row of the table in section 5; the fill and eviction
  families stay 0 and the build and mode gauges differ between instances.
- **T4 lease and entries.** Two proxies with slot ids over one hot tier have registrations of their own; v-next's reconciler
  makes no `ensure` call before it holds the lock file (a counted event), then takes the lease v-current released; two
  starts never write the same lease body.
- **T5 drain.** P1 on a paused clock over in-memory pipes (the pattern of `drain_tests.rs`): every answer after the drain
  begins says `Connection: close`, none is 503. P2, deterministic, over a pair of `duplex` streams, events counted, no
  clock: an idle keep-alive connection when accepting stops; in order, the server shut down its write half (the client's
  read returns 0 bytes), the connection is not dropped (open count 1), the client closes and the count goes to 0. A
  request the client writes after the FIN is read, dropped and counted (`late_requests` 1), and the close still waits for
  the client. Over real loopback TCP, ordered by events: a client holding unsent request bytes when the server closes
  reads a clean end of stream, never `ECONNRESET`; a plain `close()` fails it.

## 11. Questions for the maintainers

1-3. *Settled in the updater protocol, 3.2 and 3.4*: a failed `cleanup` after the stop is re-run forward up to
   `cleanup_retries`, then `Failed(after_commit)`, never rolled back; after a failed `prepare` or `cutover` the updater
   runs `rollback`, and `cleanup` runs only after a successful `verify` (the hook still refuses without `verified`);
   capabilities may be given per step; tierstore gives one hook-wide grant (section 2).
4. *Decided (accepted 2026-11-01).* **Listeners** (the wildcard decision is the updater protocol, Question 6). A repeatable `--listen` with explicit
   `IPV6_V6ONLY`, and the same for `--oci-listen`. Recommendation: both in the first tierstore change, or the OCI front's
   clients see a gap at every update.
5. *Decided (accepted 2026-11-01).* **P1 and P2** (the updater protocol, Question 14) change the plain restart's drain too (a connection is closed, never
   answered 503). Recommendation: the only behaviour, not a flag.
6. *Decided (accepted 2026-11-01).* **The residual race** (a request within one round trip of the FIN on a connection idle since before the drain).
   Recommendation: accept it as the stated limit, counted, and do not ask clients to retry a create-only put.
7. *Decided (accepted 2026-11-01).* **First lock release** goes in by `mode=restart` (the updater protocol, Question 15), and so does a `dir:` hot tier.
   Recommendation: accept. The `dir:` case is declared, not detected: packaging sets `mode = "restart"` in
   `[hook.params]` for a host whose hot tier is `dir:`, since the lease's compare-and-swap there is not atomic across
   processes; it is the only value the key takes.
8. *Decided (accepted 2026-11-01).* **Version source.** Recommendation: the build gauge, with the commit baked into the binary rather than the crate version.
9. *Decided (accepted 2026-11-01).* **`state_dir` mode.** The slot links must be executable by the service, so `state_dir` cannot be `0700`
   (the updater protocol, 3.4.1 says read access "through group membership", the 2.4 example says `0700`). Recommendation: mode
   `0750`, group `tierstore-update`, the service account a member, read only; the record stays unwritable by the service.

