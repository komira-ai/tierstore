# Acknowledgement levels for a put: page cache, local disk, hot tier, cold tier

Status: **proposed, not built.** No code, no format and no configuration change comes with this page; its slices (section 16) are not started. What it says about today was read in the code of `main`; each claim names the file. Figures are marked as in [put-without-lock.md](put-without-lock.md): **measured** (with its source), **inferred** (computed from measured figures or read from code, and how), **estimate** (an assumption with no measurement behind it). No figure on this page was measured on AWS, or on a local NVMe or EBS volume. Configurations A to D are the four shapes of the configuration file ([tierstore-design.md](tierstore-design.md), section 4; summarised in 1.4); rule ids (B8, B16, B20, B22) are those of the repository's `AGENTS.md`; the farm is the project's own build and test machines ([docs/farm.md](../farm.md)).

Two layouts matter on AWS. **Layout 1**: an instance's local NVMe, in front of S3 Express One Zone, in front of S3 Standard, with a put that returns once the bytes are on the NVMe while a background writer copies them on. **Layout 2**: an EBS volume as the local disk, in front of S3 Standard. (An EBS volume in front of Express One Zone pays twice for one zone's durability and is not considered.) Both want a put that is answered before the object is in the upstream store, and a caller who can choose how far it must get first, down to an answer once the bytes are in the page cache, which is Kafka's default. So a put gets **levels** of acknowledgement.

## 0. Recommendations

| # | Decision | Recommendation | Section |
|---|---|---|---|
| D1 | The levels | Four, weakest first: `page`, `disk`, `hot`, `cold`. A level is a floor: a put is answered at the asked level or a stronger one, never a weaker one, and the answer names the level reached. | 1 |
| D2 | Who chooses | A key `ack` on `[[policy]]` (per prefix) and a request header `x-tierstore-ack` (per request). With neither, a put behaves exactly as today. | 1.3 |
| D3 | Where write-behind lives | A new **outbox** in the local cache layer (configurations A and C), owned by the one process that holds the cache lock, copying to the node's upstream through the existing `Backend`. Not a new mode of the two-tier hot tier. | 3 |
| D4 | Two writers of one key | Write-behind (`page`, `disk`) only under a prefix whose policy says who writes its keys: `writers = "digest"` (the key names the SHA-256 of its bytes, checked on the put), `"node"` (one node's prefix, claimed in the upstream at start) or `"interchangeable"` (a cache whose writers all write an equally good value). No round trip on the put. | 4 |
| D5 | Order | A put at `hot` or `cold` waits until every earlier `page` or `disk` put of its **order group** has reached that level, or fails with 503 when it cannot; a flush call waits for a whole group. | 5 |
| D6 | The local write | Append-only segment files, one per concurrent body (a **lane**), data and record together, the hash in a trailer after the body, one `fdatasync` per dirty lane per group commit, no timer. | 6 |
| D7 | Back-pressure | Past a bound on uncopied bytes, records or age, a new write-behind put is written through (today's path); at the outbox's budget, 503 `SlowDown`. | 7.3 |
| D8 | Reads from another node | No peer read: an object only one node holds is a 404 elsewhere until it is copied. Every node with an outbox publishes what it has not copied, so a loss can be counted and named. | 9 |
| D9 | S3 Express One Zone as a tier | A separate decision. Section 13 lists what this design needs from any fast tier. | 13 |

## 1. The levels, and what each loses

### 1.1 The four levels

| Level | The put is answered when | Survives | Lost by | Layout 1 (NVMe, Express One Zone, Standard) | Layout 2 (EBS, Standard) |
|---|---|---|---|---|---|
| `page` | the body and its record are written to the outbox's lane file, in the kernel's page cache; no sync | a crash or kill of the process; a hand-over to a new process | a kernel crash, a power loss, the loss of the instance | lost with the instance | lost on a kernel crash or power loss; a clean shutdown writes the page cache out |
| `disk` | the lane file holding them has been synced (`fdatasync`) | the above, and a kernel crash or power loss | the loss of the disk | lost with the instance: an instance-store volume does not outlive it | kept when the instance is lost and the volume is not; lost with the volume or its zone |
| `hot` | the node's upstream acknowledged the put: in configuration C, the proxy's hot tier has it; in configuration A, the upstream store has it | the loss of the node | the loss of the hot tier | in Express One Zone: lost with its zone | not used: the one upstream is the final store, `cold` (1.4) |
| `cold` | the cold tier has it | the loss of a zone | the loss of the region or the bucket | in Standard | in Standard |

`page` is like Kafka's default, where a broker answers once the bytes are in its page cache; but tierstore has no replica on another node, so `page` and `disk` are single-node levels: an acknowledged object is in one place until the copier has copied it (section 7). With Standard alone behind the NVMe (layout 1 without its fast tier) the layout is layout 2 on another disk: `hot` and `cold` are the same store.

### 1.2 A level is a floor

A put is answered only when it has reached at least the level it asked for, and the answer carries `x-tierstore-ack: <level reached>`. It can be answered at a stronger level than it asked for: on a node with no outbox, under a prefix not open to write-behind (4.3), or under back-pressure (7.3). That costs latency and never durability, and the answer header says so. A level the node cannot confirm (`cold` where 1.4 cannot tell `hot` from `cold`) is refused with `501 NotImplemented`, naming the level, before the body is read; an unknown value of the header is `400 InvalidArgument`. The other errors a write-behind put can meet are those of sections 4.3, 5.2, 6.2 and 7.3. Terms used before their section: a record **passes** when the upstream has it or it no longer needs copying (5.2); a **kept** record is a conflict or refusal held for the operator (7.2, 7.4); a **tombstone** is a delete's entry in the outbox index (6.3, 7.5).

### 1.3 Who chooses, and the default

- **Per prefix**: `[[policy]] ack = "page" | "disk" | "hot" | "cold"`. Today `[[policy]]` has two fields, `prefix` and `cache` (`crates/tierstore/src/server/config_policy.rs:8-15`), and is read only by the local cache: without `[local_cache]` a policy is checked and has no effect (`crates/tierstore/docs/configuration.md`, the `[[policy]]` table). That is the right home, since the outbox is in the local cache layer (section 3). The legal combinations of the new keys are in 4.4.
- **Per request**: `x-tierstore-ack: <level>` on a `PUT` or `DELETE`. It may ask for `page` or `disk` only under a prefix open to write-behind (4.3), and for `hot` or `cold` anywhere; elsewhere a weaker ask is answered at the policy's level. The S3 front reads a fixed set of request headers and ignores the rest (`crates/tierstore/src/server/ops.rs`, `auth.rs`; no `x-tierstore-` header is read today), so the header is additive: a release that does not know it answers as today (12.2).
- **The default**: a policy without `ack` and a request without the header get exactly today's put. In configuration A with `write_through = true`, that is a put sent to the upstream before it is answered (`crates/tierstore/src/server/local_cache/mod.rs`, "Writes": nothing is acknowledged before the upstream has it); in configuration B it is answered when the hot tier has it. No outbox directory is made unless `[local_cache.outbox]` is configured.

Multipart uploads are not write-behind: their parts and completion keep today's path at every level (section 15). A large object at `disk` is one `PUT`, up to `server.max_request_bytes` (256 MiB by default).

### 1.4 How `hot` and `cold` are told apart

- **Configuration C** (`[local_cache]` + `[hot_store]` + `[cold_store]`, one process: the cache, then the proxy over hot and cold; `docs/design/tierstore-design.md` section 4): the proxy's answer is `hot`. `cold` needs the proxy to wait for its own copier; that is slice 13 (section 16), with a design of its own, and until then a `cold` request is `501`.
- **Configuration A** (`[local_cache]` + `[cold_store]`): the upstream is one store. Its answer counts as `hot` unless the operator sets `local_cache.upstream_level = "cold"`, which says the upstream is the final store (layout 2: S3 Standard). The default is `hot` because the upstream may be a storage node's tierstore, whose answer means its hot tier has the object. A storage node that implements slice 13 answers `x-tierstore-ack: cold` itself when it has reached it.
- **Configurations B and D** (no local cache) are unchanged: no outbox, so a `page` or `disk` request is answered at the node's own level (`hot`) with the header, and `cold` is `501` until slice 13.

## 2. Today's put, and the target per level

### 2.1 The serial steps today

**Configuration B, entry protocol** (format `active`, which first start gives every `dir:` hot tier, `crates/tierstore/src/server/first_format.rs:27-39`). `put_on_entry` (`crates/tierstore/src/tiered/entry_put.rs:168-197`), each step waiting for the one before:

1. a check that this proxy's registration is live (`require_live`, `entry_put.rs:105-112`); it reads the heartbeat's deadline and makes no request, but a proxy whose heartbeat has not been acknowledged refuses the put;
2. hot `HEAD` of the key (412 if found);
3. create-only write of the entry `(PUT, HELD)` (`take_put`);
4. hot `HEAD` and cold `HEAD` together (`check_tiers`, `entry_put.rs:202-211`); a cold tier that cannot be asked fails the put with 503 (`cold_has`, `entry_put.rs:94-103`);
5. create-only write of the object to the hot tier (to the cold tier when the hot tier is full);
6. entry CAS `HELD` to `PENDING` (`land_put`, `entry_put.rs:144-164`): the acknowledgement; the proxy must still be live before and after it.

Five request waves and three durable writes. On a `dir:` hot tier each write syncs its file (`create_file` and `write_object_stream` in `crates/tierstore/src/dir_write.rs`) and its directory (`publish` in `crates/tierstore/src/dir.rs`, through `crates/tierstore/src/dir_sync.rs`) before it answers, so a put waits for six fsyncs in three serial durable waves (**inferred** from those functions; no test counts them). **Measured**: a new 4 KiB key takes **47.3 ms** p50 through tierstore against 8.5 to 9.3 ms straight to the hot store, with the farm's MinIO as the hot tier, not a `dir:` tier (put-without-lock.md, section 1).

**Configuration B, marker protocol** (format `active-marks`, which first start gives an `s3:` hot tier; `format-epoch --activate-marks` moves an `s3:` hot tier to it, and a hot tier that is not `s3:` has no marks bucket in this release, `crates/tierstore/docs/operations.md`). `put_marked` (`crates/tierstore/src/tiered/marked_put.rs:190-198`): the liveness check, then wave 1, hot `HEAD`, cold `HEAD` and marker `GET` in parallel (`require_absent`, `marked_put.rs:242-272`), then wave 2, the object and a `DIRTY` marker in parallel (`land_marked`, `marked_put.rs:204-239`), which is the acknowledgement: four hot-side requests and a cold `HEAD` (**inferred** from that code). **Inferred**: 17.3 ms for a create-only put of a new 4 KiB key on the farm's MinIO, the sum of measured unit costs and an assumed 1.4 ms of proxy work per wave; the put itself was not timed (put-without-lock.md, sections 6 and 6.1). Both protocols ask the cold tier on every create-only put of a new key, and both refuse it with 503 while the cold tier is down.

**Configuration A, write-through** (`local_cache.write_through = true`). `put_through` (`crates/tierstore/src/server/local_cache/keep.rs`) sends, for a create-only put, one `HEAD` of the key to the upstream first (`local_cache.head_before_put`, on unless set; a plain put sends none), sends the body to the upstream while keeping a best-effort, unsynced local copy, and answers once the upstream has answered and the keep has finished or `keep_wait` has passed. For a create-only put against a storage node on the entry protocol that is about **54 ms** on the farm (**inferred**: a HEAD, the 47.3 ms put and a HEAD, put-without-lock.md 6.1). Against Amazon S3 it is one S3 `HEAD` and one `PUT`; tierstore has no measurement of it.

So on the farm the protocol, not the store, sets today's floor (**inferred**: 47.3 ms through tierstore against 8.5 to 9.3 ms to the store). A `dir:` hot tier on NVMe would still pay the five waves, the cold `HEAD` and the three durable writes.

### 2.2 The target per level

| Level | What is on the answer path | Target p50, 4 KiB, one in flight | Basis |
|---|---|---|---|
| `page` | the front's work (signature check, hash of the body), one write to a lane in the page cache, an index insert | **under 1 ms** | estimate; the front's own cost per put has not been measured apart from a store |
| `disk` | `page`, plus waiting for the group commit's `fdatasync` | **under 2 ms** on NVMe, **under 5 ms** on a gp3 EBS volume | estimate; a sync's cost depends on the device and is unmeasured on AWS |
| `hot` | today's write-through to the upstream (A), or today's proxy put (C), after the order wait of 5.2 | today's figure (2.1); this design promises no change | 2.1 |
| `cold` | in A with `upstream_level = "cold"`, as `hot`; in C, `hot` plus the wait for the proxy's copy to cold | as `hot` in A; in C, seconds while idle until slice 13 adds a wake-up: the reconciler waits 1 s doubling to 10 s between passes, with no wake-up (`crates/tierstore/src/entry_reconciler.rs`, module doc) | 2.1 |

Slice 10 (section 16) measures the `page` and `disk` rows on the farm before any of them is quoted as a result.

## 3. Where the write-behind lives

**Option 1: the two-tier hot tier** (configuration B on a node-local `dir:` hot tier). It already answers before the cold tier has the object. But its protocols were built for a hot tier that many processes and nodes share, and each put pays for that sharing: the entry protocol's five waves and three durable writes, and a cold `HEAD` per new key (with a 503 while cold is down); a put also needs a live registration, and the copier a leader lease. Its copier works in hash or listing order, not put order (`crates/tierstore/src/entry_reconciler.rs`, the step's unordered pass over every entry; `entry_reconciler/marks.rs`, listing order a page at a time), and a full hot tier sends puts straight to cold (entries) or answers 503 (markers). A node-local hot tier cannot be read by another node anyway, so the sharing buys it nothing.

**Option 2 (recommended): an outbox in the local cache layer.** The local cache is already owned by one process at a time: it holds the lock of its directory, a second process waits read-only, and a hand-over passes the lock with the sockets (`crates/tierstore/docs/operations.md`, "The cache lock and read-only mode"). An outbox under the same lock has one writer, so it needs no lease, entry, marker or heartbeat. Its upstream is whatever the cache's upstream is, through the same `Backend` interface: an S3 store (configuration A: layout 2, or layout 1 without its fast tier) or the proxy over hot and cold in the same process (configuration C: layout 1, with Express One Zone as the hot tier once that is decided, section 13). The copier is an ordered queue that calls the upstream's ordinary puts and deletes, so the proxy's own locks, markers and copier are not touched.

The outbox is not the cache. The cache keeps copies it may drop at any time and never syncs them (`operations.md`, "The cache and a power cut"); the outbox holds the only copy of acknowledged objects, syncs them at `disk`, and frees an object only after the upstream acknowledged it. It has its own directory, budget and format (section 6).

## 4. Two writers of one key

### 4.1 The problem

Today the upstream's create-only put is the one arbiter between two writers of a key. In configuration A the cache sends every put upstream before it answers, and its `HEAD` of the key to the upstream only saves work (`crates/tierstore/src/server/local_cache/write.rs`, module doc); in configuration B the put asks the cold tier under its entry or beside its marker. A put that is answered before it reaches the arbiter loses that: nodes N1 and N2 both accept a create-only put of key K at `disk`, both answer 200, and only one copy can land. What the loser's copier sees today depends on the protocol: the entry copier compares the bytes on `AlreadyExists` and keeps the hot object, recording a mismatch that nothing reports (`crates/tierstore/src/entry_reconciler/work.rs`, `publish` and `compare`; `StepReport::mismatched` has no reader in `server/`); the marker copier, finding a cold object that is not its own copy, overwrites it with `If-Match` on its tag (`crates/tierstore/src/entry_reconciler/copier.rs`, `mark_copy` and `mark_publish`). Either way, two clients were told their write of K succeeded, and readers on different nodes see different bytes.

### 4.2 The options

| Option | What a put pays | What it gives up |
|---|---|---|
| (a) Write-behind only where one writer is guaranteed, or two writers are harmless | nothing on the put; a digest check where the key names the bytes; one claim per prefix at start | general S3 semantics under those prefixes: they are not for keys that unrelated clients may both create |
| (b) A claim in the fast tier before the answer: a create-only put of a claim object for K in the hot tier, then the answer | one round trip to the fast tier per put | most of the latency `page` and `disk` exist to save; and it needs a fast tier, which layout 2 does not have |

**Recommendation: (a).** Option (b) makes `page` and `disk` cost about what `hot` costs on a fast tier, which removes the reason for them. The workloads that gain from write-behind (shuffle and spill blocks, model-cache offload, content-addressed build outputs) already have one writer per key or interchangeable writers; the workloads where the put is the commit (table manifests, write-ahead logs) must not be write-behind at all (5.1).

### 4.3 The `writers` rule

A prefix is **open to write-behind** exactly when its policy has `writers`. A policy with `ack = "page"` or `"disk"` and no `writers` is refused at load.

- **`writers = "digest"`**: the last segment of every key is the lowercase hex SHA-256 of the body. The node hashes the body as it arrives (read once, B16) and answers a mismatch with `400 BadDigest`; the record is ended void (6.1), so nothing is stored or visible. Keys are write-once: a put of a key the outbox or the upstream already has carries the same bytes by construction, and a copier that finds the key upstream counts it copied when a `HEAD` shows the same size; another size is a conflict (7.4), which only a writer that broke the rule upstream can cause.
- **`writers = "node"`**: the prefix is this node's alone. At start the node lists the claims under `<state_prefix>claims/` in the upstream: a claim of another node whose prefix equals or nests with one of its own fails the start, naming both; then it writes its own claims, create-only, at `<state_prefix>claims/<sha256 of the prefix>`, naming `[local_cache.outbox] node`. The node is the only writer, so plain puts overwrite: the copier sends a plain put for a plain put and a create-only put for a create-only one. A create-only put answered 200 whose copy then finds the key upstream is a conflict (7.4): the client was told it created a key that existed. Plain overwrites need an upstream that replaces a key: Amazon S3 does, a tierstore storage node on `active-marks` does, and one on `active` (entries, every `dir:` hot tier) answers 412, which the copier keeps as a refusal (7.2).
- **`writers = "interchangeable"`**: every writer of a key writes a value as good as any other's (a cache of results any node can compute, such as a model's prefix cache). Puts are create-only upstream; the first copy to land wins, and a copier that finds the key upstream drops its record after a `HEAD`, counts it `superseded`, and drops any cached copy of its own bytes, so the node serves the upstream's from then on. Until that copy, readers on different nodes may see different values of K: that window is the price of this rule, and only a workload whose values are truly interchangeable may take it.

A create-only put (`If-None-Match: *`) of a key the node's index holds as a live object is `412` at once, under every rule.

The claims of `"node"` are one listing and one create-only write per prefix at start, and they turn a configuration mistake that would corrupt data silently (two nodes given one prefix) into a refused start. They are checked once, at start, never on a put (B20).

**Relation to put-without-lock.md.** That design decided that the per-key lock between proxies stays for every user, with no write-once or content-addressed mode (its decision 5 and section 7.5): dropping the lock for a write-once policy would let eviction, copy-back and delete race on a key that is deleted and recreated in the shared hot tier. This design does not reopen that. The proxy, its lock and its markers are unchanged; the `writers` rule applies only to the outbox, which is in front of the proxy, has one process as writer, and enters the proxy through its ordinary puts and deletes. Q1 (section 17) asks that this be confirmed.

### 4.4 The legal combinations of the policy keys

| `ack` | `writers` | Meaning |
|---|---|---|
| none | none | today's put; a header may ask only `hot` or `cold` |
| none, `hot` or `cold` | set | written through by default; the prefix is open, so a header may ask `page` or `disk` |
| `hot` or `cold` | none | written through at that level; a header may not ask `page` or `disk` |
| `page` or `disk` | set | write-behind at that level by default |
| `page` or `disk` | none | refused at load |

`order` (5.2) may be set with any of these, alone too (a commit prefix with no `ack` of its own). `cache` is independent: a copied object under a `cache = true` prefix is kept in the cache (7.1).

## 5. Order and flush

### 5.1 Why order matters: a table that commits by writing a manifest

Table formats on object storage commit by writing one small object that names data objects written before it: Lance's version files under `_versions/`, Iceberg's metadata files, a Delta log entry under `_delta_log/`. A reader trusts that every object a committed manifest names exists. If the data objects are written at `disk` and the manifest at `hot`, and the manifest is copied while the data objects are not, a lost node leaves a durable, committed manifest that names objects nobody has: the table is corrupt for every reader, with no error at write time. Even with no loss, a reader on another node sees the manifest before the objects it names and fails.

### 5.2 The order rule

Each `[[policy]]` belongs to one **order group**: its own, or the one named by `order = "<name>"`, which several policies may share (Lance's `data/` and `_versions/` are sibling prefixes: two policies, one group). Every outbox record gets a sequence number when it is committed (6.1).

- A put or delete at `hot` or `cold` under group G is a **barrier**. On arrival it takes the group's last assigned sequence number, n; it is sent upstream only when every record of G up to n has passed the level it asked for (records committed after its arrival do not hold it), and then it takes today's write-through path. A commit written at the default level is therefore a barrier, and a client needs no new call. A barrier whose group has no earlier record waits for nothing, so a put under a policy with none of the new keys, which can have no records, is exactly today's put.
- A record **passes** when its copy was created, found the same, was superseded or was cancelled by a delete (7.4, 7.5). A record kept as a conflict or a refusal (7.2, 7.4) does not pass until the operator drops it (`tierstore outbox drop`), which records it as lost (section 9).
- The copier keeps for each group the highest n such that every record up to n has passed: the **watermark**. Copies run in parallel and finish out of order; the watermark moves only over a gap-free run.
- A barrier waits at most `barrier_wait` (30 s). Past it, or at once while a record of G it waits for is kept as a conflict or refusal, it is answered `503 SlowDown` naming the count it waited for and, for a kept record, its key; nothing of the barrier is stored, so the client may retry. A commit is never written over data that has not landed.
- A write-behind put (`page`, `disk`) is not a barrier: it names nothing.

A layout whose commit objects sit under its data prefix (Delta keeps data files at the table root and its log in `_delta_log/` below) cannot be split into sibling policies, because two non-empty policy prefixes may not nest (`crates/tierstore/src/server/config_policy.rs:47-53`). Such a client either sends the header on its commits or keeps the table at the default level (Q2).

### 5.3 The flush call

`POST /_tierstore/v1/flush`, signed as any request, on a node with an outbox, body JSON `{"group": "<name>"}` or `{"prefix": "<prefix>"}`, optional `"level"` (default `hot`) and `"deadline_ms"`. It is a barrier with no object: it answers `200 {"level": ..., "through": <n>}` once every record of the group up to the last one assigned at its arrival has passed the level, and `503` with the count still waiting, and the keys of any kept records, if the deadline passes first or a kept record blocks it (the copier keeps trying). A flush of `{}` covers every group. It follows the existing signed control calls on the S3 port, `/_tierstore/v1/hydrate` and `/_tierstore/v1/release` (`crates/tierstore/src/server/hydrate.rs`: a path outside every bucket, JSON whose unknown fields are ignored); a server without the call answers 404. A read-only key may not flush.

## 6. The local write

### 6.1 Lanes and records

The outbox (`[local_cache.outbox] path`) is a directory of append-only **lane** files. A put that writes behind takes a free lane (there are `lanes` of them, 16 by default, so at most that many bodies are written at once; a put waits for a free lane) and writes one record into it:

- a **header**: format version, record kind (put or delete), a must-understand flag, the key, the asked level, the order group, the write mode (plain or create-only) and the body's length if the request declared one;
- the **body**, as a run of chunks, each a length, a CRC32C and up to 1 MiB of bytes, ended by an empty chunk: the body is streamed once, never held whole, and the outbox sets no size cap of its own (B16; the request limit `server.max_request_bytes` still applies);
- a **trailer**: the sequence number, the body's total length, its SHA-256 (known only now, which is why it follows the body), the outcome, `live` or `void`, and a CRC32C of the header and the trailer.

A body that fails (the client goes away, its checksum or its digest key does not match) is ended with a `void` trailer and its space is dead; nothing of it is indexed or visible. The sequence number is assigned when the trailer is written, so sequence order is commit order. Data and record are in one file, so one `fdatasync` of the lane makes both durable, where a file per object with a journal beside it would pay a file sync and a directory sync per object (the `dir:` tier's discipline, 2.1) and a journal sync besides. When a lane passes `lane_bytes` (64 MiB) after a record, the next record starts a new lane file; the next files are made, and their directory synced, ahead of need, so a put never waits for a directory sync.

This keeps the order rule of `crates/tierstore-journal` (data durable before the record that names it is written: there a data `fdatasync`, then a journal `fdatasync`; `src/lib.rs`, module doc) and reuses its fault-injecting file system for crash tests, not its format: that journal records 1 MiB cells of a pack entry, and nothing in the server uses it today (only `tierstore-entry` and `tierstore-evict` depend on it).

### 6.2 Group commit and the sync per level

- `page`: the record is written (no sync), indexed and answered.
- `disk`: as `page`, then the put joins the next group commit. There is no timer: one round runs at a time, it syncs every lane written since the round before (one `fdatasync` per dirty lane, in parallel), a put that arrives during a round is in the next one, and the next starts as soon as the one before ends. One writer alone pays one sync; many writers share one round. The put is answered when the round that covers its record returns.
- A sync that fails (`EIO`) fails every put of its round (`500 InternalError`). After a failed `fdatasync` the kernel may have dropped the lane's dirty pages, so every record of that lane since its last good sync is suspect: each is re-read and checked against its trailer, those that fail are **lost on a live node** (named by key in the log and in the report, counted in `tierstore_outbox_lost_records_total`, section 9), and those that pass stay for the copier. The outbox then takes no write-behind put until the process restarts (they are written through, reason `sync_failed`, except a put of a key with a record that has not passed, which may not overtake it and is answered `503 SlowDown`): a device that failed a sync is not trusted again without an operator.
- `hot` and `cold` write nothing to the outbox: after the order wait (5.2) they take today's write-through path.

### 6.3 The index, and its size

The index is in memory: for each live record, the key, its lane, offset and length, its sequence number and state, about 100 bytes plus the key (**estimate**). `max_records` (1,000,000 by default, so about 150 MB at 50-byte keys, **estimate**) bounds it with the back-pressure of 7.3. Delete records stay in the index as tombstones until the upstream delete is acknowledged. The index is ordered by key, so a listing can merge it (8.1).

### 6.4 Restart of the process

At start the node takes the cache lock, then replays the outbox. For each lane, from its first live record, it reads each header and trailer, stepping over the body chunk by chunk (one small read per MiB of body), checks the header and trailer CRCs, and rebuilds the index and each group's watermark; the bodies' chunk CRCs and SHA-256 are checked when the copier reads them (7.2), not at start. Replay of a lane stops at its first bad record (torn, zeroed or cut short) and the rest of the lane is dead. That loses no `disk` record: an `fdatasync` of a lane makes every byte before it durable, so a bad record had no completed sync after it in its lane, and every record after it was unsynced as well. Each cut is counted (`tierstore_outbox_dropped_total{why="torn"}`). Replay costs about one read per record and per MiB of body (**estimate**: about 10 s per million records on a volume that serves 100,000 reads a second).

- A **process crash or kill** loses nothing: `page` writes are in the page cache and in the file.
- A **kernel crash or power loss** loses the `page` records written after their lane's last sync, and no `disk` record. Those `page` records may leave no trace, so they cannot all be counted; the start logs, per lane, the time of its last synced record, after which `page` writes may be lost.
- A record whose upstream copy landed but whose passing was not yet written is copied again; a create-only put then finds the key and counts it `same` (7.4), and a plain put or a delete repeats harmlessly.

### 6.5 Hand-over and a process that does not hold the lock

A live update (`docs/design/tierstore-handover.md`) stops the old process's write-behind puts, syncs every lane (so even `page` records are durable), and waits for every copy in flight to answer or be cut before it passes the cache lock; the new process replays. One residual remains: a request the upstream took whose answer never came may still apply after the new process has sent a later record of the same key, so the two land out of order. Every client that retries after a timeout has the same residual today. A barrier or flush waiting in the old process is answered 503 and retried by its client against the new one. A process that does not hold the cache lock answers a write-behind put `503 SlowDown`, since only the outbox's owner may order it.

### 6.6 A downgrade

A release before this design warns about the configuration keys it does not know and starts without them (`crates/tierstore/docs/configuration.md`, the rule for unknown keys), so it runs write-through, the stronger behaviour, and leaves the outbox directory alone; its uncopied records are then neither copied nor served until a release with the outbox starts again. A clean downgrade therefore: removes `ack` from the policies and restarts or hands over on the current release (write-behind stops), waits until every record has passed (a flush of `{}` at `hot` once slice 9 lands; before it, `tierstore_outbox_unpassed_records` at 0), drops or resolves any kept records the flush names, and only then starts the older release. The operations doc says so in slice 6.

## 7. The background copier

### 7.1 Order and parallelism

The copier takes records in sequence order and keeps up to `copy_parallel` (16) copies in flight: a put record becomes an upstream put of its body, read back from its lane (create-only or plain, by the record's write mode and the rule of 4.3), and a delete record an upstream delete. It never runs two records of one key at once and applies them in sequence order, so a put, a delete and a put of one key land upstream in that order. After a put is copied, an object under a `cache = true` prefix is written into the cache as a write-through put's keep is today, unless it was superseded (4.3), and the record passes.

### 7.2 Retries and refusals

A copy that fails for a reason that can pass (503, 429, a timeout, a reset connection) is tried again after `retry_min` (1 s), doubling to `retry_max` (60 s), with no limit on attempts: the copier never gives up an acknowledged write by itself (the backoff of `crates/tierstore/src/entry_reconciler/backoff.rs`, reused). A refusal that will not pass (403, a 412 to a plain put, or a 400 that is not a timeout) keeps that record: counted (`tierstore_outbox_copy_failures_total{reason}`), logged with the key and the answer, listed by `tierstore outbox list`, and holding its group's barriers (5.2) until the operator drops it. Later records of other keys go on. A body read back from a lane is checked against its chunk CRCs and its SHA-256 as it is sent; a mismatch aborts the upstream put before it completes (komira-ai/tierstore#591 covers a short object left by a put whose body failed) and is a record lost on a live node (section 9).

### 7.3 How far behind it may fall, and back-pressure before the disk is full

Three bounds, checked on each write-behind put:

- **bytes**: the bytes of the outbox's lane files above `high_water` (80%) of `[local_cache.outbox] budget`;
- **records**: the index above `max_records`;
- **age**: the oldest record that has not passed older than `max_age` (60 s).

Past any bound, a new `page` or `disk` put is written through: it takes today's path, answered at `hot` (the header says so), counted by `tierstore_outbox_written_through_total{reason}`; it is not a barrier, since it asked for no order. A put of a key that has a record not yet passed cannot overtake it, so it is written into the outbox as asked, past the bound, while there is room. At the budget itself, a write-behind put that cannot be written through without overtaking (or any, while the upstream refuses) is `503 SlowDown` with `Retry-After`. So the outbox stops growing under a slow upstream, puts get slower rather than refused while the upstream is up, and `max_age` bounds the loss window in time as well as in bytes.

The bytes bound counts lane files, not uncopied bodies, because space is freed a whole lane file at a time: a file is deleted once every record in it has passed or is void. A file whose remaining live records are only a few, or only kept ones, is **compacted**: its live records are written forward into a current lane (keeping their sequence numbers) and the file deleted. At start the outbox refuses a `budget` larger than its file system's size less the cache's `budget` when both share one file system, so the bound is reached before the disk is full; the check runs once, at start.

Today's two-tier store has none of these bounds: its usage counts an object only once the copier has looked at it, and a full hot tier sends puts to cold or answers 503 (`crates/tierstore/src/tiered.rs`, `classify` and `level`; `entry_reconciler/usage.rs`, `Tally::copied`).

### 7.4 What a copy finds upstream

| The upstream answers the copy's create-only put | `writers = "digest"` | `"node"` | `"interchangeable"` |
|---|---|---|---|
| created | passes (`created`) | passes (`created`) | passes (`created`) |
| the key exists (`412`) | `HEAD`: the same size passes (`same`); another size is a conflict | a conflict | `HEAD`, then passes (`superseded`) |

A conflict is kept, not retried and not overwritten, so that bytes a client was told were stored are not deleted by tierstore: `tierstore outbox list --conflicts` names it, and the operator drops it or copies it by hand. A plain put of a `"node"` key has no "exists" outcome; its refusals are those of 7.2.

### 7.5 A delete, and a delete of a key not yet copied

A delete under a prefix open to write-behind, at `page` or `disk`, is itself a record: it is committed like a put (synced at `disk`), the index holds a tombstone, and it is answered `204` at once; the client never waits for the copier. The copier cancels the key's earlier records that have not started (they pass as `cancelled` and are never sent), applies the delete after any copy of the key in flight, and sends the upstream delete in their place, since an older version may be upstream. Reads on the node see the tombstone (404) until the upstream delete is acknowledged, so a deleted object is never served again from the upstream meanwhile. Blocks deleted before they were copied (shuffle data) therefore cost one upstream request instead of a put and a delete. A delete at `hot` or `cold` is a barrier (5.2) and goes upstream after the key's earlier records. `DeleteObjects` is one such delete per key. (Today the entry protocol already cancels a copy that has not started, and waits up to `hot_store.delete_wait`, 30 s, for one that has: `crates/tierstore/src/tiered/entry_delete.rs`.)

## 8. Reads

### 8.1 On the writing node

`GET` and `HEAD` ask the outbox index first, then the cache, then the upstream. A live record is served from its lane, whole or by range, with an `ETag` of its SHA-256; after the copy, reads get the upstream's tag, which may differ, as tags of one object may differ between tiers today (`crates/tierstore/docs/limits.md`). A tombstone is a 404. Conditional reads compare against that tag. A listing of a prefix open to write-behind merges the index into the upstream's pages in key order: records not yet copied are added and tombstones are removed, so a client on the writing node (a table's scan of `_versions/`) sees its own writes. `GET` and `HEAD` of a record answer `x-tierstore-ack` with the level it has reached, so a client can ask whether it has been copied.

### 8.2 On another node

Another node does not know the object exists: it asks its own cache and its upstream, and gets 404 until the copy lands. A peer read from the writer's disk is not offered: the only peer protocol is in the storage tier of [fetch-cache-serve.md](fetch-cache-serve.md), which is proposed and not built, and a peer read would turn one node's loss into failed reads on others. So write-behind gives read-your-writes on the writing node only. A workload that reads across nodes before a copy (a reducer on another node) writes at `hot`, or flushes (5.3) before it publishes the names.

## 9. Losses: how they are counted and named

A 404 from the upstream cannot tell "not copied yet" from "lost" from "never written", and nothing on the read path can: the knowledge was on the lost disk. So a node with an outbox must set `[local_cache.outbox] node` and `state_prefix` (the configuration is refused without them when any policy is open to write-behind), and it publishes a **report** to the upstream every `report_interval` (10 s): a create-only object `<state_prefix><node>/report/<n>` with the time, the count, bytes and age of its records that have not passed, the keys of the oldest `report_keys` (10,000) of them, and the keys of records it has lost on a live node or dropped; then it deletes the report before it (create-only writes and deletes work on every upstream, including a hot tier that refuses to replace a key).

- **A node that is lost.** `tierstore outbox lost <node>` reads its last report, asks the upstream for each named key, and prints those still absent: the acknowledged writes that may be lost. It is at most `report_interval` out of date (writes of the last interval are in no report) and names keys only up to `report_keys`, then gives a count. The same verb releases the node's claims (4.3).
- **A node that comes back with an empty or different outbox** (a new instance-store disk under the same node name) finds at start that its last report lists records its replay does not have; it logs them, counts them in `tierstore_outbox_lost_records_total`, and writes them to its next report as lost.
- **A record lost on a live node** (6.2, 7.2, or dropped by the operator, 5.2) is named by key in the log and the report and counted in the same metric.

A reader learns of a loss only through these: the report under `state_prefix`, the metric and the operator's verb. There is no per-read signal; an application that must know per key reads the answer header of its own put, or writes at `hot`.

## 10. Metrics

On the node's `/metricz`, new families (an additive change to the metrics text):

| Family | Kind | Meaning |
|---|---|---|
| `tierstore_outbox_acks_total{level}` | counter | puts and deletes answered, by the level reached |
| `tierstore_outbox_unpassed_records` | gauge | records that have not passed |
| `tierstore_outbox_unpassed_bytes` | gauge | their bytes: the uncopied bytes |
| `tierstore_outbox_lane_bytes` | gauge | the bytes of the lane files (the bytes bound of 7.3) |
| `tierstore_outbox_oldest_unpassed_seconds` | gauge | the age of the oldest: the copy lag |
| `tierstore_outbox_copy_seconds` | histogram | from the answer to the upstream's acknowledgement |
| `tierstore_outbox_passed_total{outcome}` | counter | `created`, `same`, `superseded`, `cancelled` |
| `tierstore_outbox_copy_failures_total{reason}` | counter | failed attempts, by reason |
| `tierstore_outbox_kept_records` | gauge | records kept as conflicts or refusals |
| `tierstore_outbox_written_through_total{reason}` | counter | write-behind puts written through: `bytes`, `records`, `age`, `sync_failed`, `not_open` |
| `tierstore_outbox_barrier_waits_total{outcome}` | counter | barriers and flushes: `passed`, `deadline`, `kept` |
| `tierstore_outbox_dropped_total{why}` | counter | lane tails cut at replay: `torn` |
| `tierstore_outbox_lost_records_total{why}` | counter | acknowledged records lost: `sync_failed`, `read_back`, `missing_at_start`, `dropped` |

Today `/metricz` carries none of this: a storage node exports read counters only (`crates/tierstore/src/metrics.rs`), and the copy lag that the two-tier reconciler publishes in the hot tier (`_tiering/usage`, field `copy_lag`) is the bytes of keys whose copy failed or that it adopted, not the backlog (`entry_reconciler/usage.rs`, `Tally::lags`). komira-ai/tierstore#542 and komira-ai/tierstore#543 (open) are the two-tier store's own signals; this design does not need them.

## 11. What is reused from the parked two-tier work, and what is the wrong shape

**Reused:**
- the `Backend` interface and the configuration C wiring (cache, then proxy; `crates/tierstore/src/server/combined_cli.rs`), so the copier's upstream is any store;
- the per-put switch that already threads through `Backend` and its wrappers, `put_stream_hot_only` and `put_stream_answered` (`crates/tierstore/src/backend.rs`), as the model for carrying a level;
- the cache lock and its hand-over, for the outbox's one owner;
- the reconciler's retry backoff (`entry_reconciler/backoff.rs`);
- `tierstore-journal`'s order rule and fault-injecting file system (6.1);
- the signed control call of `hydrate.rs`, for the flush route;
- the compatibility tests of `crates/compat/tests/`, as the pattern for those of 12.2.

**The wrong shape for this design:**
- **The entry protocol**: three durable writes and five waves per put, and a registration, heartbeat and lease, for a hot tier many proxies share. komira-ai/tierstore#537 removes it once the farm runs on markers; this design needs nothing from it and does not wait for it.
- **The markers**: they serialise writers that share a hot tier, with a second bucket and four hot-side requests and a cold `HEAD` per create-only put (2.1). An outbox with one writer needs no marker, and a fast tier priced per request should not pay them per put.
- **The cold `HEAD` on the put path**, and its 503 while cold is down: the arbiter of a write-behind key is the `writers` rule, not a cold read.
- **Copy order by hash or listing**, and eviction by the write times of the last listing: the outbox needs put order (5.2).
- **`copy_lag` as bytes of failed copies**: the outbox's lag is its backlog (section 10).
- **A full tier that answers 503 or lands in cold**: the outbox writes through before it is full (7.3).
- **The bookkeeping of shared registrations** (komira-ai/tierstore#539, komira-ai/tierstore#541): no value for one owner.

## 12. Tests

### 12.1 Behaviour tests

Each slice of section 16 adds its tests as Rust tests in the crate it touches. None needs a network, a fixed port or a sleep that races a timeout: the copier's upstream is an in-process fake that holds or releases each request on an explicit step, the clock is injected for `max_age`, `barrier_wait` and `report_interval`, and crashes run on a fault-injecting file system that keeps unsynced writes in a page cache it can drop, tear or zero.

| Test | Asserts |
|---|---|
| `a_kill_of_the_process_loses_no_page_or_disk_write` | after n `page` and n `disk` puts and a stop with no sync, a restart serves all 2n and the copier copies each once |
| `a_power_loss_keeps_every_disk_write_and_cuts_page_writes_after_the_last_sync` | with the unsynced pages dropped, every `disk` record is served, the lane is cut at its first bad record, `dropped_total{why="torn"}` counts it, no partial object is served |
| `a_torn_page_record_before_a_synced_one_cannot_occur` | a sync after a `page` record makes it durable: tearing is offered only to bytes after a lane's last sync, and replay keeps every record before it |
| `a_restart_copies_what_was_not_copied_and_nothing_twice` | with the upstream held after k of n copies, a restart sends n - k; a copy that landed before its passing was written counts `same` |
| `a_hand_over_loses_no_page_write_and_waits_for_copies_in_flight` | the old process passes the lock only after its held copies answer; the new one serves and copies every `page` record |
| `a_write_behind_put_without_the_cache_lock_is_503` | a process waiting read-only answers `SlowDown` and stores nothing |
| `a_failed_sync_names_the_records_it_lost_and_writes_through_after` | the round's puts are `500`; suspect records that fail their trailer are named in the log and report and counted; later puts are written through with `reason="sync_failed"` |
| `a_body_that_fails_is_void_and_invisible` | a client that goes away mid-body leaves a `void` record: no index entry, no copy, the next record follows it |
| `a_write_behind_put_outside_an_open_prefix_is_answered_at_the_policy_level` | a `disk` header under a prefix with no `writers` is stored upstream before the answer, which says `hot` |
| `a_digest_key_whose_body_does_not_hash_to_its_name_is_refused` | `400 BadDigest`, a `void` record, nothing upstream |
| `two_nodes_writing_one_digest_key_leave_one_object` | two outboxes over one upstream: one `created`, one `same`, no conflict |
| `a_digest_key_found_upstream_with_another_size_is_a_conflict` | kept, not retried, named by `outbox list --conflicts` |
| `a_node_prefix_claimed_by_another_node_does_not_start` | equal and nested prefixes both fail the start naming both nodes; the other claim is unchanged |
| `node_keys_need_a_state_prefix` | a policy open to write-behind with no `state_prefix` or `node` is refused at load |
| `a_create_only_node_key_found_upstream_is_a_conflict_and_a_plain_one_overwrites` | the create-only copy is kept as a conflict; the plain copy replaces the upstream object |
| `an_interchangeable_key_found_upstream_is_superseded` | the record passes `superseded`, the cache holds no copy of the node's bytes, the next read serves the upstream's |
| `a_create_only_put_of_a_live_indexed_key_is_412` | answered from the index, nothing written |
| `a_commit_at_hot_is_not_sent_until_earlier_records_of_its_group_pass` | with data put A held, a commit C of the same group reaches the upstream only after A is released; a put of another group is not held; a put committed after C's arrival does not hold C |
| `the_watermark_moves_only_over_passed_records` | copies finishing out of order (3 before 2) move the watermark to 1, then to 3; cancelled and superseded records pass |
| `a_barrier_behind_a_kept_record_is_503_naming_it` | the commit is not stored; after `outbox drop` the next commit passes and the drop is counted lost |
| `a_barrier_past_barrier_wait_is_503` | with the clock moved past `barrier_wait`, 503 with the count it waited for; nothing stored |
| `a_flush_answers_when_every_earlier_record_passes` and `a_flush_past_its_deadline_answers_503_with_what_waits` | the upstream has every earlier record at the answer; with one held, 503 names the count |
| `a_write_behind_put_past_each_bound_is_written_through` | one case each for `bytes`, `records` and `age`: answered `hot`, counted by reason, not a barrier |
| `a_put_past_a_bound_does_not_overtake_its_own_key` | with an earlier record of K unpassed, a new put of K goes into the outbox and lands after it |
| `a_put_at_the_budget_with_the_upstream_down_is_503` | `SlowDown` with `Retry-After`, nothing stored |
| `a_lane_file_is_freed_when_its_records_pass_and_compacted_when_few_remain` | the file is deleted; a file holding one kept record is compacted and the record keeps its sequence number |
| `a_delete_of_an_uncopied_key_cancels_its_copy` | the upstream sees one `DELETE` and no `PUT` of the key; `passed_total{outcome="cancelled"}` is 1 |
| `a_deleted_key_is_not_served_from_the_upstream_before_its_delete_lands` | with the upstream delete held, a `GET` on the node is 404 |
| `the_records_of_one_key_land_upstream_in_order` | put, delete, put of K land as put, delete, put |
| `a_listing_on_the_writing_node_shows_its_own_writes` | uncopied keys are listed in key order with the upstream's, tombstoned keys are not |
| `a_lost_node_is_reported_by_key` | with a report written and the outbox then discarded, `outbox lost` prints exactly the reported keys the upstream lacks |
| `a_node_back_with_an_empty_outbox_reports_what_it_lost` | the start counts the keys of its last report that replay lacks and writes them to the next report |
| `a_put_with_no_level_is_exactly_todays_put` | with no `[local_cache.outbox]`, and with one but no `ack` and no header: the existing write-through tests pass unchanged; no outbox directory is made |
| `a_cold_request_the_node_cannot_confirm_is_501_before_the_body` and `an_unknown_level_is_400` | the body is not read; nothing is stored |
| `the_outbox_gauges_match_the_records` | each gauge of section 10 equals the fake's state after each step |

### 12.2 Compatibility (B8)

- **The lane file** is a new on-disk format, version 1, with a fixture `outbox_lane_v1.bin` described in `docs/stored-formats.md`. `a_lane_v1_fixture_replays` reads it. A later record kind is additive: a reader skips a record whose kind it does not know when its must-understand flag is clear, and keeps it untouched (never copied, never freed) when the flag is set, taking no write-behind puts while such a record is live and naming it in the log; it never drops an acknowledged record. `a_record_of_an_unknown_kind_is_skipped_or_kept_by_its_flag` covers both.
- **The report and claim objects** are JSON whose unknown fields are kept; `a_report_with_fields_of_a_later_release_reads` and `a_claim_with_fields_of_a_later_release_reads` read fixtures with extra fields.
- **The previous release on new input**: `the_previous_release_starts_over_a_config_with_the_new_keys` (it warns about `ack`, `writers`, `order`, `upstream_level` and `[local_cache.outbox]` and starts write-through) and `the_previous_release_answers_a_put_with_the_ack_header_as_today` (the header is ignored; the answer has no `x-tierstore-ack`, from which a client knows the level was not applied), in `crates/compat/tests/`.
- **The new release on old input**: `a_put_with_no_level_is_exactly_todays_put` (12.1) is the new release on an old configuration; there is no old outbox to read.

### 12.3 Why this list is complete

Each failure of section 6 (kill, power loss, torn and void records, failed sync, hand-over, the lock, downgrade), each outcome of 7.4 for each rule, each bound of 7.3 and the budget, each rule of order in 5.2 (the barrier, the watermark, kept records, the deadline, the flush), each refusal of 1.2 and 4.3, each loss path of section 9 and each format of 12.2 has a row. The loss of a node is tested through its report, the only place its knowledge survives.

## 13. Not in this design: S3 Express One Zone as a tier

Whether and how Express One Zone becomes a tier (its `CreateSession` authentication, its zonal endpoints, its listings that are not in key order) is a separate decision. tierstore has none of it today: the S3 client signs with the service name `s3` (`crates/objstore-s3/src/s3/sigv4.rs`) and has no `CreateSession` flow, sending only the session token of ordinary temporary credentials (`crates/objstore-s3/src/s3/mod.rs`), and its listing refuses a page that is not in key order (`crates/objstore-s3/src/listing.rs`). This design works without it: layout 2, and layout 1 with Standard alone behind the NVMe, need no fast tier.

What this design needs from any fast tier (the hot tier of configuration C, or the one upstream of configuration A):

1. a create-only `PUT` (`If-None-Match: *`) that refuses an existing key with 412: the arbiter of 7.4;
2. strong read-after-write for `GET`, `HEAD` and listings, so that a copied object is visible to every node at once and a barrier (5.2) means what it says;
3. `HEAD` with the size, and `DELETE`; a plain `PUT` that replaces a key, for `writers = "node"`;
4. room for the claim and report objects of 4.3 and section 9 (create-only writes, deletes and a listing of the claims), under a prefix the operator names;
5. a request price that allows one `PUT` per acknowledged object, plus one report every `report_interval` (the copier sends no more than that), and, if it is the hot tier of configuration C, whatever its own protocol costs per put (today, under markers, four hot-side requests and a cold `HEAD`: 2.1).

It does not need `If-Match`, multipart or versioning for this design.

## 14. Configuration and wire surface proposed

All additive; nothing exists yet. Every default below is an **estimate** with no measurement behind it; slice 10 measures before any is fixed in a release.

| Name | Where | Values and default |
|---|---|---|
| `[local_cache.outbox]` | config table | absent: no outbox, today's behaviour; needs `local_cache.write_through = true` |
| `outbox.path`, `outbox.budget` | config | required in the table |
| `outbox.node`, `outbox.state_prefix` | config | required when any policy is open to write-behind (section 9) |
| `outbox.high_water`, `outbox.max_records`, `outbox.max_age` | config | 80 percent; 1,000,000; `60s` (7.3) |
| `outbox.lanes`, `outbox.lane_bytes` | config | 16; `64MiB` (6.1) |
| `outbox.copy_parallel`, `outbox.retry_min`, `outbox.retry_max` | config | 16; `1s`; `60s` (7.1, 7.2) |
| `outbox.barrier_wait` | config | `30s` (5.2) |
| `outbox.report_interval`, `outbox.report_keys` | config | `10s`; 10,000 (section 9) |
| `local_cache.upstream_level` | config | `"hot"` (default) or `"cold"` (1.4) |
| `policy.ack` | config | `"page"`, `"disk"`, `"hot"`, `"cold"`; none: today's put |
| `policy.writers` | config | `"digest"`, `"node"`, `"interchangeable"` (4.4) |
| `policy.order` | config | a group name; none: the policy's own group |
| `x-tierstore-ack` | request header on `PUT` and `DELETE`; answer header on `PUT`, `DELETE`, `GET`, `HEAD` | a level |
| `POST /_tierstore/v1/flush` | S3 port | 5.3 |
| `tierstore outbox list`, `drop`, `lost` | CLI verbs | 7.2, 7.4, section 9 |
| `tierstore_outbox_*` | `/metricz` | section 10 |
| lane file v1; report and claim JSON | on disk; in the upstream | 12.2 |

## 15. Decisions

Choices this page makes, each with its reason:

- **Level names are tier names** (`hot`, `cold`), not cloud names: they are tierstore's own words for its two tiers (`[hot_store]`, `[cold_store]`); the fast tier is `hot` and the cheap tier `cold`.
- **A stronger answer instead of a refusal** for `page` and `disk` where they are not allowed: the client asked for at least that durability, and the answer header tells it what it got.
- **Multipart is never write-behind**: an upload's parts would have to stay in the outbox until completion, and the completion is a commit; one `PUT` up to 256 MiB covers the workloads of 4.2.
- **Write-behind deletes are not barriers and never wait for the copier**: a delete names no other object, and the tombstone keeps reads right until it lands; holding it behind a backlog would make clean-up of shuffle blocks wait for their own copies.
- **A barrier fails with 503 rather than waiting for ever, or committing over a kept record**: a commit over data that did not land would corrupt the table, and a client that is told to retry can.
- **Back-pressure writes through rather than refusing**, while the upstream is up and nothing is overtaken: it degrades to today's put.
- **The copier never gives up a write that can still land**, and a kept record is dropped only by the operator: a write dropped by a timer would be a silent loss.
- **A failed sync stops write-behind until a restart**: a device that dropped acknowledged pages once is not trusted again without an operator.
- **No full count of `page` writes lost to a power loss**: they may leave no trace; the start names the time after which they may be lost, which is all that is knowable.

## 16. Slices, in build order (one pull request each; none started)

Until slice 8, a policy or header asking `page` or `disk` is refused at load or answered `hot`, so nothing a user runs depends on a half-built outbox; each slice's tests build the outbox directly.

1. **Lane files**: the record format, lanes, group commit, void records, replay and its cut rule, on the fault-injecting file system; `docs/stored-formats.md` row and fixture. A library; no server change.
2. **The index**: the ordered index, tombstones, `max_records`, freeing and compaction of lane files. A library.
3. **The copier**: sequence and per-key order, watermarks, retries, the outcomes of 7.4, cancellation by a delete, against the in-process fake upstream. A library.
4. **Configuration and headers**: `[local_cache.outbox]`, `policy.ack`, `policy.writers`, `policy.order`, `local_cache.upstream_level`, the table of 4.4, the request and answer header, the `501` and `400` of 1.2; `page` and `disk` still refused. `cold` in configuration A with `upstream_level = "cold"` works from here.
5. **The server's write and read paths**: puts and deletes into the outbox, reads, ranges and listings from the index (8.1), replay at start, the copier running; behind the refusal.
6. **Hand-over and the lock**: 6.5, the downgrade steps of 6.6 in the operations doc.
7. **Back-pressure and a failed sync**: the bounds and the budget of 7.3, the start's budget check, 6.2's failed sync.
8. **Reports, losses and the operator's verbs; `writers = "digest"` enabled**: `node`, `state_prefix`, the report, the start-time check of section 9, `outbox list`, `drop` and `lost`, the metrics of section 10, and the operations, configuration and limits docs. The first slice a user can run (content-addressed build outputs).
9. **Order groups, barriers and the flush call**: 5.2 and 5.3.
10. **Measurement**: the `page`, `disk` and `hot` rows of 2.2 on the farm, as a Buck2 target of `storebench` run there, recorded in `docs/measurements/`.
11. **`writers = "node"`**: the claims at start, plain and create-only copies (shuffle and spill blocks).
12. **`writers = "interchangeable"`**: `superseded` and the cache rule of 4.3 (model-cache offload).
13. **`cold` through a two-tier upstream**: the proxy's wait for a key's copy and a wake-up of its reconciler (configuration C), and a storage node's `x-tierstore-ack: cold` answer. It changes the proxy and the reconciler, so it gets a short design of its own first (B22).

Slices 1, 2 and 3 are independent of one another; 4 needs none of them; 5 needs 1 to 4; 6, 7 and 9 need 5; 8 needs 5 and 7; 10, 11 and 12 need 8; 13 needs 4.

## 17. Open questions

- **Q1.** Is a content-addressed and per-node ownership rule *in front of* the proxy (4.3) acceptable, given that put-without-lock.md declined a write-once mode *inside* it? The recommendation depends on it; option (b) of 4.2 is the alternative.
- **Q2.** Table formats whose commit objects sit under their data prefix (Delta) cannot use sibling policies (5.2). Allow a nested policy that sets only `ack` and `order`, or leave them to the header? Recommendation: the header, until a user asks; section 5.1 is why their commits should not be write-behind in any case.
- **Q3.** `max_age` 60 s and `high_water` 80% are guesses at a loss window an operator accepts. Should the defaults be stricter (for example 10 s) at the cost of more written-through puts in bursts?
- **Q4.** Slice 10 measures on the farm only. A run on AWS (an instance-store NVMe volume and a gp3 volume) costs money; until one is run, the `page` and `disk` targets stay estimates.
