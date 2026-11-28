# tierstore as one service: one process, one unit, one configuration file

Status: **approved by the owner on 2026-11-24, with the privilege design simplified to what is here (the process holds
`CAP_SYS_ADMIN` for its life). Release M (phase 1, P1 to P7 of section 6) is built on Linux and macOS: the `fs` interface
(komira-ai/tierstore#448, komira-ai/tierstore#450), the core's calls into the cache (komira-ai/tierstore#445), the start of the one process (komira-ai/tierstore#454), the one hand-over's
wire format (komira-ai/tierstore#452), the macOS hand-over to the next image (komira-ai/tierstore#456, komira-ai/tierstore#462, komira-ai/tierstore#463), the unit and plists (komira-ai/tierstore#446), and the update hook and
the switch (komira-ai/tierstore#458). The Linux start now also takes the cache lock with the sockets, and its verify waits for read-write
(komira-ai/tierstore#571). Not built: the Linux start does not yet take the mount, the control socket and the sockets in one
conversation (`take_service`, `Offer::with_mount` and `treefs::handover::offered` have no caller outside tests; the start takes
the mount by `take-fs 1`), and the successor still scans the cache at `written` before it binds the mount-provider socket
(komira-ai/tierstore#479, komira-ai/tierstore#522). Phase 2 (P9, release M+1, section 5) is built in part: the fill socket and the `Fills` trait
are gone (the core calls the node's cache in the process alone), `local_cache.fill_socket` is
read and ignored with a line on stderr, and `tierstore fs` serving exits 2 (komira-ai/tierstore#523).
Not built: `tierstore attachments`, and `fs.toml`, the `tierstore-fs` units and the hook's second
arm still ship (komira-ai/tierstore#524).
P8 is in the deployment's repository.** It reverses decision C5 of
[virtual-build-fs.md](virtual-build-fs.md) (section 13.1, "a second process on Linux build nodes holding `CAP_SYS_ADMIN`,
with no network") on the owner's instruction: tierstore is one service, one process and one configuration file, and the
file system is an `[[interface]]` of type `fs` inside it, as section 4 of [pluggable-interfaces.md](pluggable-interfaces.md)
first sketched it. This pull request also rewrites section 6.2 of virtual-build-fs.md and the `fs` paragraph of section 4
of pluggable-interfaces.md to match. What the code does today was read at
the commit this document lands on; each claim names the file.

**Claim tags**, as in virtual-build-fs.md: **read in code** (with the file), **inferred** (reasoned here, and how), and
**unverified** (not checked against a primary source; each such claim is a test of section 6 that must pass before the
pull request that relies on it merges).

## 0. The change in one table

| | Today (read in code and packaging) | After this design |
|---|---|---|
| Processes on a build node | two: `tierstore` (S3, OCI, cache, mount provider, fill socket) and `tierstore fs` (the trees as a file system) | one: `tierstore`, which also serves the trees |
| Linux units | `tierstore@.service` and `tierstore-fs@.service`, each with instances `a` and `b` and its own marker files | `tierstore@.service`, instances `a` and `b` |
| macOS launchd jobs | `tierstore` and `tierstore-fs` (`packaging/macos-pkg/*.plist`) | `tierstore` |
| Configuration files | the node's file (`config.toml`, `tierstore.toml`) and `fs.toml` with an `[fs]` table; three keys must agree by hand (`fill_socket`, `cache_path`, `mountpoint`) | the node's file alone, with `[[interface]] type = "fs"` |
| Between the two | the fill socket (a Unix socket; `server/fill_socket/`, client `server/treefs/fill_client.rs`) | calls inside one process |
| Live update, Linux | two hand-overs: `take 1` (listening sockets, `server/handoff/`) and `take-fs 1` (the FUSE descriptor and the control socket, `server/treefs/handover.rs`), on two control sockets | one hand-over on `server.handoff_socket`: listening sockets, the FUSE descriptor, the control socket and the cache lock together |
| Live update, macOS | none: every update boots the fs job out (which unmounts) and restarts both jobs (`server/update_hook/launchd/`) | a hand-over to the next image of the same process (section 3.4): the NFS mount stays |
| Update hook | stop order and a second arm per platform (`update_hook/systemd/fs.rs`, `update_hook/launchd/fs.rs`) | one service, no stop order |
| Privilege, Linux | the fs process holds `CAP_SYS_ADMIN` for its whole life; the S3 process holds none | the one process holds it for its whole life, to mount (section 2.1) |

What does not change: the mount point (`/run/tierstore/fs` on Linux, `/private/var/tierstore/fs` on a Mac), the tree layout
under it, the mount-provider socket and its protocol, the control socket and its protocol, the runtime directory and the
formats in it (`docs/stored-formats.md`), the job runtime's flags, the metric names, the user the service runs as
(`tierstore`, `_tierstore`), and every other configuration key.

## 1. The shape

### 1.1 One process

```
                               tierstore (one process per node; one unit; one file)
 ┌──────────────────────────────────────────────────────────────────────────────────────────────────────┐
 │ fronts:   s3 | oci | trees (gRPC) | mount-provider (Unix socket) | fs (FUSE on Linux, NFS on loopback on macOS) │
 │ core:     proxy or local_cache (+ trees store, put ingest)  ◀── calls ──  the file system's core          │
 │ state:    cache directory (+ its lock) | state directory (hand-over socket) | runtime directory (fs)       │
 └──────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

The file system's core (`server/treefs/core*`) and its two frontends (`server/treefs/serving.rs` and `node_fs.rs` over
`tsfuse` on Linux, `server/treefs/nfs/` on macOS) are kept as they are. What changes is what they are started by and what
they talk to:

- **Started by** the server's start of a build node (`server/cache_cli.rs`, `serve_front`) when the file has an interface of
  type `fs`, instead of by the verb `tierstore fs` (`server/verbs/fs.rs`, `server/treefs/launch.rs`).
- **Talking to** the node's `LocalCache` and `TreeStore` by calls, instead of over the fill socket (section 1.5).
- **Stopped** as part of the server's stop (section 3.5).

The `fs` interface is allowed only on a build node: a file with `[local_cache]` and `[cold_store]` and no `[hot_store]`
(`Settings::Cache`, `server/config.rs`), the one shape that has a tree store, as the `trees` and `mount-provider` interfaces
already are (`no_trees`, `server/config_interface.rs`). One `fs` interface per file.

### 1.2 The configuration

The `[fs]` table of `fs.toml` becomes the `params` of the interface. Its sub-tables `[fs.nfs]` and `[fs.put]` become the
sub-tables `nfs` and `put` of the params. TOML lets a sub-table of the last `[[interface]]` element be written as
`[interface.params]` and `[interface.params.put]`, so the long table needs no inline braces. The `fs` params are a table type
of their own that refuses a key it does not have, as every `[[interface]]` entry does today (`config_unknown.rs`: "keeps
refusing a key it does not have").

| `fs.toml` key (`server/treefs/config.rs`, `nfs/config.rs`, `put/config.rs`) | One file |
|---|---|
| `fs.mountpoint` | `params.mountpoint`, same default `/run/tierstore/fs` |
| `fs.owner` | `params.owner`; also accepts a table, below |
| `fs.client_group`, `fs.control_socket`, `fs.runtime_dir`, `fs.threads`, `fs.recover_wait`, `fs.reattach_concurrency`, `fs.tree_idle_secs`, `fs.fetch_timeout` | the param of the same name, same default and range |
| `fs.handoff_wait` | `params.handoff_wait`, same default (120 s): it bounds the file system's part of the one hand-over (section 3.1) |
| `fs.nfs.*` except the three below | `params.nfs.*`, same name, default and range |
| `fs.put.*` | `params.put.*`, same name, default and range |
| `fs.fill_socket` | gone: the file system calls the cache |
| `fs.cache_path` | gone: it is `local_cache.path` of the same file |
| `fs.handoff_socket` | gone: the one hand-over is on `server.handoff_socket` |
| `fs.nfs.handoff_socket`, `fs.nfs.handoff_wait`, `fs.nfs.handoff_ready_wait` | gone: section 3.4 |

Rules the one file adds, each a refusal by name at load (and so by `--check-config`):

- An `fs` interface on any shape but a build node is refused, with the words the `trees` interface uses today.
- A second `fs` interface is refused.
- The `mount-provider` interface's `mountpoint` param may be left out; it is then the `fs` interface's `mountpoint`. When both
  are given they must be equal. (Today they are two files that agree by hand: `provider/config.rs`, "the mount of `tierstore
  fs` on the node (its `fs.mountpoint`)".) A `mount-provider` interface in a file without an `fs` interface kept today's
  meaning and default in release M, so a file of today still loaded; since `tierstore fs` no longer serves (0.3.196) nothing
  serves that mount, and the load refuses such a file, naming the provider's entry.
- `params.put.stage_dir` must be on the file system of `local_cache.path` (today: of `fs.cache_path`).
- `params.runtime_dir` must not be inside `local_cache.path`, and the control socket must differ from `server.handoff_socket`
  and the mount-provider socket, as the sockets of one file are compared today (`check_provider_socket`).

**The owner on Linux.** On a Linux node `fs.owner` is the host ids a job runs as: the job user's first subordinate uid and
gid (the first range of `/etc/subuid` and `/etc/subgid` with at least 65536 ids) plus 1000, because a job is id 1000 of its
user namespace. Those ids differ from node to node, which is why today's `fs.toml` is written per node and cannot be part of
a group's configuration package. The one file would inherit that. So `params.owner` also takes a table,
`owner = { subids_of = "<job user>", id = 1000 }`, which the start resolves to `<first subuid + id>:<first subgid + id>`,
refusing by name a user with no such range or ids past 4294967294. With it the whole file is the same on every node of a
group. A Mac keeps the `user:group` form with names (`_tierstore:_tierstore`), which already serves every Mac.

**Interfaces and the fill socket keys after the change.** `local_cache.fill_socket` names a socket nothing needs once no
`tierstore fs` process is left; section 4 keeps it for one release and section 5 retires it (a file that still has it starts
and says on stderr that it is ignored, as the removed `fs.passthrough` is today). `local_cache.fill_socket_concurrency` and
`local_cache.fill_socket_wait` keep their names and meanings: they bound the fills the file system asks for, which keep their
own pool of permits and wait for one (`local_cache/stall.rs`), whether they arrive on a socket or by a call. Renaming them
would break files for no gain.

### 1.3 The two groups' files as they would be

The two host groups of the lab, as they would be configured. Addresses are documentation addresses; a deployment puts its
own. Only the tables that change are shown in full.

**`all-in-one`**: six Linux build nodes, each a local cache over the cold store, serving S3 to the lab, the trees to their
jobs over FUSE, and the mount provider to the job agent. `/etc/tierstore/config.toml`:

```toml
[[interface]]
type   = "s3"
params = { port = 9300 }

# The job agent's mount provider. Its mountpoint is the fs interface's (left out).
[[interface]]
type   = "mount-provider"
params = { socket = "/var/lib/tierstore/provider.sock" }

# The trees as a file system: one FUSE mount, the jobs' input roots and put directory.
[[interface]]
type = "fs"
[interface.params]
mountpoint   = "/run/tierstore/fs"
client_group = "tierstore"
owner        = { subids_of = "<job user>", id = 1000 }
[interface.params.put]
enabled              = true
prefixes             = ["cas/b/"]
stage_dir            = "/var/lib/tierstore-cache/cas/put-stage"
stage_ttl_secs       = 86400
commit_deadline_secs = 120
min_free_bytes       = 21474836480

[server]
handoff_socket      = "/var/lib/tierstore/handoff.sock"
proxy_id_from_facts = { path = "/var/lib/hostkeeper/facts.json", prefix = "tierstore-" }
bucket              = "cas"
# ... the other [server] keys as today

[local_cache]
path          = "/var/lib/tierstore-cache/cas"
budget        = "185GiB"
write_through = true
# ... the other [local_cache] keys as today; no fill_socket

[cold_store]
kind             = "s3"
bucket           = "cas"
endpoint         = "http://127.0.0.1:8333"
credentials_file = "/etc/tierstore/cold.credentials"

# [[policy]] as today
```

**`mac-build`**: the Macs, each a build node: a cache on local disk in front of the storage nodes' S3 servers, the trees to
its jobs over NFS on loopback, the mount provider and the put ingest. `/usr/local/tierstore/etc/tierstore.toml`:

```toml
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9300"] }

[[interface]]
type   = "mount-provider"
params = { socket = "/usr/local/tierstore/run/provider.sock" }

[[interface]]
type = "fs"
[interface.params]
mountpoint   = "/private/var/tierstore/fs"
client_group = "_tierstore"
owner        = "_tierstore:_tierstore"
# launchd makes no runtime directory: the installer package makes this one.
runtime_dir  = "/usr/local/tierstore/run/fs"
[interface.params.nfs]
# A fixed loopback port, so that the clients' hard mount finds the server again after a restart.
listen = "127.0.0.1:12049"

[server]
proxy_id_from_facts  = { path = "/var/lib/hostkeeper/facts.json", prefix = "tierstore-cache-" }
bucket               = "cas"
access_key_file      = "/usr/local/tierstore/etc/access.key"
read_only_key_files  = ["/usr/local/tierstore/etc/daemons.key"]
s3_endpoint_attempts = 6

[local_cache]
path             = "/usr/local/tierstore/cache/cas"
budget           = "200GiB"
whole_fill_limit = "128MiB"
write_through    = true
put_ingest       = "/usr/local/tierstore/put"
# no fill_socket

[cold_store]
kind             = "s3"
bucket           = "cas"
endpoints        = ["http://192.0.2.1:9300", "http://192.0.2.2:9300", "http://192.0.2.3:9300",
                    "http://192.0.2.4:9300", "http://192.0.2.5:9300", "http://192.0.2.6:9300"]
credentials_file = "/usr/local/tierstore/etc/upstream.key"

[[policy]]
prefix = "cas/"
cache  = true
```

What leaves each group's configuration package: `fs.toml`, the three keys repeated between the two files, and on the Macs
the hook param `fs_launchd_label`. What the deployment's own checks of its files (that `fs.toml` repeats the node file) have
left to compare: the mount-provider socket and the mountpoint against the job agent's flags.

### 1.4 Units and jobs

**Linux.** One template, `tierstore@.service` (`packaging/hostkeeper/units/`, and its copy `packaging/tierstore@.service`
for the Debian and RPM packages, which differs only in the binary), instances `a` and `b` with the markers `run-a` and `run-b` as
today. It gains what the file system needs from `tierstore-fs@.service`, and loses what the host mount namespace rules out
(section 2.2 has every option with its reason):

```ini
[Unit]
After=network-online.target tierstore-guard.service
Wants=network-online.target tierstore-guard.service
ConditionPathExists=/var/lib/tierstore/run-%i
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=notify
NotifyAccess=main
User=tierstore
StateDirectory=tierstore
StateDirectoryMode=0700
RuntimeDirectory=tierstore-fs tierstore
RuntimeDirectoryPreserve=yes
ExecStart=/usr/local/libexec/tierstore/tierstore --config /etc/tierstore/config.toml
KillSignal=SIGTERM
TimeoutStartSec=300
TimeoutStopSec=960
Restart=on-failure
RestartSec=2
RestartPreventExitStatus=2 3
LimitNOFILE=1048576
UMask=0077
NoNewPrivileges=yes
CapabilityBoundingSet=CAP_SYS_ADMIN
AmbientCapabilities=CAP_SYS_ADMIN
DevicePolicy=closed
DeviceAllow=/dev/fuse rw
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
RestrictNamespaces=yes
RestrictRealtime=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service @mount pidfd_open pidfd_getfd
FileDescriptorStoreMax=1
FileDescriptorStorePreserve=restart
```

- `Type=notify`: `READY=1` once the process serves its mount and its fronts (a successor: once the old instance has said
  `bye`, section 3.1). The cache may still be opening then; `/readyz` says so, as today.
- `RestartSec=2`, `StartLimitIntervalSec=60`, `StartLimitBurst=5`: the file system's values. While the process is down,
  jobs' requests queue in the kernel, so it comes back fast; a process that cannot come back makes the unit fail, systemd
  drops the descriptor store, the kernel aborts the connection, and jobs fail instead of hanging (virtual-build-fs.md 7.2).
  The S3 unit's values today are 30 s and 5 in an hour.
- `TimeoutStopSec=960`: the S3 drain (10 s settle, 15 minutes deadline). `TimeoutStartSec=300`: above `params.handoff_wait`
  plus the attachments made again.
- The runtime directory keeps the name `/run/tierstore-fs`, so a process of the new layout reads the state a `tierstore fs`
  process left there (section 4.2), and `/run/tierstore` is the mount point's parent, as today.
- The job runtime's unit orders itself after the mount by `After=` and `Wants=` on `tierstore@a.service tierstore@b.service`
  (only the instance whose marker exists starts), instead of on `tierstore-fs@a.service`.

A node with no `fs` interface runs the same template. `packaging/tierstore.service`, the example unit for a host placed
by hand, is not a copy of the template: it has no instances, no marker and no hand-over (`packaging/README.md`). It keeps
its options and gains none, and its header says that a node with an `fs` interface runs the template, whose descriptor
store and hand-over the file system needs.

**macOS.** One job, `tierstore` (`/Library/LaunchDaemons/tierstore.plist`, the installer package's and the archive package's
copies as today), `--config /usr/local/tierstore/etc/tierstore.toml`, as `_tierstore`. It takes from `tierstore-fs.plist`
what the file system needs:

- `ExitTimeOut` 210 s: on bootout the process drains S3 (1 s and 60 s on a Mac) while the mount is still served, then
  unmounts while it still serves NFS (at most four steps of 25 s and 5 s to collect a killed command, 120 s), and exits; 210
  s is the sum with a margin, so launchd never kills it in the middle of an unmount. Today: 90 s and 150 s for the two jobs.
- `ThrottleInterval` 10 s (today 30 s and 10 s): a hard mount waits for its server, so the server comes back fast.
- `NumberOfFiles` 1048576, `Umask` 63, no environment, `AbandonProcessGroup` not set: as both plists have today.

The installer package's postinstall keeps making `/usr/local/tierstore/run/fs` and the mount point.

### 1.5 The fill socket becomes a call

Every verb of the fill socket (`server/fill_socket/request.rs`, `mod.rs`, `mount.rs`, `put.rs`) is already a thin wrapper
over a method of the cache or the tree store (read in code). The file system's core calls those methods directly:

| Fill-socket request | The call |
|---|---|
| `fill <sha256> <size> from <key> @ <offset>`, `fill <sha256>`, `fill-tree <sha256> <ms>` | `LocalCache::fill_blob`, `fill_manifest`, `fill_tree` |
| `touch`, `pin`, `unpin`, `pins-live` | `LocalCache::touch_blobs`, `pin`, `unpin`, `pins_live` |
| `put <key> <sha256> <size> from <path>` | `fill_socket::put::answer`'s body: open the staged file once and stream it through `LocalCache::put_stream_sha256` (read once, hashed and stored in one pass, no size cap: B16) |
| `place`, `mounted`, `flat`, `image`, `span`, `unplace`, `places-live` | `TreeStore::mount_place` and `attach`, `mount_paths`, `flat`, `mount_tree`, `fetch_span`, `detach`, `holds_live` |
| the reports `started`, `attached`, `fetched`, `levels`, `descriptors`, `opens` | `LocalCache::run_started`, `tree_attached`, `manifests_fetched`, `tree_levels`, `descriptors_reported`, `opens_reported` |

- **Threads.** The FUSE readers are blocking threads (`fs.threads`, 16 by default), and the cache is async. A reader blocks
  on the call through the runtime's handle; the readers are not runtime threads, so the wait cannot starve the runtime
  (inferred from `server/treefs/serving.rs`, which starts them as `std` threads).
- **The read-only window.** Today a fill that meets no socket (the instance does not hold the cache lock) is asked again
  every second for `fetch_timeout` (`fill_client.rs`). The call keeps that behaviour: while the cache is read-only, a fill,
  pin or put waits for the write role (`LocalCache::watch_mode`), up to `params.fetch_timeout`, and then fails as a missed
  fill fails today (`EIO` on `open`, recorded on the attachment). Section 3.2 says when that window occurs.
- **The peer-credential check** of the socket (same user) has nothing left to check.
- **What it removes:** a line parser, a listener, a connection pool, a reports thread with a queue that drops on overflow
  (`fill_client.rs`), and the two-file agreement on the socket's path.

## 2. Privilege and the start

### 2.1 Privilege

**Linux.** The process holds `CAP_SYS_ADMIN` for its whole life (the unit's `CapabilityBoundingSet` and
`AmbientCapabilities`), because it makes the FUSE mount (`crates/tsfuse/src/mount.rs`: `mount(2)` with `fd=N` in the
options) and detaches a dead one at a fresh start (`server/treefs/run.rs`: `discard_with`, `umount2(MNT_DETACH)`). Since
passthrough was removed after 0.3.121 (`server/treefs/config.rs` lists `passthrough` and `max_backing_ids` as removed),
that is all the capability is for. It is what `tierstore-fs@.service` grants today; the one process holds it instead.

**macOS:** none. The process runs as `_tierstore`; the NFS listener is on an unprivileged loopback port, and `mount_nfs`,
`umount` and `diskutil unmount force` of a loopback export need no root when the mounting user owns the mount point (read in
the `tierstore-fs.plist` the installer package shipped until komira-ai/tierstore#624; Measured: macos E14 in
virtual-build-fs.md).

### 2.2 Unit hardening: what the host's mount namespace rules out

The mount must be made in the host's mount namespace, so that the job runtime sees it and the next instance can take it. Every
systemd option that gives a service a mount namespace of its own, from which a mount does not reach the host, is therefore
out. That is the reason the S3 unit and the fs unit differ today (`tierstore-fs@.service`, header). Besides the options of
the table, the path options make a mount namespace too (`ReadWritePaths=`, `ReadOnlyPaths=`, `InaccessiblePaths=`,
`BindPaths=`, `BindReadOnlyPaths=`, `TemporaryFileSystem=`, `RootDirectory=`, `PrivateUsers=`, `ProtectProc=`); the unit
sets none, needs none (without `ProtectSystem=strict` no path has to be made writable), and P6's test refuses every one, so a
drop-in that adds one is the operator's change, not the package's. The table is the one unit against both:

| Option | S3 unit today | fs unit today | One unit | Why |
|---|---|---|---|---|
| `User=tierstore` | yes | yes | yes | |
| `NoNewPrivileges=yes` | yes | yes | yes | nothing the process executes gains a privilege |
| `CapabilityBoundingSet` | empty | `CAP_SYS_ADMIN` | `CAP_SYS_ADMIN` | the mount |
| `AmbientCapabilities` | none | `CAP_SYS_ADMIN` | `CAP_SYS_ADMIN` | how a non-root service gets it |
| `PrivateDevices=yes` | yes | no | **no** | makes a mount namespace; replaced by `DevicePolicy=closed` and `DeviceAllow=/dev/fuse rw`, a cgroup device filter with no namespace: the standard pseudo devices and `/dev/fuse`, nothing else |
| `ProtectSystem=strict` | yes | no | **no** | makes a mount namespace |
| `ProtectHome=yes` | yes | no | **no** | makes a mount namespace |
| `PrivateTmp=yes` | yes | no | **no** | makes a mount namespace |
| `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs`, `ProtectControlGroups` | yes | no | **no** | each makes a mount namespace (read-only or hidden `/proc/sys`, `/sys`, module directories, cgroup tree) |
| `PrivateMounts` | no | no | no | a mount namespace by definition |
| `RestrictNamespaces=yes` | yes | no | **yes** | a system-call filter, not a namespace (systemd.exec); the fs unit left it out with the namespace options, but it does not keep a mount from the host. The process makes no namespace. |
| `RestrictAddressFamilies` | `AF_UNIX AF_INET AF_INET6` | `AF_UNIX` | `AF_UNIX AF_INET AF_INET6` | the fronts |
| `RestrictRealtime`, `LockPersonality`, `MemoryDenyWriteExecute`, `SystemCallArchitectures=native` | yes | yes | yes | |
| `SystemCallFilter` | `@system-service` | `+ @mount pidfd_open pidfd_getfd` | `+ @mount pidfd_open pidfd_getfd` | the mount and the adoption of a stored descriptor without `unsafe` |
| `FileDescriptorStoreMax=1`, `FileDescriptorStorePreserve=restart` | no | yes | yes | the mount outlives a crash |

### 2.3 The start (Linux)

The file system's start order of today (`server/treefs/launch.rs`: `start`) is kept; what is new is that the server's own
start runs it, and that the take is the one hand-over of 3.1:

1. Read and check the configuration file, and the facts file it names.
2. If the file has an `fs` interface, get the mount, in this order:
   1. **Take** it, with the sockets, from a running instance over `server.handoff_socket` (3.1). In release M only, also
      take it from a `tierstore fs` process of the old layout over `handoff.sock` in the runtime directory when one
      answers there (4.2); when both answer (the switch of 4.2), the mount comes from the process that holds it.
   2. Else **adopt** the descriptor systemd kept in the store under the name `fuse` (`LISTEN_FDS`, `LISTEN_FDNAMES`,
      `LISTEN_PID`: platform markers, allowed by S2; `server/treefs/sd.rs`).
   3. Else **mount afresh**: find any mount of type `fuse.tierstore-fs` at the mount point in `/proc/self/mountinfo` without
      touching the mount point (a `stat` of a mount whose descriptor some process holds but does not read blocks; today's
      `discard_dead_mount` does a `stat` and acts only on `ENOTCONN`). Such a mount has no server: nothing answered the
      hand-over and the store had nothing. Abort its connection and detach it, make the mount point, open `/dev/fuse`,
      `mount(2)`, and put the descriptor in the store (`FDSTORE=1`, `FDNAME=fuse`, and wait for the barrier, as today).
3. Open the cache (read-only until it holds the lock), bind the trees sockets beside a running instance (`SO_REUSEPORT`),
   take or bind the S3 and OCI sockets (in the same conversation, for a take), make the file system's state ready (the tree
   table, the attachments made again from their manifests, the control socket or, for a take, the one received, the put
   directory's stage and journal), start the session (the `INIT` handshake of a fresh mount, or the recorded `INIT` reply
   after a take or an adoption, with `FUSE_NOTIFY_RESEND` where the kernel offers it) and the readers, and bind the
   mount-provider socket when the cache holds its lock, as today.
4. `READY=1`.

## 3. Live update: one hand-over

### 3.1 Linux: the conversation

One control socket, `server.handoff_socket` (default `handoff.sock` in the state directory, as today), and one
conversation that carries everything a successor needs. It is the listening-socket hand-off of tierstore-handover.md
section 0 (`take 1`, with the `ready` step of the NFS front), with the steps of the mount's hand-over (`take-fs 1`,
`server/treefs/handover.rs`) folded into it, and the cache lock added. The new process opened its cache before the first line (2.3):

```
new (tierstore@b)                                              old (tierstore@a, running)
 | "take 1" / "ready" / "mount" ─────────────────────────────▶ |  one taker at a time; peer of the same user
 | ◀── "offer" / "s3 <addr>" ... / "oci <addr>" / "fuse <mountpoint>" / "control <path>" / "lock <path>"
 |       + the listening sockets, /dev/fuse, the control socket, the cache lock file (SCM_RIGHTS)
 | peer of the same user? addresses cover its settings? mountpoint, control path, lock path equal its own?
 | /dev/fuse is the FUSE device, the lock descriptor the file at the lock path (fstat = stat)?
 | the runtime state loads (tree table, INIT record)?
 | puts /dev/fuse in its store (FDSTORE=1, FDNAME=fuse), waits for the barrier
 | "ack" (else "nak": the old goes on as it was) ────────────────▶ |
 |                                                                | quiesces the file system: each reader answers
 |                                                                | the request it holds and stops; the control socket
 |                                                                | is paused; no table or attachment write; the put
 |                                                                | directory's commits end (put-handover written);
 |                                                                | removes its hand-over socket file. S3 and OCI go on.
 | ◀── "done"                                                     |
 | reads the runtime state again, prepares the session from the recorded INIT reply (reads nothing
 | of the device yet), binds the hand-over socket
 | "ready" ───────────────────────────────────────────────────▶ | stops accepting S3/OCI; starts no new cache write
 |                                                                | (its misses go upstream without a fill, as a read-only
 |                                                                | instance serves them today); removes its own store
 |                                                                | entry (FDSTOREREMOVE=1, FDNAME=fuse); from here it
 | ◀── "bye"                                                     | never reads the device again
 | reads the device; accepts S3/OCI; READY=1                      |
 | (its cache is read-only: fills wait, 1.5)                      | ... in-flight cache writes end
 | ◀── "written"   (or the connection ends: it exited)           |
 | takes the write role: what a take of the lock does today       | drains S3 (in-flight requests), exits at once,
 | (stage emptied, objects rescanned, index replaced); fills go; |  without unlocking and without freeing its state
 | binds the mount-provider socket                                |
```

Where each step comes from:

| Step | From |
|---|---|
| request `take 1`, the offer's peer check, one taker at a time, `offer` with `<front> <address>` lines, the covers check, `ack`/`nak`, the old removes its socket file at `ack`, the taker binds it after `done` | `take 1` (`server/handoff/offer.rs`, `take.rs`) |
| `ready` and `bye`, the old serves on until `ready` and gives up a taker silent past the wait, so the two never serve together | the `ready` step of the NFS front (tierstore-handover.md section 0; `offer.rs`: `awaits_ready`) |
| `fuse` and `control` lines and descriptors, the check that the runtime state loads before `ack` (`Prepared::load` in the take's callback, `server/treefs/launch.rs`), the successor stores the device before `ack`, the quiesce at `ack` (readers answer what they hold, `core.hold(true)`, `hand_put`), the old resumes if the taker goes away before `ready`, and exits at once after (`mem::forget` of its state) | `take-fs 1` (`server/treefs/handover.rs`, `run.rs`: `quiesce`, `resume`; `launch.rs`) |
| the `mount` request line, the `lock` line and descriptor, the `written` frame; the taker's own peer check and the check of each descriptor's kind; the device read only after `bye`; the old's store entry removed at `ready` | new, sections 3.1 and 3.2 |

- **Additive on the wire.** A frame is lines and descriptors (`server/handoff/wire.rs`, at most 253 descriptors); a taker
  skips a front line it does not know and closes its descriptor, which is how `nfs` was added. The `mount` line is a third
  request line, as `ready` was a second: an instance that does not know it, or holds no mount, answers `refused`, and the
  taker asks again without it, which takes the sockets alone (section 4.2 uses that). An instance that holds the mount
  refuses a taker that does not ask for it, so the mount is never left with an instance that is about to exit: a taker that
  asks `take 1` alone at its request, and one that asked for the `ready` step at that `ready` (`refused` in place of `bye`),
  unless the mount was taken meanwhile on the instance's `take-fs 1` (the two conversations share a claim of the mount, so
  one taker at a time is at it). That is how the start of the previous release, which asks `take 1` / `ready` and takes the
  mount on `take-fs 1` before its `ready`, still takes over from this one, so that an update back to it is a hand-over. The
  B8 tests of P4 replay a previous release's taker against this offer and this taker against a previous release's offer.
- **The lock line** is offered, by an instance that has a cache, to a taker that asked for the `ready` step; a taker that
  asks `take 1` alone gets the sockets as today and stays read-only until the old one exits (today's behaviour, which 4.2's
  rollback uses). A previous release's taker skips the line and closes the descriptor, which does not unlock (3.2).
- **The taker's checks.** Today only the offering side checks its peer's user (`peer_cred` in `offer.rs` and in
  `handover.rs`); the taker relies on the socket being in the service's state directory (mode 0700). The taker now checks
  its peer too, and checks each descriptor's kind before it uses it: the `fuse` descriptor is the character device
  `/dev/fuse`, the listening sockets are listening sockets, the lock is a regular file.
- **Who reads the device.** The old reads it until `ack` and never after `bye`; the new reads it only after `bye`. Between
  `ack` and `bye` nobody reads it and requests wait in the kernel's queue. A taker that is slow or gone before `ready` leaves
  the old to resume (it has not read); after `ready`, the old has handed over for good. Today's `take-fs 1` lets the taker
  read the device before it says `ready`, so an old instance whose wait ran out can resume while its successor already
  reads; release M gives `take-fs 1` the same `bye` and the same rule.
- **Waits.** The old instance waits for a connected taker's request line up to `params.handoff_wait` and serves meanwhile;
  other takers are refused while it waits, as while it is at any taker. `server.handoff_wait` (10 s) bounds the taker's wait for `offer`; `params.handoff_wait` of the `fs` interface
  (120 s, as `fs.handoff_wait` today) bounds the quiesce from `ack` to `done` (a reader may hold a request that waits for a
  fill, and a put commit waits up to `put.commit_deadline_secs`) and the old's wait for `ready`. Between `done` and `ready`
  the new process only reads the runtime state and prepares the session (it opened its cache before it asked, 2.3), so
  that wait is seconds.
- **Failure at each step:** a taker that says `nak` or goes away before `ack` leaves the old as it was; one that goes away
  after `ack` and before `ready`, or is silent for the wait, leaves the old to resume reading the device, release its hold,
  make its hand-over socket again and serve on (as both protocols do today). A successor that dies after `ready` has the
  descriptor in its own store, so systemd restarts it and it adopts (3.5); what the old does with the lock then is in 3.2.
  The old instance, once it has handed over, is not started again: it removed its store entry at `ready`, and the hook
  removes its marker as soon as the new one serves (as it does for the fs unit today), so a crash in its drain is not
  restarted by systemd.
- **Capability.** A release that speaks this lists `handoff-service-v1` in `tierstore --capabilities`. The hook hands a node
  with an `fs` interface over only when both releases list it (section 3.6).
- **Built** (P3's wiring of P4, read in code): the running instance offers its mount on `server.handoff_socket`
  (`Offer::with_mount` with `treefs::handover::offered`, from `cache_cli.rs`) beside its `take-fs 1` on the runtime directory's
  socket, and its start asks `take 1` / `ready` / `mount` there (`cache_cli_mount.rs`): with an offer of the mount it takes
  everything in this one conversation (`Mounted::take_handed`: the device stored before `ack`, the runtime state read again
  after `done`, `ready` said before the device is read) and sends no `take-fs 1`; with an offer without it (the old layout's
  S3 process, or an instance whose mount a `take-fs 1` took), it takes the mount on `take-fs 1` in the order of 4.2. Tests:
  `a_running_one_process_instance_offers_sockets_mount_control_and_lock_on_the_handoff_socket`,
  `a_successor_of_one_process_is_writer_at_written_not_at_the_old_exit`,
  `a_taker_with_a_mount_and_a_cache_is_read_write_at_bye_without_the_old_process_exiting`,
  `a_previous_release_start_takes_the_mount_on_take_fs_and_the_sockets_with_the_lock` (`treefs/interface_service_tests.rs`).

### 3.2 The cache lock

Today the cache lock is an `flock(2)` on `<cache>/.lock` (`server/lock.rs`). Before komira-ai/tierstore#571 the successor of a hand-over did not get it:
it served read-only until the old process exited and the kernel freed the lock, and then took it at the next tick of
`local_cache.lock_interval` (`local_cache/hold.rs`). Meanwhile the old process kept answering the fill socket, so the file
system's fills went on through the old process's whole drain.

In one process that does not work: from `bye` the file system is served by the new process, whose fills are calls into its
own cache, and a cache that stays read-only for the old process's whole drain (up to 15 minutes) would hold those fills that
long. So the lock passes in the hand-over:

- **Passing it.** An `flock` belongs to the open file description, not to the process, so the descriptor sent with
  `SCM_RIGHTS` holds the same lock, and the lock is freed only when the last descriptor of that description is closed
  (flock(2)). The taker checks that the descriptor is the file at the lock path (`LockFile::is_current`). From then on no
  third process can take the lock while either holds it.
- **Nobody unlocks a shared lock.** `flock(LOCK_UN)` through any descriptor of the description frees it for every holder, and
  `LockFile` unlocks when it is dropped (`server/lock.rs`). So the taker keeps the received descriptor in a type whose drop
  only closes it, and turns it into a `LockFile` only when it becomes the writer; on every path that ends before (a
  descriptor of another file, `nak`, a taker gone before `ready`) it only closes it, and the old keeps the lock. (On macOS
  there is no old holder after the exec: an image that refuses its offer exits, which frees the lock, 3.4.) The old process closes its descriptor only by exiting, but for the one case below.
- **One writer at a time.** The old process stays the writer until `ready`. At `ready` it starts no new cache write (its
  mode becomes read-only, as on a lost lock today but without dropping its `LockFile`, so its own mount-provider and fill
  sockets close) and lets the writes in
  flight end; when none is left it sends `written`. The new process becomes the writer on `written`, or when the connection
  ends (the old exited or crashed, so it writes nothing more).
- **A successor that dies after `ready`.** Its descriptor of the lock closes with it, and the old process, which writes
  nothing new any more, is the lock's only holder. The old keeps the connection open until it exits; when it ends early,
  the old lets its cache writes in flight end (as before `written`) and then unlocks (the only `flock(LOCK_UN)` of the
  hand-over), so the restarted successor takes the lock at its next tick of
  `local_cache.lock_interval` instead of after the old one's whole drain. The connection ends only when the successor's
  process ends: the successor holds it until it exits.
- **A lock handed on.** A successor offers the lock it received to a successor of its own only once the connection to the
  instance it came from has ended (`LocalCache::lock_to_hand`, `predecessor_gone`): until then that instance may still
  unlock it when its own successor's connection ends, which would free it under the third. A third instance that takes
  over meanwhile gets the sockets alone and waits for the lock at its tick, as a taker without the lock does (read in code,
  `server/local_cache/handing.rs`).
- **The read-only window** is now from `ready` to `written`: the longest cache write in flight on the old process (a fill,
  bounded by its own deadline, or a put, bounded by the S3 request's), not its drain. Fills of the file system wait across
  it (1.5), up to `params.fetch_timeout`. The take of the write role does what a take does today (empty the stage, rescan,
  replace the index), at `written` instead of at the old process's exit; its cost is unchanged.
- **Built** (read in code, `server/cache_cli_mount.rs`, `server/cli_serve.rs`): the Linux start of a node whose file has an
  `fs` interface takes the lock with the mount and the sockets in the one conversation of 3.1 on `server.handoff_socket`
  (`handoff::offered_with`), holding the received descriptor in a type whose drop only closes it (`Received`); against an
  instance that holds no mount it takes the sockets and the lock there and the mount by `take-fs 1`, as in 4.2, and says
  `ready` on `server.handoff_socket` after the mount's `bye`. Either way it says `ready` before it serves or binds its own
  control socket, and its cache becomes the writer at `written` or at the end of the connection (`handoff::follow`), and
  binds the mount provider then. A lock the running instance offers of another file (a configuration that moves
  `local_cache.path`) is closed and left with it, not refused with `nak`: the two caches are two directories. One
  that does not answer `bye` serves on, with its lock: the start then fails before it serves anything, leaving the
  mount it took to its descriptor store, and the restart adopts it and takes the sockets again. Left as it is: the
  successor's scan of the cache at `written` (`promote`) is as long as before, and the mount provider is bound only
  after it.

### 3.3 What jobs see

- **Open files** survive, as with `take-fs 1` today: the FUSE connection never closes (the old holds the descriptor until it
  exits; the new holds it in its store from before `ack` and in the process from `done`, reading it from `bye`), each handle names its inode and
  generation (`server/treefs/handle.rs`), and the new process answers any inode of the table of tree numbers, which it reads
  from the runtime directory (the table is reused on a take: `TreeTable::open`).
- **Lookups and opens** between `ack` and `bye` wait in the kernel's queue: the old process's quiesce (seconds; at most
  `params.handoff_wait`) and what the new one does between `done` and `ready` (seconds: it opened its cache and made its
  attachments before it asked, 2.2). A `stat` of the mount by any process waits the same.
- **Fills** wait from `ready` to `written` (3.2).
- **The mount provider** socket is closed by the old process at `ready` and bound by the new one when it becomes the writer;
  the job agent asks again after 200 ms within its bounds (10 s to prepare, 30 s to mount, `--mount-provider-wait` 120 s from
  admission; read in the job agent's `provider/mod.rs` and `lifecycle/mount_providers.rs`), as it does over today's gap of a
  lock tick plus the old drain.
- **S3 clients** connected to the old process finish on it; new connections queue in the shared listening socket until the
  new process accepts at `bye`. Nothing is refused.

### 3.4 macOS: the hand-over to the next image of the same process

There is one launchd job, so there is no second instance to hand over to: launchd tracks the job's process by its pid, and
starts a second process of the job only after the first exits (and kills the job's process group when it does, as
`AbandonProcessGroup` is not set). That launchd keeps a job whose process executes another image under the same pid, and
delivers `launchctl kill SIGUSR2`, is inferred from launchd's tracking of the job by pid and unverified; P5's macOS
integration test `a_launchd_job_keeps_its_pid_and_is_not_restarted_across_the_exec` checks it before P7 relies on it. What stays the same across an `execve(2)` is the pid, the open descriptors that are not
close-on-exec, and the `flock` on them. So a Mac hands over to the next image of the same process:

1. The hook installs the new binary at the job's program path, as today, and sends the process `SIGUSR2` (by
   `launchctl kill SIGUSR2 system/<label>`).
2. The process stops accepting on the S3, OCI and NFS listening sockets (it keeps them open: new connections queue in
   their backlog), drains S3 as on a stop (`server.drain_lb_wait` 1 s and `server.drain_deadline` 60 s on a Mac), and
   ends its NFS connections as the NFS hand-over does today: it answers every request it has read, shuts down the write side
   of each connection and waits for the client to close it (`params.nfs.drain_wait`; `server/treefs/nfs/run.rs`). The mount
   is not touched: it is hard, so its client waits.
3. It lets its cache writes in flight end and starts none (as at `ready` in 3.2), holds its state (no table or attachment
   write; the hold of today's NFS hand-over), and writes the offer of section 3.1
   (`s3`, `oci`, `nfs`, `control` and `lock` lines, with their descriptors) into one end of a `socketpair`, and executes
   its own program path (`std::env::current_exe`, which names the new binary) with its arguments and `--handed <n>`, the
   number of the other end, the one descriptor not marked close-on-exec. The other end is placed at descriptor 0, the new
   image's standard input, so `<n>` is `0` and `--handed` takes no other number: a descriptor number from the command line
   can be made a descriptor only by `unsafe` code, which tierstore does not have (AGENTS.md R2), while the standard
   library places a descriptor at 0 for an exec (`Command::stdin`), and the new image reads standard input without it
   (built: `server/handoff/exec.rs`, `server/next_image.rs`). After the offer it writes one more frame, `keep`, with a
   copy of the other end: from the exec to the new image's read, nothing but the message holds the offer's descriptors,
   and macOS's collector of descriptors in flight (`unp_gc`, run whenever a Unix socket is freed) walks only from
   descriptors that are in a message themselves, so without it the offer's sockets are flushed and every connection the
   new image accepts has its receiving side shut. The end in a message of its own is where the walk starts and reaches
   the offer. No image reads `keep`: a read takes one frame and leaves the next in the socket.
4. The new image reads the offer from that descriptor with the taker's code and checks it as a taker does (addresses cover
   its settings, paths equal, the lock is the file), reads its runtime state (the table of tree numbers is reopened for the
   same boot epoch, `nfs/boot.rs`; the mount is found and not made again, `nfs/run.rs`), and serves on the inherited
   sockets. It holds the cache lock from the first instruction, and takes the write role as a take of the lock does today
   (stage emptied, objects rescanned, index replaced) before its first cache write; fills wait for it (1.5).

- **What jobs see.** NFSv3 has no open state, and handles are derived from the inode and the boot epoch
  (`server/treefs/nfs/handle.rs`), so an open file is read on after the pause with no error (Measured: macos E16 in
  virtual-build-fs.md 7.5, for a restarted server). The pause is the drain plus the new image's start. The mount's
  `deadtimeout` (`fetch_timeout` plus 30 s, 90 s by default) must stay above it; a start that must wait for the cache
  answers NFS reads that need a fill with `NFS3ERR_JUKEBOX` after `params.nfs.jukebox_after` (5 s), so the client retries
  and does not count the server dead.
- **Compared with today**, where every update boots the fs job out (which unmounts: `launchd/fs.rs`), and every job that
  holds a tree then fails, this is a pause instead of a loss.
- **Compared with Linux**, it is weaker in two ways. There is a gap: nothing is accepted from step 2 to the end of step 4
  (Linux has none). And there is no way back: once the process has executed the new image, the old one is gone. If the new
  image refuses the offer or fails, it exits, launchd starts the job again (`KeepAlive`), and the new start binds its own
  sockets on the same addresses and finds its mount; if the new binary cannot serve at all, the hook's rollback puts the old
  binary back and does the same again. `prepare` runs the staged binary's `--check-config` first, so a configuration it
  refuses never gets that far.
- **Capability.** `handoff-exec-v1`, listed by the builds for macOS. Without it in both releases, the update is the restart
  of today, which unmounts: section 3.6 says when the hook does it.
- **What P5 built** (section 6): a build node hands its S3 and OCI sockets and its cache lock over this way on SIGUSR2,
  and the NFS front's part is built and tested (`Running::hand_to_image`, `Running::start_handed` in
  `server/treefs/nfs/run.rs`: the drain of the owed replies, the hold, the `nfs` and `control` lines). The start of the
  one process on macOS serves the file system by that front and adds its part to the offer
  (`server/treefs/nfs/interface.rs`), as P3's start serves it on Linux by FUSE.

### 3.5 Stop, crash and a fresh start with a mount there

- **A stop** (SIGTERM with no successor): the process drains its fronts first and serves the mount throughout, since a fill a
  job waits for may need the upstream. Then, on Linux, it stops reading the device and leaves the mount to the descriptor
  store (`Running::leave` today); the store is kept only while a restart is pending, so a `systemctl stop` drops it and the
  kernel aborts the connection, failing the jobs that use it (as `systemctl stop tierstore-fs@a` does today). On a Mac it
  unmounts while it still serves NFS, then exits (`Running::leave` of `nfs/run.rs` today). The hook never stops the unit
  while the mount is in use (section 3.6); `uninstall` does.
- **A crash** (Linux): systemd keeps the descriptor (`FileDescriptorStorePreserve=restart`), restarts the unit after 2 s, and
  the new process adopts it (step 2.ii of 2.3) and reads the runtime state, sends `FUSE_NOTIFY_RESEND`
  where the kernel offers it, and otherwise watches `waiting` for `params.recover_wait` and aborts if it never empties
  (today's `launch.rs` and `recover.rs`). The S3 and OCI sockets are bound again, with a gap of the restart, as a crash of
  the S3 unit has today. The cache lock is freed with the process unless an old instance of a hand-over still holds it, in
  which case that one unlocks when its connection to the crashed process ends (3.2); the new one takes it at its next tick.
- **A crash** (macOS): launchd starts the job again after its throttle; the new start binds its sockets on the same
  addresses, finds its mount and reopens the table for the boot epoch (today's NFS restart arm).
- **A fresh start that finds a mount at the mount point** (Linux): it gets there only when no process answered on the
  hand-over socket (nor, in release M, on the old layout's fs socket) and the store was empty, so no process serves the
  mount. The hook starts one instance at a time, as today. A running instance that accepts on the hand-over socket and
  never answers fails the start after `server.handoff_wait`, as today, and is not treated as absent. What remains is a dead
  mount: the start aborts its connection, detaches it (`MNT_DETACH`), and mounts afresh, with
  a new mount id and an empty table of tree numbers. Jobs that held clones of the dead mount see `ENOTCONN`, which the job
  agent records as an unreadable input (`crun/fs/kernel.rs`: `unreadable`) and fails as an infrastructure failure; that
  happens only after the unit gave up or the node lost every holder, as today.
- **If the unit gives up** (5 starts in 60 s): systemd drops the store, the kernel aborts the connection, jobs fail fast,
  and the next start mounts afresh.

### 3.6 What the hook does

- **Linux, an update** of a node whose file has an `fs` interface: a hand-over when both releases list `handoff-service-v1`.
  Otherwise `prepare` refuses (`mode_unsupported`, a new reason): a restart would drop the mount under running jobs. The
  params' `mode = "restart"` overrides it, for an operator who has drained the node's jobs. (Today the hook leaves the fs
  unit on its old release for a pair that cannot hand it over, `update_hook/systemd/fs.rs`; with one process the release
  cannot be split.)
- **Linux, a configuration update** (the config hook): the same hand-over, to the other instance on the new file. A new
  file that names another mount point, control socket or runtime directory cannot take the mount (the taker sends `nak`,
  as for other addresses today), so the config hook's `prepare` refuses it unless `mode = "restart"`.
- **macOS, an update or a configuration update**: the hand-over of 3.4 when both releases list `handoff-exec-v1`; otherwise
  the same refusal and override. The bootout and bootstrap of today remain for a first install, `restore` and `uninstall`.
- **No stop order** and no second arm: `update_hook/systemd/fs.rs`, `update_hook/launchd/fs.rs`, the `Family::Fs` units and
  markers, `fs_live` and `fs_next`, and `fs_launchd_label` go (section 5). The record keeps decoding `fs_live` and `fs_next`
  (B8: `tests/stored_formats/fixtures/hook_record_systemd_fs.json` stays and its test still passes); a release that no
  longer writes them reads them only to refuse an update of a node that still runs a `tierstore-fs` unit (section 4.1).
- **The start wait.** `Type=notify` makes `systemctl start` return at `READY=1`, as for the fs unit today
  (`FS_START_TIMEOUT`, 330 s in `update_hook/system.rs`); the hook then looks at `/readyz` for the target version as today.

## 4. Migration: one window per group

### 4.1 Two releases: M carries both layouts, M+1 only the new one

The switch of a node from two processes to one has to move a live mount from one process to another without a gap a job
can see. The only code that can do that is code that speaks both sides. So:

- **Release M** (the build plan's phase 1) adds the `fs` interface, the one hand-over and the hook's switch, and keeps everything of today: `tierstore fs`, `fs.toml`, the fill socket, `take-fs 1` on both sides, the
  hook's fs arms. A node whose file has no `fs` interface runs as today, under the unit of 1.4, which M ships. M's
  `take-fs 1` gets the `bye` of 3.1. M is released and reaches every node by the normal
  rollout, before any switch.
- **The window** of a group is a configuration change, run by M's code on both sides of it (4.2, 4.3).
- **Release M+1** (phase 2) deletes the old layout (section 5). It is merged after both groups have switched. Its `prepare`
  refuses, by name, an update of a node that still runs a `tierstore-fs` unit or job (`fs_live` in the record, a
  `run-fs-*` marker, or a loaded `tierstore-fs` job): "switch this node to the fs interface with release M first". So a node
  that missed its window fails loudly instead of losing its mount.

### 4.2 `all-in-one` (Linux), node by node

Before the window, every node runs M in the old layout: `tierstore@<x>` with the node file, `tierstore-fs@<y>` with
`fs.toml`, both of release M (the normal update hands both over, as today).

The switch is the configuration hook's cutover with the new file (section 1.3). The group's configuration package carries
the file and runs the hook; where that package is not yet deployed, the operator's held step for the node writes the file
and runs the same hook steps. Per node, canary first, one node at a time:

1. **prepare**: M's `--check-config` on the new file (as today), and the record notes the switch: the file has an `fs`
   interface, and a `tierstore-fs` instance runs (`fs_live`).
2. **cutover**: the hook starts `tierstore@<next>` on the new file. Its start (2.3) is:
   1. Connect to `server.handoff_socket` (the old S3 process answers) and to `handoff.sock` in the runtime directory (the
      old fs process answers; the runtime directory is `/run/tierstore-fs` in both layouts).
   2. Ask both for their offers, before it acks either: `take 1` / `ready` / `mount` to the old S3 process, which holds no
      mount and answers `refused`, then `take 1` / `ready`, which offers the S3 and OCI sockets and, as both are M, the lock;
      `take-fs 1` to the old fs process, which offers the device and the control socket. It checks both offers whole
      (addresses cover its settings, the mount point and paths are its own, each descriptor's kind, the runtime state loads)
      and stores the device. If either check fails it says `nak` to both and the start fails with the reason: both old
      processes serve on as they were, and the hook's step fails with nothing changed.
   3. `ack` to the old fs process: it quiesces (the requests it holds are answered by it; the fills they wait for go to the
      old S3 process's fill socket, which still serves), `done`. Then `ack` to the old S3 process: `done`.
   4. `ready` to the old fs process, `bye`, it exits at once; `ready` to the old S3 process, `bye`: the new process reads
      the device and accepts on the sockets. Its cache is read-only until the old S3 process sends `written` (3.2); a fill
      waits for it (1.5), for the length of that process's cache writes in flight, bounded by `params.fetch_timeout`.
   If the new process dies after the first `ack`, the device is in its store and systemd starts it again; that start finds
   whichever old process still serves (step 2.i of 2.3) and takes from it, or adopts. The hook then removes both
   `run-fs-*` markers, so the old fs unit never starts again, and records `fs_live` as none.
3. **verify**: `/readyz` of the new instance names the target, and the mount is served by it (`attachments` answers on the
   control socket, which moved with the mount).
4. **cleanup**: as today; the old S3 instance has drained and exited.
5. **After the window**, nothing restarts: the operator's step removes the old unit file and its drop-ins, changes the job
   agent's ordering drop-in from `tierstore-fs@a.service` to `tierstore@a.service tierstore@b.service`, runs
   `systemctl daemon-reload`, and removes `fs.toml`.

What runs on the node during the switch: the jobs, reading their trees through the mount; the mount's connection, held by
the old fs process until its exit and by the new process (its store, then itself) from before `ack`; requests between `ack`
and the new process's first read queue in the kernel; S3 clients, whose connections are handed over as in any update.
No job loses its tree: the connection is never closed, and no request is read by two processes or by none.

**Rollback of the switch** (verify fails, or the operator asks): the same steps backwards, with M on both sides. M's
one-process instance also answers `take-fs 1` on `handoff.sock` in the runtime directory while it holds the mount, and binds
the fill socket while it is the writer. Answering `take-fs 1`, it hands over the file system alone: it quiesces it, never
reads the device after `bye`, and goes on serving S3 and OCI, without the exit at once of an fs process that has handed over. The hook lays the `run-fs-<y>` marker and starts `tierstore-fs@<y>` on the old
`fs.toml` (which is removed only at step 5), which takes the mount with `take-fs 1`; then it starts `tierstore@<live>` on the
old file, which asks `take 1` as today and gets the S3 and OCI sockets from a process that no longer holds a mount (one that
still holds the mount refuses a taker that does not ask for it, so the mount is never left behind). As in any hand-over of
today, the one-process instance stays the cache's writer through its drain and answers the old fs process's fills on its
fill socket meanwhile; the old-layout instance takes the lock when it exits.

### 4.3 `mac-build` (macOS)

A Mac has no hand-over today, and its file service unmounts when it stops (`nfs/run.rs`: `Running::leave`; the update hook
boots it out before the main job: `launchd/fs.rs`). So the switch on a Mac is a restart, and it unmounts, as every tierstore
update on a Mac does today. A job that holds a tree on that Mac during the switch fails. The window therefore drains the
Mac first: the job agent takes no new job and its jobs end, as for any Mac update today. After the switch, updates are the
hand-over of 3.4 and no longer unmount (section 4.5).

1. M is on the Mac (two jobs). The Mac is drained.
2. The group's configuration package is updated: `tierstore.toml` with the `fs` interface (1.3), no `fs.toml`, and no
   `fs_launchd_label` in its params. Its hook (M's config hook) sees an `fs` interface in the staged file and a
   `tierstore-fs` job loaded or installed, and: boots the `tierstore-fs` job out and checks that its mount is gone (as
   today); disables it (`launchctl disable system/tierstore-fs`, kept across reboots, so the plist that the installed package
   still has never starts it again); boots the `tierstore` job out and in on the new file, which mounts and serves; verify
   reads `/readyz` and checks the mount is listed.
3. The Mac is let take jobs again.

Rollback: the package's previous files, `launchctl enable system/tierstore-fs`, and the old restart (fs job out, main job out
and in, fs job in), with the Mac still drained. The installer package no longer ships `tierstore-fs.plist`
(komira-ai/tierstore#624); hostkeeper's activate of it removes the one an earlier package installed, since it deletes the
files the old version shipped and the new one does not. Its postinstall does nothing about it.

### 4.4 What changes where

| Package or system | Change | When |
|---|---|---|
| tierstore (release M) | adds the `fs` interface, the one hand-over, the switch in the config hook, the unit and plist of 1.4; keeps the old layout | before any window, by the normal rollout |
| tierstore (release M+1) | deletes the old layout (section 5) | after both windows |
| `all-in-one`'s configuration package | `config.toml` with the `fs` interface; no `fs.toml`; needs M or later | its window |
| `mac-build`'s configuration package | `tierstore.toml` with the `fs` interface; no `fs.toml`; params without `fs_launchd_label`; needs M or later | its window |
| hostkeeper manifest | Linux `run` unchanged (`tierstore@a.service`, `tierstore@b.service`); macOS installer package's params lose `fs_launchd_label` and its payload loses `tierstore-fs.plist` (M+1); the cutover bound (4000 s, which includes the wait for the new store to take the cache lock) and the rollback bound (1500 s) still cover a Linux take (300 s start) and a Mac hand-over (61 s drain, start) | M, M+1 |
| job agent (jobscheduler) | nothing: the mount-provider socket paths (`/var/lib/tierstore/provider.sock`, `/usr/local/tierstore/run/provider.sock`) and the mount roots do not move. On Linux, the ordering drop-in names `tierstore@a.service tierstore@b.service` | 4.2 step 5 |

### 4.5 Updates of a Mac's release after the switch

komira-ai/tierstore#623, #624. Since 0.3.196 `tierstore fs --config` serves nothing and exits 2; the file system of a Mac is
served only by the `fs` interface of the node's one process. The macOS package's manifest still named `fs_launchd_label`, so
an update booted the `tierstore-fs` job out (which unmounts) and in again after the main job served onto a binary that exits
2, reported success, and nothing was mounted. The decision:

- **A Mac that was switched** (its file has an `fs` interface) is updated by the hand-over of 3.4: the manifest names no
  fs job, so between releases that both list `handoff-exec-v1` the hook copies the staged binary and sends `SIGUSR2`. The
  process keeps its pid and its mount, ends its NFS connections as a hand-over does, and executes the new image, which serves
  the same NFS listening socket and control socket. The jobs on the Mac see their reads of the mount pause for the drain and
  the exec (the mount is `hard`, so the clients retry) and go on; nothing is unmounted and their files stay open. Before,
  every update was a restart: a boot-out of the job (the drain of 60 s, then the unmount, up to 120 s) under the running jobs.
- **A Mac that was not switched** (its file has a `mount-provider` interface and no `fs` interface) is refused in `prepare`
  with `mode_unsupported`, and the detail says to switch the node to one process first. The refusal is in `prepare` because it
  runs before `activate` and before anything is recorded, started or stopped: hostkeeper runs `rollback`, which finds
  nothing in flight, and re-activates the previous package, so the Mac stays on its release with its mount served. A refusal
  after the cutover would have stopped the one process that serves the mount. The config hook is not refused: its switch (4.3)
  is the way off the layout.
- **`verify` proves the volume before the commit.** The hand-over's own check is the build gauge, which says the new image
  runs, not that it serves the mount. For a release on launchd whose file has an `fs` interface, `verify` requires the mount
  listed at the interface's mount point (which is the mount provider's root, the loader holds the two equal) and a look-up
  of `<mount point>/trees` that finds a directory: every mount of the file system has that namespace at its root, a plain
  directory that is not mounted lacks it, and the look-up needs the server to answer. A failure is `not_ready` and hostkeeper
  rolls back, which for a hand-over is the hand-over to the previous release by `SIGUSR2`. After the commit `verify` only
  observes the version.
- **`fs_launchd_label` is still read**, because the switch's rollback of a config package names it to go back to the
  two-process layout: a params file that names it is driven as before (the fs job out before the main job, in after),
  except in an update of the release on a node whose file has an `fs` interface. There every step, the boot step and the
  rollback included, leaves the fs job alone whatever the params say: on the four switched farm Macs the update to 0.3.203,
  whose manifest still named the job, failed in `cutover` and again in `rollback` at `launchctl bootstrap` of the disabled
  `tierstore-fs.plist` (`Bootstrap failed: 5`) after the main job served, and its boot step fails the same way. No
  release manifest names it, and the package ships no `tierstore-fs.plist`; hostkeeper's activate removes the file from a Mac
  that has it, and a rollback to the old release lays it down again, where the job stays disabled on a switched Mac.
- **Failure modes.** A mount that is not listed, or has no `trees`, fails `verify` and rolls the update back. On a hard NFS
  mount whose server is gone, the look-up waits until the kernel marks the mount dead (`fs.nfs.dead_timeout`) and then fails,
  so a dead server fails `verify` after that time and not at once. A Mac whose file lacks the `fs` interface stays on its
  release until its config package switches it.

## 5. What it deletes (release M+1)

- **The verb `tierstore fs`** (`server/verbs/fs.rs`, `server/treefs/launch.rs`'s own start, the `[fs]` loader of
  `server/treefs/config.rs`). `tierstore fs` then exits 2 with "`tierstore fs` is no longer a process of its own: the file
  system is the `[[interface]]` of type `fs` of the node's file", so a unit left behind fails loudly and its
  `RestartPreventExitStatus=2` stops it from looping. `tierstore fs attachments` becomes `tierstore attachments --config
  <file>`, which reads the control socket from the `fs` interface (decision 8).
- **`fs.toml`**: the packaging example `packaging/macos/fs.toml` (deleted in komira-ai/tierstore#624), the copies in each
  group's configuration package.
- **The units and jobs**: `packaging/hostkeeper/units/tierstore-fs@.service`; `packaging/hostkeeper/units/tierstore-fs.plist`,
  `packaging/macos-pkg/tierstore-fs.plist` and the installer package's `FS_*` entries
  (`crates/xtask/src/release/macos_pkg.rs`), deleted in komira-ai/tierstore#624.
- **The fill socket** (built by komira-ai/tierstore#523): `server/fill_socket/mod.rs` and `request.rs` (listener, sessions, parser, refusals) and their tests,
  `server/treefs/fill_client.rs` and its tests, the socket parts of `server/config_fill.rs`, `Fronts.fill_socket` and the
  task that serves it. Kept, moved beside the provider listener that uses them: `bind_socket`, `SocketFile`, `pause_if`;
  kept beside the put ingest: `put::chunks`. `local_cache.fill_socket` is read and ignored with a line on stderr (B8: a file
  of today still starts).
- **The second hand-over**: the `take-fs 1` offer and taker (`server/treefs/handover.rs`), the capability `handoff-fs-v1`,
  and `fs.nfs.handoff_*` (the NFS front's `take 1` with `ready` is replaced by 3.4).
- **The hook's second arm**: `update_hook/systemd/fs.rs`, `update_hook/launchd/fs.rs`, `FsPolicy`, `Family::Fs`, the fs
  units and markers ports, `FS_LAUNCHD_PLIST`, `FS_CONFIG_FILE`, `FS_START_TIMEOUT`, the param `fs_launchd_label` (a params
  file that still has it is read as today, where an unknown key is ignored), and the fs details of the reasons. Decoding of
  `fs_live` and `fs_next` stays (3.6).
- **Docs**: the operator pages' sections on `tierstore fs`, `fs.toml`, the fill socket and the second unit
  (`crates/tierstore/docs/configuration.md`, `operations.md`, `update-hook.md`, `metrics.md`), `packaging/README.md` and
  `packaging/macos/README.md`, `docs/ci-and-release.md`'s plist list, and the glossary rows of `docs/design/tierstore-design.md`
  ("`tierstore fs`", "Fill socket"), each in the pull request that changes the code it describes (P10).

Kept: the core, both frontends, the control socket and its protocol, the runtime directory and its formats, the table of
tree numbers, the put directory, the metric names (`tierstore_fs_*`, `tierstore_cache_fill_socket_*`), the mount-provider
interface.

## 6. Build plan

Phase 1 builds release M; phase 2 is M+1. Each pull request keeps every crate it touches at 99% of lines and branches (T1)
and lists its tests by name. The interfaces each lands on exist today or are fixed by this document, so the pull requests of
a phase can be written in parallel; "needs" says which must merge first.

| PR | What | Lands on (interface) | Needs | Tests (each asserts the behaviour it is named for) |
|---|---|---|---|---|
| P1 | The `fs` interface type: params into `FsSettings`, the rules of 1.2, `--check-config`, docs `configuration.md` | `FsSettings` (exists, `treefs/config.rs`); `Interfaces` gains `fs: Option<FsSettings>` | none | `an_fs_interface_makes_the_settings_the_fs_table_made`, `an_fs_interface_outside_a_build_node_is_refused`, `a_second_fs_interface_is_refused`, `an_fs_param_this_release_lacks_is_refused`, `fill_socket_cache_path_and_handoff_socket_are_refused_as_fs_params`, `a_provider_mountpoint_left_out_is_the_fs_mountpoint`, `a_provider_mountpoint_other_than_the_fs_one_is_refused`, `a_put_stage_off_the_cache_volume_is_refused`, `a_runtime_dir_inside_the_local_cache_is_refused`, `a_control_socket_equal_to_the_handoff_socket_is_refused`, `a_file_of_today_without_an_fs_interface_loads_as_today` |
| P1b | `owner = { subids_of, id }` | `params.owner` | P1 | `the_owner_is_the_first_range_of_at_least_65536_plus_the_id`, `a_user_without_such_a_range_is_refused_by_name`, `ids_past_4294967294_are_refused`, `a_range_named_by_uid_is_found` (all over a fixture file, no host file: T12) |
| P2 | The core calls the cache and the tree store (1.5): the core holds `InProcess` and calls it, and no trait stands between them (built by komira-ai/tierstore#523, with the removal of the fill socket) | the calls of 1.5 (exist) | none | each core test run over a real cache; `a_fill_waits_for_the_write_role_then_fills`, `a_fill_still_read_only_after_fetch_timeout_fails_with_eio`, `a_put_reads_the_staged_file_once_and_stores_it`, `the_reports_reach_the_cache_without_a_queue` |
| P3 | The one process starts and stops the `fs` interface: the order of 2.3, the stop order of 3.5 | `serve_front` (exists) | P1, P2 | `a_start_takes_before_it_adopts_and_adopts_before_it_mounts`, `a_fresh_start_finds_a_dead_mount_in_mountinfo_without_touching_it`, `a_start_in_m_with_a_live_old_fs_process_takes_from_it_and_never_detaches`, `a_switch_start_connects_to_both_old_processes_and_takes_the_mount_from_the_fs_one`, `a_running_instance_that_never_answers_fails_the_start_and_detaches_nothing`, `the_fronts_drain_before_the_mount_is_left`; farm integration target (`*_integration`, S-2): `the_mount_reaches_the_host_namespace_from_the_unit` |
| P4 | The one hand-over (3.1, 3.2) and the old-layout sides of 4.2 (`take 1` without `mount`; `take-fs 1` taker and offerer); `handoff-service-v1` | `server/handoff/` frames and `Offer`/`take`; `Hold` (exists) | none for the wire; P3 to wire it into the start | `an_offer_with_the_mount_hands_sockets_mount_control_and_lock_together`, `a_lock_descriptor_of_another_file_is_refused_before_ack`, `a_descriptor_of_the_wrong_kind_is_refused_before_ack`, `a_taker_refuses_an_offer_from_another_user`, `the_old_reads_no_request_after_ack_and_answers_what_it_read`, `the_new_reads_the_device_only_after_bye`, `a_taker_gone_before_ready_leaves_the_old_resuming_and_one_gone_after_ready_does_not`, `a_successor_killed_before_ready_leaves_the_old_serving` (every frame), `a_successor_killed_after_ready_is_started_again_and_adopts_from_its_store`, `the_old_removes_its_store_entry_at_ready`, `the_old_starts_no_cache_write_after_ready`, `the_new_writes_the_cache_only_after_written_or_the_end_of_the_connection`, `a_taker_that_naks_or_goes_away_leaves_the_lock_held` (a third `try_take` gets none), `the_lock_outlives_the_old_process_exit`, `the_old_unlocks_when_its_successor_dies_after_ready`, `an_instance_without_a_mount_refuses_the_mount_line_and_is_asked_again_without_it`, `an_instance_with_the_mount_refuses_a_taker_that_does_not_ask_for_it`, `take_fs_says_bye_and_the_old_fs_process_never_resumes_after_it`; B8: `a_previous_release_taker_skips_the_lock_fuse_and_control_lines_and_gets_the_sockets`, `a_taker_against_a_previous_release_offer_is_refused_the_mount_and_asks_again_without_it` (each replays the previous release's frames, kept as fixtures) |
| P5 | macOS: the hand-over to the next image (3.4); `handoff-exec-v1` | the offer frame of P4 on a `socketpair`; `nfs/run.rs` | P4's frame | `the_next_image_reads_the_offer_from_the_handed_descriptor`, `every_owed_nfs_reply_is_sent_before_the_exec`, `only_the_handed_descriptor_survives_the_exec`, `an_offer_the_new_image_refuses_starts_it_cold_on_the_same_addresses_and_mount`, `the_cache_writes_in_flight_end_before_the_exec`, `the_new_image_takes_the_write_role_before_its_first_cache_write`; macOS integration: `a_launchd_job_keeps_its_pid_and_is_not_restarted_across_the_exec` (a real launchd job sent `SIGUSR2`: same pid, no restart, the new image serves); macOS runs these in `beta.yml`, not in the pull request's checks (B9) |
| P6 | Packaging: the unit of 1.4 (the template and its package copy), the plists, the postinstall, the manifest's comments and bounds | the files | none | `the_unit_sets_no_option_that_makes_a_mount_namespace` (every option of 2.3 and its path options, with a negative case: a copy with `ProtectSystem` added fails, T15), `the_unit_grants_cap_sys_admin_and_no_other` (these two replace `the_fs_template_differs_from_the_s3_template_as_the_design_lists`, whose fs template goes in P9), `the_package_template_is_the_archive_template_but_for_the_binary` (exists), `the_plist_waits_210_seconds_on_exit`, `the_binary_accepts_the_units_command_line_and_the_example_file` |
| P7 | The hook: the gating of 3.6 for both systems, the macOS hand-over arm, the switch and its rollback of 4.2 and 4.3 in the config hook | `update_hook` ports (exist) | P4, P5 capability words | `an_update_without_handoff_service_v1_in_both_is_refused_for_a_node_with_a_mount`, `mode_restart_overrides_the_refusal`, `a_config_update_that_moves_the_mount_point_is_refused`, `the_switch_checks_both_offers_before_either_ack_and_clears_the_fs_markers`, `a_switch_whose_offers_do_not_both_match_naks_both_and_changes_nothing`, `a_switch_whose_new_process_dies_after_ack_is_started_again_and_takes_or_adopts`, `the_switch_rolls_back_mount_first_then_sockets`, `the_one_process_instance_binds_the_fill_socket_while_it_is_the_writer_in_m`, `a_mac_switch_boots_out_and_disables_the_fs_job_before_the_main_job_starts`, `a_mac_update_with_handoff_exec_v1_sends_sigusr2_and_waits_for_the_target`, `a_mac_update_without_it_is_refused` |
| P8 | The deployment's files and steps (sections 1.3, 4.2 step 5, 4.3), in the deployment's own repository | the configuration packages | M released | the deployment's own checks of its files |
| P9 | Phase 2, M+1: the deletions of section 5 | | both windows done | `the_fs_verb_exits_2_and_names_the_interface`, `a_file_with_local_cache_fill_socket_starts_and_says_it_is_ignored`, `a_record_with_fs_live_still_decodes` (the stored-format fixture), `prepare_refuses_a_node_that_still_runs_tierstore_fs`, `attachments_reads_the_control_socket_from_the_fs_interface`; P2's trait is removed (R9). The fill-socket part of this phase (the fill socket, `local_cache.fill_socket` read and ignored, `tierstore fs` serving exits 2) is built by komira-ai/tierstore#523; the rest is not built |

The design documents follow the code: each pull request rewrites the sections of virtual-build-fs.md (5, 6.3, 6.5, 7, 8,
12) and tierstore-handover.md (section 0) that describe what it changes.

## 7. Decisions made here

1. **The process holds `CAP_SYS_ADMIN` for its life** (2.1): it is needed to mount, as the fs unit has it today.
2. **A fresh start reads `/proc/self/mountinfo` and never `stat`s the mount point** (2.3, 3.5): a `stat` of a mount whose
   descriptor is held but not read blocks.
3. **The new reads the device only after `bye`, and the old removes its store entry at `ready`** (3.1): no late `ready`
   lets two processes read the device, and a crash of the old in its drain does not bring it back.
4. **The cache lock passes in the hand-over and the old stops writing at `ready`** (3.2): otherwise the file system's fills
   would wait for the old process's whole drain. Nobody unlocks a shared lock, but the old once its successor has died and its own writes in flight have ended.
5. **One hand-over on `server.handoff_socket`, made of the two protocols' steps**, additive on the wire (3.1).
6. **macOS hands over to the next image of the same process** (3.4): the one launchd job keeps one pid, and an exec keeps
   the descriptors and the lock.
7. **Two releases**: M carries both layouts so that M's code is on both sides of every switch and its rollback; M+1 deletes
   (4.1).
8. **`tierstore attachments --config <file>`** replaces `tierstore fs attachments`: the operator's look at the attachments
   stays, under the one process.
9. **The `fs` params refuse unknown keys**, as every interface's params do, where `[fs]` warned: a new table has no old
   files to keep loading.
10. **`owner` from subordinate ids** (1.2): without it the one file is per node on Linux.
11. **The fill-socket permit keys keep their names** (1.2): they still bound the file system's fills.
12. **The unit takes the file system's restart values** (`RestartSec=2`, 5 in 60 s) (1.4): a hard-waiting mount needs a fast
    return, and a process that cannot return must let the kernel abort.
13. **`RestrictNamespaces=yes` is kept** (2.2): it is a system-call filter, and the process makes no namespace.
14. **`DevicePolicy=closed`** replaces `PrivateDevices` (2.2): the same device fence without a namespace.
15. **An update that cannot hand the mount over is refused, not restarted**, unless `mode = "restart"` (3.6): a restart drops
    the mount under running jobs.
