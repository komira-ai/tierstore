# Build inputs as a filesystem: lazy, writable, digest-addressed trees served by tierstore

Status: **proposal, for the CEO's review; nothing here is built, and no code changes come with this document.** The farm's build executor (the service that runs build actions, called "the executor" below) keeps
copying (or linking) each action's inputs into a scratch directory and removing it afterwards. This document designs, in
parallel, how tierstore could serve those inputs as a filesystem instead, so that a job scheduled by jobscheduler reads its
inputs without a copy, fetches only what it opens, may write anywhere in them privately, and runs on Linux and on macOS.
Section 11 is the measurement plan against today's path on real builds run by the executor; section 12 is the phased plan, which starts
with probes and builds nothing before their answers. Section 13 sets out the decisions the CEO is asked to make, each with
its options and a recommendation.

**What this document starts from.** An earlier proposal, built on researched findings, preceded it and recommended: one tierstore
filesystem core in Rust with thin frontends (FUSE with passthrough on Linux, an NFSv4.0 loopback server on macOS, FSKit
later); the per-job writable layer, copy-up included, in that core; pinning of a job's tree in the local cache; and three
farm-Mac probes before anything else. This document does not redo that research. It takes its findings (cited here to
their public sources), checks the items it left unconfirmed (section 3.2), designs the parts it left open (copy-up and
whiteouts, 6.6; the live hand-off of FUSE and NFS sessions, section 7; pinning, 6.5), and, where it disagrees, says so and
why (section 3.1). The one place it disagrees is the Linux writable layer: this document recommends the kernel's overlayfs
there and the core's writable layer on macOS, and puts the choice to the CEO (decision C4).

**Claim tags.** **Read in code** means read in the named repository (tierstore at this commit; the public
repositories of the build executor and of jobscheduler at their main branches when this was written), with the file named (paths in the executor's repository leave out the prefix its crate names share). **Sourced**
means a public source listed at the end, cited as [S*n*]. **Inferred** means reasoned here from those, and says how.
**Unverified** means not checked against a primary source; each such claim is a probe or spike of section 12 or is not
load-bearing. **Unmeasured** means a performance expectation with no measurement behind it yet; section 11 exists to
replace every one of them. Probes and spikes are named MP1 to MP6 and SP1 to SP5, so they are not confused with sources
[S*n*] or with the rules S1 to S4 of AGENTS.md.

**Terms.** The defined terms below avoid the words this repository's glossary already defines (entry, lease, hold, slot, registration, node id); where such a word appears in its ordinary sense (a directory entry, the cache lock's holder, an NFS lease) the sense is the ordinary one. A **frontend** here is a protocol adapter of `tierstore fs`, not one of the server's S3 or OCI **fronts**.

| Term | Meaning here |
|---|---|
| **tree** | An immutable directory tree of content-addressed files, named by the SHA-256 of its root **tree manifest** (6.1) |
| **tree manifest** | The JSON listing of one directory of a tree (6.1) |
| **core** | The protocol-independent filesystem of `tierstore fs`: trees, views, copy-up, the handle scheme; the FUSE and NFS **frontends** translate a protocol into its calls (6.2) |
| **attachment** | A client's statement that a run will use a tree on this node: `attach <digest> for <run>`, ended by `detach <run>` (6.5) |
| **view** | A run's writable, private view of an attached tree: the tree below, the run's **upper** directory above (6.6) |
| **copy-up** | Copying a file (or creating a directory) of the tree into the upper directory the first time the run changes it (6.6) |
| **whiteout** | A record in the upper directory that a name of the tree has been deleted in this view (6.6) |
| **pin** | A tree's claim on the node's cache: the blobs a pinned tree has opened are not evicted while it is attached (6.5) |
| **tree number** | A small number `tierstore fs` gives a tree digest (or a view) the first time it is attached on a mount (6.3) |
| **inode number** | The 64-bit number the kernel names a FUSE inode by (FUSE calls it the node id); the NFS file handle carries the same number (6.3) |
| **run directory** | The per-run directory a job runtime makes and removes (the executor: the lease directory; jobscheduler: the lease dir under the scratch root) |
| **source fill** | A copy of one blob into the node's tierstore cache, from a key and an offset that the tree manifest names (6.2) |

## 1. The problem

### 1.1 What happens today (read in code)

The executor prepares each action's **input root** (a REAPI `Directory` tree, SHA-256 digests) in the action's run directory before
the action starts, and removes the directory after it ends (the executor's `crates/daemon/src/tree.rs`,
`crates/driver-container/src/runtime.rs`, `crates/driver-container/src/remove.rs`). There are three ways in:

1. **Plain copy.** Every file is fetched from the CAS and written; a second path with the same digest is copied from the
   first (`tree.rs`: `materialize`, `write_files`, `write_blob`).
2. **Node cache, hard links.** With `--cache-dir`, each file is a hard link to an entry of a per-node NVMe file cache (on
   macOS a `clonefile`), so the bytes are written once per node and each action pays one link per file
   (`crates/daemon/src/cache/lease.rs`, `cache/clone.rs`). Inside the container such a file shows the overflow owner,
   mode 0444 or 0555, a fixed mtime, and one inode for identical inputs (the executor's `docs/design/worker-cache.md` section 6.2).
3. **Shared trees.** A `Directory` with at least 256 files (a toolchain) is built once per node as a tree of hard links
   and bind-mounted read-only into each action that has it, at most 8 per action; the action's input root holds an empty
   directory there, and an action that writes into one fails (the executor's `docs/design/fast-sandbox.md` section 4,
   `crates/daemon/src/trees/`). On macOS the tree is one `clonefile` of the directory instead, and is writable
   (fast-sandbox.md section 8).

On Linux the action runs in a rootless `crun` container whose input root is an overlayfs: lower is the materialised input
root, upper is a per-run directory, and outputs are read from the upper layer only, each hashed once (the executor's
`crates/driver-container/src/outputs.rs`). Before the run, the executor moves each input that an output path names from the
input root into the upper layer, and makes the working directory and the outputs' parents there (fast-sandbox.md section
3.1). The action runs as container user 1000 (`crates/driver-container/src/container.rs`: `ACTION_ID`).

jobscheduler's own `crun` path has the same overlay: `/work` is an overlayfs whose lower is an empty directory today
(jobscheduler `crates/js-contain/src/oci/spec.rs`, `crates/js-contain/src/container.rs`). Its wire can name a
digest-addressed input, `InputRef::Stage { stage_id: "sha256:<hex>", dest, placement: ReadOnly | Writable }`, but only
inside a `run` or `native` body; the Linux `crun` runtime refuses both bodies today (`run-unsupported`,
`native-unsupported`), `dest` may not be empty, and `stage_id` names a tree of jobscheduler's own tree store
(`crates/js-agent-wire/src/native.rs`, `crates/js-agent-wire/src/run.rs`, `crates/js-agent/src/lifecycle/runtime.rs`,
`crates/js-treestore/src/lib.rs`). On macOS a job runs under `sandbox-exec` as a fresh per-run user, its inputs are copied
or cloned into its home, reads are open by default and writes are allowed only under the run's home, and the agent refuses
`Stage` inputs ("this agent keeps no stage trees", `crates/js-agent/src/native/mod.rs`,
`crates/js-agent/src/native/profile.rs`).

So "copy-to-scratch" is, in the farm's configuration, **link-to-scratch plus shared read-only trees**: every input file
costs at least one directory entry made and removed per action, a shared tree costs one hard link per file per node, and
every byte of every input is on the node's disk before the action starts, whether or not the action reads it.

### 1.2 What is not known (and why this document ends in a measurement plan)

- **How much of its input root an action opens.** The executor's `inputs:` log line has the file count (`files`, `tree_files`)
  and the bytes fetched from the CAS (`fetched_bytes`), and `inputs phases:` has `walk_ms`, `link_ms`, `trees_ms` and
  `chown_ms` (the executor's `crates/daemon/src/cache/fill.rs`, `crates/driver-container/src/input_phases.rs`). Nothing
  records the input root's total bytes (the executor's `docs/design/ui-data.md` lists input bytes as not recorded) or which inputs an
  action opens. A lazy filesystem wins only where actions open a small part of what they are given, or where much of what
  they are given is not yet on the node.
- **What setup costs today on the shipped backend.** The executor's fast-sandbox.md section 9: "No benchmark of the shipped backend
  is in this repository."
- The executor has already weighed a lazy input root and rejected it **for now**: "FUSE/NFSv4 lazy input root ... Revisit only if
  actions read a small fraction of huge input roots" (the executor's `docs/design/worker-cache.md` section 11). That condition is
  the first thing section 11 measures.
- **Where laziness loses.** Buildbarn reports its virtual filesystem running actions at over 95% of the speed of a fully
  downloaded input root in aggregate, and slow on workloads with poor cache hit rates, because each miss is a
  synchronous round trip; it added a cache of what similar actions read, to prefetch it [S7]. So the plan reports tails,
  not only medians, and keeps a prefetch arm.
- **macOS has no published benchmark** of an NFS loopback or FSKit build filesystem against copies [S20, S25]; the probes
  of section 12 are the first numbers.

### 1.3 What a filesystem could change

- Setup becomes one request to tierstore, independent of the number of files.
- Bytes are fetched when a file is first opened, so an action that opens a small share of a large toolchain fetches that
  share on a cold node (unmeasured: no share is assumed).
- A node's disk holds each blob once, in tierstore's cache, and with passthrough the kernel's page cache holds it once,
  shared by every job (section 6.9).
- Teardown removes only what the action wrote.

And what it costs: tierstore becomes part of every running job's filesystem. Its crash or update is now a job's problem,
unless the design makes it not one (section 7).

## 2. Goals and non-goals

Goals:

- **Lazy data.** A file's bytes are fetched from tierstore's tiers on the file's first open, not before the job starts.
- **One tree, one name.** A job's inputs are immutable trees addressed by digest. A job sees its trees and nothing else of
  the store.
- **Writable to the job, private to the job.** A job may create, change and delete files anywhere in its view, as it may
  in a copy, and no other job sees it.
- **One core, thin frontends.** The filesystem's logic is written once; FUSE and NFS (and later FSKit) only translate.
- **No job fails because tierstore was updated.** A live update hands the filesystem session over, as the update hook
  already hands over listening sockets.
- **No job fails because tierstore's filesystem process crashed and came back**; if it does not come back, jobs fail with
  an error the job runtime can tell from a build error.
- **Linux first, macOS with a stated route.** The farm's kernel is 6.17 (as its operators state; checked on a node in SP1).
- **Measured against today** before anything ships to the farm (section 11), with today's path kept as the kill-switch.
- **tierstore stays generic.** It knows trees of content-addressed files, not REAPI, the executor or Buck2.

Non-goals:

- Changing the executor now. The executor keeps its materialisation; this design ends where the executor and jobscheduler would call it.
- A per-job disk quota (CEO, 2026-11-09: whole-node pool; section 6.9).
- Serving a job's writes back as a new tree. Outputs are collected as today (6.6).
- Windows.
- A network filesystem for remote clients: the filesystem is local to a node, for the jobs on that node.
- FUSE inside the job's container. No job gets `/dev/fuse`.
- Building on an existing S3 filesystem (Mountpoint for S3, JuiceFS, s3fs, goofys and the like): each is built for a
  mutable, key-addressed namespace, none gives copy-on-write over an immutable base, and each needs macFUSE on macOS
  [S26, S27]. Their lessons are kept: metadata from an in-memory manifest with long timeouts, an LRU with a free-space
  floor (which tierstore's cache already is), a warm-up before the job starts, and clear early errors for what is not
  supported.

## 3. What this document was handed, evaluated

### 3.1 The framing and the proposal, point by point

| Point | Verdict | Why |
|---|---|---|
| One directory per input tree, addressed by digest (`/tierstore/trees/<digest>/...`), never the whole store | **Kept** | A tree is immutable, so the kernel may cache everything below its root for as long as it likes (6.3). "Never the whole store" is enforced by attachment: a digest no client has attached is not there (6.3). |
| One protocol-independent core with thin frontends, as Buildbarn split its FUSE code into a virtual layer plus FUSE and NFS adapters [S5] | **Kept** | 6.2. The core is Rust (decision C2). |
| Resolvable handles: an immutable file's handle carries what is needed to serve it, so the server keeps no per-file state [S5] | **Kept, extended** | Every inode number and NFS handle is derivable from the tree's manifests and a small table (invariant I1, 6.3), and so is every handle of a view (6.6). That is what makes a crash and an update survivable (section 7). |
| Lazy Merkle expansion, one directory level on first access [S6] | **Changed: metadata eager per tree, data lazy** | A directory expanded on first access gets an inode number that depends on the order of lookups, which a restarted process cannot recover. Flattening a tree when it is first attached keeps numbers derivable; its cost is spike SP4 (6.3, D7). |
| Pin the job's tree in the local cache; the evictor skips pinned; refcount per job tree, not per file | **Kept, designed** | Pins are kept by the cache's single writer, per attached tree, over the blobs the tree has opened, with a cap on pinned bytes (6.5). Correctness never depends on them: an evicted blob is fetched again (6.4). |
| The per-job writable layer, copy-up included, lives in the core, so no overlayfs is needed | **Kept for macOS; for Linux put to the CEO (C4)** | On macOS there is no overlay, so the core's writable layer (6.6) is the design. On Linux a kernel overlayfs over a **read-only** FUSE tree keeps every job's writes on the node's disk, outside tierstore, in the directory both job runtimes already collect outputs from: tierstore then holds no job data, a crash loses nothing, and the FUSE server stays read-only, which keeps its crash and update simplest (section 7). The core's writable layer works on Linux too (writes to upper files go by passthrough), so this is a choice of simplicity and risk, not of possibility. |
| Linux: FUSE with passthrough (6.9+), `max_background` raised to about 1024 (the default of 12 wedged busy Buildbarn workers [S28]) | **Kept** | Passthrough needs `CAP_SYS_ADMIN` in the initial user namespace [S1], which the network-facing tierstore process must not have, hence a separate filesystem process (6.2, C5). The kernel binds an inode to one backing file and forbids mixing passthrough and cached opens of one inode [S2], which shapes 6.4. |
| macOS: one NFSv4.0 loopback server, mounted once per node at a fixed path, a per-job view per run, Seatbelt `(subpath ...)` isolation | **Kept, with two additions and one open choice** | 8. Additions: the server also checks the caller's uid against the run (jobscheduler gives each run its own user), and a view's handles are derivable so a restart is a pause. Open choice: NFSv4.0 (Buildbarn's, compound operations, open state to recover) or NFSv3 (stateless, chattier, a mature Rust crate); probe MP3 measures both, and C2 asks the CEO to let the version follow the probe. |
| FSKit later, as a third frontend behind a flag | **Kept (C3)** | No passthrough for file-backed volumes, interactive per-user enablement, and Apple's own statement that it is not yet suited to a network-backed filesystem [S20, S25]. |
| APFS `clonefile` as the fallback with no server | **Kept** | It is the executor's macOS path today and stays the default and the kill-switch (8). |
| A live update hands over the FUSE session as the hook hands over listening sockets | **Kept, plus crash, plus NFS** | 7.1 to 7.3 for FUSE (descriptor store, recorded `INIT`, `FUSE_NOTIFY_RESEND`); 7.5 for NFS: new on macOS, which has no hand-over today; derivable handles and an on-disk duplicate request cache make a restart a pause; NFSv4.0 state is handed over on an update, and after a crash reclaims are answered `NO_GRACE` and the client opens again. |
| Output capture by hashing in place, uploading only when no writer is open [S6] | **Kept in substance** | The runtimes' existing collectors read outputs once from the upper directory after the job's processes are gone, so no writer is open (6.6). tierstore does not store outputs (D4). |
| Disk accounting on a whole-node pool | **Kept** | 6.9. |

### 3.2 The proposal's unconfirmed items, checked

| Item | Status now |
|---|---|
| Does the `fuser` crate expose FUSE passthrough? | **Yes**: since 0.16, and `BackingId::into_raw` since 0.18; it does not send `FUSE_NOTIFY_RESEND`, and `Session::from_fd` cannot adopt an initialised connection (read in its source and changelog [S4]). 6.2 says what is added. |
| FUSE descriptor hand-over on Linux | **Designed (7.1 to 7.3), from the kernel source**: the connection lives while any device file of it is open; `FUSE_NOTIFY_RESEND` requeues what a dead server read; a released clone fails its requests [S2, S3]. The run of it is spike SP2. |
| NFSv4.0 open-state recovery after a server restart | **Client side confirmed in source**: Apple's current NFS client reclaims opens with `CLAIM_PREVIOUS` after a stale client id (`kext/nfs4_vnops.c`, `open_reclaim` [S24]). Whether the whole sequence works against our server, and how long new opens wait in the grace period, is probe MP3. 7.5 hands NFSv4.0 state over on an update, and after a crash keeps no grace period: a server with no stable record must answer reclaims `NO_GRACE` (RFC 7530 [S33]), and the client opens again. |
| Licence and reuse of Buildbarn's NFS and XDR code | **Apache-2.0**, both `bb-remote-execution` and `go-xdr` (their repositories' licence records [S5]). A Rust port is allowed with the notice kept; Go code is not linked into tierstore (C2). |
| Buildbarn's copy-up of pre-existing inputs | **None**: its CAS files refuse an open for write or truncate and a size change with an access error, and its write path panics if reached ("Files created by this factory are entirely immutable") [S6]. Copy-up is ours (6.6); EdenFS's materialize-on-write is the closer template [S9]. |
| How Seatbelt treats paths on an NFS mount | **Not checkable without a Mac**: probe MP2 (`(debug deny)` trace). Until it answers, the server's own uid check (6.8) is the isolation that does not depend on it. |
| Whether `mount(2)` of NFS over a Unix socket needs root, and whether a Unix socket or loopback TCP works on current macOS | **Probe MP1**. EdenFS reports the client buggy over Unix sockets [S10]; a report of the Unix-socket mount failing on macOS 26 is unconfirmed [S29]. |
| FSKit: a LaunchDaemon mounting with no user logged in, a device manager pre-approving the extension, sockets from the extension, `fskitd`'s user | **Deferred with FSKit (C3)**: probe MP6 only if C3 is revisited. |
| Mountpoint for S3 being Linux-only | Not pursued: no S3 filesystem is a base (non-goals). |

## 4. Options

### 4.1 Linux

| | L1: today (copy / link / shared trees) | L2: FUSE writable view in the core (Buildbarn shape, the proposal) | **L3: kernel overlayfs over a read-only lazy FUSE tree (recommended, C4)** | L4: composefs (EROFS metadata, overlay data-only lower over the local cache) | L5: EROFS over fscache on-demand (Nydus) |
|---|---|---|---|---|---|
| Lazy data | No | Yes | Yes, per file | No: composefs reads objects from a local directory and has no fetch on demand [S15] | Yes, by block range [S16] |
| Setup per action | One link (or copy) per file; one bind per shared tree | One attachment, with one host-wide mount | One attachment; the overlay mount the runtime makes today | An EROFS image per tree, fetch missing blobs, mount | An image per tree, mount |
| Metadata path | Native | FUSE on a miss of the kernel's caches; the writable parts need short timeouts or explicit invalidation | FUSE on a miss of the kernel's caches only; the tree is immutable, so cached for the longest time the protocol allows, absent names included (6.3) | Native (kernel EROFS) | Native |
| Data path, blob local | Native | Passthrough to the cache file for tree files and to the upper file for written files [S1] | Passthrough to the cache file; written files are native in the overlay | Native | Native (cachefiles) |
| Where a job's writes live | Upper dir on disk | Upper dir on disk, managed by the core (6.6) | Upper dir on disk, managed by the kernel, as today | Upper dir on disk | Upper dir on disk |
| tierstore crash | Nothing | Lookups and opens pause, then resume, with the core's view state on disk in order (6.6, 7.2) | Lookups and opens pause, then resume (7.2); no write is ever in tierstore | Nothing at run time | Reads of missing ranges fail; restart not documented |
| Live update of tierstore | Nothing | Hand over the descriptor; view state is on disk (7.3) | Hand over the descriptor; the state is derivable (7.3) | Nothing | Not documented |
| Privilege | Rootless | Mount and passthrough need `CAP_SYS_ADMIN` | Same, in a process with no network listener | Root mount: data-only lower layers need trusted xattrs [S17] | Root (`/dev/cachefiles`) |
| Stacking depth | 1 (overlay) | 1 | 2: the FUSE mount declares depth 1 for passthrough, the overlay adds 1, and 2 is the kernel's maximum [S1, S17] | 2 | 2 |
| New code on the job's path | None | The core's writable layer: create, rename, link, truncate, chmod, whiteouts, copy-up, all through FUSE | A read-only filesystem | Image builder | Image builder, cachefiles daemon |
| Verdict | The baseline and the kill-switch | The macOS design; the Linux alternative (C4) | **Recommended for Linux** | Not lazy; the fallback if section 11 finds laziness does not pay (D8) | Deprecated in the farm's own kernel: `EROFS_FS_ONDEMAND` is marked deprecated and scheduled for removal [S16] |

### 4.2 macOS

macOS has no overlay: union mounts are documented as unsupported, `nullfs` is not in Darwin, APFS snapshots are read-only
and need an entitlement, library interposition is stripped from hardened and system binaries, and Endpoint Security can
allow or deny but not redirect [S30]. So a writable lazy view must be served by a filesystem
process, or the tree must be copied.

| | M0: today (`clonefile` of trees, copies) | **M1: one NFS loopback mount per node, writable views in the core (recommended)** | M2: FSKit module | M3: macFUSE / FUSE-T |
|---|---|---|---|---|
| Lazy | No | Yes | Yes | Yes |
| Writable | Yes (copy-on-write clone [S21]) | Yes: the core's views (6.6) | Yes | Yes |
| Kernel side | APFS | Apple's NFS client; no kext | FSKit: from macOS 15.4, one resource per volume; URL resources from macOS 26 [S18] | macFUSE: kext needs approval and reboot, or its FSKit backend [S19]; FUSE-T: an NFSv4 server under a non-permissive licence [S14] |
| Read path | Native | Every read that misses the client's cache is an RPC to `tierstore fs`; no passthrough | Every lookup, directory read and read that misses the cache crosses XPC to the module (a directory read about 120 µs against a trivial module; higher CPU than macFUSE reported [S20]); kernel-offloaded I/O needs a device node [S20] | FUSE requests; FUSE-T as NFS |
| Install | Nothing | A process; one mount by root at boot | An app extension, enabled per user and interactively in System Settings, not globally [S18, S25] | Kext approval, or a non-permissive licence |
| Crash | Nothing | A hard mount waits for the server [S22, S10]; derivable handles and the client's reclaim make the return a pause (7.5, MP3) | Not documented | Not documented |
| Verdict | Default and kill-switch until M1 is measured | **Recommended**, gated by probes MP1 to MP5 | Later, behind a flag (C3) | Rejected: licence (FUSE-T), or a user-enabled extension or kext (macFUSE) |

## 5. The recommendation in one picture

```
                         tierstore fs  (one process per node; Rust; no network)
 ┌───────────────────────────────────────────────────────────────────────────────────────────────┐
 │ core                                                                                          │
 │   trees:   tree manifests -> flattened, derivable inode numbers (I1)            read-only     │
 │   views:   tree + upper directory: copy-up, whiteouts, derivable handles (6.6)  macOS (and C4) │
 │   opens:   fill on first open; passthrough backing ids (Linux)                                │
 │   control: attach / detach (Unix socket);  state in a runtime directory (7.1)                 │
 │ frontends: FUSE + passthrough (Linux)  |  NFSv4.0 or v3 on loopback (macOS)  |  FSKit (later) │
 └────────────┬───────────────────────────────────────────────────┬──────────────────────────────┘
              │ /dev/fuse  or  NFS                                │ fill socket (Unix): fill, touch, pin, unpin
              ▼                                                   ▼
  Linux job: overlayfs /work = upper (run dir)            tierstore (build node, as today)
             over /run/tierstore/fs/trees/<digest>          [local_cache]: one writer; pins; LRU
  macOS job: /private/var/tierstore/fs/views/<run>          upstream: shared tiers (S3)
             (Seatbelt: own subpath only; server checks uid)
```

In words: a client (jobscheduler's agent, or the executor's daemon for as long as the executor schedules its own runs) asks `tierstore fs`
to **attach** a tree by its digest for a run. `tierstore fs` reads the tree's manifests through the node's tierstore cache
and makes the tree appear on its one node-wide mount. On Linux the job runtime gives `trees/<digest>` to the job's
container as the overlay's lower layer, exactly where it gives a materialised input root today. On macOS `tierstore fs`
also makes a **view** for the run, `views/<run>`, writable through the core, and the runtime starts the job there. The
first open of a file asks the node's tierstore to fill that blob into its cache; on Linux the open is then passed through
to the cache file, so the job's reads never visit `tierstore fs`. When the job ends, the runtime collects outputs from the
upper directory and detaches.

## 6. The design

### 6.1 Trees and tree manifests

tierstore does not read REAPI. A tree is described in tierstore's own format, so any client can use it, and so the
location of each blob, which only the client knows, can be part of it.

A **tree manifest** is the listing of one directory: a JSON object, version 1, with the directory's children sorted by
name (bytewise), each name unique, none empty, `.` or `..`, none containing `/` or NUL, and none beginning `.wh.` (a view's whiteout prefix, 6.6; refused by name,
`unsupported entry name`):

```json
{"v":1,"children":[
  {"name":"bin","kind":"dir","sha256":"<64 hex of the child tree manifest>","size":812},
  {"name":"cc","kind":"file","exec":true,"sha256":"<64 hex>","size":1048576,
   "source":{"key":"cas/d/n7/b2/0000000000000041","offset":524288}},
  {"name":"empty.h","kind":"file","sha256":"e3b0c442...","size":0},
  {"name":"lib","kind":"symlink","target":"bin"}
]}
```

- The **digest** of a tree manifest, and so of the tree whose root it lists, is the SHA-256 of its bytes as stored. Two
  encodings of one tree are two trees; that costs a cache miss and nothing else, so no canonical form is needed.
- A **source** says where the blob's bytes are in tierstore: an object key and an offset, the length being `size`. The executor's
  blobs live inside packs, at a key of the form `cas/d/<node>/<boot>/<16 hex>` and an offset (the executor's
  `crates/pack/src/keys.rs`; the location record is `Location { pack_key, offset, length }` in
  `crates/pack-index/src/lib.rs`), so the source is how tierstore reads one without knowing the executor's index. With no
  `source`, the blob is the whole object `cas/l/<sha256>`, the content-addressed key the executor writes large objects to (best
  effort, so a manifest writer that omits `source` must know the object is there: the executor's `crates/front/src/large_backfill.rs`).
  A file of size 0 has no source.
- Child tree manifests are fetched the same way, from `cas/l/<sha256>`: a tree manifest is itself a content-addressed
  blob, written by the client with a create-only put (a content-addressed key is never overwritten).
- No other fields in version 1. A reader ignores unknown fields; an unknown `kind`, or a `v` it does not know, makes the
  attachment refused by name (`unsupported entry kind`, `unsupported manifest version`), so a release never serves a tree
  it cannot render: leaving a file out of a build's inputs would be a wrong answer, not a degraded one. Section 10 says why
  this refusal is the one exception to additive change, and how the layout of inode numbers is versioned.
- tierstore never deletes a tree manifest or a blob; the client that wrote it does, and must not delete one that an
  attached tree uses (6.5 says what happens if it does).
- Limits, each refused by name: at most 2^24 - 1 entries below a tree's root, counted across all its directories (`tree
  too large`; the root makes 2^24 inodes, 6.3); a tree manifest at most 64 MiB; depth at most 4096.

Child manifests are separate objects so that a shared subtree is one set of manifests written once and read once per
node, whatever the number of action input roots that contain it. Only the directories on the path from a changed file to
the root are new per action.

### 6.2 The core, the processes and their privileges

**The core** is a Rust library module of tierstore with no protocol in it. Its calls are the filesystem's operations on
inode numbers (lookup, getattr, readdir, open, read, release, and, for views, create, mkdir, unlink, rmdir, rename, link,
setattr, write), answered from trees (6.3) and views (6.6). Each frontend decodes one protocol into those calls: the FUSE
frontend through the `fuser` crate on Linux, the NFS frontend on macOS (C2), an FSKit frontend later (C3). Buildbarn's
split of its FUSE code into a virtual layer plus FUSE and NFS adapters is the precedent [S5]. The core has more than one
frontend from its first macOS release, so it is not a single-implementation abstraction (R9).

Two processes on a build node, one binary, one package:

- **`tierstore`**, the build-node instance of today (`[local_cache]` in front of an upstream). It stays exactly as
  hardened as it ships (read in code: `packaging/hostkeeper/units/tierstore@.service`: `User=tierstore`,
  `PrivateDevices=yes`, an empty capability set). It gains a **fill socket**: a Unix socket in its runtime directory, mode
  0600, its own user only, peer credentials checked as the hand-off socket checks them (`server/handoff/`). It accepts:
  - `fill <sha256> <size> from <key> @ <offset>`: fetch that range from upstream, check that it hashes to `<sha256>`,
    and publish it in the cache under the key `_tiering/fill/<sha256>`, then answer `ok`, or the reason it did not
    (`upstream <status>`, `mismatch`, `no space`). `_tiering/` is a reserved prefix (`journal.rs`: `RESERVED_PREFIXES`)
    that every front refuses to clients and the cache refuses to read for them (`local_cache/hit_tests.rs`:
    `a_read_of_a_reserved_key_is_refused_before_the_upstream_is_asked`), so a source fill is never visible to an S3
    client, on any release. A request without `<size>` and source (a tree manifest, whose root size the attaching client
    does not know) fills the whole object `cas/l/<sha256>`, up to the 64 MiB manifest limit.
  - `touch <sha256> ...`: mark those blobs used now, for the cache's recency. Not answered; best effort.
  - `pin <mount id> <tree> <sha256> ...`, `unpin <mount id> <tree>`, `pins-live <mount id> <trees>`: section 6.5. Each
    is answered.

  The ranged fill of a blob inside a larger object is new cache code (the cache fills whole objects by key today,
  `local_cache/fill.rs`). Fill-socket requests have their own pool of permits (`local_cache.fill_socket_concurrency`,
  default 16) and **wait** for one, up to `fs.fetch_timeout`; they never take the S3 path's `max_concurrent_fills`
  permits, which skip rather than wait (`local_cache/mod.rs`). The fill socket is bound only by the instance that holds
  the cache lock (`local_cache/hold.rs`): a read-only instance fills nothing, so during a hand-over of this process the old
  instance keeps answering fills through its drain, and the new one binds the socket in the tick in which it takes the
  lock (7.3).
- **`tierstore fs`**, new, a subcommand of the same binary with its own unit, `tierstore-fs@.service`, alternating
  instances `a` and `b` as the S3 unit does (on macOS, a second launchd job). It runs as the same user as the cache, so it
  can read the cache's files. On Linux it holds `CAP_SYS_ADMIN` as its only capability (to mount, and for passthrough's
  `FUSE_DEV_IOC_BACKING_OPEN`, which checks `CAP_SYS_ADMIN` in the initial user namespace [S1]); on macOS it needs no
  privilege, because root mounts its NFS export at boot (8). It has **no network**: on Linux `RestrictAddressFamilies=AF_UNIX`;
  on macOS its NFS listener is bound to loopback (or a Unix socket, MP1) and it makes no outbound connection. It talks to
  the node's tierstore over the fill socket only, and opens the cache's files read-only, at the path the cache's layout
  gives the key `_tiering/fill/<sha256>` (`local_cache/layout.rs`: `name_of`, `data_path`).

The Linux unit of `tierstore fs` must make its mount in the host's mount namespace, so that the job runtime and the next
instance see it. Every option of `tierstore@.service` that gives a service a mount namespace of its own, from which a mount
does not reach the host [S11], is therefore left out: `PrivateDevices`, `PrivateTmp`, `ProtectSystem`, `ProtectHome`,
`ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs`, `ProtectControlGroups` and `RestrictNamespaces`;
`PrivateMounts` is not set. Added: `DeviceAllow=/dev/fuse rw`; `CapabilityBoundingSet=CAP_SYS_ADMIN` and
`AmbientCapabilities=CAP_SYS_ADMIN`; `SystemCallFilter=@system-service @mount`; `Type=notify`, `NotifyAccess=main`,
`FileDescriptorStoreMax=1`, `FileDescriptorStorePreserve=restart` (7.2); `StartLimitIntervalSec=60` and
`StartLimitBurst=5` (the bound of 7.2); `RuntimeDirectory=tierstore-fs` with `RuntimeDirectoryPreserve=yes` for its
state (7.1) and its control socket. Changed: `RestrictAddressFamilies=AF_UNIX` (the S3 unit also allows `AF_INET` and
`AF_INET6`). Kept: `User=tierstore`, `NoNewPrivileges=yes`, `MemoryDenyWriteExecute=yes`, `UMask=0077`, and the
template's instance marker and restart settings. The pull request that adds the unit lists its diff against
`tierstore@.service` line by line; spike SP1 checks which of the left-out options a host-visible mount really needs, and
that the mount survives a restart of the unit and is seen by the agent. What each left-out option protected, and what an
attacker gains without it, is in C5.

**Why two processes**, and not a front inside the one (the `type = "fs"` interface the configuration reserves,
`crates/tierstore/docs/configuration.md`): the S3 process parses requests from the network and responses from upstream;
giving it `CAP_SYS_ADMIN` would give anyone who finds a bug in those parsers the right to mount filesystems on the host.
`tierstore fs` parses FUSE requests from the kernel and tree manifests that clients wrote; manifests are hash-checked
before they are parsed and are size, depth and entry limited (6.1), and the process has no network and writes only its
runtime directory and the views' upper directories. It is the one place where a manifest bug becomes a bug in a process
that can mount; that trade is decision C5. This supersedes the `fs` front sketched in
`docs/design/pluggable-interfaces.md` section 4, which put the mount in the S3 process and its descriptor in the S3
hand-off; nothing relies on that sketch, and if this design is accepted the pull request that adds `tierstore fs`
rewrites that paragraph and the `"fs"` row of `configuration.md` (P10).

**The FUSE protocol** is spoken through the `fuser` crate (MIT; kernel passthrough since 0.16, `BackingId::into_raw`,
which lets a backing id outlive the process that made it, since 0.18 [S4]). Our code adds no `unsafe` (R2): the mount is
`rustix::mount`, descriptor passing is the `rustix` code the hand-off already uses, and `FUSE_NOTIFY_RESEND` is a 16-byte
header written to the device. Two things `fuser` lacks (read in its source and changelog [S4]): it cannot send
`FUSE_NOTIFY_RESEND`, and `Session::from_fd` waits for `FUSE_INIT`, which the kernel sends once per mount, so it cannot
adopt a connection that is already initialised. The second is what crash recovery and hand-over need (7.1). The plan is a
constructor contributed to `fuser` upstream that starts a session from a recorded `INIT` reply; if upstream does not take
it, a vendored fork with that one change. Spike SP2 decides.

### 6.3 The mount, the namespace and the inode numbers

On Linux `tierstore fs` makes one FUSE mount at boot, `/run/tierstore/fs` (`fs.mountpoint`), before the job runtime
starts (systemd ordering `Before=` the agent's unit), with `allow_other` (jobs run as other users), `default_permissions`
(the kernel checks modes; `tierstore fs` makes no access decision of its own on Linux), `max_background` 1024 [S28], and
`max_stack_depth = 1` (passthrough needs at least 1, and the job's overlay above it must stay within the kernel's depth of
2; the cache's filesystem must then be a non-stacked one such as XFS or ext4, which `tierstore fs` checks at start and
refuses by name [S1, S17]). On macOS the mount is the NFS export of section 8. Under the mount:

```
<mount>/                 mode 0711: anyone may pass through, nobody may list
<mount>/trees/           mode 0711: same
<mount>/trees/<digest>/  the root of an attached tree, read-only; absent unless attached
<mount>/views/<run>/     a run's writable view (macOS, and Linux under C4 option B); absent unless attached
```

- **Attachment is the only way in.** A lookup of `trees/<digest>` or `views/<run>` succeeds only for what some client has
  attached (6.5). A listing of the mount's root, `trees/` or `views/` is refused (`EACCES`). Any digest of any manifest
  anybody wrote to the store is not reachable by naming it; that is "never the whole store".
- **The namespace is not cached; the trees are.** Replies for the mount's root, `trees/`, `views/` and the names in them
  carry entry and attribute timeouts of 0, and a name that is not attached is answered `ENOENT` with no cached negative
  entry, so an attach or a detach is seen by the next lookup. Below a tree's root everything is immutable, so every entry,
  attribute and absent name carries the longest timeout the protocol allows: an absent name is answered with inode number
  0 and a timeout, which the kernel keeps as a negative entry until the timeout ([S2]: `fs/fuse/dir.c`, "Zero nodeid is
  same as -ENOENT, but with valid timeout"). That is what keeps a compiler's repeated include-path misses out of
  `tierstore fs` (unmeasured; section 11 counts negative lookups). A job's overlay holds its lower root's dentry, so the 0
  timeout on `trees/<digest>` costs a lookup per path walk from the host, not per read in a job. Views are writable and
  are cached as 6.6 says.
- **Opens and directory reads.** A passthrough open sets `FOPEN_PASSTHROUGH` alone (the kernel refuses it combined with
  `FOPEN_KEEP_CACHE` [S2]: `fs/fuse/iomode.c`); an open of an empty file, which has no backing file, sets
  `FOPEN_KEEP_CACHE`; directory opens set `FOPEN_CACHE_DIR`; the mount negotiates `READDIRPLUS` and `PARALLEL_DIROPS`.
  `GETXATTR` and `LISTXATTR` are answered `ENOSYS`, so the kernel stops sending them after the first (the overlay above
  asks for its own `user.overlay.*` attributes on every lower directory); every request that would change a tree is
  answered `EROFS`.
- **Attributes.** Every entry of every tree reports one owner, `fs.owner` (`uid:gid`, host ids): the host id of the
  **action user** (the executor: container user 1000, `ACTION_ID`; jobscheduler: its action user), not of the container's root;
  on macOS a view reports the run's own user (8). Files report mode 0644, or 0755 when `exec`; directories 0755; symlinks
  0777. The action can therefore change and create anything (the overlay or the view copies up), as in a copy it owns, and
  every other user can read and traverse a tree, as the executor's cache files and tree store already allow (the executor's fast-sandbox.md
  section 4.4). mtime is fixed, the same value the executor's materialisation uses (D10). `nlink` is 1 for files, and 2 plus the
  subdirectories for a directory. `st_ino` is the inode number.

**Inode numbers are derivable (invariant I1).** The kernel names every inode by a 64-bit number it got from a lookup
reply (an NFS client by the handle it got), and keeps using it until it sends `FORGET` (NFS never does). The 64 bits are
split by their top bits into three ranges that cannot meet:

| Top bits | What | Layout |
|---|---|---|
| `0` | tree inodes, and the namespace | `tree_number << 24 \| index`, tree numbers below 2^39 |
| `10` | a view's entries that are still the tree's (6.6) | `view_number << 24 \| index`, view numbers below 2^38 |
| `11` | entries a view's run created or copied up (6.6) | `view_number << 24 \| id`, ids below 2^24 per view |

- Tree number 0 is the namespace: index 1 is the mount's root (`FUSE_ROOT_ID` is 1 [S2]), index 2 is `trees/`, index 3 is
  `views/`, index 0 is never used.
- A **tree number** from 1 up is given to a tree digest the first time it is attached after the mount was made; a **view
  number** to a view when it is attached. Neither is reused while the mount lives, so no inode number is ever reused and
  the FUSE generation is always 0. The tables (tree number to digest; view number to run, tree and directory) are files
  of fixed-size records in the runtime directory, indexed by number, written before the first reply that names a number
  of the new tree or view, and read by number, so a start reads neither whole. They grow by about 40 bytes per tree or
  view attached, and start empty when the mount is made again at boot. They must survive the process (a crash, a
  restart, an update), not the machine, because a reboot takes the mount with it.
- An **index** is the entry's position in the tree flattened depth-first in manifest order, the tree's root being 0. It is
  a function of the manifests alone.
- An **id** is the number a view gives an entry its run creates or copies up, from a counter that never goes back; it is
  stored on the entry itself (6.6), so a new process finds it on disk. A view that has used 2^24 ids answers `ENOSPC` to
  its next create.

So any instance of `tierstore fs`, given the table and the manifests (which are content-addressed, and in the cache or
upstream), can answer any request about any inode number this mount ever handed out, without having seen the lookup that
created it. This is Buildbarn's resolvable handle [S5] made total: nothing about a tree's inode depends on the process
that first answered for it. File handles are derivable too: `fh` carries the inode number and, for a passthrough open,
the backing id (6.4). Lookup counts from `FORGET` are kept only to drop a flattened tree from memory; a dropped tree
needed again is flattened again. Sections 7 and 8 stand on this. The layout is a format between releases while a mount
lives; section 10 says how it is versioned.

The cost of I1 is that a tree's manifests are all read and flattened when it is first attached: metadata is fetched
eagerly, data lazily (D7). The executor's setup walks every `Directory` of every action's input root today (`walk_ms`), so the
walk itself is not new; the flattening is (its cost is spike SP4). Buildbarn instead loads each directory on first access
[S6]; doing that here would make inode numbers depend on the order of lookups, which a new process cannot recover. Each
new root digest also costs a table append and cold dentries: the first lookup of each path under it is a round trip. That
is why a toolchain should be a tree of its own, attached once per node and bound read-only into every action that uses it,
so that its dentries and inodes are shared across actions (6.7).

### 6.4 Opens: lazy fetch and passthrough

The kernel's rules for passthrough (`fs/fuse/iomode.c` and `fs/fuse/passthrough.c` at v6.17 [S2]) decide the Linux half
of this section: an inode with an open passthrough file is bound to that one backing file, and a passthrough open with
another backing id fails `EBUSY`; a non-passthrough open of an inode with a backing file fails `EIO`; a passthrough open
of an inode with a cached open fails `ETXTBSY`. Every inode of a tree that has a non-empty file is therefore opened by
passthrough, always, and always with the same backing id for as long as any open of it lives.

On an `OPEN` of a non-empty file of an attached tree:

1. If a backing id is live for the blob (some open of it, in any tree, is not released), reply `FOPEN_PASSTHROUGH` with
   that id and count the open.
2. Else, if the cache file of `_tiering/fill/<sha256>` exists, open it read-only, register it
   (`FUSE_DEV_IOC_BACKING_OPEN`), record blob, id and count in the backing table (7.1), send `touch <sha256>` (and `pin`,
   6.5) on the fill socket, and reply as in step 1. From then on the job's `read`, `mmap` and `splice` go to the cache
   file in the kernel and never reach `tierstore fs` [S1].
3. Else send `fill` for it and wait, then step 2. Concurrent opens of one blob share one fill.
4. If the fill fails, the open fails with `EIO`, and the run's attachment records the path, the digest and the reason
   (7.4). There is no uncached fallback: the kernel would refuse it for an inode another job has open, and it would read
   bytes nobody verified (D11).

On the last `RELEASE` of a blob's opens (the `fh` carries the backing id), `tierstore fs` closes the id
(`FUSE_DEV_IOC_BACKING_CLOSE`). So ids exist only for open files, their number is bounded by open files (and by
`fs.max_backing_ids`, default 65536, past which an open waits for a release), no eviction signal is needed, and an
evicted blob that is still open keeps being served from the same backing file: the kernel holds a reference to it [S1],
and later opens reuse the live id. Its blocks are freed at its last close, as for any evicted cache file a reader still
holds (`local_cache/mod.rs`).

On macOS (NFS) the same steps run without backing ids: an `OPEN` (v4) or the first `READ` of a file (v3) fills the blob
if needed, and `READ`s are served by `tierstore fs` from the cache file. A blob is opened once per process and its
descriptor kept while any view references the file, so a `READ` costs one `pread` (unmeasured; the RPC cost is probe MP3).

A blob is verified before it is opened: a source fill publishes only bytes that hash to the name. The price is that the
first open of a large blob waits for the whole blob (today the whole tree is fetched before the job starts, so a single
file is never slower than today; unmeasured overall). The cache's S3 path already serves a miss from the staged file
while the fill runs (`local_cache/stream.rs`); `tierstore fs` cannot on Linux, because passthrough needs a published file.
Block-wise fetch with read-ahead (JuiceFS-style, 4 MiB blocks [S27]) is possible on macOS, where reads already pass
through the server, and on Linux only for files read before their fill ends; both are future work, for when section 11
shows large cold blobs on the critical path.

**Prefetch** (for arm D of section 11, not before): an attachment may ask for `prefetch = "all"`, and every blob of the
tree is filled in the background, in manifest order, on the fill socket's permits, without delaying the attach's answer;
it is the proposal's warm-up before the job starts, done without delaying the start. Learning what an action opens and
prefetching that, as Buildbarn's file system access cache does with a Bloom filter of paths per action [S7], is phase 3.

### 6.5 Attachments, pins and eviction

An **attachment** is a client's statement that a run will use a tree: `attach <digest> for <run> [view <upper dir>]`,
where `<run>` is the client's id of the run (jobscheduler's run id, the executor's lease id) and `view` asks for a writable view
whose upper directory the runtime has made in the run's directory (6.6). It is answered when the tree's manifests are read
and flattened and the tree (or the view) resolves, with the path to give the job. `detach <run>` ends every attachment of
the run and answers with what they recorded (7.4). Attachments are made on the control socket (`fs.control_socket`, a Unix
socket in the runtime directory, mode 0660, group `fs.client_group`), and kept in a file in the runtime directory, so they
survive a restart or an update of `tierstore fs`. A client re-asserts its live runs after its own restart with
`detach-all-except <runs>` (jobscheduler's agent already adopts its live runs from a ledger,
`crates/js-agent/src/crun/mod.rs`); `tierstore fs attachments` lists them for an operator.

Two different things depend on a tree, and the design keeps them apart:

- **Whether the tree can be read** depends on the kernel: as long as any inode of the tree is referenced (a job's overlay
  holds its lower root), every inode number of it must be answerable. I1 makes that free: `tierstore fs` never stops being
  able to answer a tree number. The last detach of a tree only makes `trees/<digest>` absent for new lookups.
- **What the cache keeps** decides only speed. An evicted blob that is open stays readable (6.4); one that is not open yet
  is filled again on its next open.

**Pins**, designed for the second. A pin is held by the cache's own writer, the build-node tierstore, so the cache keeps
one writer and one budget:

- `tierstore fs` sends `pin <mount id> <tree> <sha256> ...` for each blob the first time it is opened under an attached
  tree (the pin is per tree, so two runs of one tree share it), and for every blob of the tree when the attachment asked
  for `prefetch = "all"` (the warm-up), batched to at most one request per tree per second; it sends
  `unpin <mount id> <tree>` when the tree's last attachment is detached. `<mount id>` is the boot id of the mount. Each
  request is answered, and `tierstore fs` sends it again until it is, so one lost while the cache changes hands (7.3) is
  not lost. `tierstore fs` records, per attached tree, the blobs it has pinned, in its runtime directory.
- The unit of reference counting is the attached tree, not the file and not the process, so no process attribution is
  needed (which FSKit, for one, cannot give [S25]).
- The cache keeps, per entry, the set of trees that pin it, in memory, and writes the pin set to `<cache>/pins`
  (rewritten by rename, by the cache's lock holder only, at most once a second and when it gives up the lock), so pins
  survive a restart and a hand-over of the cache process: the new holder reads the file when it takes the lock.
- **Pins never outlive their mount or their attachment.** At every start of either process, and after a hand-over of
  either, `tierstore fs` sends `pins-live <mount id> <trees>`, the trees attached now, and the cache drops every pin of
  another mount id or of a tree not in the list. A reboot makes a new mount id, so the old mount's pins go at the first
  `pins-live`.
- Eviction skips a pinned entry and counts it in a new counter, `tierstore_cache_pin_skips_total`; the existing
  `tierstore_cache_pinned_skips_total` counts entries passed over for their hard links and keeps that meaning
  (`crates/tierstore/docs/metrics.md`). Pinned bytes stay charged to the budget. A pin that would take pinned bytes over
  `local_cache.pin_limit` (default half the budget) is refused, and the blob is kept by recency only (`touch`): pins buy
  speed, so running out of them must not stop a job.
- Removals that are not eviction (a scrub that finds bad bytes, a revalidation, a write-through delete) still remove a
  pinned entry: correctness wins over a pin. The next open fills it again.
- Pins are entries of the cache's own index, not hard links, so the cache keeps its single writer
  (`local_cache/hold.rs`) and no file escapes its budget (its non-eviction removals unlink an object whatever its link
  count, `local_cache/mod.rs`: `drop_entry`).

A source that no longer exists upstream (the client deleted a pack while a tree using it was attached) fails that blob's
open with `EIO`, recorded on the attachment (7.4). That is the client's contract (6.1), and it is the failure the executor
gets when it materialises from a missing blob, moved from before the job to during it.

### 6.6 Writes: the overlay on Linux, the core's views on macOS

**Linux (recommended, C4 option A).** The job's writes never reach `tierstore fs`. The runtime mounts, inside the job's
mount namespace, the overlay it mounts today, with the attached tree as its lower directory:

```
lowerdir=/run/tierstore/fs/trees/<digest>,upperdir=<run dir>/upper,workdir=<run dir>/work,userxattr,...
```

(the options both runtimes already pin: `userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off`). A write to an
input copies the file up into `upper` (reading it through the tree, so lazily, once); a new file goes to `upper` at the
disk's speed; a delete is a whiteout in `upper`. Outputs are read from `upper`, by the code that reads them there today,
once per file, hashed and stored in one pass (B16), after the job's processes are gone, so no writer is open.

**The core's views** (macOS, and Linux under C4 option B; built in phase 2). A view is the tree below and an **upper
directory** above. The runtime gives `tierstore fs` a directory in the run's directory (so it is on the run's volume and is
removed with it, 6.9); the core keeps the view's mirror of paths in `<dir>/root` and its own records in `<dir>/meta`, so
no name the run can create collides with a record. The mirror uses the whiteout encoding of the OCI image layer format
[S31], so a person and the runtimes' collectors can read it:

```
<dir>/root/a/b.o               a file the run created or copied up: a plain file at its path
<dir>/root/a/.wh.c.h           a whiteout: the tree's a/c.h is deleted in this view
<dir>/root/d/.wh..wh..opq      an opaque directory: none of the tree's d shows through
<dir>/meta/intents             the intent log of changing operations (below)
<dir>/meta/tmp/                copies and new directories being made, renamed into root/ when whole
```

Every file and directory in `root/` carries an extended attribute `user.tierstore.id` with its id (6.3), and a copied-up
file also `user.tierstore.tree` with the tree index it replaces, both set on the temporary copy before it is renamed into
place. Extended attributes go with the file through renames and hard links, so a file's identity is never a path: the
macOS NFS client renames an open file that is removed to `.nfs…` and keeps reading it by handle [S10], and the handle
still leads to it.

**Reading a view.** A name in `root/` is that entry. A whiteout `.wh.<name>` hides the tree's entry. If the directory is
opaque, nothing of the tree's directory shows. A listing merges the two in name order and hides `.wh.` names. A name in
`root/` together with its own whiteout is that entry, with the tree's entry of that name hidden (opaque, if a directory);
this state exists only while an operation is in flight (below), and reads the same as the state on one side of it.

**Changing a view: intents, fixed steps, roll forward.** Each operation that changes a view (create, mkdir, symlink,
unlink, rmdir, rename, link, and the copy-up of a `setattr` or of an open for write) is done in three steps:

1. Append an intent record to `meta/intents`: the request's identity (the FUSE request id, which a resent request keeps,
   with the `FUSE_UNIQUE_RESEND` bit set [S3]; for NFS, the client and its transaction id), the operation, its arguments,
   and the ids it will assign.
2. Do the operation's steps in the fixed order of the table below. Each step checks whether it is already done (an entry
   with the intended id is at the target; the whiteout exists) and skips it if so, so any prefix of the steps can be run
   again.
3. Mark the intent done, with its reply, then reply.

At start, before serving a view, the core runs every unfinished intent forward to its end. A resent request whose identity
has an intent is answered from it: rolled forward if unfinished, with its recorded reply if done. So a crash or an update
at any point leaves the view before or after the operation, never between, and a retried request is never executed twice
(a resent `unlink` answers success, not `ENOENT`). The log drops done intents older than any request the kernel or client
can still resend (a minute; the client's retransmission window is measured by MP3). Nothing is `fsync`ed: the view must
survive the process (a crash, an update), not the machine, which takes the run with it.

| Operation | Steps, in order |
|---|---|
| Copy-up of a tree file (open for write or truncate; `setattr` of size, mode or times; the source of `link` or `rename`) | Copy up the parents (each a directory made in `meta/tmp` with its id and the tree directory's mode, then renamed into place). Make the copy in `meta/tmp` from the blob's cache file: `clonefile` on APFS and `FICLONE` on XFS when cache and run share a filesystem (no bytes copied), else a copy after the blob is filled; set its id and tree index; rename it into place. |
| Create, symlink | Make the entry in `meta/tmp` with its id; rename it to `<name>` (a `.wh.<name>` stays for now: the name with its whiteout reads as the new entry); then remove `.wh.<name>`. |
| Mkdir | As create; when `.wh.<name>` exists or the tree has `<name>`, `.wh..wh..opq` is made inside the new directory in `meta/tmp` first, so it arrives opaque. |
| Unlink of a name the tree has | Create `.wh.<name>`; then remove `root/<name>` if present. |
| Unlink of a name only in `root/` | Remove it. |
| Rmdir | The view must show the directory empty (no entry in `root/` but whiteouts, every tree child whited out), else `ENOTEMPTY`. If the tree has the name: create `.wh.<name>` first; then remove the whiteouts in `root/<name>`, then it. Else remove the whiteouts in it, then it. |
| Rename | Refuse a source directory that the tree has (`EXDEV`, as both runtimes' overlays answer with `redirect_dir=nofollow`, so tools copy instead), a destination named `.wh.…`, and the POSIX type and emptiness errors (`EISDIR`, `ENOTDIR`, `ENOTEMPTY`, judged on the view). Copy up the source if it is the tree's. If the destination is a directory the tree has, make the source directory opaque. If the tree has the source's name, create `.wh.<source>`. Rename within `root/`. If the destination had a whiteout, remove it. |
| Link | Copy up the source if it is the tree's; refuse a destination named `.wh.…`; link within `root/` (both names carry the one id); remove a whiteout of the destination. |

**Rules a view keeps.**

- Names beginning `.wh.` cannot be created, renamed to or linked to (`EPERM`), as in an OCI layer; a tree manifest cannot
  contain them either (6.1).
- **Handles.** An entry still the tree's has the `10` form (view number, tree index); an entry in `root/` has the `11`
  form (view number, id); a copied-up file answers to both, so a handle the client took before the copy-up still leads to
  the copy (6.3). After a restart the core finds an entry by its id or tree index by walking `root/` once and reading the
  attributes, which is bounded by what the run wrote.
- **Caching.** A view's entries change, so on FUSE they carry short timeouts (`fs.view_entry_timeout`, default 1 s) and the
  core invalidates what it changes (`FUSE_NOTIFY_INVAL_ENTRY`, `FUSE_NOTIFY_INVAL_INODE`); on NFS a view's entries report
  their real mtime, which the macOS client turns into short attribute caching (8). On Linux, a view's files in `root/` are
  opened by passthrough too, so writes to them go at the disk's speed.
- **Outputs.** The runtime reads the run's outputs from `root/` after the job's processes are gone (an output is a plain
  file at its path there); an output the run never changed is still the tree's, and `detach <run> outputs <paths>`
  returns its digest from the manifest. tierstore does not hash or store outputs: the executor's output path packs small blobs into
  segments, which tierstore cannot do for it (D4).

This is EdenFS's design in miniature: an overlay that records what was materialised, written in an order that loses
nothing when the process is killed [S9]. The intent log is what that ordering needs to become when the protocol may resend
a request whose effect is already on disk.

Compared with the executor's plain copy, an action sees the same paths and contents, a tree it owns (6.3), writable everywhere,
private. Two differences it shares with today's overlay over a materialised root: renaming a directory of the input root
fails `EXDEV`, and a hard link from an input copies it up. Compared with the executor's cached path (worker-cache.md section 6.2),
the action sees its own uid instead of the overflow owner, modes 0644 and 0755 instead of 0444 and 0555, and identical
inputs as separate inodes with `nlink` 1 instead of one shared inode; a tool that relies on inode identity (hard-link
records in `tar` or `rsync -H`) behaves as it does over the plain copy. Two improvements: an action that opens an input for
writing gets a private copy instead of `EACCES` (the executor's worker-cache.md section 12, Q1), and, for a toolchain left inside the
writable tree, an action that writes into it works instead of failing (the executor's fast-sandbox.md section 4.4).

### 6.7 Integration with the job runtimes

**jobscheduler, Linux (`crun`).** Its wire has the shape (`InputRef::Stage`) but not the path: inputs live only in `run`
and `native` bodies, which the `crun` runtime refuses today, `dest` may not be empty, and `stage_id` names a tree of
jobscheduler's own tree store (1.1). The jobscheduler change is therefore (its own design, B22):

- a new input kind, `InputRef::Tree { digest, dest, placement }` (an additive variant, sent only to hosts whose hello
  lists the capability `tierstore-trees`, as `scratch-quota` and `run-files` are sent, jobscheduler
  `crates/js-agent-wire/src/command.rs`), where an empty `dest` with `Writable` means "the exec root", so there is no clash
  with `Stage` and its tree store;
- the `crun` runtime accepts it: at the step where it makes the run directories (jobscheduler
  `crates/js-agent/src/lifecycle/machine/mod.rs`, before `create_and_start`) it attaches the tree; a `Writable` tree at
  the exec root becomes the overlay's `lowerdir` in place of today's empty `input_root`; a `ReadOnly` tree becomes a
  read-only bind at `dest` through `ContainerSpec.shared`, which the spec already supports (`spec.rs`);
- it detaches in `remove`, after outputs are read and before the scratch delete;
- its disk offer subtracts the unused budget of tierstore's cache when the cache is on the scratch volume (6.9).

The host-wide FUSE mount is made before the agent starts, so the agent's post-job integrity check sees it in its baseline
(jobscheduler `crates/js-agent/src/integrity/mod.rs`); no per-job mount is made on the host, so none can be left behind.

**jobscheduler, macOS (`sandbox-exec`).** The agent attaches the tree with a view whose upper directory it makes in the
run's home, starts the job with its working directory in `<mount>/views/<run>`, and grants the run its view in its Seatbelt
profile through the builder's allow-list read model (jobscheduler `crates/js-mac-pool/src/seatbelt/`: an allow-list of
places, plus metadata reads of their ancestors so that `realpath` and `getcwd` work), with the mount's root not granted,
so the run's own view, and a tree it was given read-only, are the only parts of the mount it can touch. Under the open
read model a deny of the mount's root is emitted after every grant and would win over the view's allow, so a run with a
view uses the allow-list model, or the builder gains a policy item emitted after the read denies and before the floor;
probe MP2 runs the profile the builder generates, not a hand-written one. Every path given to the profile is
`realpath`-canonical, since rules match the resolved path [S32]. The mount itself is made by root at boot (8), since the profile's floor denies
`file-mount` to every job (jobscheduler `crates/js-mac-pool/src/seatbelt/profile.rs`). A `ReadOnly` tree input is served
as `<mount>/trees/<digest>` under a read-only allow rule. The agent's refusal of tree inputs ("this agent keeps no stage
trees") is lifted for hosts that list the capability.

**The executor's own container driver**, for as long as the executor schedules its own leases, behind a daemon flag, with the materialised
path kept as the fallback when `tierstore fs` is not configured or refuses an attach. Three changes in
`crates/driver-container`: the overlay's lower directory comes from the attach; each shared tree (a `Directory` of at
least 256 files) is attached as its own tree and bound read-only where it is bound today, so its inodes are shared across
actions as today; and `prepare_outputs`, which today **renames** each input that an output path names from the input root
into the upper layer, copies it instead (a lazy read of that blob), and makes the mount points, the working directory and
the outputs' parents in the upper layer. The executor's macOS driver uses the view as jobscheduler's does.

**The executor's other half**: write each `Directory`'s tree manifest with its blobs' sources (it walks the `Directory` messages
today and knows each blob's pack location), put the manifests to tierstore, and name the root digest in the job. That is
a design of the executor's own; spike SP5 is its throwaway form, and section 12 lists its production form.

### 6.8 Isolation

- **Linux.** A job's container sees its overlay (whose lower is one tree root), its read-only tree binds and its other
  mounts, and nothing of `/run/tierstore/fs`: the host mount namespace is gone after the container's pivot (inferred from
  the OCI runtime model; checked in SP1). It cannot name another tree, because no path in its namespace leads to one. It
  cannot write to a tree: its writes go to its own upper directory, a read-only bind is read-only, and the tree answers
  every changing request `EROFS`.
- **macOS.** The Seatbelt profile grants the run its own view only (6.7); whether Seatbelt's path rules behave on an NFS
  mount is probe MP2. `tierstore fs` also answers a request on `views/<run>` only when the caller's uid (NFS `AUTH_SYS`)
  is the run's user, which jobscheduler creates fresh for each run, else `NFS4ERR_ACCESS` (or `NFS3ERR_ACCES`). That
  second check is a fence only against a process that cannot speak NFS to the server itself, because `AUTH_SYS` is a
  claim, not a proof: a job given loopback network (jobscheduler's `Loopback`, `Ports` or `On`) can reach a loopback TCP
  server and claim any uid. So the transport decides: over a Unix socket (if MP1 finds it works) whose path every job's
  profile denies, the check holds for every run; over loopback TCP, a view is refused at attach for a run whose network
  is above `Off`, and such a run keeps today's copy. Buildbarn's `bb_virtual_tmp` serves a different symlink target per
  `AUTH_SYS` uid [S23]; that is the same mechanism, and Buildbarn does not offer it as an isolation boundary.
- A host process (outside every container and sandbox) of any user can read an attached tree by its digest, as it can read
  the executor's cache and tree store today (mode 0711 directories, files readable by others). Attaching is limited to the control
  socket's group. A tree's contents are build inputs, not secrets; a deployment that needs them hidden from host users sets
  the mountpoint under a 0710 directory of the runtime's group (D6). A view is readable only by its run's user.

### 6.9 Disk and memory accounting

There is no per-job disk quota (CEO, 2026-11-09). The node's disk is one pool, and the design keeps that:

- **Inputs** cost the node one copy of each blob opened, in tierstore's cache, under the cache's budget (`budget`,
  `high_water`, `critical`, `free_floor`: `crates/tierstore/docs/configuration.md`), pinned blobs included (6.5). An
  evicted blob that a job still has open is outside the budget until the job closes it, as for any evicted cache file a
  reader holds today. jobscheduler offers the scheduler the scratch volume's free space minus the unused budgets of its
  bundle and image stores (`crates/js-agent/src/disk.rs`); with the cache on the same volume, its unused budget is
  subtracted the same way (6.7).
- **A job's writes** are in its run directory (the overlay's upper on Linux, the view's upper on macOS), on the scratch
  volume, as today, and count where they count today. Copy-ups are the only new bytes, and only for inputs the job
  changes, and none at all where the copy is a clone (6.6).
- **What a job no longer costs:** the per-file copy, clone or link and its removal, and, on a cold node, the bytes of every
  input it does not open.
- **Page cache.** With passthrough, an input's pages are the cache file's pages: one copy on the node, however many jobs
  and trees read the blob (inferred from the passthrough mechanism [S1]; unmeasured). A fill's pages are written by the
  build-node tierstore and charged to its service's memory cgroup; a page dropped since and read again is charged to the
  job that reads it (inferred from the kernel's rule that a page is charged to the cgroup that brings it in). On macOS the
  NFS client caches pages of its own on top of the server's reads of the cache file, so a blob may be cached twice
  (inferred; measured in section 11).

## 7. Failure, crash and live update

This is the hard part: the filesystem server is in the path of every lookup and open of every running job, and, on macOS
(and Linux under C4 option B), of every read and write. Sections 7.1 to 7.4 are Linux (FUSE); 7.5 is NFS.

### 7.1 What makes it tractable, and what it needs (Linux)

- **I1, derivable inode numbers** (6.3) and derivable view handles (6.6): any instance can answer any request of the
  mount.
- **Under option A of C4, no job data in tierstore** (6.6): a crash loses nothing a job wrote. Under option B, the views'
  upper directories hold it, written in order (6.6), so a crash loses nothing either, provided every reply was sent after
  its writes.
- **All other state is in the runtime directory** (kept across restarts of the unit): the table of tree and view numbers,
  the attachments, the backing table (blob, backing id, open count; written on each first open and last release, without
  `fsync`, since only the process has to be outlived), and the server's `INIT` reply (flags, `max_write`,
  `max_readahead`, `time_gran`, `max_stack_depth`), written once at mount. A successor reads them at start.
- **The kernel keeps a connection alive until the last `/dev/fuse` file of it is released** (`fs/fuse/dev.c`:
  `fuse_dev_release` aborts the connection only when the count of its devices reaches zero [S2]), and queues new requests
  while nobody reads them.
- **`FUSE_NOTIFY_RESEND`** (protocol 7.40, `FUSE_HAS_RESEND`) makes the kernel queue again every request that was read
  from a still-open device file but not answered, with the `FUSE_UNIQUE_RESEND` bit set in its id [S3]. The kernel's
  condition is that the requests are idempotent. Every request on a tree is (it is read-only, and its answers are derived
  from manifests); the backing-table updates of `OPEN` and `RELEASE` are made idempotent by keying them on the request's
  `fh` and the open count. Under option B, a view's changing requests must also be made idempotent: each is applied as
  "make the upper directory look like this" (a create that finds its file made answers it; an unlink that finds the
  whiteout made answers it), which is why the view records go before the change (6.6).
- **Requests read on a device file that is then released are lost**: `fuse_dev_release` ends them with `ECONNABORTED`
  [S2]. So `tierstore fs` reads its one `/dev/fuse` descriptor from several threads and makes no clone
  (`FUSE_DEV_IOC_CLONE` gives each clone its own queue of requests in progress, which a crash would fail). If SP2 shows
  one descriptor is the limit, every clone is stored too (`FileDescriptorStoreMax` = clones + 1) (D12).
- **A session that starts from a recorded `INIT` reply** (6.2), since the kernel sends `INIT` once per mount.

### 7.2 A crash of `tierstore fs` (Linux)

1. After the mount, `tierstore fs` puts the `/dev/fuse` descriptor in systemd's descriptor store
   (`sd_notify("FDSTORE=1\nFDNAME=fuse")` with the descriptor attached) [S11].
2. It crashes. systemd keeps the descriptor (`FileDescriptorStorePreserve=restart` keeps the store while a restart is
   pending [S11]), so the connection is not aborted. New requests queue in the kernel; jobs block in `open` and `lookup`;
   reads of files already open go by passthrough and do not block at all.
3. systemd restarts it (`Restart=on-failure`) and hands the descriptor back. `tierstore fs` reads `LISTEN_PID` (checked
   against its own pid), `LISTEN_FDS`, `LISTEN_FDNAMES` and `NOTIFY_SOCKET`, all set by the service manager: platform-set
   markers under the rule S2, each read in one function behind an `#[expect(clippy::disallowed_methods, reason = "...")]`.
4. It reads its state (7.1), starts the session from the recorded `INIT` reply, sends `FUSE_NOTIFY_RESEND`, and the
   blocked jobs go on.

The stall is the restart time (unmeasured; `RestartSec` plus a start that reads a few small files and flattens trees on
demand). `tierstore fs` refuses to mount on a kernel that does not offer `FUSE_HAS_RESEND` in `INIT` (6.17 does [S3]).
**If it does not come back**: when the unit gives up restarting (`StartLimitBurst` restarts within
`StartLimitIntervalSec`) and is left failed, systemd drops its descriptor store, the last device file of the connection is
released, the kernel aborts it, and every blocked and later request fails with an error. Jobs fail fast instead of
hanging; the next start mounts afresh. The bound is the unit's start limit (`StartLimitBurst=5` within
`StartLimitIntervalSec=60`, set in the unit, 6.2 [S11]), not a separate watchdog, so no extra machinery is added (B20). A job that fails this way fails with an I/O error from a tool,
which looks like a build failure; 7.4 says how the runtime tells it apart.

### 7.3 A live update of `tierstore fs` (Linux)

The hand-over of tierstore-handover.md section 0 is the model; `tierstore fs` has its own control socket for it (not the
S3 instance's `handoff.sock`) and its own offer, built on the frame code of `server/handoff/wire.rs`. The offer today
carries `<front> <address>` lines with one descriptor each, and a taker refuses any other line
(`server/handoff/take.rs`), so the offer of `tierstore fs` is a new kind of conversation, not new lines in the old one:

```
new (tierstore-fs@b)                                    old (tierstore-fs@a, running)
 | connect its hand-off socket, "take-fs 1" ───────▶     |
 | ◀── "offer-fs", "fuse <mountpoint>", "control <path>" + the /dev/fuse and control socket descriptors (SCM_RIGHTS)
 | mountpoint and paths match its settings?              |
 | FDSTORE=1 (FDNAME=fuse) and wait until systemd has it |
 | "ack" ──────────────────────────────────────────▶     |
 |                                                       | sets its stop flag: each reader thread answers the
 |                                                       | request it holds and stops polling the device
 |                                                       | (readers poll the device, non-blocking, with a stop
 |                                                       | eventfd); stops accepting on the control socket
 | ◀── "done" (no thread holds a request)                |
 | reads the state (7.1), starts the session from the recorded INIT reply, serves; "ready" ─▶ |
 |                                                       | exits 0; its unit goes inactive, its store is dropped
```

- No moment without a durable holder: the new instance's store has the descriptor before the old one stops, and the old
  one keeps its own until it exits after `ready`. If the new instance fails before `ready`, the old one resumes reading;
  nothing was released.
- No request is lost or answered twice: each request is read by exactly one process; the old one answers every request it
  read before `done`; the kernel queues the rest for the new one.
- Control requests in the gap wait in the listening socket's backlog, as S3 connections do in today's hand-off.
- Backing ids belong to the connection, so the kernel keeps every one across the hand-over; the new instance learns them
  from the backing table in the runtime directory.
- Capability: a release that can do this lists `handoff-fs-v1` in `tierstore --capabilities`, and the hook's `prepare`
  chooses a restart when either release lacks it, or when the two releases' layouts differ (section 10). A restart of
  `tierstore fs` is 7.2 without the crash: the descriptor store carries the session across it.
- **The S3 instance's update** does not interrupt fills: the fill socket is bound only by the instance that holds the
  cache lock (6.2), so the old instance answers fills through its drain, and the new one binds the socket in the tick (one
  second, `local_cache/hold.rs`) in which it takes the lock after the old one exits. `tierstore fs` retries a fill that
  meets a closed socket within `fs.fetch_timeout`, and re-sends its pins to the new holder.

### 7.4 Failures a job sees, and how its runtime tells them from build errors

| Failure | What the job sees | What the attachment records |
|---|---|---|
| A blob's fill fails (upstream down past `fs.fetch_timeout`, default 60 s; source missing; hash mismatch; no space) | `EIO` on `open` (or `read`, NFS v3) | the path, the digest and the reason |
| `tierstore fs` left failed by systemd (7.2), or down past the macOS bound (7.5) | an error on lookups and opens | the abort, recorded by the next start |
| A tree manifest is invalid (unsorted, duplicate, bad name, unknown kind, too large, wrong hash) | nothing: the attach is refused, by name, before the job starts | n/a |
| A view's upper directory runs out of space | `ENOSPC` on the write, as on a copy | nothing: the same as today |
| A stall that ends well (a slow fill, a restart, an NFS grace period) | a slower run | `stall_ms`: the time the run's requests waited |

`detach <run>` answers with what the run's attachments recorded. A runtime that gets a recorded failure marks the run as
an infrastructure failure (retryable) whatever the job's exit status, so a compiler's "cannot read file" caused by a fetch
is retried as today's failed materialisation is, and never reported as the user's build error. A runtime adds `stall_ms`
to the run's deadline, as Buildbarn compensates an action's timeout for slow lazy loading [S8], and marks a timeout as
infrastructure-retryable when `stall_ms` was over a tenth of the limit.

### 7.5 Crash and live update of the NFS frontend (macOS)

What NFS gives: a hard mount makes the client wait and retry while the server does not answer [S22]; EdenFS reports
exactly that, I/O hangs while its server is gone, and a dialog the client shows when the server stays unresponsive [S10].
So the server must come back on the same endpoint, quickly, and answer the handles the client holds.

- **macOS has no hand-over today.** tierstore's hand-off code is portable (`server/handoff/mod.rs`), but a Mac update is
  a restart of one launchd job and the macOS package does not set `server.handoff_socket` (tierstore-handover.md section
  0, `crates/tierstore/docs/update-hook.md`), and the hand-off carries only the TCP listening sockets of the S3 and OCI
  fronts (`Front`, `<front> <address>` lines). A live update of the NFS frontend is therefore new on three counts: two
  launchd jobs of `tierstore fs` that alternate as `tierstore-fs@a` and `@b` do on Linux, with the hook's record and
  rollback for them; an offer of its own (as `take-fs` is in 7.3) that can carry a Unix listening socket; and the
  hand-over of NFS state below. Pull request 12 carries all three and changes tierstore-handover.md section 0 and
  update-hook.md with them. Until it lands, an update of `tierstore fs` on a Mac is a restart, which the crash path below
  covers.
- **The endpoint.** After a crash or a restart, launchd starts the server again (`KeepAlive`) and it binds the same
  endpoint. In a hand-over the listening socket passes with `SCM_RIGHTS`, and the old instance stops accepting at `ack`,
  so a reconnecting client reaches the new one.
- **The handles** are derivable (6.3) with the mount's boot id, so the new instance answers every handle the client holds;
  a handle of an earlier mount answers `STALE`. The `nfsserve` crate's default handle embeds its start time and answers
  stale after every restart, so it is overridden (`id_to_fh`, `fh_to_id`, `serverid` [S12]) if that crate is used (C2).
- **Retransmitted requests.** A client resends a request whose reply it did not get, on its new connection; a
  non-idempotent one (create, remove, rename, link) that the old process had already executed would then fail (`ENOENT`
  on a resent remove) and look like a build error. A view's changing operations carry the client and transaction id in
  their intent (6.6), so a resent one is answered from the intent instead of executed again: the intent log is the
  duplicate request cache, on disk, so it survives a crash and a hand-over. Requests on trees are read-only and
  idempotent. In a hand-over the old instance stops reading, sends every reply it owes, shuts down the write side of each
  connection, and only then exits, so no written reply is lost to a reset.
- **NFSv3** has no other state: with the above, a restart, crash or update is a pause, as long as it is shorter than the
  client's patience (MP3 measures when the dialog appears).
- **NFSv4.0** (if MP3 chooses it) has state: client ids, open and lock owners with their sequence ids and each owner's
  last reply, opens, and the lease.
  - **Update: hand the state over.** The old instance stops taking RPCs, writes its state table to the new one over the
    hand-off socket with the listening socket, and exits; the new one serves the same state, so the client sees a short
    pause. EdenFS's takeover passes an NFS mount's socket and inode map, but for NFSv3, which has no open state [S9, S10];
    handing over NFSv4.0 state is new, and MP3 tries it.
  - **Crash: no grace period.** A server that keeps no stable record of its clients must answer a reclaim with
    `NFS4ERR_NO_GRACE` (RFC 7530 [S33]), and a grace period would refuse every new open on the node, and reads and writes,
    for a lease time. So a new instance with no state answers the client's old client id with `NFS4ERR_STALE_CLIENTID`,
    every reclaim with `NFS4ERR_NO_GRACE`, and serves new opens at once; Apple's client makes a new client id and opens its
    files again (its reclaim code is read in its source [S24]; that it reopens after `NO_GRACE` is unverified, and MP3
    checks that files open across a kill stay usable). Share-deny modes are honoured while the server runs, not across a
    crash; a view has one run, so a deny that is lost could only have been against the run's own opens.
- **If the server stays down**, launchd keeps restarting it. The mount is `hard,intr` (8), so past `fs.recover_wait`
  (default 120 s) the runtime kills the run's processes, which `intr` lets a signal end, and fails the run as an
  infrastructure failure; the node-wide mount is not unmounted, since other runs use it. MP3 checks that a killed process
  ends while the server is down.
- **A slow fill** is answered `NFS3ERR_JUKEBOX` or `NFS4ERR_DELAY` after a second, and the client retries, so one slow
  upstream does not hold an RPC for the whole fetch and trip the client's not-responding handling (MP3).

## 8. macOS

macOS has no overlayfs, no FUSE in the kernel and no passthrough. The design is M1 (4.2), gated by probes, with M0 (today's
`clonefile`) as the default and kill-switch until M1 is measured. It needs the core's views (6.6) and the NFS frontend's
crash and hand-off rules (7.5).

- **One mount per node.** At boot, root mounts `tierstore fs`'s export at `/private/var/tierstore/fs` (`fs.mountpoint`),
  before the job agent starts (the agent waits for the mount), with `hard,intr` (a down server makes I/O wait, and a
  signal can still end a waiting process, 7.5), `locallocks` (locks stay in the client, so a tool that locks a file in its
  view works without lock state in the server) and `rdirplus` [S22]. Per run, setup is one attach, with no per-file work
  and no per-run mount: the cost moves to first touch. If MP1 finds that only root can mount NFS, root's one boot mount is
  all that needs it.
- **Attribute caching.** The macOS client does not take cache times from replies (Buildbarn's mount code notes that NFSv4
  has no per-reply way to give them [S5]). Within the mount's minimum and maximum, it caches a node's attributes for one
  tenth of the age of its mtime, and for the maximum under a delegation (read in the client's source [S24]). The design
  uses that rule: the mount's maxima are long; tree entries report one fixed, old mtime (D10), so they are cached up to the
  maximum; a view's entries report their real mtime, so what the run just changed is cached briefly; `trees/` and
  `views/` report the time of the last attach or detach, so an attach is seen soon. How soon, and whether the client's
  negative name cache (which `nonegnamecache` turns off for the whole mount) hides an attach after a miss, is probe MP4;
  the runtime attaches before it first looks up a path, so a miss before an attach does not arise in normal use.
- **The version** is NFSv4.0 or NFSv3, decided by probe MP3: NFSv4.0's compound operations cut round trips, which is why
  Buildbarn chose it ("NFSv3 ... far more chatty") [S5]; NFSv3 has no state to recover (7.5) and a mature Rust crate
  (`nfsserve`, BSD-3-Clause [S12]). The macOS client also accepts `vers=4.1` [S24], and a young Rust NFSv4.1 crate exists
  (`embednfs`, MIT [S13]); Buildbarn targets 4.0 because the macOS client did not support 4.1 when it was written [S5].
- **Executables.** `nfsserve` answers `ACCESS` for a read-only filesystem without the execute right, which the macOS client
  turns into `EACCES` for `execve` (inferred from nfsserve's `nfs_handlers.rs` and the client's `nfs_vnop_access` [S12,
  S24]). The core answers `ACCESS` itself from the mode bits, whatever crate frames the RPCs.
- **No `FORGET` in NFS** [S10]: the server cannot know when the client is done with an entry. A tree's flattened form and a
  view's state are dropped when the attachment is detached (the runtime detaches after the job's processes are gone); a
  handle of a dropped view answers `STALE`.
- **Known client limits** (EdenFS [S10], Buildbarn [S5]): no xattrs over v3 (the core's own `user.tierstore.*` attributes
  are on its upper files, never shown to the client); `.nfs*` names for unlinked open files, which the core hides from
  listings as Buildbarn does (`hidden_files_pattern` [S8]); readdir without types costs lookups (`rdirplus` above); the
  first casing of a name becomes the client's.
- **The transport.** Loopback TCP works for EdenFS [S10]; a Unix socket, which Buildbarn uses for NFSv4.0 (it needs
  `NFS_MFLAG_NOCALLBACK` and calls `mount(2)` itself) [S5], is reported buggy by EdenFS [S10] and, by an unconfirmed
  report, failing on macOS 26 [S29]. Probe MP1 decides. The Unix socket is preferred, because a job's profile can deny its
  path, which makes the server's uid check a real fence (6.8); with loopback TCP only, runs whose network is above `Off`
  keep today's copy.
- **Isolation**: 6.8.
- **OCI jobs in a Linux VM on a Mac** (if the farm runs any): the VM shares the tree from the host over VirtioFS or NFS and
  mounts a real overlay inside the guest; the host's VirtioFS metadata speed is unmeasured [S34] and is probe MP5.
- **FSKit** (M2) is deferred (C3): Apple's own engineers say it is not yet suited to a network-backed filesystem, there is
  no `fsync` delivery on URL-backed volumes and no locks, enablement is per user and interactive and cannot be made
  global, kernel-offloaded I/O needs a device node, and reported CPU cost is well above macFUSE's [S18, S20, S25]. It
  returns as a third frontend behind a flag, on the same core, when those change (MP6).

## 9. Edge cases the design handles

| Case | What happens |
|---|---|
| Two jobs attach one tree | One tree number, one set of pins; on Linux two overlays with their own upper directories; on macOS two views. With passthrough, one set of pages per blob. |
| A tree attached, detached and attached again | The same tree number (tree numbers are per digest for the mount's life); new lookups work again. |
| A job's overlay still references a tree whose last attachment was detached | Its lookups keep working (6.5): a tree number is answerable for the mount's life. |
| A host process looked up `trees/<digest>` before it was attached | Not cached (6.3): the attach is seen by the next lookup. |
| A blob is evicted while open | The open files keep the backing file; later opens reuse the live backing id (6.4). |
| A blob of an attached tree reaches the evictor | Skipped while pinned (6.5); if the pin was refused for `pin_limit`, it may be evicted and is filled again on its next open. |
| One blob in two places in a tree, or in many trees | One cache file and, while any is open, one backing id; separate inodes (not reported as hard links); one set of pages. |
| An empty file | No fill, no backing file; every open of that inode is a cached open; reads return 0 bytes. |
| A symlink | Returned as written; resolved by the kernel in the job's namespace, as for a copied symlink. |
| `mmap` of an input | Passthrough maps the cache file [S1]; on macOS the NFS client pages it in. |
| A file the job opens for writing | Copy-up into the upper directory: by the overlay on Linux, by the core in a view (a clone where the volume allows it). |
| A file deleted, then created again under the same name, in a view | Whiteout on delete; the create removes the whiteout and puts the new file in upper (6.6). |
| A directory of the tree deleted and made again, in a view | Whiteout, then `mkdir` over it marks the new directory opaque: none of the tree's children show. |
| A rename of a tree directory | `EXDEV`, as both runtimes' overlays answer today; tools fall back to copying. |
| `tierstore fs` crashes in the middle of a view operation | Its intent is rolled forward at the next start, so the view is before or after the operation, never between (6.6). |
| A request resent after a crash or an update (FUSE resend, NFS retransmission) | Answered from its intent, not run again: a resent `unlink` answers success, not `ENOENT` (6.6, 7.5). |
| An upper directory renamed over a tree directory that the view shows empty | The moved directory is made opaque first; none of the tree's children reappear (6.6). |
| `tierstore fs` crashes during a copy-up | The half copy is in `meta/tmp/`, not at the path; the intent's roll-forward makes it again. |
| The macOS client renames an open, removed file to `.nfs…` | The file keeps its id attribute, so the client's handle still leads to it (6.6). |
| A job creates, renames to or links to a name beginning `.wh.` in a view | `EPERM`, as in an OCI layer. |
| A tree manifest with a name beginning `.wh.` | The attach is refused by name (`unsupported entry name`). |
| A tree manifest that does not hash to its key, does not parse, or breaks a limit | The attach is refused by name; nothing appears. |
| A tree of exactly 2^24 - 1 entries; one of 2^24 | Served; refused (`tree too large`). |
| Two attaches of one tree for one run | One attachment; `detach` ends it once. |
| A detach of a run that attached nothing | Answered `not attached`; nothing changes. |
| `tierstore fs` restarted while no job runs | The descriptor store (Linux) or the derivable handles (macOS) keep the mount; nothing is visible. |
| `tierstore fs` killed while a thread waits on a fill for a job's open (Linux) | The request was read from the stored descriptor, so it is resent to the next instance (7.1, SP2). |
| `tierstore fs` killed with NFSv4.0 opens outstanding (macOS) | No grace period: reclaims are answered `NO_GRACE` and the client opens again (7.5, MP3). |
| A request for a view from another run's user (macOS) | Refused by the server (6.8), whatever Seatbelt does. |
| Node reboot | The mount and the runtime directory are gone with the kernel; `tierstore fs` mounts at boot before the agent; runs that did not survive are detached by the agent's `detach-all-except`. |
| The S3 instance of the node is updated | Fills are answered by the lock holder throughout; pins are re-sent to the new holder (7.3). |
| Upstream slow | First opens of missing blobs wait (recorded in `stall_ms`); files already open are unaffected. |
| Cache volume full | The fill answers `no space`; the open fails `EIO` and the run is an infrastructure failure. |
| The kernel lacks passthrough or resend, or the cache is on a stacked filesystem | `tierstore fs` refuses to start, by name; the runtime keeps materialising (6.7). |

## 10. Compatibility (B8) and one-way doors

New formats, each versioned and additive from its first release:

- **The tree manifest** (6.1): JSON with `v`; readers ignore unknown fields and refuse an unknown kind or version by name.
  The refusal is the one exception to B8's "never refuse unknown data", and deliberate: a reader that skipped an entry it
  did not know would give a build a wrong input tree. So a new kind is written only after every node runs a reader that
  knows it, as jobscheduler sends a new input kind only to hosts that list it. The manifest is
  shared between releases and with clients as soon as any client writes one, which makes it the one-way door of this
  design. The pull request that adds the reader names the test that runs the previous release's reader on a newer
  manifest and the newer reader on the previous release's manifests.
- **The inode and handle layout** (6.3, 6.6: `tree_number << 24 | index`, view handles, depth-first in manifest order,
  number 0 for the namespace) is a format between the releases that serve one live mount. It is written as `layout 1` in
  the header of the table of tree and view numbers, and never changes while a mount lives: a release with another layout
  is not offered the session (7.3, 7.5), and the hook restarts only a node with nothing attached, else defers. That is a
  floor on updating a busy node to such a release, accepted because the alternative is to answer a running job's inodes
  wrongly; a layout change is expected to be rare, and the next boot lifts it. The pull
  request that adds the hand-over names the test in which the next release answers handles the previous one handed out,
  and the reverse.
- **A view's upper directory** (6.6: `root/` with OCI whiteout names and the `user.tierstore.*` attributes; `meta/intents`,
  `meta/tmp`) is read by the release that serves the view and by the runtime's output collector; it is versioned by
  `meta/layout` and lives only as
  long as its run.
- **Source fills and pins** (6.2, 6.5) are cache entries under `_tiering/fill/`, a prefix every release already refuses to
  clients, and a new file `<cache>/pins`. A release without them finds the entries in its index at start and evicts them
  like any other, and ignores the pins file (the cache's scan reads `obj/` and `.stage/` only, `local_cache/layout.rs`;
  to be pinned by a test in the pull request that adds pins).
- **The fill socket, the control socket and the hand-off conversation of `tierstore fs`** are local interfaces between
  processes of one node, not public ones; each is versioned by its first line (`take-fs 1`), and a release offers the
  session only to a taker that lists `handoff-fs-v1`.
- **The state in the runtime directory** (7.1) is local to one node and one mount; a new mount starts it afresh.
- **jobscheduler**: the new input kind is sent only to hosts that list the capability (6.7).
- **The S3 front** does not change.

One-way doors, each a decision of section 13: the tree manifest format and its blob sources (D1), and the place of the
mount and its paths as jobs see them (D3).

## 11. Measurement plan

The plan answers two questions in order, and the second is not run unless the first says it is worth it.

**Q-A: is there anything for laziness to win?** (No new filesystem; the executor and the farm as they are; it runs beside the
macOS probes of phase 0.)

- Instruments (changes to the executor, in Rust (T11), off by default):
  - add the input root's total bytes to the `inputs:` line (the file count is there already);
  - count, per action, the input files it opens and the summed size of those files: a `fanotify` watch (`FAN_OPEN`,
    `FAN_OPEN_EXEC`) on the action's input mounts (the overlay and each shared-tree bind, reached through the container's
    root at `/proc/<pid>/root`), in the executor's daemon behind a flag, one line per lease. `fanotify` reports which files are
    opened, not how many bytes are read; L3 fetches whole blobs on open (6.4), so **opened bytes** is the number L3 would
    fetch, and bytes read is not claimed. Placing those marks needs `CAP_SYS_ADMIN` (C7).
  - macOS has no such watch that an unprivileged daemon can run: there, Q-A measures setup times and bytes cloned, and a
    one-off `fs_usage` run as root on one node over one build gives opened files, labelled as measured on one node.
- Builds: a full build of the komira engine and a full build of the executor itself, with the action cache disabled so every
  action runs, on the farm's Linux nodes and on its macOS nodes, three times each, on a cold node (the executor's node cache and
  the page cache emptied first) and on a warm one.
- Report, per build and per platform: the distribution over actions of input bytes, input files, files opened, opened
  bytes, and the share opened (p50, p90, p99, and weighted by bytes); today's `inputs_ms` and its parts (`walk_ms`,
  `link_ms`, `trees_ms`, `chown_ms`) against `run_ms`; bytes fetched from the CAS per action, cold and warm.
- **Decision rule.** Continue to Q-B if, on either build, `link_ms + trees_ms + chown_ms` (what L3 removes; `walk_ms` is
  the floor of L3's own attach, so it is reported and not counted as saved) is at least 5% of the build's summed `run_ms`,
  or, on cold nodes, opened bytes are under half of input bytes (byte-weighted p50). Otherwise stop: write the result into
  the executor's `worker-cache.md` section 11 and this document, and leave the executor as it is. Either way the executor's "revisit only if" is
  then answered by a measurement. The 5% and the half are judgment calls for the CEO to confirm (C6), not derived figures.

**Q-B: does it win, by how much, and where does it lose?** (Needs the Linux core of phase 1, spike SP5, and on macOS
the frontend of phase 2.)

- Arms, on the same nodes and the same commit, interleaved run by run (A, B, C, D, E, A, ...):
  - **A**: the executor as shipped (cache, hard links, shared trees);
  - **B**: L3 with `fs.passthrough = false` (every inode a cached open; the cost passthrough saves);
  - **C**: L3 with passthrough;
  - **D**: L3 with passthrough and `prefetch = "all"`;
  - **E** (once the core's views exist, for C4): L2, the core's views on Linux, with passthrough;
  - on macOS, arms **M0** (today) and **M1**, with the NFS version MP3 chose.
- State between runs: a **cold** run starts with the executor's node cache, tierstore's cache and pins and the page cache
  (`drop_caches`; `purge` on macOS) emptied; a **warm** run starts after a run of the same arm; a **partial** run starts
  with tierstore's cache holding only the previous commit's blobs. Each state is judged separately.
- Each arm: the two builds, in each state, at the farm's usual concurrency per node and at twice it, five repetitions each.
- Measured, per action: lease-to-result wall time; setup and teardown (`input_fetch_start` to `input_fetch_completed` in
  `ExecutedActionMetadata`, which the executor stamps today, plus `attach_ms` from `tierstore fs`, split into manifest fetch,
  flatten and table append, and `detach_ms`); `client_ms` (SP5's manifest build and put); run time (`run_ms`); `stall_ms`
  (7.4); first-touch latency (open of a blob not yet in the cache) as a distribution, p99 included; outputs time. Per
  node: bytes fetched from upstream against bytes copied or linked in arm A, bytes written to the disk (`/proc/diskstats`;
  `iostat` on macOS), page cache (`Cached` in `/proc/meminfo`, and `file` in each job cgroup's `memory.stat`), major
  faults per job (`pgmajfault`), CPU of `tierstore fs` and of the build-node tierstore under concurrent actions, requests
  by kind and their latency (lookups, negative lookups, getattrs, opens, NFS RPCs by procedure, fills and their wait),
  cache hit rate, evictions against pinned skips and refused pins, backing ids live. Per build: total wall time, failed
  actions and why.
- Report: per arm and state, median, p90 and p99, each with a 95% interval from a bootstrap over the repetitions; every
  figure marked measured or inferred (P11).
- **Decision rule.** Recommend C (or D) for Linux if, on both builds, in every state separately, the median and p90 of
  lease-to-result wall time (setup, `client_ms`, fetch and run together; setup alone moves into run time under L3 and is
  reported, not judged) are not worse than A beyond the intervals' overlap, and no action fails because of the filesystem.
  Bytes written to the disk and bytes fetched are reported as the gain. If C loses on run time (metadata-heavy actions
  paying round trips the kernel's caches should have absorbed), the next candidate is L4 (composefs), whose metadata is
  native and which removes the per-file links the same way, without laziness. macOS: M1 replaces cloning if it passes the
  same rule against M0. For C4: E replaces C if it passes the same rule against C.
- **Soak and faults** (phase 3; not timed): a week of real builds on the farm's nodes with the new path, counting hung-I/O
  incidents (a request waiting over 60 s) and stale-handle errors; and fault runs: kill `tierstore fs` under load with a
  thread waiting on a fill; update it under load; update the build-node tierstore under load; stop the upstream
  mid-build; fill the cache volume. Each must end with every action either succeeding or failing as an infrastructure
  failure (7.4), first-touch p99 during the update reported, and the node clean (no mount but the node-wide one, no
  attachment left).

## 12. Phases, probes and pull requests

Probes and spikes are throwaway Rust programs (T11), run once on a farm node, whose results are written into this
document before the phase that depends on them starts; none is merged.

**Phase 0: probes (gated by C1), and Q-A.** On a farm Mac, at the macOS version the farm runs:

| Probe | Question | Decides |
|---|---|---|
| MP1 | Does `mount(2)` (or `mount_nfs`) of a loopback NFS export need root? Does a Unix-socket transport work, for NFSv4.0 and NFSv3, on current macOS? | 8: the transport; who mounts |
| MP2 | Does a Seatbelt `(subpath ...)` rule confine reads and writes on an NFS mount as on APFS? A `(debug deny)` trace of a job reading another run's view, and its own. | 6.8: whether Seatbelt is one of the two fences |
| MP3 | Stat, open and read throughput, and RPCs per compile, over NFSv4.0 (a minimal server) and NFSv3 (`nfsserve`), against APFS; restart the server under load with files open and locked: does the v4.0 client reclaim, how long is the grace stall, does a stale handle or a dialog appear? Does `execve` from the mount work? | C2: the NFS version; 7.5 |
| MP4 | Does the client honour per-entry attribute changes enough to cache `trees/` long and `views/` short under one mount? | 8: cache times |
| MP5 | `clonefile` cost of a 50,000-file tree (the fallback's size; one unconfirmed report puts a 77,000-file clone at about two minutes [S30]); VirtioFS stat latency from a Linux guest. | M0's cost; OCI jobs on Macs |
| MP6 | (Only if C3 is revisited.) FSKit: can a LaunchDaemon mount with no user logged in; can a device manager pre-approve the extension; may the extension open a socket to tierstore; does `fskitd` run as root? | C3 |

On Linux, in the same phase: Q-A of section 11 (the executor's instruments; phase 0 needs none of the code below), and spikes SP1 to
SP4 and SP5:

| Spike | Question | Decides |
|---|---|---|
| SP1 | On a 6.17 farm node: does a rootless `crun` container mount overlayfs (`userxattr`, the pinned options) with a lower on a root-made FUSE mount (`allow_other`, `default_permissions`, passthrough, `max_stack_depth` 1)? Do copy-up, whiteouts, `mmap`, the reported owner and `EROFS` behave as 6.3 and 6.6 say? Is `/run/tierstore/fs` invisible in the container? How many `GETXATTR` and negative `LOOKUP` requests reach FUSE during a `find` and a compile? Does the mount survive `systemctl restart` of its unit, seen from the agent? Does an attach after a failed lookup of the same digest succeed? | L3 at all (else L2 or L4); the unit of 6.2 |
| SP2 | With `fuser` plus a constructor from a recorded `INIT` reply: serve a connection another process initialised; kill the server while a thread waits on a slow fill, with the descriptor in systemd's store; does `FUSE_NOTIFY_RESEND` from the new server answer it, with no error in the job? Run the hand-over of 7.3 with requests arriving between `ack` and `done`. Is one descriptor read by many threads enough for the farm's concurrency? Does upstream `fuser` take the constructor? | 7.1 to 7.3 |
| SP3 | Passthrough: two inodes sharing one backing id; one inode opened in two processes; a backing id registered by one process and closed by the next; `BackingId::into_raw` across exit; an evicted cache file still open; a view's upper file opened by passthrough for writing. | 6.4, 6.6, 7.1 |
| SP4 | Flatten time and memory of the largest input root of each build (from Q-A's data). | I1's eager metadata (6.3, D7) |
| SP5 | The executor, throwaway: a tree manifest writer from the `Directory` messages with pack locations, plus the container driver changes of 6.7, enough to run the two builds of Q-B. | Q-B can run |

**Phase 1: the Linux core** (if Q-A says continue under C6, or at once if C6 is no). Pull requests, one subject each (P7), each naming its tests:

1. tierstore: this document, with the probes' and spikes' results.
2. tierstore: the tree manifest format, reader and flattener (a library module, no I/O): a test by name for every refusal
   and limit of 6.1, the exact-boundary test of 2^24 - 1 entries, and the cross-version manifest test (B8).
3. tierstore: the fill socket on the build-node instance: source fills under `_tiering/fill/`, its own waiting permits,
   bound only by the lock holder; tests include "twenty cold fills with the S3 path's permits all taken complete, none
   passes through" and "a fill during an update of the S3 instance is answered by the old instance".
4. tierstore: pins in the cache: the pin set, `pin_limit`, the pins file, eviction's skip; tests include "a pinned entry
   survives an eviction pass over budget", "a pin past `pin_limit` is refused and the blob is kept by recency", "a scrub
   removes a pinned entry with bad bytes", and "a release without pins starts on a cache with a pins file".
5. tierstore: the core and `tierstore fs` on Linux, read-only trees, no passthrough: mount, namespace, attachments, the
   control socket, the unit file (with `max_background` 1024); the rewrite of `pluggable-interfaces.md` section 4 and the
   `"fs"` row of `configuration.md`; the terms of the table at the top added to the glossary of tierstore-design.md.
6. tierstore: passthrough, the backing table, the `fs.passthrough` switch and `prefetch` (arms B and D need them).
7. tierstore: crash recovery (descriptor store, recorded `INIT`, resend) and the hand-over (`handoff-fs-v1`); the hook
   learns the second unit; the cross-release layout test.
8. The executor: the Q-A instruments. Numbered here, but it lands first, in phase 0, and needs none of the above.
9. jobscheduler: the tree input kind, its capability, attach and detach in the `crun` runtime, the cache budget in the
   disk offer (its own design document first, B22).
10. The executor: tree manifests, and naming the root digest in a job: the production form of SP5 (its own design document first).

**Phase 2: the macOS frontend** (if MP1 to MP3 pass):

11. tierstore: the core's views (6.6), with their tests: every view case of section 9, a kill at every step of every
    operation of the table of 6.6 followed by roll-forward, a resent request after each, and handle stability across
    copy-up and across the client's rename to `.nfs…`.
12. tierstore: the NFS frontend (the version MP3 chose), its handle scheme, the uid check, two alternating launchd jobs
    and the hook's hand-over arm for them, the NFS hand-off of 7.5 (with the changes to tierstore-handover.md section 0
    and update-hook.md); the drill of updating and killing it under load.
13. jobscheduler: views on the macOS agent (the Seatbelt rules of 6.7), with today's copy as the kill-switch.

**Phase 3: prefetch and soak.** Access-profile prefetch (Buildbarn ADR 10 [S7]) if Q-B's tails ask for it, and the soak of
section 11 on real builds run by the executor.

**Phase 4: FSKit** (only if C3 is revisited and MP6 answers): a third frontend behind a flag, on the same core, measured
against M1 on the same builds, since no benchmark of FSKit for builds exists.

## 13. Decisions

### 13.1 For the CEO

Each is a yes or a no (C4: A or B), with what follows from either answer. C1 to C3 are the decisions the earlier proposal
asked for; C4 to C7 are this document's.

- **C1. Run the macOS probes (MP1 to MP5) on a farm Mac before any macOS work?**
  - Recommendation: **yes.** Every macOS choice rests on them: the transport and who mounts (MP1), whether Seatbelt fences
    a view on NFS (MP2), the NFS version, its crash and hand-off behaviour and `execve` (MP3), attribute caching (MP4), and
    the cost of today's clone fallback (MP5).
  - Cost: throwaway Rust programs on one Mac, about one or two engineer-weeks (an estimate, not a measurement).
  - If no: macOS stays on M0 (today's clone) and phase 2 does not start; Linux is unaffected.
- **C2. Build the core, and its NFS frontend, in Rust, rather than port or run Buildbarn's Go NFS server?**
  - (a) **Rust**: the core in tierstore, and the NFS frontend in Rust over it: for NFSv4.0 a port of the logic of
    Buildbarn's NFSv4.0 server (Apache-2.0, its notice kept [S5]), for NFSv3 the `nfsserve` crate (BSD-3-Clause [S12])
    with the core's handles and `ACCESS`. The version is MP3's answer, not part of this decision.
  - (b) **Go**: run Buildbarn's NFS server and virtual layer as a separate macOS process, in a repository of its own
    (B22), talking to `tierstore fs` over a local socket for trees, fills and pins; the views of 6.6 are written again in
    Go on Buildbarn's virtual layer, which has no copy-up (3.2); the Linux core stays Rust.
  - Recommendation: **(a)**. tierstore is Rust; one core serves FUSE and NFS (and later FSKit); its rules (no `unsafe`,
    coverage, Rust tests) and its hand-off apply to one codebase.
  - If (b): phase 2 becomes a Go project with its own coverage gate and release, and 7.5 is designed again for it.
- **C3. Defer FSKit: not a blocker, not the macOS data path, a third frontend later behind a flag?**
  - Recommendation: **yes.** Apple says it is not yet suited to a network-backed filesystem; it lacks `fsync` delivery on
    URL-backed volumes and locks; enablement is per user and interactive; and its reported CPU cost is high [S18, S20,
    S25]. Probe MP6 is the condition for revisiting.
  - If no: MP6 joins the probes, and phase 4 starts after phase 2.
- **C4. On Linux, where does a job's writable layer live: (A) or (B)?**
  - (A) **The kernel's overlayfs over a read-only FUSE tree (L3)**, as both runtimes mount today: writes never reach
    tierstore; the FUSE server is read-only and fully derivable, so its crash and update are simplest; outputs are
    collected as today. Costs: a stacking-depth budget of 2 (spike SP1 must pass), and two writable layers in one design
    (overlay on Linux, the core's views on macOS).
  - (B) **The core's views on Linux too (L2, the earlier proposal's choice)**: one writable layer for both platforms,
    tested once; no stacking depth; writes to files in `root/` go by passthrough. Costs: every directory operation of
    every job goes through FUSE; the views' intent log is on the Linux crash path; the runtimes' output collectors read the
    views' `root/` (6.6) instead of the overlay's upper; and the views must be built in phase 1, before the macOS probes say
    whether macOS will use them.
  - Recommendation: **(A).** If phase 2 builds the views, Q-B measures them on Linux as arm E, and a later decision may
    switch to (B) on that measurement; this decision does not approve the switch in advance.
  - If (B): the views move into phase 1 (pull request 11 before 5), spike SP1 shrinks to the FUSE mount alone, and arm C
    of Q-B runs on the views.
- **C5. A second process on Linux build nodes holding `CAP_SYS_ADMIN`, with no network?**
  - What an attacker who controls `tierstore fs` gains: `CAP_SYS_ADMIN` in the initial namespace is close to root. It can
    mount and bind-mount over any path of the host (a configuration file, a binary another service runs), change mount
    propagation, and use the other operations the capability gates (namespaces, some cgroup and kernel interfaces). The
    options the unit leaves out to reach the host's mount namespace (6.2) are the ones that would have confined it:
    `ProtectSystem` and `ProtectHome` (read-only or hidden system and home trees), `PrivateDevices` (no device nodes but
    `/dev/fuse`), `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs` and `ProtectControlGroups` (no writes
    to `/proc/sys`, no module loading, no kernel log, no cgroup tree), `PrivateTmp` and `RestrictNamespaces`. What it
    keeps: no network (`AF_UNIX` only), no new privileges, no writable and executable memory, a system-call filter. Its
    inputs are FUSE requests from the kernel, answers on the fill socket, and tree manifests that clients wrote,
    hash-checked and limited before they are parsed (6.1). A member of `fs.client_group` can attach any tree whose
    manifests it can name, which makes those trees readable by digest on the host (6.8), and nothing more.
  - Recommendation: **yes.** The alternatives are the network-facing S3 process holding the capability, which is worse,
    or no passthrough (below).
  - If no: `tierstore fs` holds no capability; a setuid `fusermount3` makes the one mount at boot and hands it the
    descriptor; there is no passthrough, so every read of a job's input is a FUSE round trip (arm B of Q-B becomes the
    design and measures the cost), and the unit keeps every hardening option of `tierstore@.service` that does not stop
    the mount reaching the host.
- **C6. Measure first (Q-A) before building phase 1, and continue only on the rule of section 11 (per-file setup at least
  5% of run time, or actions opening under half of their input bytes on cold nodes)?**
  - Recommendation: **yes, with those thresholds.** They are judgment calls, not derived figures; Q-A answers the executor's own
    condition for revisiting a lazy input root (1.2).
  - If no: phase 1 starts at once, and Q-B answers both questions, after the building rather than before it.
- **C7. Run the executor's `fanotify` instrument of Q-A as root on the measured nodes only, rather than give the executor's daemon
  `CAP_SYS_ADMIN` behind a flag?**
  - Recommendation: **yes, as root on the measured nodes only**, so the executor's daemon gains no privilege in its shipped form.
  - If no: the executor's daemon unit gains the capability when the flag is set, and the executor's own review judges that change (G3).

### 13.2 Decided here (the CEO may overturn any)

- **D1.** tierstore's own tree manifest format, JSON, one object per directory, with blob sources; not REAPI. Why:
  tierstore stays generic, and only the client knows where a blob is (the executor's packs).
- **D2.** The reserved front type `fs` is not used by `tierstore fs`; the pull request that adds it frees the name or uses
  it for the `[fs]` table.
- **D3.** One node-wide mount (Linux `/run/tierstore/fs`, macOS `/private/var/tierstore/fs`), trees at `trees/<digest>`,
  views at `views/<run>`, one owner per node on Linux (`fs.owner`, the action user's host id). Why: one connection or
  endpoint to hand over, no per-job mount to leak.
- **D4.** tierstore does not hash or store outputs. Why: B16's one read is kept by the runtimes' existing collectors, and
  the executor's packing is its own.
- **D5.** All state of `tierstore fs` outside the views (tree and view numbers, attachments, backing table, `INIT` reply)
  is in its runtime directory, and the views' state in their upper directories, written in order and never `fsync`ed.
  Why: it must outlive the process, not the machine; a reboot takes the mount and the runs too.
- **D6.** Attached trees are readable by host users who know the digest, as the executor's cache is today; views only by their
  run's user. Why: parity; a deployment can narrow it with the mountpoint's parent directory.
- **D7.** Metadata eager per tree, data lazy (6.3). Why: inode numbers must be derivable for crash and update.
- **D8.** If Q-B finds laziness does not pay on Linux, the next candidate is composefs (L4), not more FUSE.
- **D9.** Pins are kept by the cache's writer, per attached tree, over the blobs opened (all blobs with prefetch), capped
  by `pin_limit`; not hard links. Why: one writer and one budget for the cache; a refused pin costs speed, never a job.
- **D10.** Files of a tree report one fixed mtime, the value the executor's materialisation uses; the manifest has no mtime field in
  version 1. Why: parity with today, and no field without a client (B20).
- **D11.** No uncached fallback for a non-empty file on Linux: a fill that fails fails the open. Why: the kernel forbids
  mixing cached and passthrough opens of one inode, and an uncached read would be unverified.
- **D12.** No clone of the `/dev/fuse` descriptor unless SP2 shows one is the limit, and then every clone is stored. Why:
  a released clone fails the requests it was serving.
- **D13.** Each knob, counter and optional mode lands only in the pull request whose measurement needs it (`prefetch` and
  `fs.passthrough` in pull request 6, for arms D and B).
- **D14.** A view's upper directory uses the OCI layer format's whiteout names. Why: a known, documented encoding that a
  person and a collector can read, rather than a private one.
- **D15.** A view's changing operations go through an intent log on the view's disk, in fixed idempotent steps, rolled
  forward at start and answering resent requests (6.6). Why: crash ordering alone cannot make a resent `unlink` or
  `rename` answer as it first did; the log is the duplicate request cache of both protocols.
- **D16.** After an NFSv4.0 crash, no grace period: reclaims are answered `NFS4ERR_NO_GRACE` and clients open again (7.5).
  Why: a server without a stable record of its clients must not grant reclaims, and a grace period would stall every open
  on the node for a lease time.

## 14. Alternatives rejected

| Alternative | Why not |
|---|---|
| FUSE inside the job's container (`/dev/fuse` given to the job, a server per job) | A server per job multiplies the update and crash problem by the number of jobs; passthrough needs `CAP_SYS_ADMIN` the container must not have [S1]. |
| One FUSE or NFS mount per job | A connection, a descriptor or endpoint, and a mount to hand over and to clean up per job; jobscheduler's integrity check would see each one. One node-wide mount with per-job overlays or views gives the same isolation. |
| An existing S3 filesystem as the base (Mountpoint, JuiceFS, s3fs, goofys, GeeseFS, Alluxio FUSE) | Built for a mutable, key-addressed namespace; no copy-on-write over an immutable base; macFUSE on macOS [S26, S27]. |
| `fuse-overlayfs` (a userspace overlay) on Linux | Every write would cross to userspace; the kernel overlay is what both runtimes already run, and the core's views are the userspace overlay where one is needed. |
| tierstore reads REAPI `Directory` messages | Binds tierstore to one protocol, and still needs blob locations from the client. |
| Lazy metadata (load a directory on its first lookup, as Buildbarn [S6]) | Inode numbers would depend on lookup order, which a restarted or new process cannot recover (6.3). |
| Source fills under the client-visible key `cas/l/<sha256>` | A `HEAD` would say the object exists on this node when upstream has no such key, and a client could skip an upload; `_tiering/fill/` is invisible to clients on every release. |
| Pins as hard links made by `tierstore fs` in the cache directory | A second writer in a single-writer directory, and the cache's non-eviction removals would leave the linked copy outside the budget (6.5). |
| Pins by an in-memory count only | Lost on the cache's restart and update; the pins file keeps them. |
| A whole-store namespace (`/tierstore/blobs/<digest>`) | Exposes every blob to every reader; the CEO's framing excludes it. |
| An abort of the FUSE connection from `ExecStopPost` after a wait | `ExecStopPost` runs at every stop, including a restart and the old instance's exit at hand-over, before the restart it would wait for [S11]; the unit's start limit gives the same bound with no extra unit. |
| macOS union mounts, `nullfs`, APFS snapshots, library interposition, Endpoint Security redirects | Unsupported, absent, read-only, stripped from hardened binaries, or allow/deny only (4.2, [S30]). |
| FUSE-T or macFUSE on macOS | Licence (FUSE-T [S14]); a user-enabled extension or a kext, licence unchecked (macFUSE [S19]). |
| EROFS over fscache (L5) | Deprecated in the farm's kernel [S16]; read-only, an image format per tree, a privileged daemon. |

## Sources

- [S1] Linux 6.17, FUSE passthrough: `Documentation/filesystems/fuse-passthrough.rst` and `fs/fuse/passthrough.c`:
  https://elixir.bootlin.com/linux/v6.17/source/Documentation/filesystems/fuse-passthrough.rst,
  https://elixir.bootlin.com/linux/v6.17/source/fs/fuse/passthrough.c
- [S2] Linux 6.17, FUSE: `Documentation/filesystems/fuse.rst`, `include/uapi/linux/fuse.h`, `fs/fuse/dir.c` (negative
  entries), `fs/fuse/iomode.c` (passthrough and cached opens), `fs/fuse/dev.c` (release of a device file):
  https://elixir.bootlin.com/linux/v6.17/source/Documentation/filesystems/fuse.rst,
  https://elixir.bootlin.com/linux/v6.17/source/include/uapi/linux/fuse.h,
  https://elixir.bootlin.com/linux/v6.17/source/fs/fuse/dir.c,
  https://elixir.bootlin.com/linux/v6.17/source/fs/fuse/iomode.c,
  https://elixir.bootlin.com/linux/v6.17/source/fs/fuse/dev.c
- [S3] `FUSE_NOTIFY_RESEND`: the kernel commit "fuse: Introduce a new notification type for resend pending requests"
  (protocol 7.40, with `FUSE_HAS_RESEND`; merged in the 6.9 cycle by the commit's listing, unverified), `fuse_resend()` in
  `fs/fuse/dev.c` at 6.17 [S2], and passthrough in 6.9: https://www.phoronix.com/news/Linux-6.9-FUSE-Passthrough.
  Restarting a server so that it keeps its inode numbers is an unmerged proposal: https://lwn.net/Articles/923845/
- [S4] The `fuser` crate: https://crates.io/crates/fuser, https://docs.rs/fuser (changelog and `src/session.rs`,
  `src/lib.rs`, `src/passthrough.rs` in the repository linked there)
- [S5] Buildbarn ADR 0009, NFSv4: https://github.com/buildbarn/bb-adrs/blob/master/0009-nfsv4.md; macOS mount code:
  https://github.com/buildbarn/bb-remote-execution/blob/main/pkg/filesystem/virtual/configuration/nfsv4_mount_darwin.go;
  licences (Apache-2.0): https://github.com/buildbarn/bb-remote-execution, https://github.com/buildbarn/go-xdr
- [S6] Buildbarn virtual filesystem: https://github.com/buildbarn/bb-remote-execution/tree/main/pkg/filesystem/virtual
  (`cas_initial_contents_fetcher.go`, `pool_backed_file_allocator.go`, `blob_access_cas_file_factory.go`)
- [S7] Buildbarn ADR 0010, file system access cache: https://github.com/buildbarn/bb-adrs/blob/master/0010-file-system-access-cache.md
- [S8] Buildbarn worker and virtual filesystem configuration (`maximum_execution_timeout_compensation`,
  `hidden_files_pattern`, lease times, attribute caching):
  https://github.com/buildbarn/bb-remote-execution/blob/main/pkg/proto/configuration/bb_worker/bb_worker.proto,
  https://pkg.go.dev/github.com/buildbarn/bb-remote-execution/pkg/filesystem/virtual/configuration
- [S9] EdenFS: takeover, https://github.com/facebook/sapling/blob/main/eden/fs/docs/Takeover.md; inodes and the overlay's
  materialisation, https://github.com/facebook/sapling/blob/main/eden/fs/docs/Inodes.md,
  https://github.com/facebook/sapling/blob/main/eden/fs/docs/InodeStorage.md
- [S10] EdenFS on macOS (NFSv3 loopback and its limits): https://github.com/facebook/sapling/blob/main/eden/fs/docs/macOS.md
- [S11] systemd: file descriptor store, https://systemd.io/FILE_DESCRIPTOR_STORE/; `sd_notify(3)`,
  https://www.freedesktop.org/software/systemd/man/latest/sd_notify.html; `systemd.service(5)`,
  https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html; `systemd.exec(5)` (namespacing options and
  mount propagation), https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html
- [S12] `nfsserve`, an NFSv3 server in Rust, BSD-3-Clause: https://github.com/xetdata/nfsserve (`src/nfs_handlers.rs`,
  `src/vfs.rs`, `src/tcp.rs`)
- [S13] `embednfs`, an NFSv4.1 server library for the macOS client, MIT: https://crates.io/crates/embednfs
- [S14] FUSE-T, design and licence: https://github.com/macos-fuse-t/fuse-t
- [S15] composefs: https://github.com/containers/composefs
- [S16] EROFS over fscache: https://lwn.net/Articles/894364/; its deprecation in 6.17's `fs/erofs/Kconfig`
  (`EROFS_FS_ONDEMAND`): https://elixir.bootlin.com/linux/v6.17/source/fs/erofs/Kconfig
- [S17] overlayfs: https://docs.kernel.org/filesystems/overlayfs.html; trusted xattrs and `userxattr` in 6.17's
  `fs/overlayfs/params.c`, stacking depth in `fs/overlayfs/super.c`:
  https://elixir.bootlin.com/linux/v6.17/source/fs/overlayfs/params.c,
  https://elixir.bootlin.com/linux/v6.17/source/fs/overlayfs/super.c
- [S18] FSKit (`FSUnaryFileSystem` from macOS 15.4; `FSGenericURLResource` from macOS 26; an app extension enabled in
  System Settings; the macOS 27 handler protocols and `FSClient.mountSingleVolume`): https://developer.apple.com/documentation/fskit,
  https://developer.apple.com/documentation/fskit/building-a-passthrough-file-system,
  https://developer.apple.com/documentation/fskit/fsvolume/handler
- [S19] macFUSE backends: https://github.com/macfuse/macfuse/wiki/FUSE-Backends; releases:
  https://github.com/macfuse/macfuse/releases
- [S20] FSKit cost and limits, as reported to and answered by Apple: a directory read of about 120 µs,
  https://developer.apple.com/forums/thread/793013; no passthrough for file-backed volumes,
  https://developer.apple.com/forums/thread/828533; CPU against macFUSE, ReadWrite not yet optimised, kernel-offloaded
  I/O needs a device node, https://developer.apple.com/forums/thread/799283
- [S21] `clonefile(2)`: https://www.manpagez.com/man/2/clonefile/
- [S22] `mount_nfs(8)` (hard mounts wait for the server; `locallocks`, `rdirplus`, `actimeo`):
  https://www.manpagez.com/man/8/mount_nfs/
- [S23] Buildbarn `bb_virtual_tmp`: https://github.com/buildbarn/bb-remote-execution/blob/main/cmd/bb_virtual_tmp/main.go
- [S24] Apple's NFS client: `mount_nfs.c` (accepts `vers=4.1`),
  https://github.com/apple-oss-distributions/NFS/blob/main/mount_nfs/mount_nfs.c; `kext/nfs4_vnops.c` (open reclaim with
  `CLAIM_PREVIOUS`), https://github.com/apple-oss-distributions/NFS/blob/main/kext/nfs4_vnops.c; attribute cache times
  from the age of the mtime, `kext/nfs_subs.c` (`nfs_attrcachetimeout`), https://github.com/apple-oss-distributions/NFS/blob/main/kext/nfs_subs.c; `nfs_vnop_access`,
  https://github.com/apple-oss-distributions/xnu/blob/xnu-8020.101.4/bsd/nfs/nfs_vnops.c
- [S25] FSKit and network filesystems, enablement and attribution, as reported to and answered by Apple:
  https://developer.apple.com/forums/thread/842736, https://developer.apple.com/forums/thread/808594,
  https://developer.apple.com/forums/thread/831396, https://developer.apple.com/forums/thread/766793; a build tool's
  report of FSKit overhead on uncached operations: https://github.com/bazelbuild/bazel/issues/29165
- [S26] Mountpoint for S3, semantics and configuration: https://github.com/awslabs/mountpoint-s3/blob/main/doc/SEMANTICS.md,
  https://github.com/awslabs/mountpoint-s3/blob/main/doc/CONFIGURATION.md
- [S27] JuiceFS architecture, cache and installation: https://juicefs.com/docs/community/architecture,
  https://juicefs.com/docs/community/guide/cache, https://juicefs.com/docs/community/getting-started/installation
- [S28] Buildbarn, FUSE `max_background` raised after busy workers wedged at the default:
  https://github.com/buildbarn/bb-remote-execution/pull/222
- [S29] A report of Buildbarn's Unix-socket NFS mount failing on macOS 26 (unconfirmed):
  https://github.com/buildbarn/bb-remote-execution/pull/210
- [S30] macOS without an overlay: APFS snapshots, `fs_snapshot_create(2)`,
  https://leancrew.com/all-this/man/man2/fs_snapshot_create.html; library interposition and hardened binaries,
  https://developer.apple.com/forums/thread/703757; Endpoint Security, https://developer.apple.com/forums/thread/129112;
  `clonefile` of a hierarchy blocks it while it runs, https://developer.apple.com/forums/thread/784446; a large clone's
  duration, https://developer.apple.com/forums/thread/49199
- [S31] OCI image layer format, whiteouts and opaque directories:
  https://github.com/opencontainers/image-spec/blob/main/layer.md
- [S32] Seatbelt rules match resolved paths (`subpath`, `literal`, `regex`): https://northpole.security/docs/workshop/rules/sandbox-rules
- [S33] NFSv4.0, grace period and reclaim: https://www.rfc-editor.org/rfc/rfc7530
- [S34] VirtioFS file sharing in a Linux VM on macOS: https://orbstack.dev/blog/fast-filesystem
