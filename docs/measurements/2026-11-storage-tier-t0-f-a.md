# Storage-tier measurements without tier code: silo's objects, placement, a GC replay (November 2026)

These are the measurements of the storage-tier design (`docs/design/fetch-cache-serve.md`,
section 16 C) that need none of the tier's code: **C-6**, what silo held (object sizes by kind,
`L_silo`, `L_live`, the raw-to-logical gap, the requesters); **C-7**, where the real key set and
new stripes land under the design's placement rules; and **C-10**, garbage collection replayed
against the design's seal, destage and stripe rules. They check the constants `large_min`,
`block_bytes`, `destage.pack_window` and `gc.rewrite_below`, and the capacity table of 10.15, for
both deployment profiles of section 21 (NOW: six combined nodes, two SMR drives each; LATER:
three storage nodes of 15 drives).

**C-1, silo's ranged GETs, is not reported.** Silo was removed from the farm while these
measurements ran (design 10.10), so there is no silo for the storage tier to be held against,
and a baseline of a store that no longer runs decides nothing.

The tables headed **C-6**, **C-7** and **C-10** are printed by the tool from the summaries
committed beside it (`crates/storebench/data/`), and a test holds this document to them word for
word (`cargo test -p storebench --test measurement_doc`). The other tables (the parked-cache
sample, the constants) and the prose figures were written by hand: each says what it was read
or derived from. **[M]** marks a measured figure, **[E]** one derived from measured
figures by a rule this document states, **[I]** an inference.

## How the data was taken

**What was read, and when.** Silo was one erasure set of 12 drives, two on each of six nodes
(here n1 to n6), with the buckets `kbf-cas` (everything the CAS service wrote) and
`tierstore-cold`. On 2026-11-18, before silo was removed, these were read:
- **The listing** of both buckets, 1,526,337 objects, from the drives, not from silo's S3 API.
  Silo answered a listing of `kbf-cas` at about 14 objects a second, a day and more for the whole
  bucket, while every drive holds every object's `xl.meta` (its size, time, tag and erasure
  layout). `storebench drive-list` walked one share of the key space per drive and read only the
  `xl.meta` of the objects its share owns. The twelve walks reported no error; their counts sum
  to the listing.
- **The pack footers**, through silo's S3 API: `storebench footers` read the footer of each pack
  (the index of its blobs: digest and size of each) in two passes, five shares on five nodes. The
  first read packs of every size and was stopped after about 50,700; the second read the 141,750
  packs over 64 KiB (`--min-pack-bytes 65537`). The second pass was still running
  when silo was removed: from then on every read failed (`get-error`, and `no-magic` for the reads
  that returned bytes of an object being removed), so 31,719 of the packs it reached were not
  read. The last record of each stopped first-pass file was cut off; `c6` merges the ten files
  (a pack read `ok` in either pass counts as read) after the cut-off record is dropped.
- **The drives' use**, `df` of all twelve, and silo's request counters
  (`/minio/v2/metrics/node`), twice, 20 minutes apart.
- **After silo was removed**, its parked buckets were still on the drives (below, the gap); one
  shard of them was listed the same way, read-only, on one drive.

**What could not be read.** The CAS service's own record of which packs are live was not
reachable, so `L_live` comes from the footers by the CAS service's first-location rule (C-6), and
the death times of C-10 are the design's synthetic ones. Two `du` walks of a silo drive did not
finish (a drive holds tens of millions of files), so the gap is explained from `df`, the object
model and a sampled listing.

**Reproducing.** The tool is `crates/storebench`. Build it with
`cargo build --release -p storebench`. The listing, footer and erasure files are not committed
(the listing alone is 193 MB) and silo is gone, so the farm-side verbs cannot be run again
against it; they run against any S3 store holding the CAS service's packs:

| step | command |
|---|---|
| listing | `storebench drive-list --bucket-dir <drive>/kbf-cas --share <i>/<n> --out walk-<i>.tsv --meta-out walk-<i>.meta.tsv`, one share per drive, concatenated (or `storebench list` through S3) |
| footers | `storebench footers --endpoint <url> --credentials <file> --listing listing.tsv --share <i>/5 --min-pack-bytes 65537 --out footers-big-<i>.tsv` |
| C-6 | `storebench c6 --listing listing.tsv --erasure erasure.tsv --footers <each footers file> --dead-out dead.tsv --out c6.json` |
| C-7 | `storebench c7 --listing listing.tsv --out c7.json` |
| C-10 | `storebench c10 --listing listing.tsv --dead-keys dead.tsv --out c10.json` |
| tables | `storebench report --data crates/storebench/data --arm c6` (and `c7`, `c10`) |

## C-6. What silo held

**C-6, silo's objects by kind and size class (objects and bytes)**

| kind | objects | bytes | <= 64 KiB | <= 1 MiB | <= 4 MiB | <= 16 MiB | <= 256 MiB | > 256 MiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| pack | 866,682 | 1.33 TB | 724,932 (3.34 GB) | 71,718 (23.0 GB) | 25,737 (60.7 GB) | 26,586 (229 GB) | 17,677 (1.00 TB) | 32 (10.8 GB) |
| large blob | 128 | 16.9 GB | 0 (0 B) | 0 (0 B) | 0 (0 B) | 0 (0 B) | 128 (16.9 GB) | 0 (0 B) |
| tree list | 659,523 | 579 MB | 659,235 (393 MB) | 256 (48.9 MB) | 16 (42.1 MB) | 16 (94.4 MB) | 0 (0 B) | 0 (0 B) |
| other | 4 | 4 B | 4 (4 B) | 0 (0 B) | 0 (0 B) | 0 (0 B) | 0 (0 B) | 0 (0 B) |
| all | 1,526,337 | 1.35 TB |  |  |  |  |  |  |

**C-6, silo's objects by bucket and deployment prefix**

| bucket or bucket/prefix | objects | bytes |
| --- | --- | --- |
| kbf-cas | 1,526,333 | 1.35 TB |
| tierstore-cold | 4 | 4 B |
| kbf-cas/ | 757,483 | 107 GB |
| kbf-cas/deployment 1 | 730,198 | 1.23 TB |
| kbf-cas/deployment 2 | 8,734 | 5.31 GB |
| kbf-cas/deployment 3 | 25,599 | 4.41 GB |
| kbf-cas/deployment 4 | 3,928 | 2.73 GB |
| kbf-cas/deployment 5 | 303 | 1.09 GB |
| kbf-cas/deployment 6 | 88 | 861 MB |
| tierstore-cold/deployment 7 | 4 | 4 B |

**C-6, silo's ingest**

| measure | value |
| --- | --- |
| first object | 2026-10-12 |
| last object | 2026-11-18 |
| multipart objects | 16,272 (940 GB) |
| last 7 days of the span | 760,114 (108 GB) |

**C-6, ingest per day, last 7 days of the span**

| date (UTC) | objects | bytes |
| --- | --- | --- |
| 2026-11-12 | 2,631 | 1.05 GB |
| 2026-11-13 | 536 | 29.1 MB |
| 2026-11-14 | 5,977 | 498 MB |
| 2026-11-15 | 236,127 | 35.8 GB |
| 2026-11-16 | 118,248 | 20.2 GB |
| 2026-11-17 | 271,280 | 34.6 GB |
| 2026-11-18 | 125,315 | 15.7 GB |

**C-6, ingest per day, whole span**

| date (UTC) | objects | bytes |
| --- | --- | --- |
| 2026-10-12 | 78 | 69.7 MB |
| 2026-10-13 | 18,757 | 524 MB |
| 2026-10-14 | 1,905 | 1.82 GB |
| 2026-10-15 | 6,050 | 5.16 GB |
| 2026-10-16 | 7,737 | 4.89 GB |
| 2026-10-17 | 8,191 | 2.28 GB |
| 2026-10-18 | 32,553 | 431 MB |
| 2026-10-19 | 66,240 | 6.59 GB |
| 2026-10-20 | 104,996 | 9.54 GB |
| 2026-10-21 | 90,007 | 4.61 GB |
| 2026-10-22 | 94,760 | 10.7 GB |
| 2026-10-23 | 117,353 | 9.78 GB |
| 2026-10-24 | 105,885 | 106 GB |
| 2026-10-25 | 15,718 | 108 GB |
| 2026-10-26 | 11,287 | 78.8 GB |
| 2026-10-27 | 15,241 | 144 GB |
| 2026-10-28 | 14,189 | 170 GB |
| 2026-10-29 | 3,166 | 54.3 GB |
| 2026-10-30 | 12,402 | 247 GB |
| 2026-10-31 | 10,024 | 188 GB |
| 2026-11-01 | 8,238 | 32.2 GB |
| 2026-11-02 | 4,415 | 17.1 GB |
| 2026-11-03 | 2,518 | 10.4 GB |
| 2026-11-04 | 1,072 | 1.20 GB |
| 2026-11-05 | 1,683 | 555 MB |
| 2026-11-06 | 1,741 | 722 MB |
| 2026-11-07 | 1,334 | 566 MB |
| 2026-11-08 | 928 | 715 MB |
| 2026-11-09 | 1,438 | 281 MB |
| 2026-11-10 | 3,575 | 21.2 GB |
| 2026-11-11 | 2,742 | 719 MB |
| 2026-11-12 | 2,631 | 1.05 GB |
| 2026-11-13 | 536 | 29.1 MB |
| 2026-11-14 | 5,977 | 498 MB |
| 2026-11-15 | 236,127 | 35.8 GB |
| 2026-11-16 | 118,248 | 20.2 GB |
| 2026-11-17 | 271,280 | 34.6 GB |
| 2026-11-18 | 125,315 | 15.7 GB |

**C-6, L_silo and L_live**

| measure | value |
| --- | --- |
| L_silo | 1.35 TB |
| packs listed | 866,682 (1.33 TB) |
| packs without a footer (not read) | 675,285 (3.19 GB, the largest 535 MB) |
| packs scanned | 191,397 |
| packs ok | 159,678 |
| packs bad (footer not read: counted as unread) | 31,719 (116 GB) |
| blobs in ok packs | 19,073,478 |
| distinct digests | 9,017,047 |
| distinct blob bytes | 388 GB |
| live packs | 138,528 (1.02 TB) |
| dead packs | 21,150 (195 GB) |
| dead share of pack bytes (%) | 16.1 |
| large blob bytes | 16.9 GB |
| tree list bytes | 579 MB |
| live packs whose first blobs could be in an unread pack | 138,528 (1.02 TB) |
| of them, the most the unread packs' bytes could make dead | 724 GB |
| L_live, at most (unread packs counted live) | 1.15 TB |
| L_live, at least (unread packs and the most they could make dead left out) | 309 GB |
| packs with status get-error | 27,622 |
| packs with status no-magic | 4,097 |
| packs with status ok | 159,678 |

**C-6, tree lists checked**

| checked | json | not json | errors |
| --- | --- | --- | --- |
| 6,599 | 0 | 0 | 6,599 |

**C-6, large_min: objects at or below (segment records) and above (whole files)**

| large_min | at or below | above |
| --- | --- | --- |
| 262 kB | 1,425,236 (9.46 GB) | 101,101 (1.34 TB) |
| 1.05 MB | 1,456,145 (26.8 GB) | 70,192 (1.32 TB) |
| 4.19 MB | 1,481,898 (87.5 GB) | 44,439 (1.26 TB) |
| 16.8 MB | 1,508,500 (317 GB) | 17,837 (1.03 TB) |

**C-6, first-fit decreasing over the objects above large_min**

| block | window | blocks | chain blocks | chains | data | mean fill (%) | fill without chains (%) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 268 MB | 16 | 6,282 | 65 | 32 | 1.26 TB | 74.7 | 74.8 |
| 268 MB | 64 | 5,260 | 65 | 32 | 1.26 TB | 89.2 | 89.5 |
| 268 MB | 256 | 4,911 | 65 | 32 | 1.26 TB | 95.5 | 96.0 |
| 537 MB | 16 | 3,904 | 0 | 0 | 1.26 TB | 60.1 | 60.1 |
| 537 MB | 64 | 2,715 | 0 | 0 | 1.26 TB | 86.4 | 86.4 |
| 537 MB | 256 | 2,437 | 0 | 0 | 1.26 TB | 96.2 | 96.2 |
| 1.07 GB | 16 | 3,046 | 0 | 0 | 1.26 TB | 38.5 | 38.5 |
| 1.07 GB | 64 | 1,511 | 0 | 0 | 1.26 TB | 77.6 | 77.6 |
| 1.07 GB | 256 | 1,259 | 0 | 0 | 1.26 TB | 93.2 | 93.2 |
| 2.15 GB | 16 | 2,793 | 0 | 0 | 1.26 TB | 21.0 | 21.0 |
| 2.15 GB | 64 | 949 | 0 | 0 | 1.26 TB | 61.8 | 61.8 |
| 2.15 GB | 256 | 671 | 0 | 0 | 1.26 TB | 87.4 | 87.4 |

**C-6, objects above large_min and of 400 to 700 MiB**

| objects | above large_min | of 400 to 700 MiB |
| --- | --- | --- |
| count | 44,439 | 1 |
| bytes | 1.26 TB | 535 MB |

**C-6, the gap between silo's drives and its objects**

| measure | value |
| --- | --- |
| erasure set | 12 drives, 8+4 |
| L_silo | 1.35 TB |
| expected raw (model) | 2.09 TB in 19,158,456 files |
| expected raw / L_silo | 1.56 |
| drives measured | 0 |

**C-6, objects by the erasure layout their xl.meta states**

| data + parity | objects | bytes | share of bytes | inlined objects |
| --- | --- | --- | --- | --- |
| 6+6 | 1,388 | 4.51 GB | 0.3 | 1,174 |
| 7+5 | 2,649 | 316 MB | 0.0 | 2,637 |
| 8+4 | 1,522,300 | 1.34 TB | 99.6 | 1,452,325 |

**C-6, the gap by bucket**

| bucket | objects | bytes | expected raw | measured allocated |
| --- | --- | --- | --- | --- |
| kbf-cas | 1,526,333 | 1.35 TB | 2.09 TB | absent |
| tierstore-cold | 4 | 4 B | 197 kB | absent |


**Sizes [M].** 90.7% of the objects are at most 64 KiB (1,384,171 of 1,526,337: the first column
of the kinds table), and 95.9% of `L_silo` is in packs of 1 to 256 MiB (the pack row's third to
fifth columns). Most of the small objects are tree lists (659,235) and 724,932 small packs of
3.34 GB in all. 32 packs are over 256 MiB; the only object of 400 to 700 MiB is a 535 MB pack, and none needs a
chain at 1 GiB. Ingest over the
38 days of the bucket was 1.35 TB, about 35 GB/day; the last 7 days, 108 GB, about 15 GB/day.
No L2 sidecar exists yet (none was written before silo was removed), so that kind has no row.

**`L_live` [M, as a range].** The CAS service keeps no reference count: a blob lives at the first
location it was written to, and a pack is live while it holds the first location of at least one
blob. Walking the read packs in write order:
- 21,150 packs (195 GB) are **dead**: every blob they hold was written earlier in a pack that was
  read. That holds whatever the unread packs hold, as long as the objects' modified times are
  the order they were written in [I], so at least 14.6% of the pack bytes are dead.
- 138,528 read packs (1.02 TB) hold a first location; with the unread packs counted live,
  **`L_live` is at most 1.15 TB**.
- The unread packs (3.19 GB never read, 116 GB whose read failed) could have held some of those
  first locations themselves. Each such blob takes its own bytes in an unread pack, so the
  unread 119 GB can make at most 724 GB of the live packs dead (the most pack bytes per
  first-located byte first). **`L_live` is at least 309 GB.**
- Among the packs that were read, 16.1% of the bytes are dead. Applied to all pack bytes, `L_live`
  would be about 1.13 TB [I].

The design's measured facts said 38% of pack bytes are dead and 14,795 packs fully dead; the read
packs show 16.1% and 21,150, and the range admits 38%.

**The tree-list check was not run.** The tree lists were to be checked last (one in 100), and
every one of those reads came after silo was removed (6,599 errors in the table).

**The gap [M, E].** Silo's drives used much more than its objects need. The seven drives that held only
silo used 813.6 to 813.8 GB each (`df`; the others held other scratch files or had less, 371 to
1,019 GB); the twelve together 9.61 TB [M]. Costing every listed object at the
erasure layout its `xl.meta` states (one `xl.meta` per drive, the shard inline up to 128 KiB,
else a part file; the gap table) gives 2.09 TB, 175 GB a drive [E]: 1.56 times `L_silo`. The
drives used 4.6 times that (9.61 TB against 2.09 TB), the design's unexplained factor.

**Most of the rest is a parked bucket that no listing shows.** Silo had kept an old Bazel
remote cache under `.minio.sys/parked-bazel-cache/` on every drive (`rbe-cas` and `bzlcache`,
each with a content store `cas.v2` and an action cache `ac`, sharded by the first two hex digits
of the key), outside its buckets, so neither an S3 listing nor the walk of `kbf-cas` saw it.
After silo was removed it was still on the drives, and one shard of each tree was listed from
one drive the same way (`storebench drive-list --bucket-dir <shard> --meta-out ...`, then
`storebench c6` on that listing for the gap model):

| tree | sampled | objects in the sample | bytes in the sample | expected raw of the sample (12 drives) | scaled to the tree |
|---|---|---|---|---|---|
| `rbe-cas/cas.v2` | 91.2% of shard `00` of 256 (the walk's 40-minute limit) | 118,582 | 7.43 GB | 17.5 GB | 33.3 M objects, 2.09 TB, 4.91 TB raw |
| `rbe-cas/ac` | shard `00` of 256 | 15,004 | 13.5 MB | 737 MB | 3.84 M objects, 189 GB raw |
| `bzlcache/cas.v2` | shard `4d` of 82 | 1,627 | 112 MB | 245 MB | 133 k objects, 20.1 GB raw |

Scaled by the shards [E], the parked cache is about 37 M objects and 2.10 TB, costing 5.12 TB raw,
426 GB a drive. With `kbf-cas`, the model gives 601 GB a drive against the 813.6 GB measured.
The remaining 212 GB a drive (26%) is outside the model: it charges no inode and no directory,
and each object is a directory and at least one file on every drive (37 M objects at two
512-byte inodes each would be about 38 GB a drive [I]), nor the file system's own metadata.
`bzlcache/ac` was not sampled (its shard `00` was empty). So the factor of 4.6 was the parked cache's
bytes counted against `kbf-cas`'s alone: with it the model expects 7.21 TB (601 GB × 12), and the
drives used 1.33 times that; against everything stored, 3.45 TB, they used 2.8 times. No import
runs, so nothing depends on this but the NOW drives' free space: the parked cache is silo's
leftover, still on those drives (426 GB a drive, about 5% of one) when this was measured. C-7
counts no other bytes on the drives, as if it is removed before the tier's cold directories are
made; left in place it would take 10 × 426 GB = 4.3 TB raw off NOW's guard, which would then hold
38.0 TB logical at `4+2` and 28.5 TB at two copies [E].

**The requesters [M, I].** Silo counted about 2.97 M GETs on each of its six nodes in 189.8 ks of
uptime (about 94 GETs/s for the cluster), about 300 DELETEs on each (1,812 in all, against the
147 of the design's measured facts, which covered 1.9 days), and about 516 k PUTs on each. Over
20 minutes the GET counter of n3, the node with no jobs, did not move, while each of the five
nodes running jobs gained about 4,300 GETs [M]. So the GETs come from clients on the nodes that
run jobs, each reading its own node's silo endpoint [I]: the build nodes' tierstore (its upstream
read counter was 0 to 1,829 per node) and the CAS service. The DELETEs, spread evenly over the
six nodes, are the CAS service's (its packer and sweep are the only deleting code paths) [I].
Request logs, which would name the client of each request, were not kept.

**What C-6 decides.**
- **`large_min` stays 4 MiB.** At 4 MiB, 1,481,898 objects (87.5 GB) are segment records and
  44,439 objects (1.26 TB) whole files (the `large_min` table); 1 MiB would make 70,192 whole
  files and 16 MiB would put 317 GB into segments. 4 MiB is within `block_bytes / 4`.
- **`block_bytes` stays 1 GiB.** The design's worry, packs of 400 to 700 MiB that leave most of a
  block empty, does not arise: one object is in that range. No object needs a chain at 1 GiB.
- **`destage.pack_window` 256, not 64.** First-fit decreasing into 1 GiB blocks fills them to
  77.6% with windows of 64 and 93.2% with windows of 256 (the first-fit table). The window is a
  cap: at a class's `destage.max_age` the window packs whatever it holds, so a larger cap costs
  nothing when a class is slow. The table packs one stream of every large object in write order
  in full windows; per expiry class at today's ingest a window may be packed before it is full,
  and then fills less. C-10 replays the design's 64.
- **The capacity table (10.15) holds without silo.** The guard's stripes are in C-7: 16,624 at
  `4+4` on LATER (71.4 TB logical, as 10.15 says); on NOW 9,499 at `4+2` (40.8 TB) and 28,498
  blocks at `cold.replicas = 2` (30.6 TB), where 21.1 says about 34.6 TB and about 26 TB because it
  still subtracts silo's bytes. Against `L_live` of at most 1.15 TB this is 1.6% of LATER's guard.
- **The import volume (10.10)** is no longer needed: nothing is imported.

## C-7. Placement

**C-7, key placement under node rank, LATER (1,526,337 objects, 1.35 TB)**

| nodes down | writer keys, % min / mean / max | writer bytes, % min / mean / max | replica keys, % min / mean / max | replica bytes, % min / mean / max |
| --- | --- | --- | --- | --- |
| none | 33.33 / 33.33 / 33.34 | 33.09 / 33.33 / 33.71 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 |
| n0 | 50.00 / 50.00 / 50.00 | 49.89 / 50.00 / 50.11 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 |
| n1 | 50.00 / 50.00 / 50.00 | 49.99 / 50.00 / 50.01 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 |
| n2 | 49.97 / 50.00 / 50.03 | 49.99 / 50.00 / 50.01 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 |
| n0+n1 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 |
| n0+n2 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 |
| n1+n2 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 | 100.00 / 100.00 / 100.00 |

**C-7, new stripes 4+4, LATER, L_silo (314 stripes; other bytes per drive: 0 B)**

| nodes down | placed | refused | why | shards per node, min / max | shards per drive, min / mean / max | shards per drive, cv | drive fill %, min / max |
| --- | --- | --- | --- | --- | --- | --- | --- |
| none | 314 | 0 | - | 828 / 845 | 43 / 55.8 / 70 | 0.131 | 0.77 / 1.25 |
| n0 | 314 | 0 | - | 1256 / 1256 | 68 / 83.7 / 105 | 0.106 | 1.22 / 1.88 |
| n1 | 314 | 0 | - | 1256 / 1256 | 66 / 83.7 / 105 | 0.108 | 1.18 / 1.88 |
| n2 | 314 | 0 | - | 1256 / 1256 | 66 / 83.7 / 101 | 0.120 | 1.18 / 1.81 |
| n0+n1 | 0 | 314 | a node would hold 8 shards, more than the 4 the code tolerates losing | 0 / 0 | 0 / 0.0 / 0 | 0.000 | 0.00 / 0.00 |
| n0+n2 | 0 | 314 | a node would hold 8 shards, more than the 4 the code tolerates losing | 0 / 0 | 0 / 0.0 / 0 | 0.000 | 0.00 / 0.00 |
| n1+n2 | 0 | 314 | a node would hold 8 shards, more than the 4 the code tolerates losing | 0 / 0 | 0 / 0.0 / 0 | 0.000 | 0.00 / 0.00 |

**C-7, new stripes 4+4, LATER, capacity guard (16,624 stripes; other bytes per drive: 0 B)**

| nodes down | placed | refused | why | shards per node, min / max | shards per drive, min / mean / max | shards per drive, cv | drive fill %, min / max |
| --- | --- | --- | --- | --- | --- | --- | --- |
| none | 16,624 | 0 | - | 44301 / 44377 | 2878 / 2955.4 / 3030 | 0.012 | 51.50 / 54.22 |
| n0 | 16,624 | 0 | - | 66496 / 66496 | 4382 / 4433.1 / 4504 | 0.006 | 78.42 / 80.60 |
| n1 | 16,624 | 0 | - | 66496 / 66496 | 4382 / 4433.1 / 4504 | 0.006 | 78.42 / 80.60 |
| n2 | 16,624 | 0 | - | 66496 / 66496 | 4401 / 4433.1 / 4475 | 0.005 | 78.76 / 80.08 |
| n0+n1 | 0 | 16,624 | a node would hold 8 shards, more than the 4 the code tolerates losing | 0 / 0 | 0 / 0.0 / 0 | 0.000 | 0.00 / 0.00 |
| n0+n2 | 0 | 16,624 | a node would hold 8 shards, more than the 4 the code tolerates losing | 0 / 0 | 0 / 0.0 / 0 | 0.000 | 0.00 / 0.00 |
| n1+n2 | 0 | 16,624 | a node would hold 8 shards, more than the 4 the code tolerates losing | 0 / 0 | 0 / 0.0 / 0 | 0.000 | 0.00 / 0.00 |

**C-7, existing 4+4 stripes under failure, LATER (stripes of L_silo)**

| nodes down | stripes | margin (surviving - k): stripes | unreadable | degraded cold reads, % |
| --- | --- | --- | --- | --- |
| n0 | 314 | 1: 211, 2: 103 | 0 | 33.12 |
| n1 | 314 | 1: 200, 2: 114 | 0 | 32.80 |
| n2 | 314 | 1: 217, 2: 97 | 0 | 34.08 |
| n0+n1 | 314 | -2: 97, -1: 217 | 314 | 65.92 |
| n0+n2 | 314 | -2: 114, -1: 200 | 314 | 67.20 |
| n1+n2 | 314 | -2: 103, -1: 211 | 314 | 66.88 |

**C-7, key placement under node rank, NOW (1,526,337 objects, 1.35 TB; writer: the first live node of the rank, standing in for T-S1's rule)**

| nodes down | writer keys, % min / mean / max | writer bytes, % min / mean / max | replica keys, % min / mean / max | replica bytes, % min / mean / max |
| --- | --- | --- | --- | --- |
| none | 16.62 / 16.67 / 16.71 | 16.09 / 16.67 / 17.33 | 33.27 / 33.33 / 33.42 | 33.00 / 33.33 / 33.67 |
| n0 | 19.96 / 20.00 / 20.06 | 19.41 / 20.00 / 20.70 | 39.95 / 40.00 / 40.05 | 39.61 / 40.00 / 40.38 |
| n1 | 19.95 / 20.00 / 20.05 | 19.63 / 20.00 / 20.63 | 39.92 / 40.00 / 40.08 | 39.68 / 40.00 / 40.33 |
| n2 | 19.95 / 20.00 / 20.04 | 19.47 / 20.00 / 20.45 | 39.91 / 40.00 / 40.11 | 39.80 / 40.00 / 40.23 |
| n3 | 19.97 / 20.00 / 20.04 | 19.56 / 20.00 / 20.59 | 39.95 / 40.00 / 40.08 | 39.81 / 40.00 / 40.25 |
| n4 | 19.93 / 20.00 / 20.06 | 19.54 / 20.00 / 20.29 | 39.94 / 40.00 / 40.06 | 39.54 / 40.00 / 40.19 |
| n5 | 19.94 / 20.00 / 20.06 | 19.57 / 20.00 / 20.61 | 39.93 / 40.00 / 40.09 | 39.53 / 40.00 / 40.44 |
| n0+n1 | 24.95 / 25.00 / 25.04 | 24.70 / 25.00 / 25.63 | 49.94 / 50.00 / 50.04 | 49.60 / 50.00 / 50.64 |
| n0+n2 | 24.96 / 25.00 / 25.06 | 24.34 / 25.00 / 25.49 | 49.94 / 50.00 / 50.06 | 49.54 / 50.00 / 50.26 |
| n0+n3 | 24.97 / 25.00 / 25.06 | 24.67 / 25.00 / 25.70 | 49.95 / 50.00 / 50.02 | 49.64 / 50.00 / 50.17 |
| n0+n4 | 24.98 / 25.00 / 25.06 | 24.55 / 25.00 / 25.29 | 49.94 / 50.00 / 50.06 | 49.58 / 50.00 / 50.25 |
| n0+n5 | 24.95 / 25.00 / 25.08 | 24.73 / 25.00 / 25.66 | 49.95 / 50.00 / 50.03 | 49.58 / 50.00 / 50.72 |
| n1+n2 | 24.95 / 25.00 / 25.05 | 24.81 / 25.00 / 25.38 | 49.89 / 50.00 / 50.11 | 49.73 / 50.00 / 50.40 |
| n1+n3 | 24.95 / 25.00 / 25.07 | 24.60 / 25.00 / 25.53 | 49.94 / 50.00 / 50.09 | 49.62 / 50.00 / 50.43 |
| n1+n4 | 24.93 / 25.00 / 25.06 | 24.70 / 25.00 / 25.37 | 49.94 / 50.00 / 50.05 | 49.84 / 50.00 / 50.20 |
| n1+n5 | 24.93 / 25.00 / 25.06 | 24.58 / 25.00 / 25.68 | 49.96 / 50.00 / 50.08 | 49.70 / 50.00 / 50.32 |
| n2+n3 | 24.96 / 25.00 / 25.04 | 24.75 / 25.00 / 25.29 | 49.94 / 50.00 / 50.06 | 49.89 / 50.00 / 50.25 |
| n2+n4 | 24.90 / 25.00 / 25.05 | 24.65 / 25.00 / 25.19 | 49.94 / 50.00 / 50.09 | 49.84 / 50.00 / 50.15 |
| n2+n5 | 24.95 / 25.00 / 25.07 | 24.69 / 25.00 / 25.44 | 49.93 / 50.00 / 50.12 | 49.78 / 50.00 / 50.28 |
| n3+n4 | 24.96 / 25.00 / 25.03 | 24.71 / 25.00 / 25.53 | 49.94 / 50.00 / 50.08 | 49.79 / 50.00 / 50.21 |
| n3+n5 | 24.95 / 25.00 / 25.02 | 24.50 / 25.00 / 25.51 | 49.96 / 50.00 / 50.03 | 49.77 / 50.00 / 50.14 |
| n4+n5 | 24.94 / 25.00 / 25.06 | 24.78 / 25.00 / 25.30 | 49.95 / 50.00 / 50.07 | 49.83 / 50.00 / 50.33 |

**C-7, replica sets of the key set, NOW**

| nodes down | set keys, % min / mean / max | set bytes, % min / mean / max |
| --- | --- | --- |
| none | 6.63 / 6.67 / 6.70 | 6.41 / 6.67 / 6.84 |
| n0 | 9.97 / 10.00 / 10.07 | 9.75 / 10.00 / 10.22 |
| n1 | 9.96 / 10.00 / 10.04 | 9.75 / 10.00 / 10.31 |
| n2 | 9.95 / 10.00 / 10.04 | 9.61 / 10.00 / 10.18 |
| n3 | 9.96 / 10.00 / 10.05 | 9.83 / 10.00 / 10.26 |
| n4 | 9.96 / 10.00 / 10.03 | 9.65 / 10.00 / 10.25 |
| n5 | 9.96 / 10.00 / 10.05 | 9.66 / 10.00 / 10.27 |
| n0+n1 | 16.62 / 16.67 / 16.70 | 16.28 / 16.67 / 17.15 |
| n0+n2 | 16.61 / 16.67 / 16.74 | 16.29 / 16.67 / 16.99 |
| n0+n3 | 16.64 / 16.67 / 16.69 | 16.39 / 16.67 / 16.88 |
| n0+n4 | 16.64 / 16.67 / 16.74 | 16.35 / 16.67 / 17.05 |
| n0+n5 | 16.63 / 16.67 / 16.70 | 16.35 / 16.67 / 17.16 |
| n1+n2 | 16.61 / 16.67 / 16.76 | 16.30 / 16.67 / 16.97 |
| n1+n3 | 16.63 / 16.67 / 16.71 | 16.46 / 16.67 / 16.97 |
| n1+n4 | 16.62 / 16.67 / 16.70 | 16.38 / 16.67 / 16.91 |
| n1+n5 | 16.62 / 16.67 / 16.72 | 16.53 / 16.67 / 16.95 |
| n2+n3 | 16.63 / 16.67 / 16.74 | 16.42 / 16.67 / 16.89 |
| n2+n4 | 16.61 / 16.67 / 16.70 | 16.15 / 16.67 / 17.05 |
| n2+n5 | 16.60 / 16.67 / 16.72 | 16.43 / 16.67 / 16.93 |
| n3+n4 | 16.62 / 16.67 / 16.73 | 16.49 / 16.67 / 16.92 |
| n3+n5 | 16.64 / 16.67 / 16.73 | 16.43 / 16.67 / 16.88 |
| n4+n5 | 16.61 / 16.67 / 16.73 | 16.41 / 16.67 / 16.88 |

**C-7, new stripes 4+2, NOW, L_silo (314 stripes; other bytes per drive: 0 B)**

| nodes down | placed | refused | why | shards per node, min / max | shards per drive, min / mean / max | shards per drive, cv | drive fill %, min / max |
| --- | --- | --- | --- | --- | --- | --- | --- |
| none | 314 | 0 | - | 314 / 314 | 145 / 157.0 / 169 | 0.046 | 1.95 / 2.27 |
| n0 | 314 | 0 | - | 371 / 386 | 173 / 188.4 / 198 | 0.041 | 2.32 / 2.66 |
| n1 | 314 | 0 | - | 370 / 380 | 178 / 188.4 / 198 | 0.034 | 2.39 / 2.66 |
| n2 | 314 | 0 | - | 371 / 384 | 177 / 188.4 / 198 | 0.034 | 2.38 / 2.66 |
| n3 | 314 | 0 | - | 366 / 388 | 171 / 188.4 / 202 | 0.046 | 2.30 / 2.71 |
| n4 | 314 | 0 | - | 371 / 386 | 176 / 188.4 / 200 | 0.037 | 2.36 / 2.68 |
| n5 | 314 | 0 | - | 371 / 385 | 172 / 188.4 / 200 | 0.043 | 2.31 / 2.68 |
| n0+n1 | 314 | 0 | - | 463 / 480 | 229 / 235.5 / 246 | 0.022 | 3.07 / 3.30 |
| n0+n2 | 314 | 0 | - | 463 / 478 | 222 / 235.5 / 245 | 0.029 | 2.98 / 3.29 |
| n0+n3 | 314 | 0 | - | 459 / 478 | 222 / 235.5 / 246 | 0.028 | 2.98 / 3.30 |
| n0+n4 | 314 | 0 | - | 465 / 477 | 222 / 235.5 / 249 | 0.040 | 2.98 / 3.34 |
| n0+n5 | 314 | 0 | - | 459 / 480 | 222 / 235.5 / 246 | 0.030 | 2.98 / 3.30 |
| n1+n2 | 314 | 0 | - | 462 / 485 | 227 / 235.5 / 246 | 0.025 | 3.05 / 3.30 |
| n1+n3 | 314 | 0 | - | 460 / 476 | 226 / 235.5 / 243 | 0.021 | 3.03 / 3.26 |
| n1+n4 | 314 | 0 | - | 463 / 475 | 227 / 235.5 / 247 | 0.033 | 3.05 / 3.32 |
| n1+n5 | 314 | 0 | - | 466 / 476 | 226 / 235.5 / 242 | 0.022 | 3.03 / 3.25 |
| n2+n3 | 314 | 0 | - | 466 / 482 | 223 / 235.5 / 246 | 0.028 | 2.99 / 3.30 |
| n2+n4 | 314 | 0 | - | 464 / 483 | 225 / 235.5 / 253 | 0.034 | 3.02 / 3.40 |
| n2+n5 | 314 | 0 | - | 466 / 475 | 226 / 235.5 / 242 | 0.019 | 3.03 / 3.25 |
| n3+n4 | 314 | 0 | - | 464 / 483 | 226 / 235.5 / 250 | 0.028 | 3.03 / 3.36 |
| n3+n5 | 314 | 0 | - | 464 / 479 | 227 / 235.5 / 243 | 0.022 | 3.05 / 3.26 |
| n4+n5 | 314 | 0 | - | 468 / 474 | 223 / 235.5 / 245 | 0.037 | 2.99 / 3.29 |

**C-7, new stripes 4+2, NOW, capacity guard (9,499 stripes; other bytes per drive: 0 B)**

| nodes down | placed | refused | why | shards per node, min / max | shards per drive, min / mean / max | shards per drive, cv | drive fill %, min / max |
| --- | --- | --- | --- | --- | --- | --- | --- |
| none | 9,499 | 0 | - | 9499 / 9499 | 4697 / 4749.5 / 4802 | 0.007 | 63.04 / 64.45 |
| n0 | 9,499 | 0 | - | 11357 / 11426 | 5646 / 5699.4 / 5748 | 0.006 | 75.78 / 77.15 |
| n1 | 9,499 | 0 | - | 11365 / 11474 | 5648 / 5699.4 / 5774 | 0.006 | 75.81 / 77.50 |
| n2 | 9,499 | 0 | - | 11369 / 11471 | 5657 / 5699.4 / 5777 | 0.007 | 75.93 / 77.54 |
| n3 | 9,499 | 0 | - | 11358 / 11490 | 5644 / 5699.4 / 5782 | 0.006 | 75.75 / 77.60 |
| n4 | 9,499 | 0 | - | 11385 / 11411 | 5661 / 5699.4 / 5736 | 0.005 | 75.98 / 76.99 |
| n5 | 9,499 | 0 | - | 11359 / 11474 | 5640 / 5699.4 / 5774 | 0.006 | 75.70 / 77.50 |
| n0+n1 | 8,426 | 1,073 | node 2 needs 2 drives below the fill limit and has 1 | 12623 / 12666 | 6296 / 6319.5 / 6333 | 0.002 | 84.50 / 85.00 |
| n0+n2 | 8,388 | 1,111 | node 4 needs 2 drives below the fill limit and has 1 | 12535 / 12666 | 6235 / 6291.0 / 6333 | 0.005 | 83.68 / 85.00 |
| n0+n3 | 8,419 | 1,080 | node 2 needs 2 drives below the fill limit and has 1 | 12566 / 12666 | 6271 / 6314.2 / 6333 | 0.004 | 84.17 / 85.00 |
| n0+n4 | 8,410 | 1,089 | node 2 needs 2 drives below the fill limit and has 1 | 12581 / 12666 | 6259 / 6307.5 / 6333 | 0.004 | 84.01 / 85.00 |
| n0+n5 | 8,423 | 1,076 | node 2 needs 2 drives below the fill limit and has 1 | 12587 / 12666 | 6262 / 6317.2 / 6333 | 0.003 | 84.05 / 85.00 |
| n1+n2 | 8,408 | 1,091 | node 4 needs 2 drives below the fill limit and has 1 | 12571 / 12632 | 6277 / 6306.0 / 6333 | 0.003 | 84.25 / 85.00 |
| n1+n3 | 8,434 | 1,065 | node 4 needs 2 drives below the fill limit and has 1 | 12644 / 12659 | 6313 / 6325.5 / 6333 | 0.001 | 84.73 / 85.00 |
| n1+n4 | 8,425 | 1,074 | node 3 needs 2 drives below the fill limit and has 1 | 12623 / 12645 | 6290 / 6318.8 / 6333 | 0.002 | 84.42 / 85.00 |
| n1+n5 | 8,426 | 1,073 | node 3 needs 2 drives below the fill limit and has 1 | 12604 / 12665 | 6293 / 6319.5 / 6333 | 0.002 | 84.46 / 85.00 |
| n2+n3 | 8,425 | 1,074 | node 4 needs 2 drives below the fill limit and has 1 | 12634 / 12644 | 6301 / 6318.8 / 6333 | 0.002 | 84.57 / 85.00 |
| n2+n4 | 8,422 | 1,077 | node 3 needs 2 drives below the fill limit and has 1 | 12607 / 12664 | 6289 / 6316.5 / 6333 | 0.003 | 84.41 / 85.00 |
| n2+n5 | 8,408 | 1,091 | node 3 needs 2 drives below the fill limit and has 1 | 12569 / 12666 | 6258 / 6306.0 / 6333 | 0.004 | 83.99 / 85.00 |
| n3+n4 | 8,428 | 1,071 | node 0 needs 2 drives below the fill limit and has 1 | 12615 / 12665 | 6282 / 6321.0 / 6333 | 0.003 | 84.32 / 85.00 |
| n3+n5 | 8,425 | 1,074 | node 0 needs 2 drives below the fill limit and has 1 | 12613 / 12658 | 6280 / 6318.8 / 6333 | 0.003 | 84.29 / 85.00 |
| n4+n5 | 8,417 | 1,082 | node 2 needs 2 drives below the fill limit and has 1 | 12567 / 12666 | 6265 / 6312.8 / 6333 | 0.004 | 84.09 / 85.00 |

**C-7, existing 4+2 stripes under failure, NOW (stripes of L_silo)**

| nodes down | stripes | margin (surviving - k): stripes | unreadable | degraded cold reads, % |
| --- | --- | --- | --- | --- |
| n0 | 314 | 1: 314 | 0 | 16.64 |
| n1 | 314 | 1: 314 | 0 | 16.08 |
| n2 | 314 | 1: 314 | 0 | 16.32 |
| n3 | 314 | 1: 314 | 0 | 17.60 |
| n4 | 314 | 1: 314 | 0 | 17.52 |
| n5 | 314 | 1: 314 | 0 | 15.84 |
| n0+n1 | 314 | 0: 314 | 0 | 32.72 |
| n0+n2 | 314 | 0: 314 | 0 | 32.96 |
| n0+n3 | 314 | 0: 314 | 0 | 34.24 |
| n0+n4 | 314 | 0: 314 | 0 | 34.16 |
| n0+n5 | 314 | 0: 314 | 0 | 32.48 |
| n1+n2 | 314 | 0: 314 | 0 | 32.40 |
| n1+n3 | 314 | 0: 314 | 0 | 33.68 |
| n1+n4 | 314 | 0: 314 | 0 | 33.60 |
| n1+n5 | 314 | 0: 314 | 0 | 31.93 |
| n2+n3 | 314 | 0: 314 | 0 | 33.92 |
| n2+n4 | 314 | 0: 314 | 0 | 33.84 |
| n2+n5 | 314 | 0: 314 | 0 | 32.17 |
| n3+n4 | 314 | 0: 314 | 0 | 35.11 |
| n3+n5 | 314 | 0: 314 | 0 | 33.44 |
| n4+n5 | 314 | 0: 314 | 0 | 33.36 |

**C-7, new stripes 2 copies, NOW, cold.replicas = 2, L_silo (1,255 stripes; other bytes per drive: 0 B)**

| nodes down | placed | refused | why | shards per node, min / max | shards per drive, min / mean / max | shards per drive, cv | drive fill %, min / max |
| --- | --- | --- | --- | --- | --- | --- | --- |
| none | 1,255 | 0 | - | 404 / 450 | 198 / 209.2 / 240 | 0.054 | 2.66 / 3.22 |
| n0 | 1,255 | 0 | - | 481 / 536 | 240 / 251.0 / 286 | 0.050 | 3.22 / 3.84 |
| n1 | 1,255 | 0 | - | 479 / 526 | 230 / 251.0 / 279 | 0.048 | 3.09 / 3.74 |
| n2 | 1,255 | 0 | - | 484 / 529 | 239 / 251.0 / 280 | 0.045 | 3.21 / 3.76 |
| n3 | 1,255 | 0 | - | 487 / 518 | 241 / 251.0 / 274 | 0.039 | 3.23 / 3.68 |
| n4 | 1,255 | 0 | - | 482 / 537 | 233 / 251.0 / 289 | 0.056 | 3.13 / 3.88 |
| n5 | 1,255 | 0 | - | 489 / 518 | 234 / 251.0 / 260 | 0.036 | 3.14 / 3.49 |
| n0+n1 | 1,255 | 0 | - | 618 / 635 | 293 / 313.8 / 342 | 0.041 | 3.93 / 4.59 |
| n0+n2 | 1,255 | 0 | - | 618 / 646 | 305 / 313.8 / 341 | 0.036 | 4.09 / 4.58 |
| n0+n3 | 1,255 | 0 | - | 609 / 638 | 298 / 313.8 / 334 | 0.033 | 4.00 / 4.48 |
| n0+n4 | 1,255 | 0 | - | 608 / 647 | 294 / 313.8 / 353 | 0.055 | 3.95 / 4.74 |
| n0+n5 | 1,255 | 0 | - | 613 / 646 | 295 / 313.8 / 332 | 0.031 | 3.96 / 4.46 |
| n1+n2 | 1,255 | 0 | - | 602 / 646 | 293 / 313.8 / 339 | 0.042 | 3.93 / 4.55 |
| n1+n3 | 1,255 | 0 | - | 611 / 639 | 294 / 313.8 / 336 | 0.038 | 3.95 / 4.51 |
| n1+n4 | 1,255 | 0 | - | 619 / 646 | 297 / 313.8 / 337 | 0.036 | 3.99 / 4.52 |
| n1+n5 | 1,255 | 0 | - | 617 / 633 | 281 / 313.8 / 336 | 0.046 | 3.77 / 4.51 |
| n2+n3 | 1,255 | 0 | - | 612 / 637 | 304 / 313.8 / 331 | 0.026 | 4.08 / 4.44 |
| n2+n4 | 1,255 | 0 | - | 603 / 651 | 290 / 313.8 / 349 | 0.058 | 3.89 / 4.68 |
| n2+n5 | 1,255 | 0 | - | 614 / 649 | 296 / 313.8 / 329 | 0.037 | 3.97 / 4.42 |
| n3+n4 | 1,255 | 0 | - | 608 / 644 | 295 / 313.8 / 339 | 0.042 | 3.96 / 4.55 |
| n3+n5 | 1,255 | 0 | - | 609 / 644 | 293 / 313.8 / 334 | 0.038 | 3.93 / 4.48 |
| n4+n5 | 1,255 | 0 | - | 615 / 640 | 290 / 313.8 / 328 | 0.039 | 3.89 / 4.40 |

**C-7, new stripes 2 copies, NOW, cold.replicas = 2, capacity guard (28,498 stripes; other bytes per drive: 0 B)**

| nodes down | placed | refused | why | shards per node, min / max | shards per drive, min / mean / max | shards per drive, cv | drive fill %, min / max |
| --- | --- | --- | --- | --- | --- | --- | --- |
| none | 28,498 | 0 | - | 9430 / 9561 | 4661 / 4749.7 / 4802 | 0.008 | 62.56 / 64.45 |
| n0 | 28,498 | 0 | - | 11324 / 11453 | 5650 / 5699.6 / 5754 | 0.005 | 75.83 / 77.23 |
| n1 | 28,498 | 0 | - | 11331 / 11437 | 5608 / 5699.6 / 5747 | 0.006 | 75.27 / 77.13 |
| n2 | 28,498 | 0 | - | 11274 / 11465 | 5580 / 5699.6 / 5738 | 0.008 | 74.89 / 77.01 |
| n3 | 28,498 | 0 | - | 11330 / 11486 | 5628 / 5699.6 / 5753 | 0.007 | 75.54 / 77.22 |
| n4 | 28,498 | 0 | - | 11325 / 11508 | 5611 / 5699.6 / 5769 | 0.008 | 75.31 / 77.43 |
| n5 | 28,498 | 0 | - | 11360 / 11473 | 5626 / 5699.6 / 5738 | 0.006 | 75.51 / 77.01 |
| n0+n1 | 25,331 | 3,167 | node 2 needs 1 drives below the fill limit and has 0 | 12664 / 12666 | 6331 / 6332.8 / 6333 | 0.000 | 84.97 / 85.00 |
| n0+n2 | 25,295 | 3,203 | node 4 needs 1 drives below the fill limit and has 0 | 12592 / 12666 | 6262 / 6323.8 / 6333 | 0.004 | 84.05 / 85.00 |
| n0+n3 | 25,280 | 3,218 | node 1 needs 1 drives below the fill limit and has 0 | 12562 / 12666 | 6263 / 6320.0 / 6333 | 0.004 | 84.06 / 85.00 |
| n0+n4 | 25,331 | 3,167 | node 2 needs 1 drives below the fill limit and has 0 | 12664 / 12666 | 6331 / 6332.8 / 6333 | 0.000 | 84.97 / 85.00 |
| n0+n5 | 25,322 | 3,176 | node 2 needs 1 drives below the fill limit and has 0 | 12646 / 12666 | 6313 / 6330.5 / 6333 | 0.001 | 84.73 / 85.00 |
| n1+n2 | 25,306 | 3,192 | node 3 needs 1 drives below the fill limit and has 0 | 12614 / 12666 | 6281 / 6326.5 / 6333 | 0.003 | 84.30 / 85.00 |
| n1+n3 | 25,331 | 3,167 | node 2 needs 1 drives below the fill limit and has 0 | 12664 / 12666 | 6331 / 6332.8 / 6333 | 0.000 | 84.97 / 85.00 |
| n1+n4 | 25,289 | 3,209 | node 2 needs 1 drives below the fill limit and has 0 | 12580 / 12666 | 6257 / 6322.2 / 6333 | 0.004 | 83.98 / 85.00 |
| n1+n5 | 25,324 | 3,174 | node 2 needs 1 drives below the fill limit and has 0 | 12650 / 12666 | 6317 / 6331.0 / 6333 | 0.001 | 84.79 / 85.00 |
| n2+n3 | 25,299 | 3,199 | node 1 needs 1 drives below the fill limit and has 0 | 12600 / 12666 | 6270 / 6324.8 / 6333 | 0.003 | 84.15 / 85.00 |
| n2+n4 | 25,316 | 3,182 | node 3 needs 1 drives below the fill limit and has 0 | 12634 / 12666 | 6301 / 6329.0 / 6333 | 0.002 | 84.57 / 85.00 |
| n2+n5 | 25,332 | 3,166 | node 3 needs 1 drives below the fill limit and has 0 | 12666 / 12666 | 6333 / 6333.0 / 6333 | 0.000 | 85.00 / 85.00 |
| n3+n4 | 25,281 | 3,217 | node 2 needs 1 drives below the fill limit and has 0 | 12564 / 12666 | 6277 / 6320.2 / 6333 | 0.004 | 84.25 / 85.00 |
| n3+n5 | 25,326 | 3,172 | node 2 needs 1 drives below the fill limit and has 0 | 12654 / 12666 | 6321 / 6331.5 / 6333 | 0.001 | 84.84 / 85.00 |
| n4+n5 | 25,298 | 3,200 | node 2 needs 1 drives below the fill limit and has 0 | 12598 / 12666 | 6281 / 6324.5 / 6333 | 0.003 | 84.30 / 85.00 |

**C-7, existing 2 copies stripes under failure, NOW, cold.replicas = 2 (stripes of L_silo)**

| nodes down | stripes | margin (surviving - k): stripes | unreadable | degraded cold reads, % |
| --- | --- | --- | --- | --- |
| n0 | 1,255 | 0: 404, 1: 851 | 0 | 17.93 |
| n1 | 1,255 | 0: 406, 1: 849 | 0 | 15.62 |
| n2 | 1,255 | 0: 428, 1: 827 | 0 | 17.05 |
| n3 | 1,255 | 0: 406, 1: 849 | 0 | 14.50 |
| n4 | 1,255 | 0: 416, 1: 839 | 0 | 17.53 |
| n5 | 1,255 | 0: 450, 1: 805 | 0 | 17.37 |
| n0+n1 | 1,255 | -1: 85, 0: 640, 1: 530 | 85 | 33.55 |
| n0+n2 | 1,255 | -1: 93, 0: 646, 1: 516 | 93 | 34.98 |
| n0+n3 | 1,255 | -1: 80, 0: 650, 1: 525 | 80 | 32.43 |
| n0+n4 | 1,255 | -1: 68, 0: 684, 1: 503 | 68 | 35.46 |
| n0+n5 | 1,255 | -1: 78, 0: 698, 1: 479 | 78 | 35.30 |
| n1+n2 | 1,255 | -1: 77, 0: 680, 1: 498 | 77 | 32.67 |
| n1+n3 | 1,255 | -1: 74, 0: 664, 1: 517 | 74 | 30.12 |
| n1+n4 | 1,255 | -1: 81, 0: 660, 1: 514 | 81 | 33.15 |
| n1+n5 | 1,255 | -1: 89, 0: 678, 1: 488 | 89 | 32.99 |
| n2+n3 | 1,255 | -1: 74, 0: 686, 1: 495 | 74 | 31.55 |
| n2+n4 | 1,255 | -1: 91, 0: 662, 1: 502 | 91 | 34.58 |
| n2+n5 | 1,255 | -1: 93, 0: 692, 1: 470 | 93 | 34.42 |
| n3+n4 | 1,255 | -1: 82, 0: 658, 1: 515 | 82 | 32.03 |
| n3+n5 | 1,255 | -1: 96, 0: 664, 1: 495 | 96 | 31.87 |
| n4+n5 | 1,255 | -1: 94, 0: 678, 1: 483 | 94 | 34.90 |


**Keys under node rank [M].** On LATER every node holds every key (`hot.replicas = 3`) and each
writes 33.33% of the keys (33.09 to 33.71% of the bytes); with one node down the other two write
half each. On NOW each of the 15 replica pairs holds 6.63 to 6.70% of the keys, and each node
33.27 to 33.42% of them; the spread of the bytes is wider (33.00 to 33.67%) because a few large
objects weigh more. NOW's writer column uses the first node of the rank: the design leaves the
writer of a pair to task T-S1 (10.2).

**New stripes [M].** Shards spread evenly over the drives: at the capacity guard the coefficient
of variation of shards per drive is at most 0.012 on every profile with every number of nodes
down. LATER places `4+4` with one node down (4 + 4 on the two survivors) and refuses every stripe
with two down (a node would hold 8 shards, more than the 4 the code can lose). NOW places every
stripe with one node down; **with two of its six nodes down and the drives filled to the guard,
about 11% of new `4+2` stripes and 11% of new copies are refused**: the guard keeps room for one
node and one drive, and with two nodes down the four survivors need both drives of two nodes
below the fill limit, which some no longer are.

**Existing stripes under failure [M].** With one node down every LATER stripe keeps one or two
shards beyond `k`, and about a third of cold reads are degraded (33 to 34%); with two down every
LATER stripe is unreadable, as the design states (two nodes lost stop service, Q5). NOW at `4+2`
keeps every stripe readable with two nodes down (at no margin) and degrades 16 to 18% of cold
reads with one down, 32 to 35% with two. NOW with two copies loses 68 to 96 of 1,255 blocks
(5 to 8%) with two nodes down: those whose two copies were both on them.

**A contradiction in the design, not settled here.** With one node down LATER's survivors hold
4 shards of a `4+4` stripe each. The design's placement test (section 14) expects exactly that
("with one node down `4+4` places 4 + 4"), but 10.4 states as a write-time invariant that no
node holds more than `max_shards_per_domain`, which is 3 on LATER (and NOW's example says "one
per node at write time"). The tables apply the cap `m`, as the test does. Under the stricter
reading every new stripe is refused with a node down on both profiles, and new data stays on the
hot tier until the node returns or is replaced.

## C-10. Garbage collection replayed

**C-10, cold tier at the end of the data and at the end of the replay**

| profile | deaths | grouping | at | cold raw | live in cold | cold_fill_efficiency % | stripes | freed whole | partial | dropped hot | compacted hot | bytes compacted |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| now | min-ttl | arrival | end of data | 118 GB | 69.4 GB | 59.0 | 42 | 448 | 0 | 0 | 0 | 0 B |
| now | min-ttl | arrival | end of replay | 0 B | 0 B | 0.0 | 0 | 508 | 1 | 0 | 0 | 0 B |
| now | min-ttl | expiry | end of data | 112 GB | 67.5 GB | 60.1 | 41 | 456 | 2 | 0 | 0 | 0 B |
| now | min-ttl | expiry | end of replay | 0 B | 0 B | 0.0 | 0 | 516 | 3 | 0 | 0 | 0 B |
| now | ttl-by-kind | arrival | end of data | 118 GB | 69.4 GB | 59.0 | 42 | 448 | 0 | 0 | 0 | 0 B |
| now | ttl-by-kind | arrival | end of replay | 0 B | 0 B | 0.0 | 0 | 508 | 1 | 0 | 0 | 0 B |
| now | ttl-by-kind | expiry | end of data | 116 GB | 69.0 GB | 59.4 | 52 | 456 | 2 | 0 | 0 | 0 B |
| now | ttl-by-kind | expiry | end of replay | 0 B | 0 B | 0.0 | 0 | 530 | 4 | 0 | 0 | 0 B |
| now | footer-dead | arrival | end of data | 112 GB | 67.6 GB | 60.3 | 41 | 445 | 0 | 2 | 12 | 959 MB |
| now | footer-dead | arrival | end of replay | 0 B | 0 B | 0.0 | 0 | 504 | 0 | 2 | 12 | 959 MB |
| now | footer-dead | expiry | end of data | 113 GB | 69.0 GB | 60.9 | 52 | 402 | 5 | 474 | 0 | 0 B |
| now | footer-dead | expiry | end of replay | 0 B | 0 B | 0.0 | 0 | 476 | 7 | 474 | 0 | 0 B |
| now-replicas | min-ttl | arrival | end of data | 140 GB | 69.4 GB | 49.6 | 165 | 1,795 | 0 | 0 | 0 | 0 B |
| now-replicas | min-ttl | arrival | end of replay | 0 B | 0 B | 0.0 | 0 | 2,030 | 0 | 0 | 0 | 0 B |
| now-replicas | min-ttl | expiry | end of data | 140 GB | 69.4 GB | 49.6 | 165 | 1,821 | 0 | 0 | 0 | 0 B |
| now-replicas | min-ttl | expiry | end of replay | 0 B | 0 B | 0.0 | 0 | 2,056 | 0 | 0 | 0 | 0 B |
| now-replicas | ttl-by-kind | arrival | end of data | 140 GB | 69.4 GB | 49.6 | 165 | 1,795 | 0 | 0 | 0 | 0 B |
| now-replicas | ttl-by-kind | arrival | end of replay | 0 B | 0 B | 0.0 | 0 | 2,030 | 0 | 0 | 0 | 0 B |
| now-replicas | ttl-by-kind | expiry | end of data | 141 GB | 69.8 GB | 49.6 | 210 | 1,821 | 0 | 0 | 0 | 0 B |
| now-replicas | ttl-by-kind | expiry | end of replay | 0 B | 0 B | 0.0 | 0 | 2,112 | 0 | 0 | 0 | 0 B |
| now-replicas | footer-dead | arrival | end of data | 140 GB | 69.4 GB | 49.6 | 165 | 1,781 | 0 | 2 | 12 | 959 MB |
| now-replicas | footer-dead | arrival | end of replay | 0 B | 0 B | 0.0 | 0 | 2,016 | 0 | 2 | 12 | 959 MB |
| now-replicas | footer-dead | expiry | end of data | 140 GB | 69.7 GB | 49.8 | 209 | 1,598 | 0 | 474 | 0 | 0 B |
| now-replicas | footer-dead | expiry | end of replay | 0 B | 0 B | 0.0 | 0 | 1,888 | 0 | 474 | 0 | 0 B |
| later | min-ttl | arrival | end of data | 60.2 GB | 23.3 GB | 38.7 | 9 | 368 | 0 | 0 | 0 | 0 B |
| later | min-ttl | arrival | end of replay | 0 B | 0 B | 0.0 | 0 | 405 | 0 | 2 | 0 | 0 B |
| later | min-ttl | expiry | end of data | 67.5 GB | 25.4 GB | 37.6 | 10 | 372 | 0 | 2 | 0 | 0 B |
| later | min-ttl | expiry | end of replay | 0 B | 0 B | 0.0 | 0 | 409 | 0 | 5 | 0 | 0 B |
| later | ttl-by-kind | arrival | end of data | 60.2 GB | 23.3 GB | 38.7 | 9 | 368 | 0 | 0 | 0 | 0 B |
| later | ttl-by-kind | arrival | end of replay | 0 B | 0 B | 0.0 | 0 | 406 | 1 | 0 | 0 | 0 B |
| later | ttl-by-kind | expiry | end of data | 61.5 GB | 23.4 GB | 38.0 | 10 | 372 | 0 | 2 | 0 | 0 B |
| later | ttl-by-kind | expiry | end of replay | 0 B | 0 B | 0.0 | 0 | 413 | 2 | 2 | 0 | 0 B |
| later | footer-dead | arrival | end of data | 58.8 GB | 22.2 GB | 37.8 | 8 | 367 | 0 | 3 | 4 | 1.66 GB |
| later | footer-dead | arrival | end of replay | 0 B | 0 B | 0.0 | 0 | 404 | 1 | 3 | 4 | 1.66 GB |
| later | footer-dead | expiry | end of data | 59.6 GB | 23.0 GB | 38.7 | 9 | 314 | 0 | 246 | 2 | 42.8 MB |
| later | footer-dead | expiry | end of replay | 0 B | 0 B | 0.0 | 0 | 354 | 2 | 248 | 3 | 107 MB |

**C-10, cold_fill_efficiency % by week**

| day | now/min-ttl/arrival | now/min-ttl/expiry | now/ttl-by-kind/arrival | now/ttl-by-kind/expiry | now/footer-dead/arrival | now/footer-dead/expiry | now-replicas/min-ttl/arrival | now-replicas/min-ttl/expiry | now-replicas/ttl-by-kind/arrival | now-replicas/ttl-by-kind/expiry | now-replicas/footer-dead/arrival | now-replicas/footer-dead/expiry | later/min-ttl/arrival | later/min-ttl/expiry | later/ttl-by-kind/arrival | later/ttl-by-kind/expiry | later/footer-dead/arrival | later/footer-dead/expiry |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 7 | 58.7 | 49.3 | 58.7 | 49.3 | 55.2 | 48.3 | 49.8 | 49.8 | 49.8 | 49.8 | 46.7 | 49.8 | 42.4 | 37.1 | 42.4 | 37.1 | 42.4 | 37.1 |
| 14 | 61.9 | 61.9 | 61.9 | 61.9 | 51.9 | 60.7 | 49.4 | 49.7 | 49.4 | 49.7 | 41.6 | 49.2 | 37.0 | 39.5 | 37.0 | 39.5 | 27.8 | 34.8 |
| 21 | 63.3 | 62.9 | 63.3 | 62.9 | 52.8 | 62.7 | 49.9 | 49.7 | 49.9 | 49.7 | 41.7 | 49.6 | 45.7 | 45.9 | 45.7 | 45.9 | 40.7 | 45.3 |
| 28 | 36.6 | 29.6 | 36.6 | 29.6 | 31.0 | 31.5 | 44.7 | 41.2 | 44.7 | 41.2 | 39.7 | 39.3 | 22.0 | 24.0 | 22.0 | 24.0 | 18.7 | 22.7 |
| 35 | 61.7 | 62.0 | 61.7 | 61.2 | 59.8 | 61.0 | 50.0 | 50.0 | 50.0 | 50.0 | 48.1 | 50.0 | 40.3 | 41.2 | 40.3 | 42.4 | 36.3 | 39.6 |
| 42 | 53.6 | 51.5 | 56.2 | 60.4 | 56.8 | 60.4 | 47.1 | 47.1 | 46.4 | 49.2 | 46.4 | 49.2 | 38.6 | 36.2 | 41.3 | 42.2 | 41.0 | 42.2 |
| 49 | 0.0 | 0.0 | 54.6 | 61.9 | 55.1 | 61.9 | 0.0 | 0.0 | 45.0 | 50.0 | 45.0 | 50.0 | 0.0 | 0.0 | 40.0 | 44.6 | 39.6 | 44.6 |
| 56 | 0.0 | 0.0 | 54.6 | 61.9 | 55.1 | 61.9 | 0.0 | 0.0 | 45.0 | 50.0 | 45.0 | 50.0 | 0.0 | 0.0 | 40.0 | 44.6 | 39.6 | 44.6 |
| 63 | 0.0 | 0.0 | 54.6 | 61.9 | 55.1 | 61.9 | 0.0 | 0.0 | 45.0 | 50.0 | 45.0 | 50.0 | 0.0 | 0.0 | 40.0 | 44.6 | 39.6 | 44.6 |
| 70 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 77 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 84 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |
| 91 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 |

**C-10, sealed blocks by size so far**

| profile | deaths | grouping | at | under seal_min_bytes | seal_min_bytes up to full | full |
| --- | --- | --- | --- | --- | --- | --- |
| now | min-ttl | arrival | end of data | 558 | 730 | 726 |
| now | min-ttl | arrival | end of replay | 573 | 731 | 726 |
| now | min-ttl | expiry | end of data | 583 | 715 | 742 |
| now | min-ttl | expiry | end of replay | 598 | 716 | 742 |
| now | ttl-by-kind | arrival | end of data | 558 | 730 | 726 |
| now | ttl-by-kind | arrival | end of replay | 573 | 731 | 726 |
| now | ttl-by-kind | expiry | end of data | 620 | 729 | 731 |
| now | ttl-by-kind | expiry | end of replay | 651 | 730 | 731 |
| now | footer-dead | arrival | end of data | 558 | 730 | 726 |
| now | footer-dead | arrival | end of replay | 573 | 731 | 726 |
| now | footer-dead | expiry | end of data | 871 | 742 | 717 |
| now | footer-dead | expiry | end of replay | 902 | 743 | 717 |
| now-replicas | min-ttl | arrival | end of data | 558 | 730 | 726 |
| now-replicas | min-ttl | arrival | end of replay | 573 | 731 | 726 |
| now-replicas | min-ttl | expiry | end of data | 583 | 715 | 742 |
| now-replicas | min-ttl | expiry | end of replay | 598 | 716 | 742 |
| now-replicas | ttl-by-kind | arrival | end of data | 558 | 730 | 726 |
| now-replicas | ttl-by-kind | arrival | end of replay | 573 | 731 | 726 |
| now-replicas | ttl-by-kind | expiry | end of data | 620 | 729 | 731 |
| now-replicas | ttl-by-kind | expiry | end of replay | 651 | 730 | 731 |
| now-replicas | footer-dead | arrival | end of data | 558 | 730 | 726 |
| now-replicas | footer-dead | arrival | end of replay | 573 | 731 | 726 |
| now-replicas | footer-dead | expiry | end of data | 871 | 742 | 717 |
| now-replicas | footer-dead | expiry | end of replay | 902 | 743 | 717 |
| later | min-ttl | arrival | end of data | 145 | 685 | 788 |
| later | min-ttl | arrival | end of replay | 145 | 689 | 788 |
| later | min-ttl | expiry | end of data | 169 | 681 | 787 |
| later | min-ttl | expiry | end of replay | 169 | 685 | 787 |
| later | ttl-by-kind | arrival | end of data | 145 | 685 | 788 |
| later | ttl-by-kind | arrival | end of replay | 145 | 689 | 788 |
| later | ttl-by-kind | expiry | end of data | 171 | 686 | 778 |
| later | ttl-by-kind | expiry | end of replay | 175 | 691 | 782 |
| later | footer-dead | arrival | end of data | 144 | 683 | 791 |
| later | footer-dead | arrival | end of replay | 144 | 687 | 791 |
| later | footer-dead | expiry | end of data | 163 | 718 | 766 |
| later | footer-dead | expiry | end of replay | 168 | 723 | 770 |

**C-10, stripes not freed by live fraction**

| profile | deaths | grouping | at | 0-10% | 10-20% | 20-30% | 30-40% | 40-50% | 50-60% | 60-70% | 70-80% | 80-90% | 90-100% |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| now | min-ttl | arrival | end of data | 1 | 0 | 0 | 0 | 1 | 2 | 0 | 0 | 1 | 37 |
| now | min-ttl | arrival | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now | min-ttl | expiry | end of data | 0 | 1 | 0 | 0 | 1 | 2 | 0 | 1 | 0 | 36 |
| now | min-ttl | expiry | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now | ttl-by-kind | arrival | end of data | 1 | 0 | 0 | 0 | 1 | 2 | 0 | 0 | 1 | 37 |
| now | ttl-by-kind | arrival | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now | ttl-by-kind | expiry | end of data | 0 | 1 | 0 | 0 | 1 | 2 | 0 | 1 | 0 | 47 |
| now | ttl-by-kind | expiry | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now | footer-dead | arrival | end of data | 0 | 1 | 0 | 0 | 2 | 1 | 0 | 0 | 1 | 36 |
| now | footer-dead | arrival | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now | footer-dead | expiry | end of data | 0 | 0 | 0 | 1 | 1 | 1 | 0 | 0 | 1 | 48 |
| now | footer-dead | expiry | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now-replicas | min-ttl | arrival | end of data | 0 | 0 | 0 | 2 | 4 | 8 | 2 | 0 | 0 | 149 |
| now-replicas | min-ttl | arrival | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now-replicas | min-ttl | expiry | end of data | 0 | 0 | 0 | 3 | 4 | 6 | 2 | 0 | 0 | 150 |
| now-replicas | min-ttl | expiry | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now-replicas | ttl-by-kind | arrival | end of data | 0 | 0 | 0 | 2 | 4 | 8 | 2 | 0 | 0 | 149 |
| now-replicas | ttl-by-kind | arrival | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now-replicas | ttl-by-kind | expiry | end of data | 0 | 0 | 0 | 3 | 4 | 6 | 2 | 0 | 0 | 195 |
| now-replicas | ttl-by-kind | expiry | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now-replicas | footer-dead | arrival | end of data | 0 | 0 | 0 | 4 | 2 | 9 | 1 | 0 | 0 | 149 |
| now-replicas | footer-dead | arrival | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| now-replicas | footer-dead | expiry | end of data | 0 | 0 | 0 | 2 | 4 | 6 | 2 | 0 | 1 | 194 |
| now-replicas | footer-dead | expiry | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| later | min-ttl | arrival | end of data | 1 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 7 |
| later | min-ttl | arrival | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| later | min-ttl | expiry | end of data | 1 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 8 |
| later | min-ttl | expiry | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| later | ttl-by-kind | arrival | end of data | 1 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 7 |
| later | ttl-by-kind | arrival | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| later | ttl-by-kind | expiry | end of data | 1 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 8 |
| later | ttl-by-kind | expiry | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| later | footer-dead | arrival | end of data | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 7 |
| later | footer-dead | arrival | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| later | footer-dead | expiry | end of data | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 8 |
| later | footer-dead | expiry | end of replay | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |

**C-10, rewrite cost below each rewrite_below**

| profile | deaths | grouping | at | rewrite_below | stripes | rewrite bytes | raw freed | written per raw byte freed | written per logical byte freed |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| now | min-ttl | arrival | end of data | 0.20 | 1 | 223 MB | 5.90 GB | 0.04 | 0.06 |
| now | min-ttl | arrival | end of data | 0.30 | 1 | 223 MB | 5.90 GB | 0.04 | 0.06 |
| now | min-ttl | arrival | end of data | 0.50 | 2 | 260 MB | 6.03 GB | 0.04 | 0.07 |
| now | min-ttl | arrival | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | min-ttl | arrival | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | min-ttl | arrival | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | min-ttl | expiry | end of data | 0.20 | 1 | 232 MB | 3.80 GB | 0.06 | 0.16 |
| now | min-ttl | expiry | end of data | 0.30 | 1 | 232 MB | 3.80 GB | 0.06 | 0.16 |
| now | min-ttl | expiry | end of data | 0.50 | 2 | 274 MB | 3.94 GB | 0.07 | 0.19 |
| now | min-ttl | expiry | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | min-ttl | expiry | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | min-ttl | expiry | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | ttl-by-kind | arrival | end of data | 0.20 | 1 | 223 MB | 5.90 GB | 0.04 | 0.06 |
| now | ttl-by-kind | arrival | end of data | 0.30 | 1 | 223 MB | 5.90 GB | 0.04 | 0.06 |
| now | ttl-by-kind | arrival | end of data | 0.50 | 2 | 260 MB | 6.03 GB | 0.04 | 0.07 |
| now | ttl-by-kind | arrival | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | ttl-by-kind | arrival | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | ttl-by-kind | arrival | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | ttl-by-kind | expiry | end of data | 0.20 | 1 | 232 MB | 3.80 GB | 0.06 | 0.16 |
| now | ttl-by-kind | expiry | end of data | 0.30 | 1 | 232 MB | 3.80 GB | 0.06 | 0.16 |
| now | ttl-by-kind | expiry | end of data | 0.50 | 2 | 274 MB | 3.94 GB | 0.07 | 0.19 |
| now | ttl-by-kind | expiry | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | ttl-by-kind | expiry | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | ttl-by-kind | expiry | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | footer-dead | arrival | end of data | 0.20 | 1 | 260 MB | 3.82 GB | 0.07 | 0.18 |
| now | footer-dead | arrival | end of data | 0.30 | 1 | 260 MB | 3.82 GB | 0.07 | 0.18 |
| now | footer-dead | arrival | end of data | 0.50 | 3 | 338 MB | 4.10 GB | 0.08 | 0.22 |
| now | footer-dead | arrival | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | footer-dead | arrival | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | footer-dead | arrival | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | footer-dead | expiry | end of data | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | footer-dead | expiry | end of data | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | footer-dead | expiry | end of data | 0.50 | 2 | 65.8 MB | 247 MB | 0.27 | 0.75 |
| now | footer-dead | expiry | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | footer-dead | expiry | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now | footer-dead | expiry | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | min-ttl | arrival | end of data | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | min-ttl | arrival | end of data | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | min-ttl | arrival | end of data | 0.50 | 6 | 280 MB | 1.35 GB | 0.21 | 0.71 |
| now-replicas | min-ttl | arrival | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | min-ttl | arrival | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | min-ttl | arrival | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | min-ttl | expiry | end of data | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | min-ttl | expiry | end of data | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | min-ttl | expiry | end of data | 0.50 | 7 | 282 MB | 1.37 GB | 0.21 | 0.70 |
| now-replicas | min-ttl | expiry | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | min-ttl | expiry | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | min-ttl | expiry | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | ttl-by-kind | arrival | end of data | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | ttl-by-kind | arrival | end of data | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | ttl-by-kind | arrival | end of data | 0.50 | 6 | 280 MB | 1.35 GB | 0.21 | 0.71 |
| now-replicas | ttl-by-kind | arrival | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | ttl-by-kind | arrival | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | ttl-by-kind | arrival | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | ttl-by-kind | expiry | end of data | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | ttl-by-kind | expiry | end of data | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | ttl-by-kind | expiry | end of data | 0.50 | 7 | 282 MB | 1.37 GB | 0.21 | 0.70 |
| now-replicas | ttl-by-kind | expiry | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | ttl-by-kind | expiry | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | ttl-by-kind | expiry | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | footer-dead | arrival | end of data | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | footer-dead | arrival | end of data | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | footer-dead | arrival | end of data | 0.50 | 6 | 276 MB | 1.35 GB | 0.20 | 0.69 |
| now-replicas | footer-dead | arrival | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | footer-dead | arrival | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | footer-dead | arrival | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | footer-dead | expiry | end of data | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | footer-dead | expiry | end of data | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | footer-dead | expiry | end of data | 0.50 | 6 | 54.5 MB | 268 MB | 0.20 | 0.69 |
| now-replicas | footer-dead | expiry | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | footer-dead | expiry | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| now-replicas | footer-dead | expiry | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | min-ttl | arrival | end of data | 0.20 | 1 | 30.5 MB | 2.12 GB | 0.01 | 0.03 |
| later | min-ttl | arrival | end of data | 0.30 | 1 | 30.5 MB | 2.12 GB | 0.01 | 0.03 |
| later | min-ttl | arrival | end of data | 0.50 | 2 | 863 MB | 8.57 GB | 0.10 | 0.38 |
| later | min-ttl | arrival | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | min-ttl | arrival | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | min-ttl | arrival | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | min-ttl | expiry | end of data | 0.20 | 1 | 191 MB | 7.21 GB | 0.03 | 0.07 |
| later | min-ttl | expiry | end of data | 0.30 | 1 | 191 MB | 7.21 GB | 0.03 | 0.07 |
| later | min-ttl | expiry | end of data | 0.50 | 2 | 469 MB | 8.69 GB | 0.05 | 0.15 |
| later | min-ttl | expiry | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | min-ttl | expiry | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | min-ttl | expiry | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | ttl-by-kind | arrival | end of data | 0.20 | 1 | 30.5 MB | 2.12 GB | 0.01 | 0.03 |
| later | ttl-by-kind | arrival | end of data | 0.30 | 1 | 30.5 MB | 2.12 GB | 0.01 | 0.03 |
| later | ttl-by-kind | arrival | end of data | 0.50 | 2 | 863 MB | 8.57 GB | 0.10 | 0.38 |
| later | ttl-by-kind | arrival | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | ttl-by-kind | arrival | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | ttl-by-kind | arrival | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | ttl-by-kind | expiry | end of data | 0.20 | 1 | 191 MB | 7.21 GB | 0.03 | 0.07 |
| later | ttl-by-kind | expiry | end of data | 0.30 | 1 | 191 MB | 7.21 GB | 0.03 | 0.07 |
| later | ttl-by-kind | expiry | end of data | 0.50 | 2 | 469 MB | 8.69 GB | 0.05 | 0.15 |
| later | ttl-by-kind | expiry | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | ttl-by-kind | expiry | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | ttl-by-kind | expiry | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | footer-dead | arrival | end of data | 0.20 | 1 | 513 MB | 7.28 GB | 0.07 | 0.21 |
| later | footer-dead | arrival | end of data | 0.30 | 1 | 513 MB | 7.28 GB | 0.07 | 0.21 |
| later | footer-dead | arrival | end of data | 0.50 | 1 | 513 MB | 7.28 GB | 0.07 | 0.21 |
| later | footer-dead | arrival | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | footer-dead | arrival | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | footer-dead | arrival | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | footer-dead | expiry | end of data | 0.20 | 1 | 116 MB | 6.74 GB | 0.02 | 0.05 |
| later | footer-dead | expiry | end of data | 0.30 | 1 | 116 MB | 6.74 GB | 0.02 | 0.05 |
| later | footer-dead | expiry | end of data | 0.50 | 1 | 116 MB | 6.74 GB | 0.02 | 0.05 |
| later | footer-dead | expiry | end of replay | 0.20 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | footer-dead | expiry | end of replay | 0.30 | 0 | 0 B | 0 B | 0.00 | 0.00 |
| later | footer-dead | expiry | end of replay | 0.50 | 0 | 0 B | 0 B | 0.00 | 0.00 |


**How it was replayed.** Every object of the listing arrives at its write time and dies by one
of three rules: `min-ttl` (7 days after arrival), `ttl-by-kind` (30 days for a task output under
`cas/b/`, 7 days for the rest) and `footer-dead` (at arrival for the 21,150 packs C-6 found
dead, `ttl-by-kind` for the rest). Each is replayed hour by hour under both groupings of 10.5
(`arrival`: one class; `expiry`: the week of death, modulo 8) and under three profiles: NOW
(`4+2`), NOW before erasure coding ships (`now-replicas`: two copies of each block) and LATER
(`4+4`), with the design's constants (21.1, 10.14). A small object is a record with its 64-byte
header in the open segment of its writer and class; large objects are packed first-fit
decreasing per class in windows of 64; a block goes cold when its oldest byte is
`destage.max_age` old (today's ingest never reaches the dirty marks, 10.15); a segment less than
half live is compacted on the hot tier.

**What it shows [M].**
- **`cold_fill_efficiency` at the end of the data** is 59.0 to 60.9% at NOW's `4+2` (ceiling
  66.7%), 49.6 to 49.8% with two copies (ceiling 50%) and 37.6 to 38.7% at LATER's `4+4` (ceiling
  50%). It dips in the fourth week (day 28: 29.6 to 36.6% at `4+2`, 18.7 to 24.0% on LATER, the
  weekly table), when the large ingest of late October dies and its stripes are partly dead [I].
  Why LATER sits further below its ceiling is not shown by these tables.
- **Expiry grouping changes little at today's mix.** With one TTL for everything (`min-ttl`) the
  two groupings are within 1.1 points of each other. With mixed TTLs it helps only where both
  TTLs are alive: at day 49 of `ttl-by-kind`, 61.9% against 54.6% on NOW and 44.6% against 40.0%
  on LATER (the weekly table).
- **Stripes are freed whole.** At the end of the data 1,272 of the 1,404 stripes not yet freed
  (91%, all runs together) are 90 to 100% live, and the rewrite tables find at most 7 stripes
  below `rewrite_below` 0.5 in any run, costing at most 0.27 bytes written per raw byte freed.
  GC by rewrite is rarely needed at these TTLs.
- **Packs dead at arrival mostly never go cold** under expiry grouping (474 blocks on NOW, 246 on
  LATER are unlinked hot); under arrival grouping they are mixed with live data and compaction
  rewrites 959 MB (NOW) and 1.66 GB (LATER) on the hot tier.
- **Partial stripes are rare**: at most 7 in any run over the whole replay.

**What C-10 decides.** `gc.rewrite_below` 0.30 stays (below it at most one stripe per run is
rewritten at today's TTLs, and the rewrite is cheap where it is); `destage.partial_after` stays (partial stripes are
rare); expiry-class grouping stays as the design has it (it costs nothing and helps under mixed
TTLs). The death times are synthetic: when the CAS service's GC runs, the same replay with its
real deaths (`--dead-keys`, or a listing of real deletes) replaces them.

## The constants

| constant | value | was | from | status |
|---|---|---|---|---|
| `hot.large_min` | 4 MiB (keep) | 4 MiB | C-6: 97.1% of objects are records, 93.5% of bytes whole files | measured |
| `cold.block_bytes` | 1 GiB (keep) | 1 GiB | C-6: one object of 400 to 700 MiB; no chains | measured |
| `destage.pack_window` | 256 | 64 | C-6: first-fit fill 93.2% at 256 against 77.6% at 64 | measured (full windows) |
| `gc.rewrite_below` | 0.30 (keep) | 0.30 | C-10: at most 1 stripe below 0.30 and 7 below 0.5 in a run | measured on synthetic deaths |
| `destage.partial_after` | NOW 3 d, LATER 7 d (keep) | the same | C-10: at most 7 partial stripes in a run | measured on synthetic deaths |
| capacity under the guard, logical | LATER 71.4 TB at `4+4`; NOW 40.8 TB at `4+2`, 30.6 TB at two copies | LATER 71.4 TB; NOW 34.6 TB and about 26 TB (with silo) | C-7 guard stripes | measured |
| `L_live` | 309 GB to 1.15 TB (about 1.13 TB [I]) | 1.34 TB [M, as `L_silo`] | C-6 | measured range |

## Inferences the measurements contradict

- **"99% of objects are under 64 KiB by count."** 90.7% are (C-6).
- **"About 85.9 k packs."** The listing holds 866,682 objects of the pack kind, 141,750 of them
  over 64 KiB (C-6).
- **"1.34 TB live."** 1.35 TB is `L_silo`, everything stored; at least 195 GB of it is in dead
  packs, and `L_live` is at most 1.15 TB (C-6).
- **"The capacity gap is unexplained."** Silo kept a parked Bazel cache of about 37 M objects and 2.10 TB
  outside its buckets; with it, the model accounts for 74% of what the drives used (C-6).
- **"NOW holds 34.6 TB logical at `4+2` under the guard."** That counted silo's 9.3 TB; without it
  the guard holds 40.8 TB (C-7), 38.0 TB if silo's parked cache stays on the drives (C-6).

## Decisions

These are the choices this report made where the plan left one, or where the plan could not be
followed.

- **No C-1.** Silo was removed from the farm; the storage tier has no silo to be held against.
- **The listing came from the drives' `xl.meta` files**, not from S3, which ran at about 14
  objects a second.
- **`L_live` is a range, not a figure.** The CAS service's live list was not reachable and silo
  was removed before every footer was read. The range is sound: the upper bound counts every
  unread pack live; the lower bound lets the unread bytes make dead the most they could, and an
  unread pack can only have been first for packs written after it.
- **A pack whose footer read failed counts as unread**, like one never read: the failures came
  from silo's removal, not from the packs.
- **The tree-list check is reported as not run.**
- **The deployment prefixes are numbered, not named:** the names are the operators' choice and
  carry nothing the measurement needs.
- **The gap was sampled, not walked:** a `du` of a silo drive did not finish in an hour, and the parked
  cache shares the drives with the store that replaced silo, so one shard of each tree was listed
  instead, read-only, and scaled by the number of shards.
- **C-7 counts no bytes but the tier's on the NOW drives**, as if silo's parked cache is removed
  before the tier's cold directories are made; C-6 gives the guard with it left in place.
- **C-7 and C-10 also model NOW before erasure coding** (`cold.replicas = 2`), the state NOW runs
  in until milestone S2 (21.1).
- **NOW's writer is the first node of the rank** (C-7, C-10's open segments), standing in for the
  rule of task T-S1.
- **New stripes with a node down are capped at `m` shards per node**, as the design's placement
  test reads it (C-7 names the contradiction).
- **C-10's deaths are synthetic** (`min_ttl` 7 days, `action_ttl` 30 days, and the packs C-6 found
  dead), as the plan allows until the CAS service's GC runs.
- **The requesters are inferred from counters.** No request log was kept.
