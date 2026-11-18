# Fetch, cache, cold tier and serve: per-cell packs, tsfuse and a cold tier on the node drives

Status: **proposed, for the owner's review; not built, and nothing in it was run.** This document is the design of the farm's read path: how a build node fetches the packs a tree names, keeps them in its cache, serves them to jobs through a FUSE mount (an NFS front on macOS), and where the packs live when no node has them (the cold tier on the nodes' hard drives, which replaces the erasure-coded store "silo"). It is written as the system reads once built; each task of section 18 makes part of it true and states so in its pull request. What it says about today was read in the code of `main` and measured on the farm. It does not change [tierstore-design.md](tierstore-design.md) (the hot and cold tiers of a storage node, the entry protocol) or [put-without-lock.md](put-without-lock.md); it replaces the fetch and prefetch parts of [prefetch-api.md](prefetch-api.md) and [node-local-trees.md](node-local-trees.md) and the per-blob serving of [virtual-build-fs.md](virtual-build-fs.md) when the tasks of section 18 land (section 13 lists the code it deletes).

Labels: **[M]** measured on the farm or in a paper, **[V]** vendor guidance, **[K]** Linux kernel behaviour from source knowledge, not yet checked on the farm (each has a named T-F0 or T0 check), **[I]** inference (a named test or measurement settles it), **[D]** a decision of this document.

## Decisions for the owner

Three questions are the owner's to decide. Each has a default that this document is written against; a different answer changes the sections named.

| # | Decision | Default | What the other answer costs |
|---|---|---|---|
| Q1 | **Per-cell digests (the L2 list, section 3).** Every pack carries one sha256 per 1 MiB cell, so any cell is verified on arrival and a read waits only for its own cells. | **Yes.** | Without them a file is verified whole before its first byte (a 1 GiB `.so` waits for 1 GiB and, without SHA-NI, about 2.5 s of hashing), part order is forced, and the hedge and copy failover of 4.6 cannot publish part-way. The CAS service's hashing CPU roughly doubles with them (3.2). |
| Q2 | **Copy count of the cold tier (10.3).** | **2 copies on distinct nodes.** | `copies = 1` doubles unique capacity (about 35 to about 70 TiB with drives kept at 80% full) and halves write bytes, and makes 1/6 of all packs unreachable while any one node restarts, removes the second spindle for hedges and bitrot refetch, and turns every drive loss into misses. Decided after T0-F C-6 measures the live bytes (10.10 states the arithmetic). |
| Q3 | **The trust gap of old trees (3.4).** The tree id covers the v1 file table, not the cell digests; for trees whose L2 sidecar was made by the backfill (3.5), the digests are as trustworthy as the packs the backfill read. | **Leave it.** | Closing it makes the tree id cover `sha256(CELLS)`: every tree id changes and the farm's action cache cold-starts once. A wrong digest can make a good cell fail (a visible EIO or miss) and cannot make a wrong cell pass unless an attacker can also write the list object, which is the trust v1 spans have today. |

Decisions of the CAS service (not the owner's, recorded in T-K1a): whether the CAS service's rebuild or sweep needs LIST from the cold tier (10.9), and that the CAS service treats a 404 on a pack it believes live as "tree not present" (3.5, 10.8).

## Measured farm facts used throughout [M]

- Each node: 2 x ST8000DM004 (5400 rpm, drive-managed SMR, 256 MB cache, 190 MB/s outer-diameter vendor rate), xfs at `/mnt/hdd1` and `/mnt/hdd2`; mq-deadline, `nr_requests` 64, NCQ depth 32, `max_sectors_kb` 4096, `read_ahead_kb` 8192. Six nodes, 12 drives, about 87 TiB raw.
- Cold store today, silo (MinIO lineage): one erasure set of 12 drives over 6 nodes, EC 8+4, 1 MiB erasure block, objects under the CAS bucket. Silo is the legacy store of this design: its geometry matters for the migration window (10.10) and the comparison (10.12).
- Local cache: `/var/lib/tierstore-cache` is xfs on a loop device backed by an image file on the root SATA SSD (about 500 MB/s [I]). Budget 185 GiB, about 204 GB free under the image. Root filesystem type unknown (T0 records it).
- Load average 131 on 88 CPUs: hashing, FUSE and cold-tier CPU compete with builds.
- Kernels: reported as Linux 7.0, not verified; T0 E records `uname -r` and `fuse.enable_uring` per node.
- FUSE baseline, one harness (`ts-fs-open-stall-load`): the fuser fork as shipped in 0.3.119, 2,769 opens/s, p50 74 ms, p99 312 ms; the fork plus the pin fix of komira-ai/tierstore#343, 28,199 opens/s, p50 7.7 ms, p99 26 ms.
- macOS: tierstore-fs on macOS is an NFSv3 server (the komira-ai/nfsserve fork) inside the tierstore process, not fuser.

## 1. Goals and non-goals

**Goals**
- Fetch every pack's needed cells exactly once per node, as ranged parts in parallel, into one cache entry.
- Serve files as spans of that entry. FUSE reader threads never wait on the network, a fill or a hash; metadata is answered from mapped memory.
- One fetch engine for demand reads, mount Prepare prefetch and S3-front misses of large objects; one engine on Linux and macOS.
- **One cold tier owned by tierstore**: every pack lives whole, sequentially, on one drive (times the copy count); no erasure sets.
- Every wait is observable, and every degradation (a stalled drive, an under-replicated pack, a slow consumer) is counted where an operator looks.

**Launch budget.** Request to command start under 200 ms, 100 ms as the target to aim for.
- Reachable only when the command's first spans are local before the action asks; Prepare at job assignment does that.
- Demand-read first-byte target: under 60 ms on an idle farm for a span of 1 MiB or less in a cold pack (a seek of about 14 ms plus a 4 MiB first part at about 150 MB/s on one drive [I]). Under load a demand preempts at the next chunk boundary (10.7), which bounds its drive wait against lower classes to about one 4 MiB chunk plus two seeks plus its own chunk (about 100 ms [I]); against equal-class demands (a cold-start storm) the wait is a slice plus up to `queue_max` queued requests, so only the idle figure is a target. T10b measures both.
- **Launch before Prepare is done.** Nothing gates launch on Prepare. A read on cells not yet present is `demand`: it promotes its part and, if the part has not started, issues a demand sub-range GET (4.5). Each such read increments `launch_before_ready_total` and records `read_wait_seconds{cause="launch_before_prepare"}`.
- Warm open and read: no upstream traffic, no RPC and no lock wait.

**Non-goals**
- Content-defined chunking or dedup across packs.
- A cluster-wide limiter service: admission is at the drive (10.7) and clients do flow control against its signals.
- Changing the pack byte layout.
- Erasure coding, a scan of the drives, or re-replication of packs nobody reads. Copies are restored for packs that are read or were written recently (10.8); an unread pack that lost a copy stays with one, and a pack that lost all is a miss (data is cache).
- Kernel fscache/cachefiles; FUSE passthrough (deleted); any reader of v1 lists.
- A general S3 server in the cold tier: multipart, LIST, versioning, ACLs and overwrite are not offered (10.9). Every key of the cold tier is write-once-ever (10.5).

## 2. Data model

**Identity**
- `PackId = (object key, pack_root)`. `pack_root` is computed by the reader from the list's cell digests (3.1); two lists naming the same pack bytes give the same id and a changed pack cannot alias an old entry. No HEAD is needed.
- S3-front large objects have no digests: `PackId = (key, ETag)` with `digest=none` (section 9). In the cold tier the ETag is the object's **generation id**: the 128-bit blake3 of the object's bytes, computed while they stream in (10.4). It is a function of the content, so every copy of an object holds the same generation, a retried or raced PUT of the same bytes yields the same file name on every drive, and one object is one `PackId` whichever copy served it.
- One cache entry per `PackId`. Packs are immutable.

**Entry on disk** (cache filesystem, section 11)
- Data: `packs-v2/aa/bb/<hash(key, pack_root)>.pack`, a **sparse** file (`ftruncate` to the pack size, never `fallocate`; space is accounted by present cells).
- Journal and snapshot: `<hash>.jnl` (append-only) and `<hash>.meta` (snapshot, atomic rename). Section 8.
- `flock(LOCK_EX)` on the data file is held by the owning tierstore process.

**Cell** = 1 MiB of pack bytes on a grid anchored at pack offset 0 (the last cell may be short). It is the digest unit, the readiness unit and the minimum fetch unit. Cell state is one byte: **low 3 bits state, bit 7 `durable`** (set by the server after the batch `fdatasync` of section 8; meaningful only on `present`).

| State | Meaning | Transitions |
|---|---|---|
| `absent` | no bytes on disk (or not trusted) | to `claimed` by CAS |
| `claimed` | a fetch attempt owns it; bytes in flight | to `present` (verified and written) or back to `absent` (attempt failed) |
| `present` | verified bytes on disk, servable by the mount without RPC | to `evicting` by CAS only |
| `recheck` | bytes are on disk from before an unclean start in another boot (section 8); re-hashed before serving | to `present` (hash matches) or `absent` |
| `evicting` | the evictor owns it; readers must not use it | to `absent` (punched) or back to `present` (evictor timeout) |
| `bad` | failed verification twice; EIO until expiry (60 s) | to `absent` |

- A cell is written once per residency. Only the claim owner writes it, only after its bytes hash to the table digest; CAS `claimed -> present` has exactly one winner, so a hedge or retry cannot publish twice.
- **Every transition is a CAS from a named predecessor state; no state is ever overwritten by a plain store.** The mount's RPC-free read path treats anything but `present` as "not ready".
- The state array lives in a shared memfd that mounts map read-only (5.1).

**Span**: `(pack, off, len)` from the list; a file is one span, or several if it crosses packs. Span readiness = all covering cells `present`. **Part**: the fetch unit (4.4): a cell-aligned run on the part grid, or a demand sub-range.

| Thing | Key |
|---|---|
| Entry | PackId |
| Readiness | cell index within the entry |
| Fill state (single flight) | PackId, in memory, sharded map |
| Hold | (PackId, holder id); holder = mount session or Prepare |
| Verification | cell index (digest from the cell table) |
| Cold-tier location | key hash, from placement (10.2), the location hint (10.6) or the existence query |

## 3. The tree list "L2": per-cell digests, no legacy reader

**Why.** One sha256 per file forces a whole file to be fetched and hashed before its first byte. Per-cell digests make any cell verifiable on arrival, in any order. **[D, Q1 default]** Readers accept only L2; no reader parses v1.

### 3.1 Layout
All integers little-endian, 8-byte-aligned sections addressed by `(off u64, count u64)`, fixed-size records, zero parse (the reader indexes into the mmap). T-FMT fixes the bytes and may keep the v1 layout of spans, ranges, strings and the file-sha table where it is already zero-parse, to limit writer churn:
- **Header, 128 bytes:** magic, `version`, `hdr_len`, flags, `default_cell_shift`, `tree_id` (the identity the list is stored under, 3.5), a section table (packs, spans, ranges, strings, file shas, cells, directories), and a crc32 of the header.
- **PACKS: one 48-byte record per pack** (key string reference, plus):

| Field | Size | Meaning |
|---|---|---|
| `pack_size` | 8 | bytes |
| `cell_first` | 8 | index of this pack's first digest in CELLS |
| `cell_count` | 4 | ceil(pack_size / cell) |
| `cell_shift` | 1 | 20 |
| `pack_flags` | 1 | bit 0: pack fully covered by spans |
| reserved | 2 | zero |
| `upload_part_size` | 8 | **0 for every pack written by the cold tier** (single streaming PUT, no parts). Lists of packs uploaded to silo as multipart keep their recorded nonzero value; it is used only when a fill reads from silo (4.4) and ignored once silo is retired. |

- **CELLS: a flat table of 32-byte sha256 digests**, one per cell, pack by pack: `digest[cell_first + i] = sha256(pack bytes [i << shift, min((i+1) << shift, pack_size)))`.
- **Spans** keep `(pack idx, offset, len, file sha index)`; the per-file sha256 stays as the end-to-end content address used by `tierstore verify` and by non-pack objects.
- **Directory records are sorted by name bytes** (T-FMT verifies the current image order; sorting lets lookup binary-search the mapped records, 5.3).
- `pack_root = sha256("TSPACK1" || pack_size_le64 || shift_u8 || digest[0] || ... || digest[n-1])`, computed when a pack is first opened and cached per list.

### 3.2 How the CAS service computes them
1. While streaming a pack to the cold tier, the CAS service keeps a 1 MiB accumulator, hashes each full cell and appends the digest; the last cell is hashed short.
2. The CAS service records `pack_size` and `cell_shift`. **The CAS service uploads every pack with one streaming PUT** (HTTP `Content-Length` if the pack size is known when the upload starts, else chunked transfer encoding) to tierstore's S3 front (10.9). There are no multipart parts; packs written to silo before the cutover keep their recorded `upload_part_size`.
3. The list is finalized by writing PACKS and CELLS, then the header last. Per-file sha256 is still computed, so the CAS service's hashing CPU roughly doubles; T-FMT measures it (the hashing pipelines with the upload).
4. A deterministic-output test (same inputs, same bytes) covers the new sections.

### 3.3 How a reader verifies a cell on arrival
1. Part bytes stream into pooled 1 MiB cell buffers (a part is a sequence of whole cells).
2. A full buffer goes to the bounded hash pool (4.7), which compares sha256 with `CELLS[cell_first + i]`.
3. Match: `pwrite` at `cell << shift`, CAS `claimed -> present`, wake waiters. Bytes are verified before they touch the file, so nothing is re-read to be verified and no later write can replace verified bytes.
4. Mismatch: 4.6 (with two cold copies the refetch goes to the other copy). A `digest=none` pack skips client verification (section 9).

### 3.4 What the reader does not do, and what the list's integrity rests on
No per-file hash on the read path: a file is correct if every covering cell is `present`. **What pins the list** depends on how the tree reference is derived, which T-FMT answers first (T-FMT Q1):
- If the reference pins the L2 list bytes, cell digests are authenticated by it.
- If it pins only the v1-canonical file table (the likely case, 3.5), cell digests are as trustworthy as the list object in the cold tier, which has the same trust as the pack bytes beside it, plus the header crc and an offline cross-check: `tierstore verify` re-derives per-file sha256 from cells and compares with the file-sha table that the tree id covers. A wrong L2 digest can make a good cell fail verification (a visible EIO or miss), and cannot make a wrong cell pass unless the attacker can also write the list object. This gap exists today for v1 spans and is not widened. Closing it is decision Q3 (default: leave it).

### 3.5 Rollout order, tree identity, refusal and rollback
**Tree identity [D].** The tree id does not change. The CAS service computes the id from exactly the bytes it hashes today (the v1-canonical serialization, computed in memory) and stores the **L2 list as an additional object under the same id** (a sidecar next to the v1 list). Action-cache keys that include tree ids are therefore unchanged and the format change invalidates no action-cache entry. **T-FMT Q1 is the first step (read only): how is the id derived today.** If it is not derived from list bytes, the sidecar is trivial; if it is derived from the v1 bytes, the CAS service keeps computing those bytes for the id only; if it cannot be preserved, the owner is told before anything ships that the cutover cold-starts the action cache.

**Dual emission for exactly one release.** The release that adds L2 makes the CAS service write, for every new tree, both the v1 list and the L2 sidecar. Old readers ignore the sidecar; new readers read only the sidecar. After every reader is new (`tierstore_build_info` on all nodes including the Macs), a following release of the CAS service stops storing v1 (the id is still computed from the canonical bytes in memory).

**Order.**
1. **The CAS service's dual emission ships first.** It is invisible to old readers. The CAS service also runs the id-preserving backfill (`l2-backfill`) over trees referenced in the last N days, rate limited, so recent trees have a sidecar before any reader needs it. The backfill reads the packs once to compute cell digests; T-FMT estimates its cost per TiB, and it runs off-peak.
2. **Readers roll node by node** under the one-live-update-at-a-time coordinator. On an updated node a tree with no L2 sidecar is a miss.
3. When all nodes are new and the sidecar coverage is complete (`l2_sidecar_missing_total` zero for 7 days), the CAS service stops writing v1.

**Refusal.** A new reader never parses v1. A tree with no L2 sidecar returns a distinct `ListVersionUnsupported` status to the caller (`list_refused_total{reason}`). **The CAS service treats that status as "not mountable through tierstore" and takes its miss path (materialize the tree from the cold tier without the mount, or rebuild and republish); it never fails the job because of it** (T-K1a confirms the miss path exists and builds it before step 2 if it does not).

**Rollback.** During the dual-emission release both lists are stored, so rolling readers back loses nothing. After v1 stops, rolling readers back makes trees unreadable (misses) until readers roll forward; the runbook (T13) says to fix forward. Wrong bytes are never possible in either case.

## 4. Fetch engine

### 4.1 Single flight
One `PackFill` per `PackId` in a sharded map, no lock across I/O. It owns the cell state array, the part queue, the claim table, the pack's cold-tier location (the copies found, each with its generation, 10.6) and the fill's **pinned generation**: the generation of the hinted copy, else of the first copy that answers, set once when the location is first resolved and never changed for the life of the fill. A silo-origin pack has no cold-tier location: its pinned generation is the ETag of its first response, which is issued alone (section 9), and no later part starts before it is set. Any reader, prefetch or S3-front miss calls `ensure(pack, cell_runs, class, rank)`, which attaches to the existing fill and raises priority; it never starts a second fill.

### 4.2 Claims
- A fetch attempt claims only the cells it is about to fetch by CAS `absent -> claimed`; `claimed` and `present` cells are skipped, so a part overlapping an in-flight demand sub-range splits into runs of still-absent cells.
- If the owner fails, its cells return to `absent` and a waiter's demand re-claims them.
- **Validation:** no cell is fetched twice except by retry after failure or verification mismatch. GETs are counted by kind (`grid_part`, `demand_sub`, `retry`, `hedge`); the acceptance bound is GETs <= grid parts + demand sub-ranges + retries + hedges, and `bytes_fetched == bytes_needed + wasted`.

### 4.3 Budget: per-drive flow control under a node ceiling
**What limits the cold tier.** Each drive serves at most `s*` sequential streams at a time (default 2, the server's `drive.streams`), admitted and ordered by that drive's scheduler (10.7). Admission is **server-side and authoritative**. A part is one contiguous byte range of one file on one drive, so one part is one stream on one drive; there is no striping across drives and no erasure width. The client's budget is flow control against what each drive advertises, not a model of the drives.

**Quantities, stated separately.** With D = 12 drives, N = 6 nodes, s* = 2, and P the current part size:

| Quantity | Formula | At D=12, N=6, s*=2 |
|---|---|---|
| Cluster stream capacity | `G = s* x D` parts | 24 parts |
| Per-node share | `share = ceil(G / nodes_active)` parts | 4 parts (`nodes_active` from the jobscheduler hint, default N if missing or older than 10 s) |
| Per-node floor | 1 part at depth 1 at `P_floor` = 16 MiB, plus the demand reserve | 16 MiB per node, 96 MiB cluster |
| Per-node ceiling | `2 x max(share, floor)` parts until T0-F replaces it with the measured knee | 8 parts |
| Bytes in flight at share | `share x P` | 256 MiB per node and 1.5 GiB cluster at P = 64 MiB; 64 MiB per node at P = 16 MiB |
| Per-drive permits | `per_drive_max[d]`, 1 or 2, **one value per drive** | 2 per drive when that drive is healthy |

The budget unit is the part, so a part size never exceeds the per-node share: share is at least one part whatever P is. A node's parts land on drives by hash, so the expected load at full share is G/D = s* streams per drive; overload is absorbed by the server queue (10.7), never by collapse.

**Two levels of state [D].** The controller keeps **per-drive state** and a **node ceiling**:
- *Per drive d:* `per_drive_max[d]`, a baseline and a decrease state of its own, fed by signals that drive d produced (its 503s, its `x-ts-queue-wait-ms` and `x-ts-service-ms`, first-byte time of GETs it served). A 503 or a doubled queue wait from drive d decreases **only drive d's permits** (2 to 1, then pacing of d), moves d's copies to the end of the read preference for that node, and leaves every other drive's fills untouched. A hot pack that queues its two drives therefore throttles those two drives and nothing else.
- *Node ceiling:* the total parts in flight across all drives, bounded by the table above and decreased only by node-wide evidence: the network signal (a) above 2 x its baseline for two windows (the node's own link or CPU is the bottleneck), or more than a third of the drives the node has traffic on in a decrease state at once (the farm as a whole is loaded). One slow drive cannot hide inside a pooled median, because no median is pooled across drives.

**Parameters [I until T0-F]:** `s* = 2`, `per_drive_max` up to 2, P from 4.4.

**Floor is a rate floor.** Under a sustained decrease state a drive steps down its own ladder, one step per window: (1) `per_drive_max` 2 to 1; (2) pacing: a minimum gap between part starts to that drive, doubling per window up to 2 s. The node ceiling has its own ladder: (1) fewer parts in flight; (2) P toward 16 MiB (64 -> 32 -> 16). P is one node-level value per size class (4.4), never per drive, so both copies of a pack share one part grid and parts stay interchangeable for hedges, failover and striping. A node never stops (one part per 2 s per drive at worst) and the farm can back off below one stream per drive.

**Controller**
- Window `max(16 parts, 2 s)`, per drive and for the ceiling. Increase only with backlog (queued parts and the permits fully used); slow start (double toward the share, then +1 part per window); state persists across idle.
- **Signals, normalized per size class:** (a) network time per MiB = (last byte - first byte) / size; (b) first-byte time; (c) **the drive's own report**: every cold response carries `x-ts-queue-wait-ms` (time the request waited for a stream slot), `x-ts-service-ms` and `x-ts-drive-state` (10.7), so the controller sees each drive's queue directly instead of inferring it. Local write and hash time are excluded.
- Baselines: windowed minimum (10 minutes) of the median of each signal per size class, **per drive** for (b) and (c), per node for (a). Decrease of drive d: median of d's (b) or (c) above 2 x d's baseline for two windows, or any `503 Retry-After` from d -> d's permits x 0.75 rounded down, at least the ladder step; then a cooldown of one window with +-50% jitter. A response with `x-ts-drive-state: stalled` or `indexing` is not a load signal: the copy is skipped (4.6) and d's permits are not changed.
- Local back-pressure is separate: the dirty-byte cap (4.8).
- Topology is the membership document (10.8) and `s*`; there are no per-fill concurrency knobs.

**Demand reserve.** `max(1 demand part, 25% of the node ceiling)` is held back from `prepare` and `background`, and one permit of every drive is reserved for `demand` while that drive has queued demand. At the drive the demand class also preempts (10.7), so the reserve buys drive time and not only node bytes.

### 4.4 Part sizing and the part grid
A part is a contiguous run of one pack file on one drive read as a stream: it reads at the drive's sequential rate whatever P is, as long as it continues in ascending order; P trades request overhead against first-byte time for the waiting reader and memory. Starting values (T0-F replaces them):

| Pack size | First piece | Ramp | Steady P |
|---|---|---|---|
| <= 32 MiB | first-touch cells (>= 2, <= 4 MiB), then one GET for the rest | - | - |
| 32 MiB to 512 MiB | 4 MiB | doubling | 32 MiB |
| > 512 MiB | 4 MiB | doubling | 64 MiB |

- **Grid.** The steady grid is anchored at pack offset 0 with cell-aligned pitch P. **The ramp is defined relative to the grid:** it covers `[first-touch cell start, next grid boundary)` (or to the pack end) in doubling pieces of 4, 8, 16 ... MiB capped at P/2, the last piece trimmed to the boundary. After the ramp every part is a whole grid part. A first touch in the middle of a pack costs one ramp and no further runts; a first touch at offset 0 ramps 4+8+16+32 = 60 MiB and the next piece is trimmed at the 64 MiB boundary. Demand sub-ranges split grid parts into runs of absent cells; a run is a valid fetch unit.
- **No upload-part boundary exists in the cold tier** (10.4). Only when a fill's source is the legacy upstream (silo, 10.10) and the pack's `upload_part_size != 0` are parts also cut at those boundaries; a fill from the cold tier ignores the recorded value. The cut makes each silo drive read one part file per GET; this rule is deleted with silo (T11b).
- **First part small (first-byte lever).** At least 2 cells, at most 4 MiB: on an idle drive about one seek plus 4 MiB at the media rate. If Prepare entered with a plan, the first part is the plan's first run.
- **Adaptation.** P moves within {16, 32, 64} MiB. While backlog exists, 1 part in 8 is issued at the next size up (the probe arm). Per size class, goodput per in-flight byte and drive-reported service time are compared over 10 minutes; move up if the probe is >= 10% better at equal-or-lower p95 first byte, back down if >= 10% worse. P is a node-level value per size class; a node-ceiling decrease state also steps it down (4.3).
- **Striping one pack across its copies (optional, measured).** With 2 copies holding the same generation, parts of a pack larger than 128 MiB alternate between the two copy drives, so two drives stream one pack in parallel (each skips the other's parts: a seek over a P-sized gap, amortized over P bytes). T0-F arm C-3 compares one copy streaming against two copies striped; default on only if it wins by >= 20% at equal p99.

### 4.5 Order of parts, demand sub-ranges, fairness across jobs
- Classes: `demand` > `prepare` > `background`. The class, job id and rank travel to the drive in request headers (`x-ts-class`, `x-ts-job`, `x-ts-rank`) so the scheduler (10.7) applies the same order.
- **Within a class: deficit round robin across jobs**, quantum one part of the current P, oldest job first on ties. **Within a job** the key is `(rank, offset)`, where `rank` is the first-touch rank of the first needed cell inside the part (profile order, else tree order).
- **Active-pack window.** At most 3 packs are partly filled per class per node; each pack's parts go in ascending offset order, so the drive sees ascending reads of one file.
- A `demand` on a part that has not started promotes it. If the needed span is 1 MiB or less (4 MiB with alignment) and the part has not started, the engine issues a **demand sub-range GET** of exactly the covering cells (1 to 4 cells), `demand` class; later grid parts skip the claimed cells.
- A demand inside a started part waits for that cell only (cells publish as they land).

### 4.6 Retries, stalls, hedging, verification failures, lost packs
- **Generation known before the first byte.** The location of a cold pack (placement, location hint or existence query, 10.6) carries each copy's generation and size, because both are in the file name (10.4). Every GET of a fill carries `If-Match: <the fill's pinned generation>` (4.1), so one fill never takes bytes of two generations, even for a `digest=none` object whose cells the client cannot verify. For a silo-origin pack the generation is the ETag of the first response, learned with the first part alone (section 9).
- **Copies of another generation are excluded per fill.** A copy that lists or answers (412) another generation holds other bytes under the key (a defect, 10.5): it is dropped from this fill's location, `cold_copy_generation_mismatch_total` counts it, and parts go only to copies of the pinned generation. The fill fails only when no reachable copy holds it.
- **Stall detection.** After the first byte: no bytes for `max(500 ms, 8 x baseline net time per MiB x 1 MiB)`. **Before the first byte** a GET is not a stall because of time alone (a queued drive is normal under load): the pre-first-byte timeout is `max(2 s, 8 x first-byte p99 baseline of the size class)`, and a response that already said `x-ts-queue-wait-ms` is not a stall while the drive is making progress. A response or ping saying the drive is `stalled` (10.7) ends the GET at once. A stalled part is cancelled and retried, **on the other copy if one exists, else on a fresh connection**, up to 3 attempts, jittered backoff.
- **Stall retries are budgeted.** A retry waits for a permit like any part, at most one stall retry per drive per window, and while that drive is in a decrease state only transport-level failures (reset, connect error, node down, drive `stalled`) are retried at once; everything else waits for the state to clear.
- **Hedge** only for `demand` class, only when the pack has two reachable copies with the same generation, the other copy's advertised queue wait is shorter, neither drive is in a decrease state, and at most one hedge permit per node. Trigger: no first byte after `max(30 ms, 2 x first-byte p50 baseline)`. The hedge shares the claim; whichever attempt first delivers a verifying cell publishes, the other's bytes are dropped, never written. A hedge goes to a different spindle, so it routes around one slow drive; with one copy a demand is never hedged.
- **Verification failure:** the claim is released and the cell refetched once as a demand sub-range **from the other copy if there is one** (else the same drive on a fresh connection). A second mismatch marks that cell `bad` for 60 s: reads covering it return EIO, other cells are unaffected. The serving drive is told (`x-ts-report: bitrot`); it quarantines that file (10.8) and the copy is restored by repair.
- **A pack lost mid-job [D].** A running job's tree may name a pack that every copy then answers 404 for (the CAS service deleted it, or every copy was lost). Tier eviction cannot cause this while the tree is held, because holds refresh the cold-tier recency of their packs and the tier never evicts a pack touched within `cold.evict_min_age` (10.8). When it happens anyway: the existence query (10.6) is asked once more, bypassing every negative cache; a hit is served. If the miss is **not definitive** (10.6 step 4: a node `down`, or a probe drive `indexing` or `stalled`), the fill waits and retries with backoff for up to `cold.unavailable_wait` (default 10 minutes, longer than a routine reboot), so a reboot of the node holding the only copy delays the job and does not fail it. If the miss is definitive, or still not definitive when that wait ends, the fill is **lost**: waiters of unsatisfied cells get EIO, the tree's mount is marked lost for every holder, `cold_pack_lost_total{phase="mid_job"}` counts it, and the mount provider reports each job holding the tree as failed with the **retriable** cause `tree-lost` to the jobscheduler (T-J1), which requeues it. On the retry the CAS service finds the tree not present and takes its miss path (republish or rebuild). No wrong byte is ever served, and the job is retried rather than failed. One lost pack marks the whole tree lost on purpose: a tree with a hole cannot build correctly.
- No reachable copy because a node is `down` or a probe drive is `stalled` or `indexing` is a non-definitive miss and takes the wait of the bullet above, never an immediate EIO. Other pack-level failure after retries (verification failing on every copy, a copy set the location rules exclude entirely): waiters of unsatisfied cells get EIO; the entry stays resumable and the next read retries.

### 4.7 Hash pool and CPU
Dedicated bounded pool, not `spawn_blocking`. Threads = `ceil(1.2 x target_rate / core_sha_rate)`, with `target_rate` the cache write ceiling (about 500 MB/s) and `core_sha_rate` measured at startup (1.5 GB/s with SHA-NI, about 0.4 GB/s without). Queue bound 32 cells; when full, receive pauses (`part_hash_wait_seconds`). The tierstore service has a higher `cpu.weight` than job scopes. `recheck` cells are hashed by the same pool at lower priority. A paused receive is a slow consumer for the serving drive; the drive's read-ahead buffer and slot release (10.7) keep it from holding the spindle.

### 4.8 Memory, dirty pages and DONTNEED
- Parts stream into pooled 1 MiB cell buffers; in-flight memory is at most `ceiling + 32 MiB` per node. A locally served cold read (copy on this node) reads with `O_DIRECT` straight into these buffers: no network, no second copy.
- **Dirty cap** `min(256 MiB, 2 x node ceiling)`: cells wait before `pwrite` when reached (`dirty_bytes`, `writeback_stall_seconds`).
- **DONTNEED is server-only and keyed to the `durable` bit.** The journal committer (section 8) sets `durable` on a cell after the batch `fdatasync`. Only the server issues `posix_fadvise(DONTNEED)`, only on cell-aligned ranges of `durable` cells, because DONTNEED on a dirty page does nothing (the mount cannot know a cell is durable). Policies are arms of T0 B and the section 17 benchmark: (a) never, (b) at commit, (c) when the pack-file page cache exceeds a cap, oldest durable groups first. Default (a) until data says otherwise. Races: DONTNEED against a splice in flight is safe (the pipe holds the old, correct pages); against a concurrent `pread` it is a re-read (performance); against a refetch after punch it is the same bytes; it can drop a neighbouring file's pages in the same cell (thrash on shared packs, never correctness).
- Parked FUSE replies hold no buffer; a pooled buffer is taken when the cell is ready.

### 4.9 Connections
One pooled HTTP client with a per-host pool of at least `ceiling / smallest part`, keep-alive, no per-request handshake. Peer connections go to each node's cold service address on the private network (10.6).

## 5. Serving

### 5.1 State page, reader counters and the eviction protocol
`open-pack` returns the **data fd** and a **state memfd** over the unix socket (SCM_RIGHTS). The data fd guarantees the right inode if the path is unlinked or recreated; the mount never reopens by path.

**State memfd.** The server creates the memfd, maps it read-write, writes the cell state bytes (and group touch epochs), then applies `F_SEAL_FUTURE_WRITE | F_SEAL_SHRINK | F_SEAL_GROW` and `F_SEAL_SEAL` before passing it. [K] `F_SEAL_FUTURE_WRITE` forbids new writable mappings and `write(2)` while the server's existing writable mapping stays valid. The mount maps it `PROT_READ` only and cannot mark a cell `present` or alter another mount's data. A conformance test asserts that a `PROT_WRITE` mapping from the passed descriptor fails (T-F7, T1).

**Counter memfd.** Each mount (holder) creates **one counter memfd for its whole life**, maps it read-write, and sends it to the server at handshake; the server maps it read-only. Inside it the mount allocates a **region per open pack**: one u16 reader counter per 16-cell group (256 groups x 2 B = 512 B for a 4 GiB pack), and tells the server `(region offset)` in `open-pack`. There is no slot table and no cap per pack, so the most shared packs (toolchains) are never refused. A new mount process always has a new memfd, so a counter is never reused across processes and cannot be underflowed by a late teardown of an old one. The memfd grows (`ftruncate`) as the mount opens more packs; a region offset beyond the server's current mapping makes the server remap that holder's memfd at the new size before it acknowledges the `open-pack`, so the evictor never reads past its mapping.
- **The server never zeroes or drops a live holder's counters.** Liveness is a **lifeline**: at handshake the mount also sends one end of a `socketpair` that is held open for the process's whole life, separate from the multiplexed protocol connection (which may reconnect many times). The server polls the lifeline; only `POLLHUP` (the process exited or closed it) ends the holder: the server then unmaps that holder's counter memfd and drops its holds. A mount whose protocol connection flaps keeps its counters, its regions and its holds; it re-sends `open-pack` with the same holder id and gets the same regions back. A pidfd is not used because mounts run in other pid namespaces.

**Reader protocol (mount, no RPC), for each 16-cell group covering the range:**
1. `fetch_add(group counter, 1, SeqCst)`.
2. `load(state of every cell in the range, SeqCst)`: if any is not `present`, `fetch_sub`, then `wait` (or EIO for `bad`).
3. Read the bytes (pread, or splice into the pipe, 5.3).
4. **Post-read recheck:** reload the state of every covered cell (SeqCst); if any is not `present`, discard the bytes (empty the pipe or drop the buffer), `fetch_sub`, and go to `wait`. One extra load of the state bytes means even a forced counter loss cannot serve zeros.
5. Commit the reply; then `fetch_sub(group counter, 1, SeqCst)`.

**Evictor protocol (server), per cell, never per group range:**
1. For each candidate cell, **CAS `present -> evicting`**; remember exactly the cells won. A cell that is `claimed`, `absent`, `recheck` or `bad` is not touched, so an in-flight fetch's ownership is never overwritten, and a neighbouring cell going `absent -> claimed -> present` during the scan is simply not in the set.
2. For each group containing won cells, `load` and sum that group's counters across every holder's region for the pack (SeqCst); wait with backoff until the sum is 0.
3. **Timeout (`evict_wait`, 2 s):** if a group's sum has not reached 0, CAS the won cells of that group back `evicting -> present` and count `evict_timeouts_total`. Nothing has been journaled yet (wait first, journal second).
4. For cells whose group is clear: **append `PUNCH` records for the whole batch and fsync the journal once** (section 8), then `fallocate(PUNCH_HOLE | KEEP_SIZE)` over each contiguous run of won cells only, then CAS `evicting -> absent`.

Why this is correct: reader and evictor each store to one location then load the other's (the Dekker pattern); with SeqCst on both, either the reader sees `evicting` or the evictor sees the counter, never neither. Once a cell is `evicting` no new reader can start on it, so a sum of 0 observed in step 2 stays valid through the punch. The post-read recheck is a second, independent guard. T10a checks the protocol with an exhaustive interleaving model (loom) **including claim, publish and evict transitions on neighbouring cells and a reader that straddles groups**, so the proof is a test.

### 5.2 The FUSE library `tsfuse` (in this repository; Linux only)
**[D]** No vendored fuser. A crate `tsfuse` in this repository, designed for tierstore, serves a read-only tree and the put directory. The komira-ai/fuser fork is retired when `tsfuse` serves in production (T-R); **macOS does not block this (5.6).**

**Scope.** Linux only. Opcodes implemented:
- **Tree mount (read-only):** INIT, DESTROY, LOOKUP, FORGET and BATCH_FORGET (no-ops: inode numbers are record indexes in the mapped image), GETATTR, OPEN/OPENDIR/RELEASE/RELEASEDIR (5.3), READ, READDIR, READDIRPLUS, READLINK, STATFS, INTERRUPT. Everything else: `ENOSYS`.
- **Put directory mount:** the above plus CREATE, WRITE, FLUSH, FSYNC, SETATTR (size, times), MKDIR, UNLINK, RENAME. T-F9 fixes the list from the current put handler.
- Not implemented: xattrs, locks, bmap, ioctl, poll, notify store/delete/inval, passthrough, macOS/BSD.

**INIT flags to negotiate (fixed in T-F1).** Required: `ASYNC_READ` (without it the kernel serialises reads per file), `PARALLEL_DIROPS`, `MAX_PAGES` with an explicit `max_readahead`, `READDIRPLUS` and `READDIRPLUS_AUTO`, `CACHE_SYMLINKS`, `SPLICE_WRITE` (and `SPLICE_READ` for the put mount), `HAS_RESEND`, `NO_OPEN_SUPPORT`, `NO_OPENDIR_SUPPORT`. Never assumed: anything the kernel did not offer. INIT negotiates down to the kernel's minor.

**Shape**
1. **ABI module.** Hand-written `#[repr(C)]` structs for the opcodes above, parsed and encoded with `zerocopy`-style derives (size and alignment checked). The uapi header of the farm kernel is pinned in the repository; **T-F1 builds a farm probe that prints `sizeof` and `offsetof` of every struct** and a Rust test compares them.
2. **tokio-native dispatch with reply affinity.** Each reader owns a **cloned** `/dev/fuse` descriptor (`ioctl(FUSE_DEV_IOC_CLONE)`) driven by `AsyncFd`. [K] A reply is matched by `unique` against the **processing queue of the device it is written to**; clones share the connection and the pending queue but each has its own processing queue. **So a reply is written to the descriptor that read the request, not to any clone.** `Replier` carries its originating device (`Arc<DevFd>` for `/dev/fuse`; a handle to the owning queue's submit channel for io_uring, since a SINGLE_ISSUER ring cannot be submitted to from an arbitrary task) and completes by `write(2)` on that fd (or by sending to the ring task). `Replier` is `Send + 'static`; completion from any task is allowed, on the originating device.
   - The `unique -> cancel token` entry is **registered at read time**, before `Fs::handle` runs; `Replier::complete` is a single atomic take, so exactly one of the real reply and the `EINTR` reply is ever written. An INTERRUPT read by another clone between read and park finds the entry and is answered correctly.
   - A reader reads into a pooled buffer, parses in place, and calls `Fs::handle(req, replier)` returning `Handled::Done` or `Handled::Parked`. Metadata and ready reads complete inline with no `await`; a parked operation copies the few fields it needs and returns the buffer. A tree session sets a tiny `max_write` (8 KiB read buffers); the put session sizes to `max_write`.
   - **Clones do not remove the shared pending-queue lock** [K]: all clones poll one input queue, so a new request wakes every `AsyncFd` and N-1 readers see EAGAIN. Clones reduce contention on the processing queues only. The benchmark (V3 with N readers) is expected to show a knee, not linear scaling.
3. **Interrupts.** INTERRUPT cancels a parked operation through the token table and replies `EINTR`; an INTERRUPT that arrives before its request is answered `EAGAIN`. A SIGKILLed build frees its parked read.
4. **io_uring transport (kernel 6.14+).** Registers `FUSE_IO_URING_CMD_REGISTER` entries per queue and replies with `COMMIT_AND_FETCH`.
   - **`fuse.enable_uring` is set at boot** (kernel command line or module options in the hostkeeper node image), never at runtime. At startup `tsfuse` reads it; if 0 it uses `/dev/fuse`. Both transports share dispatch code behind a `Transport` trait.
   - **Hybrid, not alternative [K, uncertain]:** INIT, early requests, INTERRUPT and FORGET may still arrive on the `/dev/fuse` path in uring mode. The library keeps one `/dev/fuse` reader alive beside the rings and routes replies to those requests on it. T-F0 checks INTERRUPT delivery in uring mode and the INIT-then-register order; conformance item 4 runs on the uring transport.
   - **Parked reads hold ring entries.** A cold storm can occupy every entry of a queue and stall metadata behind it. Mitigations: depth per queue large enough for the expected parked count, small payload buffers for the tree session, Prepare making cold reads rare, `uring_entries_parked`. **The storm that must pass is `depth x queues x 1.5` parked reads**, and metadata p99 must not grow.
   - **Memory:** the kernel has one queue per CPU (88 here) and every queue registers entries each with a header and payload buffer: `queues x depth x (header + payload)`. At depth 64 and 256 KiB payload that is about 1.4 GiB per mount, over 10 GiB at 8 job mounts. **io_uring is the default for a mount only if the registered memory is at most 64 MiB at the depth that passes the storm test** (otherwise a shared node-wide mount, or `/dev/fuse`); T-F0 computes the per-mount cost.
   - **No splice on this transport:** one more copy than splice from the page cache; measured.
   - **Takeover is not available on io_uring until proven:** see item 5. A mount that must survive a process update uses `/dev/fuse`.
   - T0 E records `kernel.io_uring_disabled`, the seccomp profile of job sandboxes, memlock limits, and the FUSE request timeout (`fuse request_timeout`, 6.14+: a parked cold read can abort the whole connection if an administrator set it).
5. **HAS_RESEND takeover with every clone retained.** [K] `FUSE_NOTIFY_RESEND` requeues requests from the processing queues of devices that still exist; when a process dies, `fuse_dev_release` ends the in-flight requests of each clone it alone held with an error. **The supervisor (fd store) therefore holds the original descriptor and every cloned descriptor**; the new process receives all of them (SCM_RIGHTS), skips INIT, and sends RESEND; resent requests are answered under their original unique id on the descriptor that holds them. Conformance item 7 kills the server with N > 1 readers and several parked reads on different clones and asserts no EIO and exactly-once completion. io_uring: ring entries of a dead process are torn down and their requests failed unless the kernel has uring resend [K, uncertain]; the T-F2 spike answers; until then uring is takeover-incapable.
6. **Mounting.** Direct `mount(2)` with `fd=`, `rootmode`, `user_id`, `group_id` with `CAP_SYS_ADMIN`; otherwise `fusermount3`. Options: `ro` (tree), `nosuid`, `nodev`, `default_permissions`, `allow_other`.

**Which fuser tests are ported, by file** (counted by `#[test]` in the fork; T-F1 recounts them against the fork's last tag; about 65 unit tests in total):

| Fork file (tests) | Disposition | Reason |
|---|---|---|
| `src/ll/argument.rs` (5) | **ported**: fixed struct, NUL-terminated name, rest bytes, short buffer, missing NUL | Every opcode parse depends on it |
| `src/ll/request.rs` (4) | **ported**: short header, short body, INIT parse; `mknod` dropped | MKNOD not kept; per-opcode encode/decode tests for each kept opcode are written new |
| `src/ll/reply.rs` (15) | **ported:** empty, error, data, entry, attr, open, write, statfs, create, directory (byte-exact). **Dropped:** lock, bmap, xattr_size, xattr_data | Opcodes not implemented |
| `src/reply.rs` (20) | **ported:** serialize and reply_* for the kept opcodes and **async_reply** (central for parked replies; extended: completion on the originating device). **Dropped:** reply_lock, reply_bmap, reply_xattr_* | Same |
| `src/ll/notify.rs` (6) | **ported:** `resend`. **Dropped:** inval_entry, inval_inode, store, delete, poll | Immutable tree |
| `src/session.rs` (9) | **ported (7):** from_fd handshake; refusal of an initialised connection; serve queued request without INIT; resent request marked and answered under its id; resend header-only; stopper ends every loop and leaves the device open (generalized to N clones); non-blocking device waits (`AsyncFd`). **Dropped (2):** passthrough stack depth; backing id | Passthrough deleted |
| `src/time.rs` (2) | **ported** | Timestamps |
| `src/mnt/mount_options.rs` (2) | **ported** to our option rendering | Smaller option set |
| `src/mnt/mod.rs` (2) | **dropped** | libfuse argument strings |
| `fuser-tests/` mount, users, unmount, mount_util, fusermount, fuse_conf, canonical_temp_dir | **ported to Rust conformance tests:** mount/unmount, kill-then-unmount, `allow_other`, fusermount3 path, mount table | Same behaviours against our library on a farm node |
| `fuser-tests/` libfuse, bsd_mount, macos_mount, experimental, simple | **dropped** | About fuser, BSD/macOS or the example fs |
| `tests/*.sh` pjdfs, xfstests, passthrough, BSD/macOS and Dockerfiles | **dropped as kept tests** | Shell (tests are Rust), passthrough, BSD/macOS. Read-only semantics re-expressed in the Rust conformance suite; pjdfstest may run once as a farm job for the benchmark report |

**Kernel conformance tests, on a farm node** (a test target with exec properties for `/dev/fuse`, `CAP_SYS_ADMIN`, kernel >= 6.14 and, for uring cases, `enable_uring=1`; provided by **T-ENV**, never on a developer machine):
1. INIT: record what the kernel offers; negotiated down; the flag list above.
2. LOOKUP/GETATTR/READDIR/READDIRPLUS equal an expected tree, including `.` and `..`, a directory larger than one buffer, a missing name.
3. READ: ranges inside, spanning, at and past EOF, zero length, larger than `max_read`, **a range containing a hole, a short file**; bytes compared by hash; writev, splice and uring paths. The `out.len` of a spliced reply equals the bytes actually spliced.
4. INTERRUPT on every transport: a blocked reader killed with SIGKILL gets `EINTR`; an early INTERRUPT gets `EAGAIN`; an INTERRUPT read by a different clone than the request's.
5. **Parked replies completed from another task with N > 1 clones, each on its originating device, out of order.**
6. Multi-reader: N cloned descriptors under concurrent load, no lost or duplicated replies; stopping ends all readers.
7. **Takeover with N > 1 readers**: kill mid-load with parked reads on several clones, resume with every retained descriptor, exactly-once, no EIO (`/dev/fuse`; uring per the T-F2 answer).
8. Unmount while busy, `abort` via `/sys/fs/fuse/connections/<n>/abort`, mount table consistency.
9. io_uring: queues register; with `enable_uring=0` the transport falls back; the storm of `depth x queues x 1.5` parked reads does not stall metadata.
10. ABI layout probe equals our structs. 11. A `PROT_WRITE` mapping of the passed state memfd fails.

### 5.3 Serving path
**Metadata from the mapped image.** The mount `mmap`s the tree-list image read-only (`PROT_READ`, `MAP_PRIVATE`; `MADV_RANDOM` for records). LOOKUP binary-searches the sorted directory records, GETATTR reads the node record, READDIR/READDIRPLUS encodes straight from records. Entry and attribute validity are long (the tree is immutable). The image file is immutable, never punched or truncated. **Page-in on a cold page of the image is an inline stall on a reader thread:** at tree open the mount calls `MADV_WILLNEED` on the record sections and, for a hot mount, `mlock`s the directory and node sections (bounded: the image is small relative to the pack data), so a first touch does not block on the shared cache SSD during heavy writeback. **File data is not mmapped.**

**Opens.** A tree session negotiates `NO_OPEN_SUPPORT`/`NO_OPENDIR_SUPPORT` and answers the first OPEN per connection with `ENOSYS` (that first handle costs one round trip; the kernel then stops sending OPEN). **T-F0 confirms** that the kernel keeps the page cache across opens in this mode, including for that first handle; if not, the library returns `FOPEN_KEEP_CACHE` on OPEN. The benchmark reports both regimes. The put session handles OPEN/CREATE normally.

**Reads (`/dev/fuse` transport).**
1. Resolve the node to its span(s), map to pack and cell range, run the 5.1 reader protocol. If every cell is `present`: reply from the pack's data fd.
2. **Reply by splice:** `splice(data_fd -> pipe)` then `splice(pipe -> /dev/fuse)` with `SPLICE_WRITE`. **The out header length is set from the bytes actually spliced:** loop until the full length is in the pipe; only a read at EOF may be short; a short splice or a hole read with a wrong `oh.len` makes the kernel reject the write with EINVAL and leave the request unanswered. **Pipes are sized with `F_SETPIPE_SZ` to at least `max_read` plus the header** (the default 64 KiB pipe fails a 256 KiB splice). Small replies (starting threshold 16 KiB) use `pread` into a pooled buffer and `writev`. [K, uncertain] `FUSE_SPLICE_MOVE` normally fails on page-cache pages and falls back to a copy, so splice saves one copy, not all; V4 measures it.
3. **Double page caching.** After a span is served the second copy is dropped only by the server on `durable` cells (4.8). **All three choices (splice versus pread, KEEP_CACHE/NO_OPEN, DONTNEED policy) are measured.**
4. Not ready: `Handled::Parked`; completion later on the originating device (5.2). A fill that is lost (4.6) completes every parked read of the tree with EIO.
- **io_uring transport:** same logic; the payload is copied into the ring entry's buffer and committed.

D-state is reduced, not eliminated; `fuse_d_state_threads` stays an acceptance metric.

### 5.4 Mount to tierstore protocol and runtimes
- One long-lived multiplexed connection per mount, request ids, out-of-order completion, **the server spawns a task per request**.
- Messages: `open-pack(tree_ref, pack_idx, holder) -> (data fd, state memfd, region)`; `wait(pack, off, len) -> ok | eio(cell) | lost`; `ensure(pack, cell_runs, class, rank)`; `release(holder)`. The handshake sends the counter memfd and the lifeline socket once (5.1).
- **Epoch.** The server stamps every connection and reply with a random u64 epoch; the mount drops replies from a previous epoch. On reconnect the mount re-handshakes with the same holder id (counter memfd and lifeline are unchanged), re-sends `open-pack`, and re-issues every parked `wait` idempotently. Backoff 50 ms doubling to 2 s with jitter.
- **Coalescing of waits.** A `wait` covers the remaining span up to the next 8 cells; concurrent reads join the pending `wait`.
- **Runtimes are separate.** `tsfuse` readers run on **dedicated threads** (one current-thread runtime or plain thread per reader), because ready reads run `pread`, splice or an image page fault inline and may block; the protocol client runs on its **own** runtime; they are joined by channels. A reader blocked in the kernel cannot stall the `wait` completions that unblock other reads. T10a has a test where every reader thread is blocked in a fake `pread` while `wait` completions still flow.

### 5.5 No global locks across I/O
`pinning` and `lifecycle` are removed; state is per pack. The pin fix of komira-ai/tierstore#343 stays as a bridge until this lands.

### 5.6 macOS: same engine, same cache, NFS front
macOS is not a blocker for anything in this document.
- **Front.** On macOS tierstore-fs is the NFSv3 server (the komira-ai/nfsserve fork) running **inside the tierstore process**. It does not use fuser, so retiring the fuser fork (T-R) is not blocked by macOS. `tsfuse` and the FUSE conformance suite are Linux-only.
- **Read path.** The NFS `read`, `lookup`, `getattr` and `readdir` handlers call the same engine (`ensure`/`wait`), the same cell cache and per-cell verification, the same L2 reader (the mapped list image) and the same cold-tier client (10.6) as the Linux mount. A Mac has no cold drives; it is a pure client of the peer read path. The NFS handler is async, so a not-ready read awaits readiness the same way a parked FUSE read does.
- **What differs, behind an `EntryStore` trait.** (a) The NFS server is in-process, so the state page needs no memfd, no SCM_RIGHTS and no counter memfd: an entry is an in-process object with atomic cell states and an in-process reader count per group; the same CAS rules and the same evictor ordering apply (the Dekker argument holds within one process; the loom model covers both implementations via the shared trait). (b) Hole punching uses `fcntl(F_PUNCHHOLE)` on APFS [K, T-M1 confirms on the Macs]; if a volume cannot punch, eviction falls back to whole-entry eviction only. (c) Durability uses `F_FULLFSYNC`. (d) No splice, no io_uring, no `O_DIRECT`: reads are `pread`, with `F_NOCACHE` where a measurement says it helps. (e) The journal, `recheck` and boot-id rules of section 8 are shared code.
- **L2 and macOS.** Macs run the same tierstore binary, so the reader-first rollout (3.5) covers them; the CAS service's dual emission keeps old Mac binaries working until updated.
- **Tests.** The `EntryStore` conformance suite runs against both implementations on a Linux node (in-process) and on the Macs (T-M1, T-M2). Mac builds stay release-only; these tests are not per-PR.

## 6. Prefetch
Mount provider `Prepare` at job assignment runs on the same engine.
1. Read the tree list through the cache; mmap PACKS and CELLS.
2. Planner (a pure function of list, ranges table and profile; T7): per pack, the needed cell set, ordered by the profile's first-touch order else tree order. Output `Vec<Ensure>` with ranks.
3. **Gap rule in cells (per drive):** a part reads one file on one drive, so reading through a gap costs gap bytes at the media rate against one positioning. Merge needed runs across a gap of up to `gap_cells = ceil(pos_ms x media_MBps / 1.048576 / 1000)` absent cells: about 2 at 14 ms and 150 MB/s. T0-F sets `pos_ms` and `media_MBps`. Gap cells that are fetched are real cells: verified, written and `present`, not discarded.
4. **Whole-pack rule, budget-aware.** `cov` = needed fraction of the pack's cells after merging: `cov >= 50%` fetch the whole pack; pack <= 64 MiB and `cov >= 20%` fetch the whole pack; otherwise merged runs only. If the node's queued backlog exceeds 2 s of budget throughput, a would-be-whole pack is downgraded to runs. `amplification` (bytes fetched / needed) must stay under 1.5 in T10b or the constants move. A later demand on missing cells of a `Partial` entry fills gaps; it never starts a second entry.
5. Prepare is cancellable: a released holder demotes its unstarted parts to `background`; they are dropped only if no other holder wants them.
6. Prepare never gates launch.
7. Prepare takes a hold on every pack of the tree, and that hold lasts until the job's mount session is released (one hold per tree per job), so packs the job has not opened yet stay held; holds refresh the cold-tier recency of those packs (7, 10.8).

This replaces the S3 hydrate queue, the FUSE `prefetch all` threads and the Prepare groups.

## 7. Eviction, holds and admission (the node cache)
- **Accounting by cells.** Use = (`present` + `claimed` + `recheck`) cells x cell size, charged at claim time and refunded on failure. The critical-mark reserve check applies at claim time per cell run.
- **Eviction order.** (1) Whole unheld entries by LRU. (2) If only held entries remain, **cell-group eviction** of held entries by the 5.1 protocol (candidate cells in 16-cell groups untouched longest by the touch epoch; CAS per cell), counted in `punched_bytes_total`.
- **Admission at job assignment.** The jobscheduler asks `reserve(estimated_held_bytes)`. **`held` = the planner's needed cells minus cells already `present`.** Refuse when `held_cells_charged + estimate > 0.9 x budget`; refusal reasons `budget`, `enospc`, `oversize` are metered. A pack larger than the budget is refused only if its needed set exceeds `budget - held`.
- **Holds** are refcounts per pack: mount sessions (via `open-pack`, released by lifeline hangup or `release`), Prepare, and the fill while parts are in flight. One lease per session. **Every held pack is touched in the cold tier** every `cold.touch_interval` (10 minutes), on every copy: the copies of the fill's location, or, for a pack with no fill, every probe position of `L(K)` (10.2), one batched `touch` request per node and drive (10.6). The cold tier therefore keeps every copy while a job may still read it.
- **Startup.** No eviction and no scrub until a 60 s grace window since process start, so mounts rebind their holds. The data file is `flock`ed by its owner; a process that cannot lock an entry reads and evicts nothing from it. A directory lock `packs-v2/.lock` makes exactly one process the evictor.
- Poisoning is per cell (4.6). The cold tier has its own, separate eviction (10.8); the two do not share a budget.

## 8. Crash safety and journal
- **Order of a cell's durability:** verify in memory -> `pwrite` -> state `present` (servable now) -> later, in a batch: `fdatasync(data file)` -> set the `durable` bit -> append a journal record `(ADD, run start, count, crc)`. A journal record never precedes durable data.
- **Punches are journaled before they happen, in batches:** after the evictor has observed every counter sum at 0 (5.1), it appends a **batch** of `PUNCH` records (run start, count) and fsyncs the journal **once**, then punches. A record is durable before its punch. Replay applies ADD and PUNCH in order, so after a crash a punched cell is never resurrected as `present`. A crash between the record and the punch only leaks blocks that a later refetch overwrites.
- **Batching.** One committer per node commits every 1 s or at 32 MiB pending, with one `fdatasync` per dirty entry file; never one commit per part or per demand sub-range.
- **Snapshot.** The journal is folded into `<hash>.meta` (present bitmap, pack_root, size, last use, **`boot_id`**) at entry close, at eviction and every 64 records, by write-temp, fsync, rename and directory fsync.
- **Restart.** Load the snapshot, replay the journal; only committed cells are restored. The snapshot and journal carry `/proc/sys/kernel/random/boot_id`. **Same boot and a process crash** (SIGKILL, oomd kill, panic): the kernel and the page cache survived, journaled cells follow durable data, so they are restored `present` with no recheck. **Different boot, or no boot_id (power loss, kernel crash, unclean marker):** every restored cell is `recheck`: the mount cannot serve it; the server re-hashes it on first demand or by a low-priority sweep and flips it to `present` or `absent`. This covers devices that reorder flushes; the cost is bounded by the cells actually read.
- `fdatasync` cost through the loop device is measured in T0 B and is one reason for section 11. The entry path includes `pack_root`, so a changed pack never reuses a file.

## 9. S3-front large objects
- Objects over `whole_fill_limit` (1 MiB on the node) that miss in the S3 front are routed to the pack engine with `PackId = (key, ETag)` and `digest=none`; for a cold-tier object the ETag is its content-derived generation (10.4).
- **No stale mix.** For a cold-tier object the generation is known from the location before the first part (10.6), so every part carries `If-Match` from the start. For an object with no known generation (silo-origin through the legacy upstream), the first part is issued **alone**; its response gives the ETag, and every later part carries `If-Match`. When the S3-front index already holds the ETag, no part starts without it.
- **Integrity.** For a `digest=none` object the cold tier keeps a **per-cell crc32c table in the file footer** (10.4); the serving drive verifies the crc of every whole cell it serves for such an object (hardware crc32c, negligible CPU) and returns an error on mismatch, which the client treats as a verification failure (4.6, other copy). Footer reads cost one extra seek on the first read of a `digest=none` object and never happen for packs with cell digests (the client verifies those). A small object (10.4, segments) carries one crc32c over its record, verified on every read. `tierstore cold scrub` reports bad cells and quarantines files; it never repairs.
- Ranged S3 GETs are served from `present` cells of a `Partial` entry and wait on missing cells at `demand` class. A write-through PUT's generation is the hash of its bytes, so it names the same `PackId` the cold tier will report. Keys of the cold tier are write-once-ever (10.5); a different object needs a different key. Objects up to `whole_fill_limit` stay in the node's `obj/`. T12 owns routing.

## 10. The cold tier: tierstore's own store on the node drives (replaces silo)

### 10.1 What it is and where it is used [D]
Tierstore already has the S3 front, the local cache, hot-to-cold movement and the FUSE/NFS front. The only thing silo adds is a place to keep packs on the drives and a cluster read path; a build cache needs neither parity nor an erasure set. SeaweedFS was evaluated and rejected because its erasure coding runs only on sealed volumes and its garbage collection fights a store that is deleted from; RustFS and Garage are general-purpose stores with the same erasure fan-out. The cold tier keeps the one property drive reads need, that **a pack is read as one sequential stream from one spindle**, and drops what cost throughput and tails: an 8-drive fan-out per GET (a GET waits for the slowest of 8 queues) and 128 KiB shard runs.

Each node runs the tierstore daemon with a `cold` module owning `/mnt/hdd1` and `/mnt/hdd2`. The 12 drives of the six nodes form the **cold set**. An object is stored on each of `copies` drives chosen by hash: an object larger than `cold.small_max` as one write-once sequential file, a smaller one as a record in a write-once segment (10.4). There is no parity, no metadata service, no compaction of large files, no resharding and no consensus. Everything a node needs to find an object is a pure function of its key and the membership document, plus probes and a bounded location hint.

**Scope.** The cold tier is a store kind (`cold_store.kind = "hdd"`) with the capability subset of 10.9: create-only, write-once-ever keys, ranged reads, HEAD, DELETE, no LIST and no multipart. It serves the farm's pack store: the CAS service's object store and the build nodes' cache upstream. A proxy with a hot tier (configurations A and C of tierstore-design.md) needs LIST, overwrite and multipart in its cold tier, so its configuration check refuses `hdd` as the cold store by name; those deployments keep an S3-compatible or `dir:` cold store, unchanged.

### 10.2 Placement: weighted rendezvous hashing (HRW)
- **Drives are the unit.** A drive has a stable `drive_id` (UUID written to the drive at format, 10.4), a node, a weight (its capacity in GiB; all 8 TB drives are equal today) and, in the membership document, `in` or `out` (10.8). Each node also keeps a **view state** per drive (10.8: `up`, `indexing`, `stalled`, `suspect`, `down`, `failed`).
- **Score.** For key `K` and drive `d`: `h = H(K || drive_id)` (128-bit blake3 reduced to a uniform u64), `score(d) = -weight(d) / ln(h / 2^64)`. The **drive ranking** of `K` is the `in` drives by descending score (the weighted rendezvous form: the probability that `d` is first is proportional to its weight).
- **Candidate list.** The drive ranking is projected to distinct nodes: walk it in order and keep a drive only if no earlier kept drive is on the same node. The result, `L(K)`, has one drive per node (6 positions here), the best-ranked drive of each node. **Positions are always counted in `L`, after this filter**, everywhere in this document.
- **Probe depth `c + m`** (m = 2, a setting): reads consider the first `c + m` positions of `L` (4 of 6 here), and **every write lands within them** (10.5): a copy that cannot be placed within the first `c + m` positions is not placed (the PUT writes fewer copies, or fails, and repair restores the rest later). So with up to `m` nodes down or full, every copy of every object is inside the probe.
- **Copy set.** The first `c` positions of `L` whose drives are writable (view state `up`, below the fill limit, write queue under `drive.queue_max`). Copies are on distinct nodes by construction, so a node loss never removes both. A drive that is not writable stays in `L`, so existing copies on it are still found.
- **Movement.** Removing a drive (`out`) moves the copies of the keys whose copy set contained it: about `c/12` of keys (2/12 at c = 2), and nothing else. Adding a drive takes about `weight/total` of new keys; existing objects are not moved (10.8).
- No central placement table and no per-object metadata outside the drives. Cost: one hash per drive per lookup (12 hashes), cached per key by the engine.

### 10.3 Copy count: default 2 [D, decision Q2]
`cold.copies` (cluster setting, 1 to 3) defaults to **2**, on distinct nodes. It is the number of copies repair restores (10.8). A PUT may write fewer with `x-ts-copies: 1`, used only by the migration mover (10.10); repair later raises such an object to `cold.copies` when it is read.

Why 2 and not 1 for a build cache where a loss is a miss:
1. **Rolling updates and reboots are routine** (one live update at a time; hostkeeper and CAS service updates happen continuously). With one copy, every node restart makes 1/6 of all packs unreachable for the window; each is a miss that costs a rebuild or republish, spread across the farm exactly during maintenance. With two copies on distinct nodes a single node down loses no availability for packs that have both copies; the audit (10.8) measures how many do.
2. **Tail and bandwidth.** Two copies let a demand read pick the less loaded spindle and hedge to the other (4.6), and let a large pack stream from both (4.4).
3. **Integrity.** A cell that fails verification is refetched from the other copy; with one copy the cell is lost to bitrot.
4. **Single failure.** A permanently lost node leaves about one third of objects with one copy and none lost; repair restores the ones that are read or were written in the last `cold.repair_recent` (10.8).

Cost, stated: writes double on the drives (12 drives at a sustained 100 MB/s write is about 600 MB/s of unique ingest cluster-wide, far above what the CAS service produces [I]); usable capacity is 50% of the eviction low mark: raw 87 TiB x 0.8 / 2 = about 35 TiB of unique objects, against silo's 66% parity efficiency (about 58 TiB at 100%). **T0-F C-6 records silo's logical bytes and object-size distribution** so this is checked against reality. Two nodes down at once make the objects with both copies on those two nodes unreachable (about 1/15 of those with two copies). `copies = 3` is available and unused. **The availability figures here are upper bounds**: they hold for objects that have `c` copies, and the audit's copy-count histogram (`cold_audit_copies_ratio`, 10.8) is the measured number.

### 10.4 Per-drive layout and filesystem (SMR)
**Drive facts [M]:** ST8000DM004 is a drive-managed SMR drive (the host sees an ordinary block device; shingled bands are rewritten by the firmware, with a persistent CMR cache for incoming random writes that, once full, collapses sustained write throughput). The rule that follows: **never overwrite, never write small and random, write whole files sequentially, delete whole files, keep the drive well below full.**

**Filesystem: keep xfs [D].** The drives are already xfs, so no reformat is needed (and silo's data lives there during migration). xfs gives extent allocation with a per-directory extent-size hint, allocation groups for concurrent writers, `fallocate`, `RENAME_NOREPLACE`, and cheap unlink of large files. Rejected: ext4 (no extent-size hint, weaker large-file locality), f2fs and any log-structured layout for large objects (garbage collection rewrites, the thing to avoid), btrfs and zfs (copy-on-write, fragmentation), a raw block-device log of our own (deleting needs compaction, the SMR hazard), zoned fs and dm-zoned (the drives are drive-managed and expose no zones). Mount options: `noatime,nodiratime,inode64`; no `discard` (T0-F checks `lsblk -D`; TRIM is not expected [I]).

**Alignment [I until T0-F]:** T0-F records `xfs_info` (`sectsz`, `bsize`, `agcount`) and `lsblk -t` for each drive. All cold-tier I/O is a multiple of 4 KiB at a 4 KiB-aligned offset, so a 512e drive with 4 KiB physical sectors never does a read-modify-write for us. Extent hint: the large-object directories have an xfs extent-size hint of 64 MiB (`FS_IOC_FSSETXATTR` with `fsx_extsize`, inherited by files) so that a sequentially written file is contiguous; when the PUT knows its `Content-Length`, the writer `fallocate`s the exact file length before writing. T0-F measures fragmentation (FIEMAP extent count per file) and allocated bytes against file size.

**Layout**
```
/mnt/hdd1/ts-cold/
  DRIVE                         drive_id (UUID), cluster id, format version, filesystem UUID
  v1/<hh>/                      256 subdirectories by the first byte of the key hash
    <khash>.<gen>.<size>.pack   one large object copy (`.m.pack`: a single copy the migration mover wrote on purpose, 10.8)
    .<khash>.<nonce>.up         an upload in progress (same directory, same allocation group)
    <khash>.<gen>.<size>.bad    a quarantined copy (ignored by the index, removed by scrub)
  small/<seq>.seg               write-once segments of small objects
  stamps                        last-read stamps, written once at clean shutdown
  FAILED                        present once the drive is failed (10.7); an operator removes it
```
- `khash` = 128-bit blake3 of the object key (32 hex digits); `gen` = the generation, the 128-bit blake3 of the object's bytes (32 hex digits); `size` = the object's byte count (16 hex digits). **The name carries everything the index needs**: a startup `readdir` of the 256 directories gives every large object's key hash, generation (its ETag) and size, with no header read and no `stat`. The index is an in-memory map, so a 404 or a HEAD never touches the disk.
- **Large-object file format (all multiples of 4 KiB):**
  - Header, 4 KiB at offset 0: magic, version, full key, creation time, crc. Written at the start of the upload, before the generation is known, so the header carries no generation.
  - **Data at offset 4096**: object byte `x` is at file offset `4096 + x`; the last data block is zero-padded to 4 KiB. Cells are 1 MiB multiples of 4 KiB-aligned reads.
  - Footer at offset `4096 + roundup(size, 4096)`, which the name's `size` gives: per-cell crc32c table (4 bytes per cell), size, generation, key hash, crc; padded to 4 KiB. Written by the same sequential stream. The file length is `footer offset + footer length`; nothing is truncated or rewritten.
- **Commit is atomic and has no rewrite.** The file is written under the `.up` name with `O_DIRECT` 4 MiB writes (the device's `max_sectors_kb`), then `fsync`, then the **create-only check under the key lock**, then `renameat2(..., RENAME_NOREPLACE)` to the final name, then `fsync` of the directory. A crash leaves only a `.up` file, removed by the startup sweep; an object is visible only when whole.
- **Create-only is decided by the index, under a per-key lock [D].** The owning node holds one in-memory lock per key hash (sharded) for each drive; under it the commit looks up the key in the drive's index: **absent** -> rename and insert (`committed`); **present with the same generation** -> unlink the `.up` (`identical`: the same bytes, an idempotent retry or a race of two writers of one object); **present with a different generation** -> unlink the `.up` (`conflict`). Because the generation is the hash of the bytes, two writers of the same object produce the same final name, and `RENAME_NOREPLACE` is a second guard that turns into `identical`. The same lock and rule cover small objects.
- **Delete = `unlink`** after removing the index entry; open readers keep their inode until they close. No tombstones (keys are write-once-ever, 10.5), no compaction, no vacuum.
- **Small objects [D].** An object of at most `cold.small_max` bytes (default 1 MiB; T0-F C-6's size distribution sets it before T-C2 freezes the format) is not a file of its own. It is a **record** in a per-drive segment: `small/<seq>.seg`, a write-once file of up to `cold.segment_bytes` (64 MiB) appended sequentially by the drive's write stream. A record is a 4 KiB-aligned run: record header (magic, key hash, key length, key, generation, size, crc32c of the data), data, zero padding. **Group commit:** small PUTs to a drive are batched for up to 20 ms or 4 MiB, written with one `O_DIRECT` write and one `fdatasync`, and each PUT is acknowledged after its batch's `fdatasync` (never one fsync per object, never one file per object). A delete of a small object removes its index entry and appends a delete record (key hash, generation) to the open segment in the next batch. A segment is sealed when full by appending a trailer index that lists, in order, every record and every delete record it holds (key hash, generation, kind, offset, size) and is never written again. At startup the index replays the sealed segments' trailers in segment order (one read per segment) and then the records of the one open segment, so a deleted small object stays deleted across a restart. Space of dead records is reclaimed when the whole segment goes: a segment with no live record is unlinked; eviction (10.8) takes whole segments, oldest first, and first copies forward into the open segment the records read or touched within `cold.evict_min_age` (a bounded sequential write of only those records). A delete record lost in a crash (written after the last `fdatasync`) can resurrect its object; it names the same bytes as before (write-once-ever) and is evicted with its segment. The rule "no live record" is judged on the index after replay. Small reads use the scheduler's small lane (10.7), never a stream slot behind 32 MiB slices.
- **Allocation locality [I]:** the 256 hash directories spread across allocation groups; within a large file the 64 MiB extent hint keeps blocks contiguous. If T0-F shows the churn penalty below, the fallback is per-epoch directories (one directory per about 64 GiB written, so freed space is reclaimed in large contiguous regions) at the price of an index lookup for the directory; not built unless measured.
- **Fill marks (the main SMR defence) [D].** Four marks, each a fraction of the drive's capacity as accounted in 10.8, in this order: `cold.repair_limit` (80%) <= `cold.evict_to` (80%) < `cold.evict_at` (85%) < `cold.fill_limit` (90%). Eviction starts when fill passes `evict_at` and evicts down to `evict_to` (10.8). Client writes are accepted up to `fill_limit`, a hard stop reached only when eviction cannot keep up (nothing evictable, or writes faster than eviction); above it the drive is not writable. Repair writes are accepted only below `repair_limit`. So in steady state a full drive sits between `evict_to` and `evict_at` and client writes land in the space eviction frees. T0-F's steady-state churn arm decides the numbers.

### 10.5 Write path: the CAS service PUTs an object to tierstore, tierstore places it
1. The CAS service sends one streaming `PUT /<bucket>/<key>` with `If-None-Match: *` to its **local** tierstore S3 front, as a single request (Content-Length if known, else chunked). That node is the **coordinator**. It hashes the body with blake3 as it streams.
2. The coordinator computes `L(K)` and selects the copy set (10.2): the first `c` writable positions within the first `c + m`.
3. It streams the body to every copy at once: a local drive through the local `DriveSched` (a write, 10.7), a remote drive by `PUT /cold/v1/<drive_id>/<khash>` to that node's cold service (10.6), chunked, with `x-ts-key` and `x-ts-size` if known. **After the last chunk the coordinator sends its generation in the trailer `x-ts-generation`.** Each writer hashes what it received the same way and compares: a mismatch (bytes damaged in transit) unlinks the `.up` and answers `400 generation-mismatch`, and the copy counts as abandoned. Bounded buffers; the slowest live copy paces the upload (TCP backpressure to the CAS service). With `write_through` on, the coordinator also tees the stream into the local cache as a hot entry. Objects of at most `small_max` go to each drive's segment batch instead of a file. **Choosing the path [D]:** with a known `Content-Length` the size decides; for a chunked PUT the coordinator buffers up to `small_max + 1` bytes before it sends anything to a copy: a body that ends inside the buffer takes the small path, a longer one opens the large path (an `.up` file with no `fallocate`) and sends the buffered bytes first.
4. Each drive commits independently under its key lock (10.4) and answers `committed`, `identical` or `conflict`.
5. **Abandon rule [D].** A copy is abandoned when its drive reports `stalled` or `failed`, its connection or write fails, its generation mismatched, or it **lags**: its per-copy buffer has been full for `cold.put_copy_stall` (default 10 s [I]) while another copy's buffer had room, that is, the other copies were waiting for bytes this copy held back. Time a write waits in the drive's queue counts toward that timer. A pause of the client itself fills no buffer and abandons nothing. The coordinator stops feeding the copy and closes its connection; a writer unlinks its `.up` when the coordinator's connection closes before the trailer, and never on an idle timer of its own (a coordinator crash closes the connection the same way; the startup sweep covers a writer crash). `cold_put_copies_abandoned_total{reason}` counts it. The remaining copies pace the upload. The PUT never restarts a stream, so it never buffers a whole object.
6. **Acknowledgement rule [D].** The PUT answers once every copy is `committed`, `identical`, `conflict` or abandoned — never at the first commit while another copy is still progressing:
   - any `conflict`: **412**, and the coordinator deletes the copies this PUT committed (`DELETE` with `If-Match: <generation>`), so no PUT leaves a key with two generations;
   - else `committed` + `identical` >= `cold.min_copies` (default 1): **200** with `ETag` = generation;
   - else, if the copy set was empty because no position within the first `c + m` was writable: **507**;
   - else (copies were placed but fewer than `min_copies` committed): **503** (the CAS service retries; a retry of the same bytes converges by 10.4).
   When fewer than `c` copies hold the object, the coordinator queues repair at once for the missing positions (10.8) and counts `cold_under_replicated_total{cause="put"}`, so an object is not left single-copy until someone reads it. A PUT with `x-ts-copies` (the migration mover, 10.10) wrote fewer copies on purpose: it queues no repair and is not counted.
7. **Write-once-ever keys [D].** A key of the cold tier names one byte sequence for its whole life, including after a delete. The CAS service's pack keys and tree-list keys are derived from content, so this holds by construction, and the CAS service conformance target asserts that the CAS service never PUTs different bytes under a key it used before. Consequences: two PUTs of one object always converge on every drive (same generation, same name); a copy that a DELETE missed because its node was down is the same bytes, so a reader that later finds it gets correct bytes and LRU removes it otherwise (no tombstones). Two PUTs of different bytes under one key are a client defect: no reader mixes them (every GET of a fill carries the fill's pinned generation, 4.6), each PUT that meets the other's generation answers 412 and removes its own copies, `cold_put_conflicts_total` counts it, and the key can end absent (a miss), never wrong.
8. Writes are 1 stream per drive by default (10.7). SMR: one sequential writer per drive, no random writes, 4 MiB writes.

### 10.6 Read path and the peer protocol
**Lookup for a read of `(key, range)`, once per fill (not per part):**
1. **Location hint.** Each node keeps a bounded LRU (`cold.location_hint_entries`, 256k) of `key hash -> [(drive, generation, size)]`, filled by existence-query hits, by reads served outside the first `c` positions, by repairs, and by the coordinator of a PUT that placed a copy outside the first `c`; an entry is dropped on a 404 or 412 from the hinted drive. A hint is tried first. While the legacy upstream exists (10.10) an entry may instead say **legacy** (the pack was last found in silo), with an expiry of `cold.legacy_hint_ttl` (10 minutes); a local PUT, a destage commit or any cold-tier hit for the key replaces it.
2. **Candidates.** The first `c + m` positions of `L(K)`, ordered by preference: a candidate on this node first (no network), then by lower advertised queue wait (the last `x-ts-queue-wait-ms` per drive from responses, and the ping every 2 s), `suspect` drives last; `down`, `stalled`, `failed` and `indexing` drives are not sent reads. In step 4 a `down`, `stalled` or `indexing` drive counts as **unanswered**; a `failed` drive counts as absent, because its data is never served again until an operator clears it. Each candidate answers from its in-memory index (`HEAD`, batched per node): present with generation and size, absent, or `indexing`. A 404 costs a round trip, not a seek.
3. **Existence query** when no candidate holds the key: ask every node not `down` in parallel (`HEAD` on the key hash against its in-memory indexes, one round trip [I: about 1 ms]). A hit on a non-canonical drive (membership drift, spill, an earlier view) is served from there, recorded in the hint, and queued for repair (10.8, a move by read).
4. **Definitive miss.** A key is missing only when **every** drive of the first `c + m` positions answered `absent` from the index of an `up` or `suspect` drive (or is `failed`), every node answered the existence query (none is `down`), and no answer was `indexing` or `stalled`. Anything less is **not a miss**: the S3 front answers 503 with `Retry-After` (never 404), the engine waits and retries (4.6), no negative entry is cached, nothing is destaged and no repair starts from it. While the legacy upstream exists, a fill may read silo on a non-definitive miss too (silo's bytes are correct), but only a definitive miss destages. So a pack whose only copy is on a rebooting node is delayed, never reported missing.
5. **Negative cache.** A definitive miss is cached for 5 s **only for client GET and HEAD through the S3 front**, so 100 clients asking for a missing key at once cause one query. A local PUT of the key clears it; another node may answer a stale 404 for up to 5 s after a PUT elsewhere, which the CAS service avoids because it reads back what it wrote through its own node. The engine filling a pack that a tree list names does not consult it (the tree is evidence the pack exists); it single-flights one existence query per key instead, so a pack published on another node a moment ago is found.

**The cold service (every node).** HTTP/1.1 keep-alive on the **private network address only** (no public listener; TLS is not enforced; requests carry a cluster token read from a file, never an environment variable). Routes:
- `GET /cold/v1/<drive_id>/<khash>` with `Range`, `If-Match` (the generation), `x-ts-class`, `x-ts-job`, `x-ts-rank`. Response headers: `ETag` (the generation), `x-ts-queue-wait-ms`, `x-ts-service-ms`, `x-ts-drive-queue` (depth), `x-ts-drive-state`. A queue over its bound returns `503` with `Retry-After`; a drive that is `indexing` or `stalled` returns `503` with `x-ts-drive-state` and `Retry-After`, never 404.
- `PUT /cold/v1/<drive_id>/<khash>` (create-only, `x-ts-key`, trailer `x-ts-generation`), `DELETE` (with `If-Match`), `HEAD` (batched: many key hashes in one request), `POST /cold/v1/<drive_id>/touch` (a batch of key hashes whose last-read stamps are set to now, in memory, no disk I/O; used by holds, 7), `POST /cold/v1/<drive_id>/repair` (10.8), `GET /cold/v1/ping` (10.7: per-drive state, oldest in-flight chunk age, fill, queue depths by class, streams, membership epoch).
- It uses **the same part engine**: the `Upstream` trait gets a `ColdUpstream` implementation, so single flight, claims, verification, the controller and hedging are unchanged; only the transport and the choice among copies are new. A copy on the local node is served through the same `DriveSched` without HTTP.
- The tierstore S3 front on each node (10.9) and the cold service share the same index and scheduler.

### 10.7 Per-drive I/O scheduler
Every drive has a `DriveSched`, owned by the node that holds the drive, run on **dedicated blocking I/O threads per drive, `drive.streams x drive.depth` of them** (2 x 2 = 4 by default), never the tokio blocking pool, so the configured depth is real and a drive stall cannot starve the async runtime.
- **Streams.** A *stream* is one request's sequential run over one file (a part read, an upload, a repair copy, or a segment batch). The scheduler admits at most `drive.streams` streams at once (default **2**), of which at most `drive.write_streams` (default **1**) write. A stream issues `chunk` = 4 MiB `O_DIRECT` requests at depth `drive.depth` (2), the device's `max_sectors_kb`; the arm sees ascending addresses within a file. Whether a read stream copies to the socket from an `O_DIRECT` chunk or uses buffered `sendfile` with `DONTNEED` is a T0-F arm; the default is `O_DIRECT` (exact device request sizes, no page-cache pollution on a node whose SSD is the cache).
- **Classes and order.** Waiting requests sit in class queues; a free slot goes to `demand` > `write` > `prepare` > `background` (background reads and repair writes). Within a class: deficit round robin across job ids, then `(rank, arrival)`. Among equal candidates the scheduler prefers the stream whose file's first physical extent (read once with FIEMAP at open and cached) is next above the current head position, an elevator over at most a few waiting streams.
- **Writes are never starved [D].** While a write waits, at least one slice in every four on that drive goes to a write (`drive.write_min_share`, 25%), whatever reads wait. So a read-busy drive slows a PUT, never stalls it, and the abandon rule (10.5) sees progress.
- **Slices.** An admitted stream keeps its slot for `slice_bytes` (default 32 MiB, about 200 ms at 150 MB/s, so a seek of about 14 ms costs under 7%); at the slice boundary it yields only if another stream of equal or higher class (or a write owed its share) waits.
- **Demand preemption.** A `demand` request that finds only lower-class streams admitted takes the next chunk boundary of the lowest-class stream (that stream is paused and resumes later, costing two seeks); preemptions are limited to `demand_preempt_max` per second per drive (default 4), so a demand storm cannot turn the drive into a seek machine; beyond that a demand waits for the slice boundary. A write's guaranteed share is not preempted.
- **Small lane.** A read of one small object (10.4) does not take a stream slot. On a drive with an idle I/O thread it runs at once on that thread; otherwise it runs at the next chunk boundary of any admitted stream, on that stream's thread, at most one small read per boundary, by class order; `drive_small_reads_total`. Small writes ride the write stream's segment batch.
- **Read-ahead and slot release (slow consumers) [D].** Each read stream has a buffer of two chunks (8 MiB): an I/O thread reads the next chunk only while the buffer has room, and the socket writer drains it. If the buffer is full and the socket has accepted no byte for `drive.slot_release_after` (default 1 s [I]), the stream gives up its slot: it leaves the admitted set and is **parked** at its current offset, keeping its buffered chunks, and the slot goes to the next waiter. Parked streams are a separate set per drive, bounded by `drive.parked_max` (16), and do not count toward `queue_max`, so stopped clients never make new requests see 503; beyond `parked_max` the oldest parked stream is ended with an error and its client retries. A parked stream whose socket drains rejoins the waiting queue of its class with its original arrival rank. Parked buffers per node are bounded by `cold.parked_buffer_max` (256 MiB); beyond it a parked stream drops its buffer and re-reads at readmission. `drive_slot_releases_total` counts. A client whose hash pool pauses receive (4.7) therefore holds buffered bytes, not the spindle.
- **Watchdog and liveness [D].** Every blocking call a drive's I/O threads make records its submit time (an injected clock in tests): chunk reads and writes, `fsync` of files and directories, `renameat2`, `open`, `fallocate`, FIEMAP, `unlink`, segment seals and the startup `readdir`. A watchdog task checks every drive each second; any such call outstanding longer than `drive.stall_after` (default 10 s [I]; T0-F C-9 reads the kernel's SCSI command and error-handling timeouts to place it) puts the drive in **`stalled`**: every queued request fails at once with `503`, `x-ts-drive-state: stalled` and `Retry-After`; new requests are refused the same way; other admitted streams on the drive fail at their next chunk boundary; copies of PUTs in flight are abandoned (10.5); `drive_stall_total` counts. A thread blocked in the kernel cannot be cancelled: it is counted in `drive_io_threads_stuck`, and the drive leaves `stalled` only when every stuck call has returned **and** a probe read of the `DRIVE` file's block completes within `stall_after`; it is then `up`. Three stalls within an hour make the drive `failed` (below).
- **Liveness is the scheduler's, not the process's.** `GET /cold/v1/ping` is answered from the scheduler's state: per drive its state, the age of its oldest in-flight call, queue depths and streams. A peer treats a drive whose oldest in-flight call is older than `stall_after` as `stalled` even before the owner's watchdog flips it.
- **I/O errors.** An EIO on a read of one file quarantines that copy (renamed `.bad`, removed from the index, repair queued, 10.8); the drive stays `up`. `drive.eio_limit` errors (3) on distinct files within 10 minutes, or three stalls within an hour, make the drive **`failed`**: the node writes `FAILED`, serves nothing from it, and an operator clears it (no automatic retry loop on a dying disk). A write EIO abandons that copy and counts toward the same limit.
- **Backpressure.** Each class queue has a bound (`drive.queue_max`, default 8 requests). A read beyond it gets `503 Retry-After`. A drive whose write queue is full is not writable: the coordinator places the copy at the next writable position within `c + m` (10.2), and if there is none the PUT writes fewer copies or answers 507 (10.5); a write never waits unboundedly for a full queue. The client treats a 503 as a decrease signal **for that drive** (4.3) and a reason to prefer the other copy. **No number of clients can drive a spindle past `s*` streams.**
- **Instrumentation.** `x-ts-queue-wait-ms`, `x-ts-service-ms` and `x-ts-drive-state` on every response; per drive: state, streams active, queue depth by class, wait and service histograms, slice yields, preemptions, slot releases, small reads, stalls, stuck threads, bytes, util (from `/proc/diskstats`).
- **Constants [I until T0-F]:** `streams` 2, `write_streams` 1, `depth` 2, `chunk` 4 MiB, `slice_bytes` 32 MiB, `demand_preempt_max` 4/s, `queue_max` 8, `write_min_share` 25%, `slot_release_after` 1 s, `stall_after` 10 s. T0-F sweeps streams {1, 2, 3}, chunk {1, 2, 4, 8} MiB, slice {8, 16, 32, 64, 128} MiB, depth {1, 2, 4}, and runs the mixed, slow-consumer and churn arms of section 16.

### 10.8 Membership, drive states, repair, audit and eviction
**Membership document** (`cold-membership`: epoch, and per drive id: node, address, weight, `in` or `out`), distributed by hostkeeper as a file (configuration is files and flags). Only an operator changes it. Nodes piggyback `x-ts-mem-epoch` on peer responses; a node that sees a higher epoch re-reads its file. Views may differ for a few minutes; the probe depth, the hint and the existence query make that harmless. Drive identity is checked at startup: the path's `DRIVE` file and filesystem UUID must match the document, or the drive is not opened (a wrong-mount guard).

**Drive states.** `out` is the only cluster-wide state. The others are a node's view, either reported by the drive's owner or concluded by the viewing node:

| State | Set by | Reads | Writes and repair targets |
|---|---|---|---|
| `up` | default; owner after index, stall or probation ends | yes | client writes below `fill_limit`, repair below `repair_limit` |
| `indexing` | owner, from start until its index is built | 503 `indexing`; never a miss | no |
| `stalled` | owner's watchdog, or a viewer from the ping's oldest in-flight age (10.7) | 503 `stalled`; skipped | no; copies in flight abandoned |
| `suspect` | the viewing node: 3 failed pings or error responses; back to `up` after 3 successes | last in preference | no |
| `down` | the viewing node: the owner node answered no ping for 3 intervals (6 s); applies to all its drives | not asked | no |
| `failed` | owner: `FAILED` written (10.7); cleared only by an operator | not asked | no |
| `out` | the membership document | not a candidate (removed from `L`) | no |

**What happens, in each case:**
- **Node down temporarily (reboot), drive stalled or suspect.** Reads go to the other copy; writes go to the next writable position within the probe. Nothing is copied while it lasts; a PUT that wrote fewer than `c` copies queues repair (10.5).
- **Drive or node removed for good** (`out`): its copies are gone. Objects with a second copy keep it; repair restores the copy for objects read or recently written; others stay single-copy, and objects with no copy are misses that the CAS service republishes. **No scan of drives and no mass re-replication**, so no repair storm.
- **Adding drives or nodes.** New objects hash onto them. Existing objects are **not moved**; one whose position changed under the new view is found by the probe, the hint or the existence query, then repaired into its new canonical position, and its old copy is unlinked after the new one commits (a move by read).
- **Corruption.** Detected by the client cell digest (or the footer or record crc for `digest=none`); the copy is quarantined and repaired from the other copy.

**Repair [D].** An object has a **deficit** when fewer than `cold.copies` drives among the first `c + m` positions hold its generation.
- **Triggers:** (a) any read that finds a deficit (a 404 or 412 at a canonical position, a hit only through the hint or the existence query, a quarantined copy), whether the read was of one cell or the whole object; (b) a PUT without `x-ts-copies` that ended with fewer than `c` copies (10.5); (c) the index audit below, for objects read, touched or written within `cold.repair_recent` (7 days). While `cold.budget` is set (the migration window, 10.10), trigger (c) is off, repair writes count against the budget, and no trigger repairs an object the mover wrote with one copy on purpose: such a copy is named `<khash>.<gen>.<size>.m.pack` (the `m` marks a deliberate single copy; the index and the `HEAD` answer carry it). Once 10.11 step 4 raises the budget, those objects are repaired like any other when they are read.
- **What is copied:** the whole object, **drive to drive**. `POST /cold/v1/<target drive>/repair {key, generation, size, source drive}` goes to the node that owns the target; that node streams the object from the source copy through the cold service (a `background` read on the source, a `background` write on the target) and commits it under the key lock like a PUT (`identical` means already done). It never goes through a node cache, so an object that nobody read in full is repaired the same as one that was.
- **Dedup and order:** each target node keeps one bounded in-memory repair queue keyed by key hash (one entry per key, 64k entries; overflow is dropped and counted in `cold_repair_dropped_total`, and the next read or audit finds the deficit again), served most-read first.
- **Rate:** `cold.repair_bytes_per_hour` per target drive, default 10% of the drive's sustained write rate as T0-F measures it [I], and as a `background` stream it yields to every other class.
- **Targets:** only a drive that is `up` and below `repair_limit`; never `out`, `failed`, `stalled`, `suspect`, `down` or `indexing`. If the canonical position is not writable, the next writable position within `c + m`; if there is none, the entry stays pending in the queue (`cold_under_replicated_total{cause="no_target"}`) and is retried when a drive of the probe returns to `up` or falls below `repair_limit`, and at least every 10 minutes.
- **No copy-and-evict loop:** repair writes only below `repair_limit` (80%) and eviction starts only above `evict_at` (85%), so a repair never pushes a drive into eviction; the 5-point gap is the hysteresis. A repaired copy gets a fresh stamp, so it is not evicted within `evict_min_age` either.

**Audit [D].** Copy count is measured, not assumed.
- **Index audit (continuous, no disk I/O).** Each node walks its own drives' indexes in random order at `cold.audit_rate` (default 20 keys per second per drive) and asks the other probe positions of each key with the batched in-memory `HEAD`. A sampled key is counted only by the drive that is its first holder in `L(K)` order, so a key with two copies is not counted twice as often as a key with one. It records each counted key's copy count in `cold_audit_copies_ratio{copies="1|2|3+"}` (the share of counted keys over the last 24 h; a drive of 100 thousand objects is covered in under 2 hours); this audit cannot see keys with no copy, which the live-key audit does, and queues repair for a deficit whose object is within `repair_recent`.
- **Live-key audit.** `tierstore cold audit --keys <file>` takes a random sample of live keys that the CAS service supplies from its sweep (T-K1a), queries every probe position and the existence query for each, and reports the histogram including **0 copies** (live objects lost), as a report and as `cold_audit_live_copies_ratio{copies="0|1|2|3+"}`.

**Space accounting [D].** Per drive, `used` is the sum of the **allocated** bytes of the tier's files: measured by `fstat` (`st_blocks`) at commit and kept in the index; at startup, until a low-priority background `stat` pass has measured them, each file counts as its size rounded up to 64 MiB (an upper bound under the extent hint). `capacity` is the filesystem size (`statfs` `f_blocks`). Fill is `used / capacity`; the four marks of 10.4 (`repair_limit`, `evict_to`, `evict_at`, `fill_limit`) compare it. During the migration `cold.budget` caps `used` (10.10). A second guard reads `statfs` free space: below 5% of the filesystem, the drive takes no writes whatever `used` says, because silo's data shares the drive. T0-F C-5 measures allocated against file size.

**Eviction.** Each object has a last-read stamp in the index, set by reads, writes and `touch` (7). Stamps are saved to `stamps` at clean shutdown; otherwise the background `stat` pass rebuilds them from file mtime, never lowering a stamp a read or touch has set since start. **Eviction on a drive does not run until one `touch_interval` has passed since the drive became `up`**, so every hold has touched its packs first and a restart can never make a held pack look old.
- Above `evict_at`, the node evicts the **least recently read** objects on that drive, by `unlink` (large files) or whole segment (small objects, 10.4), in batches, until it is below `evict_to`; the copy-forward of a segment's recent records is the one write allowed above `fill_limit`, because it is bounded by one segment and the segment's unlink frees more than it writes; `cold_evictions_total` and bytes are metered.
- **An object whose stamp is younger than `cold.evict_min_age` (default 1 h) is never evicted.** Holds touch their packs every `cold.touch_interval` (10 minutes, under a third of the minimum age), so a running job's packs are never evicted under it. If nothing is evictable, the drive keeps taking client writes up to `fill_limit`, then is not writable (writes go to the next position), and `cold_evict_blocked_total` counts it.
- The two copies of an object are evicted independently per drive; a copy evicted on one drive is a deficit, which repair restores only for recently used objects and only below `repair_limit`.
- Eviction is on by default because cold data is cache; with `cold.evict = false` a full drive is skipped for writes and a PUT that finds no writable position answers `507`.
- The CAS service treats a 404 on a pack it believes exists as "tree not present" (the miss path of 3.5); a job that is running when it happens takes the lost path of 4.6.

**Deletes.** `DELETE` (with `If-Match` when the generation is known) goes to every probe position, the hinted drives and every existence-query hit. A node down at that moment keeps its copy; it is the same bytes (write-once-ever) and is evicted by LRU. No tombstones.

### 10.9 The S3 API subset and how the CAS service uses it
Tierstore's S3 front serves exactly the `ObjectStore` operations the CAS service needs (`put_new`, `get_range`, `delete`, `capabilities`) and the engine's ranged reads:

| S3 operation | Cold-tier behaviour |
|---|---|
| `PUT` object, `If-None-Match: *` | Create-only streaming PUT as in 10.5. Without the header the request is still create-only. No multipart, no copy, no overwrite. `200` if absent or identical, `412` if the key holds other bytes, `503` if fewer than `min_copies` committed, `507` if no drive can take it. |
| `GET` with `Range` | One byte range (cell-aligned reads are the fast path; any range works: the drive reads 4 KiB-aligned and trims). Served by 10.6. Without a range: the whole object as a stream. `503` (never `404`) while the answer is not definitive (10.6 step 4). |
| `HEAD` | From the in-memory index of the first reachable copy: size, `ETag` (generation), no disk access. |
| `DELETE` | As in 10.8; `204` even when absent. |
| `LIST`, multipart, versioning, ACLs, tagging, conditional overwrite | `501`. If the CAS service's rebuild or weekly sweep needs `LIST` (its only users [I]), T-C5 adds an offline `tierstore cold list` that reads the indexes; whether it is needed is a **The CAS service decision (T-K1a)**, not a requirement of the tier. |

The `ObjectStore` conformance suite of the CAS service (fake, MinIO, others) gains a tierstore-cold target; it proves the CAS service does not rely on conditional writes or LIST (the suite's test that fails when the CAS service leans on either runs against this target too), and that the CAS service never PUTs different bytes under a key it used before (10.5).

### 10.10 Migration off silo
Silo stays up, read-only, for the window. Steps, each reversible until the last, with the cost of each reversal stated:
1. **Ship the cold tier dark** (T-C1 to T-C6): daemons own `ts-cold/` on every drive beside silo's data, with `cold.budget` per drive set by the capacity rule below; nothing writes to it.
2. **Add the legacy upstream:** `cold.legacy_upstream` = silo's S3 endpoint, **read-only**. A fill tries the cold tier first and reads silo when the cold tier does not serve it (10.6 step 4); only a definitive miss destages, so a restarting node never starts a burst of destage copies. The part-grid rule for silo-origin packs (`upload_part_size`, 4.4) applies on that path only. **Cost of the fallback, per fill (not per part):** up to `c + m` in-memory `HEAD`s (one batched round trip per node) and one existence-query round trip before the silo GET; the location hint then records **legacy** for `cold.legacy_hint_ttl` (10.6 step 1), so later fills of the pack on that node go straight to silo. Metric `legacy_reads_total{outcome}`.
3. **Flip the CAS service's writes:** the CAS service's object-store endpoint becomes tierstore's S3 front (config); every new pack goes to the cold tier with one streaming PUT. **Rollback of this step is not free:** every tree published after the flip names packs that exist only in the cold tier, so a rolled-back CAS service, which reads silo, finds none of them: each such tree is a miss (republish or rebuild), a cold start of every tree published since the flip. The runbook (T-C7) reads the scale from `cold_puts_total` since the flip and sets the point after which rollback is not offered. No reverse mover is built.
4. **Old packs migrate (decision: both mechanisms, in this order).** (a) *Lazy destage:* a definitive miss served from silo sends a destage request to the node owning the first position of `L` whose drive is `up` (the **destage owner**; if no such position exists the requesting node destages itself, and the duplicate converges as an `identical` PUT), which deduplicates by key, reads the **whole** pack from silo once as one stream (rate limited by `cold.destage_bytes_per_hour`) and PUTs it as coordinator. Concurrent requests from several nodes converge at the owner, and a duplicate after a restart is an `identical` PUT. Silo pays one extra full read of each distinct pack used during the window (`legacy_destage_bytes_total`); partial reads never trigger more than that one read. (b) *Age out:* the CAS service condemns and deletes packs as trees expire; DELETEs go to the cold tier and to silo. (c) *Cutoff copy:* at a date set from the `legacy_reads_total` trend, the CAS service (which knows every live pack) drives a throttled, resumable one-time mover over live packs not yet in the cold tier: read from silo, PUT to the cold tier, verify against cell digests where a list exists, else against size and silo's checksum; 1 stream per node, off-peak. Dead packs are never copied.
5. **Measure the end:** when `legacy_reads_total` has been 0 for 7 days and the mover reports nothing left, step 10.11.

**Capacity during the window [D].** Silo's data and the cold tier share the drives until 10.11 step 4, and silo is one erasure set over all 12 drives, so it cannot be drained node by node. With `L_silo` silo's logical bytes (it occupies about `1.5 x L_silo` raw at 8+4), `L_live` the bytes of packs the CAS service still lists as live (at most `L_silo`), raw capacity R = 87 TiB, budget fraction `b` per drive and `c_m` the copies the mover writes:
- both must fit under the fill limit: `1.5 x L_silo + b x R <= 0.9 x R`, so `b <= 0.9 - 1.5 x L_silo / R`;
- the tier settles at `evict_to` (80%) of its budget (10.4), so the migrated packs alone fit only if `L_live <= 0.8 x b x R / c_m`;
- Examples [arithmetic]: `L_silo` = 20 TiB gives `b <= 0.55`, room for about 19 TiB of unique packs at `c_m = 2`; `L_silo` = 30 TiB gives `b <= 0.38`, about 13 TiB at `c_m = 2` and 26.5 TiB at `c_m = 1`. At a 40% budget, about 14 TiB unique at `c_m = 2`.
- new packs are written during the window too, at `cold.copies` copies: with `W` the unique bytes the CAS service writes over the window (from `cold_puts_total`), the full condition is `L_live / c_m + 2 x W <= 0.8 x b x R`. During the window the fill marks of 10.4 apply to `b x R` instead of the drive's capacity, so eviction keeps the tier inside the budget; a migrated pack that eviction removes before silo is retired is copied again by the mover's final pass (10.11 step 1).
- **Rule:** T-C7 starts only after T0-F C-6 measures `L_silo` and the CAS service reports `L_live` and its write rate. `b` is set from the first inequality. If `L_live / 2 + 2 x W > 0.8 x b x R`, the mover writes migrated packs with `x-ts-copies: 1`; after silo's data is removed and the budget raised (10.11 step 4), repair restores their second copy as they are read (10.3, 10.8). As the CAS service's deletions shrink silo, the operator may raise `b` by the same formula. If `L_live + 2 x W > 0.8 x b x R` even at one copy, the migration does not start and the owner is told.

### 10.11 Retiring silo
Held scripts (dry-run by default, never forcing):
1. Confirm preconditions: `legacy_reads_total` zero for 7 days; the mover's final pass has copied every live pack the cold tier lacks; no live pack missing from the cold tier (the live-key audit over the CAS service's whole live list reports 0 copies for none); cold tier below the fill limit once silo's space is reclaimed.
2. Stop writing to silo (already true) and remove `cold.legacy_upstream`; this is the **last reversible step** (silo data is intact; re-adding the upstream restores the fallback).
3. Stop and disable the silo service on each node through hostkeeper, one node at a time under the one-live-update coordinator.
4. **Destructive, held script:** remove silo's data from each drive after a final listing is archived; raise `cold.budget` to the fill limit; remove the silo package and unit from the node image.
5. Delete the `upload_part_size` handling, the legacy upstream and the silo client (T11b).

### 10.12 Comparison with silo 8+4 (measured in T0-F and T-B5)
| | Silo EC 8+4 [M geometry] | Cold tier |
|---|---|---|
| Unit read from a drive | 128 KiB shard per 1 MiB block, so S/8 per drive per GET (8 MiB per drive at a 64 MiB GET) | the whole part, sequential, from one drive (16 to 64 MiB and more) |
| Drives touched per GET | 8 (a GET waits for the slowest of 8 queues) | 1 (a demand can pick the shorter of 2 queues and hedge) |
| Efficiency model at 14 ms positioning, 150 MB/s [I] | 8 MiB per switch: about 79%; 4 MiB: 66% | sustained stream: about 97% at 64 MiB and above, switches only at 32 MiB slices |
| Streams per spindle under 6-node load | uncontrolled: `G x 8 / 12` | admitted at most 2, enforced at the drive |
| Small objects | one erasure stripe each | records in write-once segments, group commit (10.4) |
| Write cost | 1.5x bytes, 12 drives, shard-sized writes | 2x bytes (copies = 2), 2 drives, large sequential files |
| Capacity | 66% of raw | 50% of the eviction low mark at copies = 2 (100% at 1) |
| Failure | any 4 drives | one copy per node; two nodes down lose the objects with both copies there (misses) |
| Repair | heal | drive to drive, for objects read or recently written; copy count measured by the audit |
| Bitrot | per-shard checksums | client cell digests, footer and record crc for `digest=none`, second copy |

T0-F measures the first rows on the same drives (section 16): **one drive streaming whole packs against silo 8+4 ranged GETs, same pack sizes, same concurrency levels, same idle node**.

### 10.13 Risks named
- **SMR churn is the steady state, not a tail.** With eviction on and the tier full, drives sit between `evict_to` and `evict_at` and write into the space eviction frees: the delete-then-write-into-holes pattern that can collapse sustained write throughput. Defences: write-once files, 64 MiB extents, whole-segment small objects, the fill marks, repair only below `repair_limit`, writes guaranteed a share of the drive. T0-F arm C-5b runs that steady state with the real eviction code and decides the marks; if sustained write after churn is under 50% of a fresh drive, the per-epoch-directory fallback of 10.4 is built.
- **Capacity at copies = 2** (10.3) and **during migration** (10.10): checked against measured bytes before T-C7.
- **Hot spots.** HRW is uniform per key, but a few very hot packs (toolchains) concentrate on two drives. The defences are the node SSD caches (each node reads a hot pack from the tier once), per-drive backoff so the hot drives throttle only themselves (4.3), the hedge to the second copy, and slot release so a slow reader does not hold a hot spindle (10.7). There is no extra copy for hot objects and no peer cache read path (a node serving its cache to other nodes); `cold_pack_reads_total` top-N (by hash, never labelled) and per-drive 503 rates show whether one-pack-many-nodes stalls happen, and if they do, a peer cache read path is the follow-up design.
- **Effective copy count below 2.** PUT abandons, evictions and lost nodes all lower it; repair restores it only for objects in use. The audit histograms (10.8) show the real number, and the availability claims of 10.3 are read against them.
- **Membership drift** is bounded by the probe depth, the hint and the existence query; the epoch piggyback shortens it.
- **Coupling with the CAS service's eviction:** with `cold.evict` on, a pack the CAS service believes exists can vanish once unheld for `evict_min_age`; the CAS service's 404 handling (the miss path, T-K1a) and the lost path of 4.6 are requirements.

## 11. The cache device: a real partition, not a loop on an image
**Decision [D]: a dedicated xfs partition of the node SSD (noatime), no loop device, subject to the T0 gate.**
1. Each `fdatasync` through xfs-on-loop-on-image costs two journals and two flushes.
2. Two page caches and two dirty accountings double memory pressure (`losetup --direct-io=on` is the stopgap, not the design).
3. Cell-group eviction frees space only if discards propagate through the loop to the image; on a partition `PUNCH_HOLE` frees directly and `fstrim` works.
4. A fixed 185 GiB image takes the space whether or not the cache is full.
5. One fewer layer between FUSE reads and the SSD.

The device is the root SATA SSD (the drives are the cold tier, SMR, and must not carry the cache): a new partition of budget + 15% (185 GiB -> 220 GiB). **If the root filesystem is xfs it cannot be shrunk; T0 records its type.** The plan is free unallocated SSD space if it exists, else a node image re-provision through hostkeeper in a maintenance window; never an in-place live repartition by tierstore.

**Gate (T0 B):** move if loop `fdatasync` p99 > 2x a file on the host fs, or sustained write < 80% of the raw SSD at 1 MiB, or mixed-load read p99 > 2x; otherwise ship `direct-io=on` and keep the loop. **T10b depends on T13 (or runs on both devices until T13 lands) so constants are frozen on the shipped device.** The partition is mounted at a new path read from `cache.root`; the old image is not converted and is removed after the soak.

## 12. Metrics (every wait observable; labels never include pack or key)

| Metric | Kind | Answers |
|---|---|---|
| `part_permit_wait_seconds` by class | histogram | Waiting for budget |
| `part_net_seconds`, `part_first_byte_seconds` by class and size class | histogram | Controller inputs |
| `part_hash_wait_seconds`, `part_write_seconds` | histogram | Local time, kept out of the controller |
| `bytes_in_flight`, `parts_in_flight`, `parts_per_drive{drive}` | gauge | Load |
| `budget_ceiling_parts`, `budget_reason`, `budget_ladder_step`, `drive_permits{drive}`, `drive_decrease_state{drive}`, `win_goodput`, `win_p95`, `win_first_byte_p95`, `win_drive_wait_p95{drive}`, `cluster_budget_sum` | gauge | Controller state and why it moved, per drive and for the node |
| `retries_total`, `stall_retries_gated_total`, `hedges_total`, `hedge_wins_total`, `hedge_wasted_bytes_total`, `stall_cancels_total`, `cold_copy_generation_mismatch_total` | counter | Tail control |
| `gets_total` by kind, `gets_per_pack` | counter, histogram | GET counts |
| `bytes_fetched_total`, `bytes_needed_total`, `amplification`, `bytes_prefetched_never_read_total` | counter, gauge | Efficiency and waste |
| `read_wait_seconds` by cause, `reads_waiting`, `fill_wait_seconds`, `tree_ready_seconds` | histogram, gauge | Why a read waited |
| `request_to_start_seconds`, `launch_before_ready_total` | histogram, counter | End to end |
| `verify_seconds`, `verify_queue_depth`, `verify_lag_seconds`, `verify_failures_total`, `poisoned_cells`, `recheck_cells` | histogram, gauge, counter | Hash cost, integrity |
| `journal_commit_seconds`, `fdatasync_seconds`, `journal_records` | histogram | Durability cost |
| `held_bytes`, `holds`, `admission_refusals_total` by reason, `punched_bytes_total`, `evictions_total`, `evict_timeouts_total`, `holder_lifeline_hangups_total` | gauge, counter | Eviction pressure and liveness |
| `dirty_bytes`, `writeback_stall_seconds` | gauge, histogram | Cache device pressure |
| `blocking_pool_queue_len`, `hash_pool_busy` | gauge | Runtime saturation |
| per-disk `r_await`, `aqu_sz`, `util` (2 drives, SSD) | gauge | Per-disk queue |
| `fuse_transport_mode`, `fuse_requests_total{op}`, `fuse_inline_total`, `fuse_parked`, `fuse_request_seconds{op}`, `fuse_interrupts_total`, `uring_entries_parked`, `uring_registered_bytes`, `splice_bytes_total`, `pread_bytes_total`, `fadvise_dontneed_bytes_total` | gauge, counter, histogram | The FUSE layer |
| `fuse_d_state_threads` | gauge | Blocked FUSE threads |
| `mount_reconnects_total`, `takeovers_total`, `stale_epoch_replies_dropped_total` | counter | Restart behaviour |
| `list_refused_total{reason}`, `l2_sidecar_missing_total`, `tierstore_build_info` | counter, gauge | Format rollout |
| **Cold tier, per drive:** `drive_state{drive,state}`, `drive_fill_ratio`, `drive_streams_active`, `drive_queue_depth{class}`, `drive_wait_seconds{class}`, `drive_service_seconds`, `drive_bytes_total{dir}`, `drive_slice_yields_total`, `drive_preemptions_total`, `drive_slot_releases_total`, `drive_parked_streams`, `drive_parked_buffer_bytes`, `drive_small_reads_total`, `drive_503_total{reason}`, `drive_stall_total`, `drive_io_threads_stuck`, `drive_oldest_inflight_seconds`, `drive_eio_total`, `drive_index_ready`, `drive_util` | gauge, histogram, counter | Per-drive scheduling and liveness |
| `cold_puts_total{outcome}`, `cold_put_copies_committed`, `cold_put_copies_abandoned_total{reason}`, `cold_put_conflicts_total`, `cold_under_replicated_total{cause}`, `cold_repair_bytes_total`, `cold_repair_queue_depth`, `cold_repair_dropped_total`, `cold_existence_queries_total{outcome}`, `cold_negative_cache_hits_total`, `cold_location_hint_hits_total`, `cold_candidate_probe_depth` (histogram), `cold_evictions_total`, `cold_evict_blocked_total`, `cold_quarantined_total`, `cold_membership_epoch`, `cold_pack_lost_total{phase}`, `cold_touches_total`, `cold_pack_reads_total` (per key hash in memory; only an offline top-N report reads it, never a label) | counter, histogram, gauge | Placement, repair, eviction |
| `cold_audit_copies_ratio{copies}`, `cold_audit_live_copies_ratio{copies}`, `cold_audit_keys_total` | gauge, counter | Measured copy count |
| `cold_small_objects`, `cold_segment_live_ratio`, `cold_small_batch_seconds` | gauge, histogram | Small objects |
| `legacy_reads_total{outcome}`, `legacy_destage_bytes_total`, `legacy_mover_bytes_total` | counter | Silo migration progress |
| `entrystore_impl{linux_memfd,in_process}`, `nfs_requests_total{op}` | gauge, counter | macOS path |

`{drive}` is the drive id (12 values), never a key. Trace each demand read end to end: handler, wait sent, budget acquired, drive chosen, GET start, drive queue wait, first byte, cell received, hash done, write, reply.

## 13. Deleted code and migration of the cache
**Deleted**
- FUSE passthrough (`treefs/backing.rs`, `core/passthrough.rs`, `Handle::Backed` and `Spilled`, `max_backing_ids`, backing table, negotiation in `fuse.rs::init`).
- The per-blob path: `Plan::Blob`, `Origin::Span`, `fill_blob`, the `fill <sha> <size> from <key> @ <off>` request, `blob_path` in `cache_file`, per-file span copies.
- `pin_once`, `pinning`, `lifecycle`, `loaded.pinned`, touch/pin/unpin/pins_live traffic.
- All three prefetch mechanisms: `local_cache/prefetch.rs`, `fs-prefetch` threads, Prepare groups and `GROUP_MAX_BYTES`.
- Extents machinery: `local_cache/extents/*`, per-range flights, `extent_save`, `extents.writing`, `move_whole`'s re-hash, `fetch_parts` gap reading.
- The v1 list reader (only refusal remains) and, after the dual-emission release, the CAS service's v1 list writer.
- The komira-ai/fuser fork dependency and the blocking mount client (T-R, T11a).
- The erasure-set budget model and the `upstream.data_shards`/`upstream.drives` topology flags (T11a); at silo's retirement, the silo client as a general upstream, the legacy upstream and the `upload_part_size` handling (T-C8, T11b).
- Config: `max_concurrent_fills`, `fill_socket_concurrency`, `prefetch_concurrency`, `trees.fill_concurrency`, `fill_chunk`, `trees.fill_queue`, `trees.fill_rate`; `trees.coalesce_gap` and `trees.whole_pack_percent` replaced by section 6.

**Kept:** the reserve and critical-mark check (at claim time), staging files and create-only link for non-pack objects, lock-generation `owns()` checks, the S3-front index, tree image serving (komira-ai/tierstore#341, komira-ai/tierstore#342), tree store placements and the provider Prepare entry point, the NFS front on macOS.

**Cache layout migration.** It is a cache; there is no conversion. The new binary writes `packs-v2/`, does not read `extents-v1/` or `origin=span` entries, and deletes them in a low-priority `tierstore gc legacy`. **Rollback:** the old binary ignores `packs-v2/`; during the overlap `packs-v2` is capped at 50% of the budget and the rollback procedure includes `tierstore gc packs-v2 --purge` (a held script) before the old binary starts. List-format rollback is in 3.5; cold-tier rollback is in 10.10.

## 14. Test plan
Behaviour tests only: an injected clock, a fake upstream with scripted per-part and per-cell completion order and corruption, a fake drive with scripted service times, hangs and errors, scripted interleavings of coordinators, and progress barriers. No test asserts timing: every "after X seconds" below is a step of the injected clock.

| Test | Proves | Defect it catches |
|---|---|---|
| N readers on spans of one pack: one `PackFill`; each cell claimed once | Single flight | Duplicate fills |
| Demand sub-range plus overlapping grid part: skips claimed cells; GETs <= parts + sub-ranges; bytes_fetched == bytes_needed | Claim state | Cell fetched twice |
| Reverse-order part completion: a read on the last cell is served after only that cell lands | Independent per-cell verification | Frontier stalls |
| A 1 GiB file span: a read at the end waits only for its cells | Per-cell digests | Whole-file wait |
| Hedge and original both deliver a cell: one publishes, the loser's bytes never written | Write-once cells | TOCTOU rewrite |
| Corrupt bytes in one cell with two copies: refetched once from the other copy; with one copy a second corruption marks only that cell `bad` | Per-cell failure scope, copy failover | Whole-entry poison |
| A copy answers 412 to its generation: that copy is dropped for the fill, the part goes to the other copy, nothing of the other bytes is written; with no matching copy the fill fails | Per-copy `If-Match` | Mixed generations; one bad copy failing the pack |
| Two lists with the same pack bytes give one PackId; a changed pack a new path | `pack_root` identity | Path collision |
| S3-front object: a cold-tier object's parts all carry `If-Match` from the location; a silo-origin object's first part goes alone; an overwrite between parts on silo yields 412 | Section 9 | Silent version mix |
| Ramp and grid: a first touch mid-pack ramps to the next grid boundary and every later part is a whole grid part; at offset 0 the 5th piece (4 MiB, after 4+8+16+32) is trimmed at the 64 MiB boundary; no runt after the ramp | 4.4 | Runt parts, boundary crossing |
| Budget: floor, share, ceiling and bytes in flight equal the table of 4.3 for (D, N, s*, P) and are recomputed when P changes; share is at least one part at every P | 4.3 | Contradictory budget |
| **Per-drive controller: two drives answer 503 and doubled queue waits (a hot pack); only those drives' permits fall and the parts in flight to the other ten drives are unchanged; the node ceiling falls only when over a third of drives with traffic are in decrease or the network signal doubles; a `stalled` or `indexing` answer changes no permit** | 4.3 | One hot pack throttling the node; a slow drive hidden in a pooled median |
| v1 list or missing L2 sidecar: refused, `ListVersionUnsupported`, no bytes served, no code path parses v1 | No legacy reader | Serving unparsed data |
| Backfill: `l2-backfill` writes an L2 sidecar under the unchanged tree id, reads each pack once, and a second run writes nothing | 3.5 | Identity change; repeated reads |
| Dual emission: the CAS service writes v1 and L2 under one unchanged tree id; old reader ignores the sidecar; new reader ignores v1; id equals the pre-change id for the same tree | 3.5 | Action-cache invalidation |
| The CAS service on `ListVersionUnsupported` uses its miss path and the job continues | 3.5 | Partial outage |
| L2 round trip: each cell digest equals independent sha256; deterministic; sorted directories | Format | Wrong digests |
| Crash between `pwrite` and journal append: cell absent after restart; no journal record visible without its data | Commit ordering | Trusting zeros |
| Crash after a batched PUNCH fsync and before/after the punch: replay never marks a punched cell `present`; one fsync per batch | Section 8 | Zeros after punch + crash |
| Process crash, same boot: restored cells `present`, no recheck; different boot or no boot_id: every restored cell `recheck`, not served by the mount, corrupted cell refetched | Section 8 | Needless rehash; silent bad serve |
| Journal batching: at most one `fdatasync` per dirty entry per interval; `durable` set only after it | Batching | Per-part commit; DONTNEED on dirty pages |
| Loom: reader/evictor protocol with per-cell CAS, neighbouring cells going `absent -> claimed -> present` during the scan, a reader straddling groups, post-read recheck: no reader returns bytes of a cell whose punch has begun; the evictor never changes a `claimed` cell | 5.1 | Zeros served; lost claim |
| Mount's state mapping is read-only: a `PROT_WRITE` mmap of the passed state memfd and a `write(2)` fail | 5.1 | Cross-mount corruption |
| Mount killed (lifeline hangup): counters dropped, eviction proceeds; mount whose protocol connection flaps keeps counters, regions and holds and is never zeroed while its lifeline is open | 5.1 | Wedged eviction; punch under a live reader |
| A mount grows its counter memfd past the server's mapping: the server remaps before acknowledging `open-pack`, and the evictor reads the new region's counters | 5.1 | Evictor reading past its mapping; a missed reader |
| Reader with a forced loss of its counter: post-read recheck discards the bytes | 5.1 | Zeros after forced clear |
| Live reader stuck beyond `evict_wait`: won cells restored `present`, nothing journaled, `evict_timeouts_total` increments | Evictor timeout | Evictor deadlock |
| More than 128 holders on one pack are all served | 5.1 | EBUSY on shared packs |
| Eviction removes an unheld entry's path while a mount holds the data fd: reads return that entry's bytes | fd, not path | Unlink race |
| Held-only cache: punch eviction admits a new job; admission refuses when estimate exceeds 0.9 x budget | Holds and admission | Deadlock, double counting |
| Scripted 2x first-byte or drive queue wait with every node at its floor: the ladder steps down and in-flight decreases; a `503` is a decrease signal for its drive | 4.3 | Floor removes authority |
| Stall before the first byte is not retried below the pre-first-byte timeout; in a decrease state only transport errors and `stalled` retry; one stall retry per drive per window; a retry goes to the other copy | 4.6 | Retry storm |
| Demand reserve granted with prepare saturating the node | Demand reserve | Starvation |
| Two jobs both at rank 0 alternate by DRR; small early-rank pack precedes the tail of a 4 GiB pack; active-pack window <= 3 | Fairness, rank | Head-of-line |
| Hedge only for demand, only with two reachable copies of the same generation, other queue shorter, neither drive in decrease | Tail control | Hedge feedback loop |
| **Pack lost mid-job: after Prepare, every copy answers 404 and the existence query finds nothing: waiters get EIO, every holder's mount of the tree is marked lost, each job is reported failed with the retriable cause `tree-lost`, no byte of another generation is served; with one probe drive `indexing`, or the node of the only copy `down`, the same fill waits and resumes when the node returns, with no `tree-lost`; still unanswered after `unavailable_wait`, it is lost** | 4.6, 10.6 | Silent EIO to a build; a reboot taken for a loss |
| **Definitive miss: one copy on a `down` node and the other absent gives 503 to a client, no negative entry, no destage, no repair; every probe drive `up` and absent and every node answering gives a definitive miss** | 10.6 | Reboot reported as "pack gone" |
| **Pinned generation: two copies of a `digest=none` object hold different generations; the fill pins one, every GET carries it, the other copy is excluded and counted, and no part of the other bytes is written** | 4.1, 4.6 | Mixed generations in one fill |
| **Held packs are not evicted from the tier: a hold touches every copy of its packs every `touch_interval`, including a held pack the job never opened; with the drive above `evict_at`, the evictor skips every object younger than `evict_min_age` and evicts an older one; with nothing evictable it counts `cold_evict_blocked_total`; after a drive restart with stale stamps, no eviction runs until one `touch_interval` has passed, so a held pack is not evicted** | 7, 10.8 | Tier eviction failing a running job |
| Placement: distribution across 12 drives within tolerance for 1e6 keys; copies on distinct nodes; marking a drive `out` moves only keys whose copy set held it; adding one steals about weight/total; deterministic across processes; weights respected | 10.2 | Skew, correlated copies |
| **Probe covers every write: with zero, one and two nodes `down` and drives full or `stalled`, every copy a PUT places is within the first `c + m` positions of `L` (counted after the distinct-node filter); when fewer than `c` writable positions remain in the probe the PUT writes fewer copies and queues repair, never a copy beyond it** | 10.2, 10.5 | Unfindable object |
| **Two coordinators race one key with the same bytes under every scripted interleaving of their per-drive commits: both copies end with one generation and one file name, both PUTs answer 200, a retry is `identical`** | 10.4, 10.5 | Diverged copies; failover 412 |
| **Two coordinators race one key with different bytes: no drive ever holds two generations of the key, each PUT that meets the other's generation answers 412 and deletes its own committed copies, a reader never mixes bytes; the key may end absent** | 10.5 | Two generations served as one |
| **A copy's bytes are damaged in transit: the writer's generation differs from the trailer, it unlinks its `.up` and answers `generation-mismatch`; the PUT succeeds on the other copy and queues repair** | 10.5 | A corrupt copy committed |
| Pack write: crash before rename leaves only `.up`, swept at start; object invisible until renamed | 10.4 | Partial object visible |
| **Startup index from names alone: a drive's index is built by `readdir` of the 256 directories with no file opened and no `stat`; size, generation and the footer offset agree with the file; until it is built the drive answers `503 indexing` to GET and HEAD, the existence query reports `indexing`, no negative entry is cached and no legacy read or repair starts** | 10.4, 10.6 | Header reads at start; a restart read as "pack gone" |
| **PUT acknowledgement: with one copy's buffer held full by its drive for `put_copy_stall` while the other copy waits for bytes, the PUT abandons it, answers 200 at `min_copies = 1`, the writer unlinks its `.up` when the connection closes, repair is queued; a client pause longer than `put_copy_stall` abandons nothing; the PUT never answers while a live copy is still progressing; all copies abandoned answers 503; no writable position in the probe answers 507; a mover PUT with `x-ts-copies: 1` queues no repair** | 10.5 | PUT failure from one bad drive; silent single copy; a slow client failing its own PUT |
| **Small or large path: a chunked PUT of exactly `small_max` bytes becomes a segment record and one of `small_max + 1` bytes a file; with Content-Length the size decides** | 10.4, 10.5 | Unknown size sent down the wrong path |
| **Hung drive (fake drive whose read never returns, and separately an `fsync` that never returns during a commit): after `stall_after` on the injected clock the drive is `stalled`, every queued request fails at once with 503 `stalled`, ping and response headers say so, `drive_stall_total` is 1, the client fails over to the other copy; the drive returns to `up` only after the stuck read returns and the probe read succeeds; three stalls in an hour make it `failed`** | 10.7 | A wedged drive that pings healthy |
| **Slow consumer: two clients stop reading; each stream fills its two-chunk buffer, gives up its slot after `slot_release_after` and parks; a third client's request is admitted and served; with more than `queue_max` parked streams a new demand is still admitted (parked streams do not count toward `queue_max`) and beyond `parked_max` the oldest parked stream is ended; the parked streams resume at their offsets with no byte lost or repeated; past `parked_buffer_max` a parked stream drops its buffer and re-reads** | 10.7 | A stalled client holding the spindle |
| **Drive scheduler (fake drive, injected clock): never more than `streams` admitted and `write_streams` writes; I/O threads = streams x depth and the fake sees that many requests in flight; slice yield only for equal-or-higher class waiting; demand preempts at a chunk boundary, limited by `demand_preempt_max`; a waiting write gets at least one slice in four with demand and prepare saturating the drive; DRR across jobs; small reads served between chunks without a slot, and at once on an idle drive; queue beyond bound returns 503** | 10.7 | Stream pile-up; starved PUT; seek storm; depth that is not real |
| **Small objects: objects at most `small_max` go to the open segment, one `fdatasync` per batch, each PUT acknowledged after its batch's sync; a crash before the sync leaves none of the batch visible; the index is rebuilt from sealed trailers in segment order and the open segment's records; a small object deleted, its segment sealed and the drive restarted stays deleted; a segment with no live record is unlinked; evicting a segment first copies forward the records read within `evict_min_age`; a record's crc mismatch is a verification failure** | 10.4 | One file and fsync per small object; lost or torn small writes |
| **I/O errors: an EIO on one file quarantines that copy, keeps the drive `up` and queues repair; `eio_limit` errors on distinct files in the window make the drive `failed`** | 10.7 | One bad sector removing a whole drive |
| Copy choice: hint first, then local copy, then shorter advertised queue; failover on 404, on 503 and on node down; hedge to the other copy | 10.6 | Slow-spindle tail |
| **Location hint: filled by an existence-query hit, an off-canonical read, a repair and a PUT placed outside the first `c`; dropped on a 404 or 412 from the hinted drive; a legacy entry expires after `legacy_hint_ttl` and is replaced by a local PUT, a destage commit or a cold-tier hit** | 10.6, 10.10 | Stale location; silo read forever |
| **View states on scripted pings (injected clock): three failed pings make a drive `suspect` and three successes `up` again; three silent intervals make every drive of that node `down`; a ping whose oldest in-flight call is older than `stall_after` makes the drive `stalled` in the viewer's view; each state changes reads, writes and repair targets as the 10.8 table says** | 10.8 | Health judged from the process, not the drive |
| **Wrong-mount guard: a drive whose `DRIVE` id or filesystem UUID does not match the membership document is not opened and serves nothing** | 10.8 | Serving another drive's data |
| **Existence query only after all candidates and the hint miss; a hit on a non-canonical drive is served, recorded in the hint and queued for repair; the negative cache absorbs a storm of client misses, is cleared by a local PUT, and is never consulted by a fill of a pack a tree names** | 10.6 | Query storm; a fresh pack reported missing |
| **Repair: a read of one cell of an under-replicated object queues a drive-to-drive copy of the whole object to the target's node; concurrent detectors produce one queue entry; with no writable target the entry stays pending and runs when a probe drive returns to `up`; the target is never `out`, `failed`, `stalled`, `suspect`, `down`, `indexing` or at or above `repair_limit` (a repair is refused at `repair_limit` while client PUTs are still accepted up to `fill_limit`); the rate cap holds; an `identical` commit ends it** | 10.8 | Partial reads never repaired; repair storm; copy-and-evict loop |
| **Audit: with a scripted population of keys holding one, two and three copies, the index audit's copy-count ratios equal the population's exactly (each key counted once, by its first holder); a deficit within `repair_recent` queues repair and an older one does not; the live-key audit over a key list reports 0-copy keys** | 10.8 | Unmeasured copy decay |
| Membership: temporary node loss changes no placement and copies nothing; permanent loss triggers no scan; repair copies only objects read or recent, under the rate cap; adding a drive moves nothing until read | 10.8 | Repair storm |
| Capacity: fill is allocated bytes over filesystem size; above `evict_at` eviction removes least recently read objects down to `evict_to` and never an open file's inode; a drive between `evict_to` and `fill_limit` with evictable old objects keeps accepting client PUTs; repair is refused above `repair_limit`; above `fill_limit` nothing but copy-forward is written; low `statfs` free space stops writes whatever `used` says | 10.4, 10.8 | Full drive; marks that never trigger; accounting against the wrong source |
| **Steady state at capacity: a fake drive driven by client writes and repair with a working set larger than its capacity stays between `evict_to` and `fill_limit`; eviction fires, no repair write starts eviction, and no repaired copy is evicted within `evict_min_age`** | 10.8 | Copy-and-evict loop |
| `digest=none` object: footer crc detects a flipped cell on the serving drive; the client uses the other copy; quarantine hides the file from the index | 9, 10.4 | Bitrot without silo |
| Deletes: a DELETE that misses a down node leaves its copy, which a later read serves as the same bytes and eviction removes | 10.5, 10.8 | Stale bytes after delete |
| S3 subset: PUT/GET range/HEAD/DELETE behave per 10.9; LIST and multipart return 501; the CAS service's ObjectStore conformance runs against the tier, including "never different bytes under a used key" | 10.9 | Leaning on LIST or conditional writes |
| **Migration: a pack missing in the cold tier is read from the legacy upstream and destaged once by its destage owner when several nodes miss it at once, and never destaged on a non-definitive miss; with the owner's drive down the requester destages it; with the tier at its budget, eviction keeps it inside `b x R`; legacy is never written; the mover resumes after a crash and skips packs already present; a mover PUT with `x-ts-copies: 1` writes one `.m.pack` copy, repair does not raise it while the budget is set, and restores two when it is read after the budget is raised; removing the upstream makes a pack a miss, not an error** | 10.10 | Lost migration state; writes to silo; destage race; silo burst on restart |
| Wait coalescing: at most one outstanding `wait` per 8 cells | Wait coalescing | Message storm |
| Server restart with a stale-epoch reply in flight: dropped; the `wait` re-issued | Epoch | Stale completion |
| No eviction before the grace window; an entry locked by a second process is never evicted | Startup safety | Eviction under a reader |
| Replier: with N > 1 clones, a parked reply completed from another task is written to its originating device; INTERRUPT read by another clone is answered once (reply xor EINTR) | 5.2 | Hung D-state build |
| Takeover with N > 1 clones and parked reads on several: supervisor retains every fd, no EIO, exactly-once | 5.2 | Lost requests on update |
| All reader threads blocked in a fake `pread` while `wait` completions still flow | 5.4 | Coupled stall |
| `EntryStore` conformance suite passes on the memfd implementation (Linux) and the in-process implementation (macOS path), loom covers both | 5.6 | Mac divergence |
| Planner: ranks, gaps up to `gap_cells`, coverage rules, backlog downgrade, cancel demotes | Section 6 | Wrong plan |
| Layout migration: old cache entries ignored, `packs-v2` served, `gc legacy` removes only legacy, rollback purge | Section 13 | Stale entries, disk overshoot |
| Ported fuser tests, `tsfuse` per-opcode encode/decode, kernel conformance suite (5.2) | The FUSE layer | Protocol misreads, lost replies, interrupt bugs |

**Why the failure and boundary list is complete for the cold tier.** Each state a drive can be in (`up`, `indexing`, `stalled`, `suspect`, `down`, `failed`, `out`, full) has a row for reads and a row for writes or repair targets; each answer a copy can give (200, 404, 412, 503 by reason, transport error, no answer) has a row in the engine; each outcome a PUT copy can have (`committed`, `identical`, `conflict`, abandoned for each reason, generation mismatch) has a row in the coordinator; and each way an object can lose a copy (abandoned PUT, eviction, EIO quarantine, node out) has a row that shows what restores it or that it stays a miss.

## 15. How constants are frozen
Starting constants in sections 4, 5 and 10 are replaced by T0, T0-F and T10b reports; a constant without a measurement behind it is marked [I] in the code comment that defines it.

## 16. Farm measurement plan (T0 and T0-F, before tuning)
Farm targets, never on a developer machine. Run on **one node drained of jobs** (no mounts, no Prepare) while the jobscheduler holds admission on the other nodes. Scratch files go in `ts-cold-scratch` directories on the drives' xfs (never silo's data). Before each run, record `iostat -x` of the node's drives for 60 s and abort if util > 5%. Each cell: 20 s after a 5 s warm-up, page cache dropped between cells; output MB/s, p50, p99, `iostat -x` (`rareq-sz`, `r_await`, `aqu-sz`, `util`). **Nothing in T0 or T0-F loads a kernel module or changes firewall or socket state on a node.**

**A. Raw drive (one drive, then both).** Sequential streams in their own regions, adjacent next requests, plus a scattered arm. Request size {1, 2, 4, 8, 16 MiB} x streams {1, 2, 4, 8} x {buffered readahead 8192, buffered 512, `O_DIRECT`} x {adjacent, scattered}. Decides `pos_ms`, `media_MBps`, `gap_cells`, and whether the cold tier uses `O_DIRECT` or buffered.

**B. Cache device.** On (i) the loop path, (ii) a file on the host root fs, (iii) the loop with `direct-io=on`: write block {128 KiB, 1 MiB, 4 MiB} x jobs {1, 4, 16} x {buffered + `fdatasync` per 32 MiB, `O_DIRECT`}; `fdatasync` latency at 1 MiB batched every 1 s; 70% 128 KiB random reads over 30% sequential writes; `PUNCH_HOLE` on 16 MiB groups and whether free space returns; DONTNEED arms (a) never, (b) at commit, (c) cap-driven, on a read-after-serve pattern. **Records `findmnt` of the root fs.** Decides the section 11 gate, dirty cap, hash pool, DONTNEED default.

**C. Cold tier (T0-F).** Same node, same two drives, same pack sizes (at least 24 distinct packs of >= 1 GiB for silo; the same count of scratch packs for the tier), each run idle of other traffic.
- **C-1 silo baseline:** ranged GETs through the local silo, part size {4, 8, 16, 32, 64} MiB x concurrent GETs per node {1, 2, 4, 8} x active nodes {1, 6}; per-drive `iostat` (`rareq-sz`, kB read per GET byte, util).
- **C-2 one drive streaming whole packs:** the cold-tier prototype on a scratch directory of one drive: whole-pack reads and 64 MiB part reads, streams per drive {1, 2, 3, 4}, chunk {1, 2, 4, 8} MiB, depth {1, 2, 4}, slice {8..128} MiB, `O_DIRECT` versus buffered+`sendfile`+DONTNEED. Reports MB/s per drive, bytes per busy-second from `iostat`, and p50/p99 of a **1 MiB and a 4 MiB demand read** arriving at an already-busy drive with and without demand preemption.
- **C-2b mixed read and write:** one write stream and one read stream on the same drive, with demand arrivals at {0, 2, 8} per second; write MB/s, read MB/s and demand p50/p99, at `write_min_share` {0, 25, 50}%. Decides `write_min_share` and confirms the class order.
- **C-3 copies and striping:** a pack on two drives of two nodes: single copy streaming versus two-copy striping (4.4), local versus peer read (network CPU, MB/s), hedge to the other copy against a loaded drive.
- **C-4 idle and loaded first byte:** GET of {1, 2, 4, 8} MiB at cluster concurrency 1 (decides the first-part size: smallest with idle first byte <= 40 ms), and the same under the 6-node load of C-1 against the tier (the thresholds of 4.6).
- **C-5 write and fragmentation:** sequential 1 GiB and 64 MiB pack writes at 1 and 2 writers per drive (`O_DIRECT` 4 MiB depth 2, with and without the 64 MiB extent hint and `fallocate`); extent count per file; allocated bytes against file size (the 10.8 accounting); sustained write MB/s over 200 GiB of scratch; small-object segment writes at the measured size distribution with group commit.
- **C-5b steady-state churn:** a drive's fill (silo's bytes plus scratch bytes over the filesystem size, always leaving 5% free; on a drive where silo's share makes a level impossible, the level is skipped and reported) held at 85%, and separately at 90%, each for **at least 3 drive capacities of writes** (about 24 TB per drive, about 3 days at 100 MB/s per level) by **the tier's own eviction code** (least recently read, unlink and whole segments), with writes landing in the freed space, a read stream running beside it, and repair writes enabled. Reports sustained write MB/s per drive capacity written, the ratio to a fresh drive, read p99 beside it, and extent counts over time. Silo's own free-space fragmentation (C-6) is recorded as part of the conditions. Decides the fill marks and whether the per-epoch-directory fallback is built.
- **C-6 drive facts and object sizes:** `xfs_info`, `lsblk -t -D`, FIEMAP of sample silo parts, free-space fragmentation (`xfs_db -r -c freesp`, read-only), silo's logical bytes, and **the object-size distribution of silo's objects, split by kind (packs, tree lists, L2 sidecars, other)**: count and bytes per size bucket {<= 64 KiB, <= 1 MiB, <= 16 MiB, <= 256 MiB, larger}. Decides `small_max`, checks the 10.3 capacity and gives `L_silo` for 10.10.
- **C-7 placement and probe:** distribution of the real key set across 12 drive ids under HRW and the `c + m` probe-depth distribution with zero, one and two nodes down (offline computation on the CAS service's key list).
- **C-8 slow consumer:** two clients that stop reading mid-stream and a third that reads normally, against one drive; with slot release on and off: the third client's wait, the drive's idle time while slots are held, and parked buffer bytes. Decides `slot_release_after`.
- **C-9 failing drive and startup:** read, never written, the kernel's per-device command timeout and error-handling timeout (`/sys/block/<dev>/device/timeout`, `eh_timeout`) on every drive, and run the tier's own injected hang (a test I/O layer that never completes a chunk) to confirm the watchdog path on real threads; a real failing-drive injection runs only if it can be induced without loading a kernel module or touching a drive with data, and otherwise is reported as not run. Also: index build time with a cold dentry cache (`drop_caches`) from `readdir` of a 100-thousand-file scratch layout, plus the trailer reads of sealed segments and the scan of one full open segment at the measured small-object share, which sets how long a restarting node answers `indexing`.
- Per run: aggregate MB/s, per-request p50/p99, first byte, per-drive iostat, CPU.
- Decides: **P**, `s*`, `per_drive_max`, chunk, slice, depth, `demand_preempt_max`, `write_min_share`, `slot_release_after`, `stall_after`, node floor, first-part size, thresholds in 4.6, `small_max`, the fill marks, copies against capacity, striping default.

**D. CPU.** `sha256` MB/s per core with the `sha_ni` flag on each node, idle and under load; blake3 (placement and generations); crc32c. Decides hash threads and `cpu.weight`.

**E. Node facts.** On every node: `uname -r`, `fuse.enable_uring`, fuse module state, CPU count, `kernel.io_uring_disabled`, memlock limits, seccomp profile of job sandboxes, the FUSE request timeout, `vm.dirty_*`; **root filesystem type** and free unallocated SSD space; and, read only, **how the tree id is derived (T-FMT Q1)**. Decides which nodes run io_uring and the cache device plan.

**Acceptance of T0/T0-F (report, not code):** one table per section, the final constants, and any contradicted inference named as such.

## 17. Benchmark plan
The benchmarks below compare candidate FUSE libraries (fuser, fuse3/rfuse3, polyfuse, fuse-backend-rs) with `tsfuse`, and the cold tier with silo. Results are published with their raw data and configuration. Numbers come from **one farm node**, one harness throughout.

### 17.1 FUSE layer
**Harness.** `ts-fs-open-stall-load`: N open/read/close processes over a real komira tree (same tree and file list for every variant) plus CPU burners (one per core), 60 s per run, **at least 5 runs per variant, median and min/max**, first run discarded. Node drained. Per run record kernel, `fuse` module parameters, CPU governor and count, mount options, `entry_timeout`/`attr_timeout`, `max_read`/`max_write`, open mode, reader count, library commit. Harness and raw data live in a bench crate; no shell.

**Variants.** V1 fuser fork as shipped in 0.3.119 (baseline 2,769 opens/s, p50 74 ms, p99 312 ms; re-run on the day). V2 fork plus the pin fix of komira-ai/tierstore#343 (28,199 opens/s, p50 7.7 ms, p99 26 ms; re-run). V3 `tsfuse` `/dev/fuse`, 1 reader and N cloned readers (N in {2, 4, 8, cores/4}), writev replies. V4 same with splice replies. V5 `tsfuse` over io_uring, default depth and larger. V6 (optional, only if each costs under a day) a minimal read-only filesystem on fuse3 (rfuse3) and fuse-backend-rs over the same mapped list; polyfuse and the rest are a **static feature table** (async dispatch, clone, splice, uring, resend, copy count per read) read from source at pinned commits; unread cells stay empty.

**Two regimes, both reported.** (a) Library regime: `entry_timeout=0`, `attr_timeout=0`, explicit OPEN each time (the shipped baselines' settings). (b) Production regime: long timeouts and `NO_OPEN`. A **null filesystem** isolates transport from tierstore logic; V3-V5 run with both.

**Metrics.** opens/s; GETATTR and OPEN latency p50, p99, max (library histograms and client side); read MB/s cold and warm; **CPU per 1k requests** (server `utime+stime`, cgroup kernel time, whole-node delta); **D-state and FUSE waiting under a real komira build** (named targets): `fuse_d_state_threads`, `/sys/fs/fuse/connections/<n>/waiting`, D-state counts, per-request latency histogram, build wall time and FUSE-wait fraction; for V5 `uring_entries_parked` and `uring_registered_bytes` under a cold-read storm of `depth x queues x 1.5` and metadata p99 during it.

**Decision rules (before the data).** io_uring is default only if it improves CPU per 1k requests or p99 by at least 20% against `/dev/fuse` with clones, does not regress cold or warm read MB/s by more than 5%, survives the storm without metadata p99 growth, **fits 64 MiB of registered memory per mount**, and satisfies the takeover requirement (5.2 item 5). Splice replaces `pread`+`writev` above a threshold only where it wins on CPU per 1k requests at equal MB/s. DONTNEED stays on only if warm MB/s does not fall and page cache falls. The number of cloned readers is the knee of opens/s versus CPU; the shared pending queue (5.2) predicts a knee.

### 17.2 Cold tier versus silo
Same node, same two drives, same pack sizes, same harness, 5 runs, one table (T-B5):
1. **One drive streaming whole packs** (tier prototype, 1, 2, 3 streams) versus **silo 8+4 ranged GETs** (8, 16, 32, 64 MiB at 1, 2, 4, 8 concurrent): MB/s per drive, bytes per busy-second, kB read per GET byte.
2. **Tail:** p50/p99/max of 1 MiB and 4 MiB demand reads under a background of {0, 2, 6} streams per drive: silo (waits for the slowest of 8 queues) versus the tier (one queue, preemption, hedge to the second copy).
3. **Cold-pack fill end to end:** time to fill a 1 GiB and a 4 GiB pack into the local cache and tree-ready time for a real komira tree, 1 node and 6 nodes concurrently.
4. **Write:** 64 MiB and 1 GiB pack PUT with copies 1 and 2 against silo PUT; small-object PUTs at the measured size distribution; the SMR sustained-write curve fresh and in steady-state churn (C-5b).
5. **Failure drills (reported here, no timing asserted in tests):** a node taken down mid-fill; a drive stalled by the injected hang; fraction of packs missed at copies 1 and 2; the audit's copy-count histogram before and after.

Raw CSVs and exact configuration are published with the result tables.

## 18. Work breakdown (PR-sized, parallel, with interfaces)
Day-0 freeze: trait interfaces, the state page layout, the wire protocol and the cold tier's on-drive names land first with in-tree fakes, so every other task runs in parallel against fakes. The cold-tier traits are frozen with the rules of section 10 already in them: content-derived generation, size in the name, create-only by index under a key lock, small-object segments (`DriveStore`); watchdog, scheduler liveness, read-ahead and slot release, write share, small lane, threads = streams x depth (`DriveSched`); per-copy `If-Match` and locations that carry generations (`Upstream`, `PeerClient`); the distinct-node candidate list (`Placement`).

| # | Task | Interface (frozen in T-I unless stated) | Depends on |
|---|---|---|---|
| T-I | **Interface freeze PR:** traits `Entry`/`EntryStore`, `StateView`, `Engine`, `Budget`, `Verifier`, `Upstream`, `ListL2`, `Placement`, `DriveStore`, `DriveSched`, `PeerClient`; the wire protocol (`open-pack` with counter regions, `wait` with `lost`, `ensure`, `release`, epoch, fd passing, counter memfd and lifeline); the **state page layout** (cell byte with `durable`, sealed read-only for mounts); `tsfuse::Fs`/`Replier` (originating-device affinity, read-time cancel registration, atomic complete); the cold file and segment names; fakes (engine, upstream, drive with scripted service times, hangs and errors, injected clock) | `Entry{state(), claim, publish, hold, evict_cell_cas}`; `Upstream{get(range, generation, class, job, rank)->Stream}`; `Placement{candidates(key, view)->Vec<DriveId>, copy_set(key, view)}`; `DriveStore{commit(key, gen, size)->Committed\|Identical\|Conflict, index}`; `DriveSched{admit(req)->Stream, state()}` | none (day 0) |
| T0 | Farm measurement A, B, D, E (section 16) | report | none |
| T0-F-a | Cold-tier farm measurement that needs no tier code: C-1, C-2 and C-3 on the prototype, C-4, C-6 (silo's logical bytes and the object-size distribution), C-7, and the timeout reads and index build time of C-9 | report, constants | none (drained node) |
| T0-F-b | Cold-tier farm measurement on the tier's own code: C-2b with the real scheduler, C-5 and C-5b with the real eviction, C-8, the injected hang of C-9 | report, constants | T-C2, T-C3 (runnable as a farm tool) |
| T-F0 | **FUSE spike on a farm node:** read the farm kernel's uapi header; confirm io_uring registration, parked-entry behaviour, reply-fd affinity with clones, takeover with N clones, INTERRUPT and FORGET in uring mode, INIT-then-register order, per-mount uring memory, `SPLICE_WRITE`, NO_OPEN page-cache retention; go/no-go per feature | report | none |
| T-B1 | Baseline benchmark: harness as a farm target; V1 and V2 re-measured | harness + data | none |
| T-FMT | L2 format: spec, mmap reader (refuses v1 and a missing sidecar), writer library, deterministic and round-trip tests; **Q1 first: how the tree id is derived (read only)**; sorted directory records; id-preserving sidecar design | `ListL2::{pack_record, cell_digest, pack_root, dir_lookup}`; `ListVersionUnsupported` | T-I |
| **T-K1a** | **CAS service, format side:** dual emission (v1 + L2 sidecar, id unchanged) for one release; behaviour on `ListVersionUnsupported` (miss path, never fail the job); `l2-backfill`; 404-on-live-pack handling; the LIST decision (10.9); the live-key sample for `cold audit` from the sweep; the conformance assertion "never different bytes under a used key"; the stop-v1 release after the reader rollout | CAS service PRs | T-FMT Q1 and format |
| **T-K1b** | **CAS service, write side:** single streaming PUT of packs to tierstore's S3 front (Content-Length or chunked, no multipart) | CAS service PR | T-C5 |
| **T-J1** | **jobscheduler:** the retriable failure cause `tree-lost` from the mount provider requeues the job (4.6) | jobscheduler PR | T-I (the cause's name and wire) |
| T-F1 | `tsfuse` ABI: structs, opcodes, parse/encode, INIT negotiation with the flag list of 5.2, farm layout probe; ported tests: argument, request, ll/reply, time | `tsfuse::abi` | T-I |
| T-F2 | `/dev/fuse` transport: mount(2) and fusermount3, `FUSE_DEV_IOC_CLONE` readers on `AsyncFd`, pools, writev, stop/abort; ported tests: session, mount_options; **spike: uring takeover** | `Transport`, `Session` | T-F1 |
| T-F3 | Dispatch: `Fs`, `Handled`, `Replier` with originating device, token table registered at read time, atomic complete, INTERRUPT; ported tests incl. async_reply | `tsfuse::Fs` | T-F1; parallel with T-F2 |
| T-F4 | Splice reply path: pipe pool with `F_SETPIPE_SZ`, exact `oh.len`, threshold, fadvise helper | `ReplyPath` | T-F2 |
| T-F5 | io_uring transport: rings, register, commit-and-fetch, hybrid `/dev/fuse` for INTERRUPT/FORGET per T-F0, depth config, memory accounting, fallback | `Transport` impl | T-F2, T-F0 go |
| T-F6 | Takeover: supervisor fd store holding **every** clone, resume without INIT, RESEND; ported resend tests | `Session::resume(fds)` | T-F2; uring only if the T-F2 spike says so |
| T-ENV | **Farm privileged-test environment:** exec properties and sandbox policy for `/dev/fuse`, `CAP_SYS_ADMIN` or fusermount3, kernel >= 6.14, `enable_uring`, no network; a test platform (a jobscheduler or CAS service capability) | platform + docs | none |
| T-F7 | Kernel conformance suite (5.2 list) | test target | T-F2, **T-ENV**, grows with T-F3..T-F6 |
| T-F8 | Tree `Fs` on `tsfuse`: mmap image serving with WILLNEED/mlock, NO_OPEN modes, read path with the 5.1 protocol (post-read recheck), parked reads, the lost path (EIO and the mount marked lost) | `TreeFs` | T-I, T-F3, T-FMT reader |
| T-F9 | Put-directory `Fs` | `PutFs` | T-F3 |
| T1 | Entry store (Linux): sparse file, **sealed state memfd, counter memfd regions with remap on growth, lifeline**, claim CAS, hold refcounts, `flock` | `Entry` | T-I |
| T-M1 | **macOS:** in-process `EntryStore`, NFS handlers on the engine/cache/L2 reader/cold client, `F_PUNCHHOLE`, `F_FULLFSYNC`, `EntryStore` conformance suite | `EntryStore` impl | T-I, T1 interface |
| T-M2 | macOS tests on the Macs (run on demand, not per-PR) | tests | T-M1 |
| T1b | Journal and snapshot: ordering, batching, replay, batched PUNCH with one fsync, `durable` bit, **boot_id**, `recheck`, fake-fs crash tests | `Entry::commit_batch()` | T-I |
| T2 | Hash pool | `Verifier` | T-I |
| T3 | Fetch engine: claims, queue (DRR across jobs, rank, offset), part sizing and grid-relative ramp, demand sub-range, stall retry, hedge to the other copy, `If-Match` with the fill's pinned generation, per-cell failure with copy failover, the lost path | `Engine` | T-I (fakes) |
| T4 | Budget: **per-drive permits and decrease state under a node ceiling**, quantities of 4.3, controller with drive-reported signals, ladder, demand reserve, jobscheduler hint | `Budget` | T-I; synthetic latency source and starting constants until T0-F-a's arrive |
| T5 | Server side of the mount protocol: multiplexed connection, task per request, epoch, fd passing, counter memfd and lifeline handling | wire protocol | T-I |
| T6 | Mount client: wait coalescing, fd table, state reads, counter regions, reconnect, **separate runtime from readers** | `PackClient` | T-I; T5 for integration |
| T7 | Prepare planner (pure), per-drive gap rule | `plan(list)` | T-I |
| T8 | Eviction and admission (node cache): LRU, per-cell CAS punch with timeout and batched journaled PUNCH, `reserve`, grace window, evictor lock; cold-tier `touch` of held packs | `evict(budget)`, `reserve` | T1, T1b (fakes) |
| T9 | Metrics and tracing (section 12) | counters | alongside all |
| T10a | Fake-upstream and fake-drive harness: the whole test table of section 14 that needs no farm, incl. loom models and the scripted coordinator interleavings | test suite | T-I, grows |
| **T-C0** | Cold-tier spec details: membership document format (`in`/`out`), drive view states, the `cold` and `drive` config (flags and files): every `cold.*` and `drive.*` name used in sections 4.3 and 10, among them copies, `min_copies`, `small_max`, `segment_bytes`, the fill marks, `budget`, `evict`, `evict_min_age`, `touch_interval`, `unavailable_wait`, repair and audit rates, `repair_recent`, `put_copy_stall`, `location_hint_entries`, `legacy_hint_ttl`, `legacy_upstream`, `destage_bytes_per_hour`, `parked_buffer_max`, and `drive.streams`, `write_streams`, `depth`, `queue_max`, `slice_bytes`, `write_min_share`, `slot_release_after`, `parked_max`, `stall_after`, `eio_limit` | doc + schema | T-I |
| **T-C1** | **Placement:** weighted HRW, the distinct-node candidate list `L`, copy sets and probe depth counted in `L`, membership view and epoch piggyback, distribution, movement and probe-coverage tests | `Placement` | T-I, T-C0 |
| **T-C2** | **Drive store:** `format-drive` (DRIVE file, held script), layout and names with generation and size, in-memory index from `readdir` and segment trailers, `.up` write with `O_DIRECT` 4 MiB, extent hint and `fallocate`, footer crcs, create-only commit by index under the key lock with `RENAME_NOREPLACE`, small-object segments with group commit, unlink delete, startup sweep, allocated-bytes accounting, stamps, read-recency eviction with minimum age and segment copy-forward, file quarantine, crash tests on a fake fs; `small_max` frozen from C-6 | `DriveStore` | T-I, T-C0 |
| **T-C3** | **Drive scheduler:** I/O threads = streams x depth, streams, slices, class queues with DRR and the write share, elevator, demand preemption, small lane, read-ahead and slot release, watchdog and `stalled`, liveness in ping, EIO counting and `failed`, bounds and 503, instrumentation, deterministic simulation tests | `DriveSched` | T-I, T-C2 interface |
| **T-C4** | **Peer service and client:** cold HTTP routes (GET, PUT with trailer, DELETE, batched HEAD, touch, repair, ping), headers (class/job/rank, queue wait, drive state), local fast path, `ColdUpstream` implementing `Upstream`, copy choice, location hint, `indexing` and definitive-miss rules, existence query and the scoped negative cache, hedge, striping, health pings and view states | `PeerClient`, `ColdUpstream` | T-C1, T-C2, T-C3, T3 |
| **T-C5** | **S3 front mapping:** create-only streaming PUT with fan-out, generation trailer, abandon and acknowledgement rules, `min_copies`, repair on a short PUT; GET range, HEAD, DELETE; 501 for the rest; the CAS service's ObjectStore conformance target; optional `cold list` | S3 front | T-C4, T-K1a |
| **T-C6** | **Repair and audit:** drive-to-drive repair with triggers, dedup queue, rate cap and target rules; move by read after membership change; index audit and the `cold audit --keys` verb with their metrics; scrub reporting | `Repair`, audit | T-C4 |
| **T-C7** | **Migration:** capacity rule with C-6's and the CAS service's numbers (10.10), legacy read-only upstream with the definitive-miss rule, destage owner, a mover driven by the CAS service with `x-ts-copies`, `legacy_*` metrics, cutoff and rollback runbook | scripts, docs | T-C5, T-C6, T-K1b, T0-F-a C-6 |
| **T-C8** | **Silo retirement:** held scripts (precondition check, upstream removal, service stop through hostkeeper, destructive data removal, budget raise), image change | held scripts | T-C7, T10b on the cold tier |
| T12 | S3-front integration: routing, generation-aware `If-Match`, first-part-alone for silo-origin objects, ranged serving from Partial entries | S3 front calls `Engine` | T3 |
| T13 | Rollout: **the CAS service's dual emission first, node-by-node reader runbook (3.5)**, cache partition plan, hostkeeper boot parameter `fuse.enable_uring=1`, overlap budgets (cache and cold), rollback purge, held scripts | scripts, docs | T0 B and E, T-FMT, T-K1a, T-M1 |
| T14 | `request_to_start_seconds` across scheduler and mount | metric | T-I |
| T-B2 | V3 and V4 runs (V6 if under a day), null fs then real core | data | T-F2, T-F3, T-F4 |
| T-B3 | V5 runs and the storm test | data | T-F5 |
| T-B4 | Real-build D-state run; FUSE result tables and feature table | report | T-B2, T-B3, T-F8 |
| **T-B5** | **Cold tier versus silo benchmark (17.2)** | data, report | T0-F-a and T-C4 (read items); T-C5, T-C6 and T0-F-b (write, steady-state churn, copies and failure items) |
| T10b | Farm validation and tuning: GET count per tree, first-byte and tree-ready p50/p99 idle and under 6-node load, D-state, amplification, DONTNEED, per-drive queues and scheduler constants, audit histograms, request-to-start; freeze constants **on the shipped cache device** | report | T0, T0-F-a, T0-F-b, T3, T4, T5, T6, T7, T9, T14, T13, T-F8, T-C5, T-C6 |
| T-R | Retire the fuser fork: switch tierstore-fs (Linux) to `tsfuse`, remove the dependency, archive the fork. **Not blocked by macOS** | cleanup PR | T-F8, T-F9, T10b |
| **T11a** | **Deletion, FUSE and engine:** passthrough, per-blob path, pinning, prefetch mechanisms, extents, v1 reader, dead config, `gc legacy`, the erasure-set budget model and topology flags | cleanup PR | T10b |
| **T11b** | **Deletion, silo:** the silo client as an upstream, the legacy upstream, `upload_part_size` handling | cleanup PR | T-C8 |

**Day 0 (no dependency):** T-I (every trait, the cold-tier ones included: their rules are in section 10), T0, T0-F-a, T-F0, T-B1, T-ENV, T-FMT Q1. **Right after T-I (about a day), in parallel against fakes:** T-FMT format, T-F1, T-F3 (on the `Replier` shape), T-C0, T-J1, T1, T1b, T2, T3, T4 (on starting constants), T5, T7, T8, T10a, T14, T-M1. **After T-C0:** T-C1 and T-C2 (`small_max` waits for C-6; the format does not), then T-C3 on T-C2's interface; T0-F-b once T-C2 and T-C3 run as a farm tool. **Then:** T-F2, T-F4..T-F9 (T-F5 and T-F6 on T-F0, T-F7 on T-ENV), T6 (on T5), T-C4 (on T-C1..T-C3 and T3), T-K1a (on T-FMT), T-C5 (on T-C4 and T-K1a), T-C6 (on T-C4), T-K1b (on T-C5). **Then:** T-C7 (on T-C5, T-C6, T-K1b and C-6's numbers), T13 (on T-FMT, T-K1a, T-M1), T-B2/T-B3/T-B5, T10b (on T13 and the rest of its column), T-C8 (on T-C7 and T10b), T-B4, T11a (on T10b), T11b (on T-C8), T-R (on T10b only).
Longest chains: T-I -> T-C0 -> T-C2 -> T-C3 -> T-C4 -> T-C5 -> T-K1b -> T-C7 -> T-C8 -> T11b (cold tier) and T-I -> T-F1 -> T-F2 -> T-F8 -> T10b -> T-R (FUSE). The task graph has no cycle: T-C5 needs only T-K1a, and the CAS service's write side (T-K1b) follows T-C5.

## 19. Critique resolution

**Critique of revision 4 (verdict: ready with named fixes). Every named fix is applied in this revision.**

| Finding | Status | Resolved by |
|---|---|---|
| N-C1 generation per PUT breaks create-only, copy convergence and failover | RESOLVED | 2, 10.4: generation = blake3 of the bytes, so identical bytes give one name on every drive; create-only decided by the index under a per-key lock, `RENAME_NOREPLACE` as second guard; 10.5: generation trailer checked by each writer, conflict rule (412 and the loser deletes its own copies), write-once-ever keys; 4.6: per-copy `If-Match`, generation known before the first byte; one `PackId` per object. Tests: same-bytes race, different-bytes race, transit damage |
| N-C2 hung drive not detected; stuck thread not cancellable | RESOLVED | 10.7: per-chunk watchdog, `stalled` with immediate 503 to queued requests, state in ping and headers, `drive_stall_total`, stuck-thread accounting and probe before `up`, three stalls make `failed`; ping reports the scheduler's liveness; 10.5: `put_copy_stall` abandon threshold. Test: hung fake drive |
| N-C3 repair and destage only on full reads; copy count decays unmeasured | RESOLVED | 10.8: repair triggered by any read that finds a deficit (partial included), by a short PUT and by the audit for recent objects; the whole object is copied drive to drive through the cold service, never through a cache; deduplicated queue at the target; never to a non-`up` drive or at or above `repair_limit`; index audit and live-key audit with histograms; 10.5: acknowledgement waits for every live copy, abandon rule stated; 10.10: lazy destage reads the whole pack once at its destage owner and its silo read cost is stated; 10.3: availability figures are upper bounds read against the audit |
| N-C4 node-wide reaction to one drive's 503; slot held by a slow consumer; "third copy" claimed | RESOLVED | 4.3: per-drive permits, baselines and decrease state; node ceiling decreased only by node-wide evidence; 10.7: two-chunk read-ahead, slot release after `slot_release_after` with parked buffers bounded per node; 10.13: the third-copy claim is dropped, the defences are named, a peer cache read path is a follow-up if metrics show the need. Tests: hot pack on two drives, slow consumer |
| N-C5 SMR churn is the steady state; no mixed arm; write class undefined | RESOLVED | 16: C-2b mixed arm, C-5b steady-state churn at 85% and at 90% for 3 drive capacities each with the tier's eviction code; 10.7: class order `demand > write > prepare > background` and a 25% write share, so a PUT is never stalled; 10.4, 10.8: fill marks in order repair limit <= evict-to < evict-at < fill limit, so writes land in freed space and repair never starts eviction (no copy-and-evict loop); 10.13 restated |
| N-C6 tier eviction or delete can fail a running job | RESOLVED | 7, 10.8: holds `touch` their packs every 10 minutes and the tier never evicts an object younger than 1 h; 4.6: a pack lost mid-job fails waiting reads with EIO, marks the tree lost and fails the job with the retriable cause `tree-lost` (T-J1), never wrong bytes; `indexing` is never a loss. Tests: lost mid-job, held packs not evicted |
| N-C7 probe depth, spill bound, positive cache, startup 404, size not in name, `down` undefined | RESOLVED | 10.2: positions counted in the distinct-node list `L`; writes never beyond `c + m` (fewer copies and repair instead); 10.6: bounded location hint; `503 indexing`, never 404, and no negative entry, legacy read or repair from a non-definitive answer; 10.4: size in the name, footer offset derived from it, no `ftruncate`, index from `readdir` alone; 10.8: drive state table with who sets each state; 10.2: the movement statement corrected (about `c/12` of keys) |
| N-C8 small objects have no policy | RESOLVED | 10.4: objects up to `small_max` (1 MiB, set from C-6) are records in write-once segments with group commit and whole-segment eviction with copy-forward of recent records; 10.7: small lane between chunks; 16 C-6: size distribution by kind before T-C2 freezes `small_max` |
| N-C9 T-K1 / T-C5 cycle; T11 waits for silo; T-C1/T-C2 start too early | RESOLVED | 18: T-K1a (format side) and T-K1b (write side, after T-C5, before T-C7); T11a (after T10b) and T11b (after T-C8); T-C1..T-C3 start after T-I and T-C0 with the fixed spec; one ordered start list; the graph has no cycle |
| N-C10 migration capacity, rollback cost, destage race, legacy fallback cost | RESOLVED | 10.10: the capacity inequalities (with new writes during the window) and examples, eviction against the budget, the rule that T-C7 starts after C-6, `x-ts-copies: 1` for the mover, no node-by-node drain (one erasure set); rollback of the write flip is a cold start of every post-flip tree, measured from `cold_puts_total`; destage at the owner converges (same bytes, same name); fallback cost per fill stated; `indexing` is never a full miss |
| N-C11 deletes without tombstones; space accounting source; negative cache staleness; threads versus depth; one EIO fails a drive | RESOLVED | 10.5: write-once-ever keys make a missed delete harmless; 10.8: accounting by allocated bytes over filesystem size, `statfs` free as a second guard; 10.6: negative cache only for client reads, 5 s, cleared by a local PUT, never used by a fill; 10.7: threads = streams x depth; EIO quarantines the file, the drive fails after `eio_limit` errors in a window |
| T0-F additions: mixed arm, steady churn, slow consumer, size distribution, failing-drive timeouts | RESOLVED | 16: C-2b, C-5b, C-8, C-6 by kind, C-9 (timeouts read without loading any module; injected hang; index build time); 18: T0-F-a on day 0, T0-F-b on the tier's code |
| Minor: counter memfd growth needs a server remap | RESOLVED | 5.1 and its test row |
| Minor: equal-class demand wait is not the loaded bound | STATED | 1 (launch budget) |

**Critique of revision 3: resolved in revision 4 and unchanged here.** F1 reply affinity and read-time cancel registration (5.2); F2 per-cell CAS eviction (5.1); F3 sealed state memfd, per-holder counter memfd, lifeline, post-read recheck (5.1); F4 takeover with every clone retained (5.2); F5 `durable` bit and server-only DONTNEED (4.8, 8); F6 id-preserving sidecar, dual emission first, node-by-node readers (3.5); F7 per-drive budget quantities and grid-relative ramp (4.3, 4.4); F8 macOS on the same engine (5.6); F9 io_uring memory, hybrid transport, environment (5.2); F10 INIT flags, splice length, pipe size, image page-in (5.2, 5.3); F11 separate runtimes (5.4); F12 T-ENV and dependencies (18); F13 boot id and batched punches (8); N2, N2b, N5, N11, S5, M1 with them. The owner's two decisions recorded then are designed: silo replaced by the cold tier (10), and macOS served by the in-process NFS front with `tsfuse` Linux-only (5.6).

**Earlier critiques, carried as resolved.**

| Finding | Where |
|---|---|
| No legacy reader; relist and tree identity | 3, 3.5 |
| Floor removes the controller's authority | 4.3 ladder |
| First-byte signal, stall retries | 4.3, 4.6 |
| Punches journaled; `recheck` | 2, 5.1, 8 |
| Admission double count | 7 |
| S3-front ETag window | 9 |
| Cross-job rank | 4.5 |
| Cache device and root filesystem | 11, T10b dependency |
| Verify-before-serve forced whole-file fetch; hash frontier needed in-order arrival | 3, 3.3 |
| TOCTOU and hedge rewrites; claim state | 2, 4.2, 4.6 |
| Crash ordering; sidecar scaling | 8 |
| Path key omitted the tag; HEAD added a seek | 2, 4.6 |
| AIMD controller; head-of-line blocking; demand reserve and the 60 ms target; hedging adds load | 1, 4.3, 4.5, 4.6, 10.7 |
| Serialization, inline pread, hash pool | 4.7, 5.3, 5.4 |
| `read_ahead_kb` amplification; raw fio does not validate the model; gap unit and whole-pack waste; first-byte path and launch before Prepare | 10.7, 16 A and C, 6, 1, 4.4, 4.5 |
| Dirty pages and DONTNEED; holds exhaust the budget; fd versus path; restart loses holds; poisoning too coarse; rollback overshoots the disk; epoch and backoff | 4.8, 7, 5.1, 4.6, 13, 5.4 |

## 20. Limits of this design
- **Nothing here was run.** [I] and [K] items have named checks: T0-F (drive behaviour, scheduler constants, SMR churn, copies against capacity, object sizes), T-F0 (kernel behaviour), T-FMT Q1 (tree id).
- **The owner's decisions** are at the top (Q1 per-cell digests, Q2 copy count, Q3 the old-tree trust gap); each default is what this document builds.
- **The CAS service's decisions (T-K1a):** whether LIST is needed (10.9), and its 404 handling (the miss path), which the tier's eviction and the lost path of 4.6 rely on.
- **SMR risk:** write-once files, extent hints, segments, the fill marks, eviction and the write share are defences, not proofs; T0-F C-5b decides and a per-epoch-directory fallback is named (10.4, 10.13).
- **The cold tier has no parity** and restores copies only for objects in use. Losing two nodes makes the objects with both copies there misses; the audit measures how many objects have fewer than two copies. That is "loss is a miss", applied.
- **Write-once-ever keys** are a contract with the tier's clients. The CAS service meets it by construction and its conformance test checks it; a client that breaks it gets 412s and misses, never wrong bytes.
- **Rollback of the CAS service's write flip** costs a cold start of every tree published since the flip (10.10).
- **io_uring FUSE** (parked entries, memory, hybrid delivery, takeover) is decided by T-F0, T-F5 and the section 17 rules; `/dev/fuse` is the default until they pass. `fuse.enable_uring` is set at boot by the hostkeeper image.
- **Kernel facts tagged [K]** (reply matching per device, resend with clones, `F_SEAL_FUTURE_WRITE`, uring INTERRUPT delivery, `F_PUNCHHOLE` on APFS) come from source knowledge; each has a T-F0, T-F7 or T-M1 check. The farm kernel version is reported, not verified.
- The CAS service's hashing CPU roughly doubles (cell digests); T-FMT measures it. The id-preserving backfill reads packs once; its cost per TiB is estimated in T-FMT before it runs.
- The 60 ms first-byte figure is an idle-farm target; the loaded bound of 10.7 is a hypothesis for T10b.
- The first-part-small recipe, the P probe arm, striping across copies, the write share and slot release are this design's own, with no external source; T0-F and T10b validate them.

## References
- Durner, Leis, Neumann, VLDB 2023: https://www.vldb.org/pvldb/vol16/p2769-durner.pdf
- AWS S3 performance guidelines: https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance-guidelines.html
- aws-c-s3: https://github.com/awslabs/aws-c-s3 ; Mountpoint for S3: https://github.com/awslabs/mountpoint-s3/blob/main/doc/CONFIGURATION.md
- Iyer and Druschel, "Anticipatory scheduling", SOSP 2001
- Li, Shen, Papathanasiou, EuroSys 2007: https://www.cs.rochester.edu/~kshen/papers/eurosys2007.pdf
- Schindler et al., "Track-aligned extents", FAST 2002
- Ruemmler and Wilkes, "An introduction to disk drive modeling", IEEE Computer 1994
- Thaler and Ravishankar, "Using name-based mappings to increase hit rates" (rendezvous hashing), IEEE/ACM ToN 1998
- Seagate ST8000DM004 datasheet (drive-managed SMR): https://www.farnell.com/datasheets/3981953.pdf
- Nydus: https://github.com/dragonflyoss/nydus/blob/master/docs/nydus-design.md ; eStargz: https://github.com/containerd/stargz-snapshotter/blob/main/docs/estargz.md
- Buildbarn storage ADR: https://github.com/buildbarn/bb-adrs/blob/master/0002-storage.md
- BLAKE3: https://github.com/BLAKE3-team/BLAKE3-specs/blob/master/blake3.pdf
- Linux kernel FUSE documentation and `include/uapi/linux/fuse.h` of the farm kernel (pinned by T-F1)
