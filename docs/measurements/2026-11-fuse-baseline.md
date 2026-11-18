# FUSE baseline, November 2026: the fuser fork as shipped, and with the pin fix

The first measurement of the FUSE benchmark (`crates/fsbench`), and the baseline that the
redesign of the serving path is held against. It measures `tierstore fs` as released twice,
on one build node, under the load the stall of the open path was found with: 256 processes
that open, read and close the files of a real tree over and over, beside 176 CPU burners.

- **V1**: tierstore 0.3.120, the `fuser` fork as shipped (`komira-ai/fuser` at `0d8f5da`,
  `fuser` 0.18.0 and two commits).
- **V2**: tierstore 0.3.121: the same fork and everything else the same, plus
  komira-ai/tierstore#343 (an open of a pinned blob takes no shared lock, and the pins of
  different opens run at once).
- **null**: `fsbench null-fs`, a file system that serves the same files as zeros on the same
  `fuser` commit with the same thread and mount settings, and does nothing else per request:
  the cost of the transport alone.

**In short.** At the settings `tierstore fs` ships with (16 threads), 0.3.120 and 0.3.121 are the
same within the spread of their runs: 5,147 and 4,780 opens/s, open p50 24.8 and 26.6 ms, p99
48.5 and 51.1 ms, 285 and 291 ms of server CPU per 1000 cycles. The null file system, on the same
library with the same timeouts and an answered `OPEN`, makes 238,428 opens/s at an open p50 of
0.94 ms: the transport is not what costs `tierstore fs` its 25 ms. All figures are medians of
five runs; the tables below give the least and the most.

## Setup

| | |
|---|---|
| Node | one build node of the farm: 2 x Xeon E5-4669 v4 (44 cores, 88 CPUs online), governor `schedutil`, kernel `7.0.0-42-generic`, `fuse` module defaults (`max_user_bgreq` 65535, `enable_uring` N) |
| Tree | the tree of a komira build action as the farm mounts it (a container root file system, and 18,122 files under the action's build output directory): 20,219 regular files, 410,896,895 bytes, listed once with `fsbench list` (list SHA-256 `a01bea3dceeb...`, in every record) |
| Load | 256 reader processes, each going round its 1/256 of the list; 176 CPU burners (two per CPU, as the earlier measurement had); 60 s per run |
| Cycle | `stat`, `open`, a `read` of the first 4096 bytes, `close`, each timed on its own (client side) |
| Whole-file reads | after the cycles and with no burner: the largest files of the list up to 256 MiB (141 files, 268,544,714 bytes; the last file taken passes the budget), by 16 readers, cold (each file's pages dropped with `posix_fadvise(DONTNEED)` first) and then warm |
| Samples | once a second: processes in D state, and the requests waiting on the mount's FUSE connection (`/sys/fs/fuse/connections/<n>/waiting`) |
| CPU | the server process's `utime` + `stime`, its control group's `system_usec` (kernel time), and the node's busy time, over the 60 s, per 1000 cycles |
| Runs | 6 per series; the first (which fills the node's cache and the kernel's) is left out, and each figure is the median of the other 5 with their least and most |

**The servers.** V1 and V2 ran as a second `tierstore fs` beside the node's own, with its own
mount point and runtime directory and the node's fill socket and cache, as the user `tierstore`
with `CAP_SYS_ADMIN` (as the node's unit runs it), with every other setting at its default:
16 threads reading one `/dev/fuse` descriptor, passthrough asked for (kernel 7.0 offers it, so
whole blobs are read by the kernel from the cache file), cached opens (`FOPEN_KEEP_CACHE`) for
spans of packs, and the timeouts of `tierstore fs`, which are not settings: the entries and
attributes of a tree are kept by the kernel for about a year (`TREE_TTL`), and every open is
sent (`OPEN`, `RELEASE`). The series `v1-t64` and `v2-t64` are the same with `threads = 64`,
the setting the earlier synthetic measurement of the stall ran with.

**The regimes.** `tierstore fs` has one, the one it ships (`caches-on`): long timeouts and an
explicit `OPEN`. The null file system ran in three:

| regime | entry and attribute timeouts | `OPEN` |
|---|---|---|
| `caches-off` | 0: every `stat` is a `LOOKUP` or `GETATTR` | answered, `FOPEN_KEEP_CACHE` |
| `caches-on` | 32,000,000 s, as `tierstore fs` | answered, `FOPEN_KEEP_CACHE` (the transport of V1 and V2) |
| `no-open` | 32,000,000 s | `ENOSYS`, so the kernel opens without asking (no `OPEN`, no `RELEASE`) |

## Results

The tables are `fsbench report` of the raw records, `crates/fsbench/data/2026-11-fuse-baseline.jsonl`
(a test holds this document to them: `cargo test -p fsbench --test baseline_doc`).

Median of the runs kept (least-most); the first 1 run(s) of each series left out.

| variant | regime | opens/s | stat p50 ms | stat p99 ms | stat max ms | open p50 ms | open p99 ms | open max ms |
|---|---|---|---|---|---|---|---|---|
| v1 | caches-on | 5147 (4990-5377) | 23.7 (22.7-24.2) | 39.1 (37.5-41.2) | 87.0 (86.1-90.7) | 24.8 (23.7-25.3) | 48.5 (44.3-51.1) | 143 (115-152) |
| v2 | caches-on | 4780 (4249-5801) | 25.6 (21.1-29.0) | 42.2 (32.9-46.9) | 150 (86.9-226) | 26.6 (22.2-30.0) | 51.1 (37.0-60.0) | 127 (113-149) |
| null | caches-off | 7650 (7552-7933) | 16.6 (15.9-16.9) | 25.6 (25.0-26.1) | 155 (126-191) | 17.2 (16.4-17.4) | 26.3 (25.6-26.9) | 147 (114-224) |
| null | caches-on | 238428 (216957-245839) | 0.01 (0.01-0.01) | 0.03 (0.03-0.04) | 26.1 (11.2-33.2) | 0.94 (0.86-1.00) | 3.19 (2.93-3.56) | 79.9 (55.8-88.4) |
| null | no-open | 1002042 (953562-1022046) | 0.01 (0.01-0.01) | 0.02 (0.02-0.03) | 95.3 (64.0-164) | 0.01 (0.01-0.01) | 0.03 (0.02-0.04) | 87.0 (60.0-107) |
| v1-t64 | caches-on | 3297 (3228-3408) | 33.2 (32.4-33.8) | 51.1 (49.5-52.2) | 93.1 (85.4-98.0) | 33.8 (33.8-34.9) | 190 (181-202) | 710 (641-766) |
| v2-t64 | caches-on | 3658 (3355-4106) | 30.3 (27.1-32.9) | 46.4 (41.2-50.1) | 87.2 (75.1-94.1) | 32.1 (30.0-33.8) | 150 (101-181) | 499 (363-654) |

| variant | regime | cold MB/s | warm MB/s | server CPU ms/1k | cgroup kernel ms/1k | node CPU ms/1k | D mean | D max | waiting mean | waiting max |
|---|---|---|---|---|---|---|---|---|---|---|
| v1 | caches-on | 5079 (4694-6865) | 5455 (5063-6887) | 285 (280-287) | 165 (161-166) | 17091 (16359-17612) | 0.04 (0.02-0.09) | 1.00 (1.00-2.00) | 281 (276-284) | 324 (317-337) |
| v2 | caches-on | 6110 (5304-6390) | 6187 (5529-6545) | 291 (283-296) | 165 (164-168) | 18398 (15157-20655) | 0.09 (0.02-6.55) | 1.00 (1.00-110) | 280 (273-284) | 316 (315-332) |
| null | caches-off | 1268 (1161-1278) | 4697 (4497-5196) | 2065 (2004-2105) | 1941 (1900-1988) | 11503 (11092-11653) | 0.06 (0.02-0.13) | 1.00 (1.00-2.00) | 254 (249-254) | 263 (262-265) |
| null | caches-on | 1274 (1247-1309) | 7369 (7189-7528) | 65.3 (63.5-66.0) | 62.4 (60.5-63.2) | 369 (358-405) | 0.04 (0.00-0.07) | 1.00 (0.00-2.00) | 438 (430-443) | 480 (471-487) |
| null | no-open | 1393 (1375-1407) | 7285 (5391-7936) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 87.8 (86.1-92.3) | 0.35 (0.16-0.56) | 2.00 (1.00-4.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) |
| v1-t64 | caches-on | 4899 (4109-5390) | 6020 (4595-6483) | 343 (336-348) | 166 (164-174) | 26680 (25799-27250) | 0.12 (0.07-0.16) | 1.00 (1.00-2.00) | 300 (296-302) | 336 (331-350) |
| v2-t64 | caches-on | 4597 (4571-5709) | 5335 (4638-6343) | 331 (322-347) | 165 (159-173) | 24050 (21423-26218) | 0.07 (0.04-0.16) | 1.00 (1.00-2.00) | 299 (297-302) | 331 (327-344) |

| variant | regime | runs kept | errors | load at start | file list | server executable | settings |
|---|---|---|---|---|---|---|---|
| v1 | caches-on | 5 of 6 | 0 | 202 (150-210) | a01bea3dceeb | e2be16acf886 | attr_timeout=32000000s, entry_timeout=32000000s, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=explicit, FOPEN_KEEP_CACHE; passthrough when offered, server=tierstore 0.3.120, threads=16 |
| v2 | caches-on | 5 of 6 | 0 | 218 (193-257) | a01bea3dceeb | 79f1fdaa8f82 | attr_timeout=32000000s, entry_timeout=32000000s, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=explicit, FOPEN_KEEP_CACHE; passthrough when offered, server=tierstore 0.3.121, threads=16 |
| null | caches-off | 5 of 6 | 0 | 228 (217-247) | a01bea3dceeb | 4bdbd5094dec | attr_timeout=0ms, entry_timeout=0ms, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=explicit, server=fsbench null-fs, threads=16 |
| null | caches-on | 5 of 6 | 0 | 260 (249-264) | a01bea3dceeb | 4bdbd5094dec | attr_timeout=32000000000ms, entry_timeout=32000000000ms, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=explicit, server=fsbench null-fs, threads=16 |
| null | no-open | 5 of 6 | 0 | 410 (361-430) | a01bea3dceeb | 4bdbd5094dec | attr_timeout=32000000000ms, entry_timeout=32000000000ms, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=none, server=fsbench null-fs, threads=16 |
| v1-t64 | caches-on | 5 of 6 | 0 | 229 (205-277) | a01bea3dceeb | e2be16acf886 | attr_timeout=32000000s, entry_timeout=32000000s, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=explicit, FOPEN_KEEP_CACHE; passthrough when offered, server=tierstore 0.3.120, threads=64 |
| v2-t64 | caches-on | 5 of 6 | 0 | 210 (208-218) | a01bea3dceeb | 79f1fdaa8f82 | attr_timeout=32000000s, entry_timeout=32000000s, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=explicit, FOPEN_KEEP_CACHE; passthrough when offered, server=tierstore 0.3.121, threads=64 |

| variant | regime | kernel | CPUs | governors | fuse parameters | mount | connection |
|---|---|---|---|---|---|---|---|
| v1 | caches-on | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=N, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | rw,nosuid,nodev,relatime shared:31 - fuse tierstore-fs rw,user_id=984,group_id=984,default_permissions,allow_other | congestion_threshold=768, max_background=1024 |
| v2 | caches-on | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=N, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | rw,nosuid,nodev,relatime shared:31 - fuse tierstore-fs rw,user_id=984,group_id=984,default_permissions,allow_other | congestion_threshold=768, max_background=1024 |
| null | caches-off | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=N, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:31 - fuse fsbench-null ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |
| null | caches-on | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=N, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:31 - fuse fsbench-null ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |
| null | no-open | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=N, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:31 - fuse fsbench-null ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |
| v1-t64 | caches-on | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=N, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | rw,nosuid,nodev,relatime shared:31 - fuse tierstore-fs rw,user_id=984,group_id=984,default_permissions,allow_other | congestion_threshold=768, max_background=1024 |
| v2-t64 | caches-on | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=N, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | rw,nosuid,nodev,relatime shared:31 - fuse tierstore-fs rw,user_id=984,group_id=984,default_permissions,allow_other | congestion_threshold=768, max_background=1024 |

### Reading

- **V1 against V2.** komira-ai/tierstore#343 takes away a lock that an open held across its
  round trip to the fill socket while it pinned a blob. Here every blob of the tree is pinned in
  the first run of a series (the one left out), so in the runs kept no open of V1 waits on that
  round trip, and the two releases measure the same at 16 threads. At 64 threads V2 is ahead
  (3,658 against 3,297 opens/s, open p99 150 against 190 ms), and both are below their own
  16-thread figures: more threads queue on the same locks (the backing table of passthrough,
  rewritten whole at each first open and last release, is one; see komira-ai/tierstore#343).
- **The earlier figures.** The synthetic measurement that found the stall gave 2,769 opens/s for
  0.3.119 and 28,199 for the fix, on a node that was running builds, through the node's own
  instance (with its whole backing table), at 64 threads and with a cycle of `open`, a 1-byte
  `read` and `close` (no `stat`). The 28,199 was of a build that also wrote the backing table
  from one thread outside the request locks; that change was taken out of
  komira-ai/tierstore#343 before it merged, so 0.3.121 is not that build. The series here are
  the baseline to compare with from now on.
- **Where the time goes.** With the same timeouts, a `stat` costs 23.7 ms (median) on
  `tierstore fs` and 0.01 ms on the null file system, and an `open` 24.8 ms against 0.94 ms; the
  requests waiting on the connection stay near 280, so about 256 readers are queued on the
  server at any moment. The null file system's `caches-off` series (7,650 opens/s, open p50
  17.2 ms, 2 s of server CPU per 1000 cycles) shows what a `LOOKUP`, a `GETATTR` and an `OPEN`
  per file cost through this library with 16 threads; `no-open` (1,002,042 opens/s, no server
  CPU) is the cost of a cycle that never reaches the server.
- **Reads.** `tierstore fs` reads whole blobs by passthrough here (kernel 7.0 offers it), so its
  cold reads (5,079 and 6,110 MB/s) are the kernel reading the node's cache files, and are not
  comparable with the null file system's (about 1,300 MB/s), whose every read reaches the server.
- **D state.** D state stays near zero in every series: under this load the readers wait for
  the server interruptibly (they show as `S`); the D counts of the loaded nodes are not
  reproduced by this load, which is one reason the measurement under a real build is still to do.
- **Load at start.** The one-minute load average when a run starts is the tail of the run before
  it (176 burners); no job ran on the node during any series (the runner counted the job
  processes before and after each).

## How to run it again

On a Linux node with `/dev/fuse`, as a user with `sudo`, from a build of this repository
(`cargo build --release -p fsbench`) and the two release tarballs
(`tierstore-0.3.120-linux-x86_64.tar.gz`, `tierstore-0.3.121-linux-x86_64.tar.gz`):

1. Write the `[fs]` table of the benchmark instance (the paths are examples; the fill socket
   and the cache are the node's own, as its `tierstore fs` has them):

   ```toml
   [fs]
   mountpoint = "/run/fsbench/ts"
   runtime_dir = "/run/fsbench/ts-run"
   client_group = "tierstore"
   owner = "0:0"
   fill_socket = "<the node's fs.fill_socket>"
   cache_path = "<the node's fs.cache_path>"
   # threads = 64      (the -t64 series only)
   ```

2. Start a release as that instance, in a unit of its own (its control group is measured):

   ```text
   sudo systemd-run --unit=fsbench-ts --collect -p User=tierstore -p Group=tierstore \
     -p AmbientCapabilities=CAP_SYS_ADMIN -p CapabilityBoundingSet=CAP_SYS_ADMIN \
     ./0.3.120/tierstore fs --config fsbench-ts.toml
   ```

3. List the tree once, then run a series against the unit's process:

   ```text
   sudo fsbench list --tree /run/fsbench/ts/trees/<tree digest> --out tree.list
   sudo fsbench run --list tree.list --tree /run/fsbench/ts/trees/<tree digest> \
     --variant v1 --regime caches-on --server-pid "$(systemctl show -p MainPID --value fsbench-ts)" \
     --out results.jsonl --setting "server=tierstore 0.3.120" --setting threads=16
   ```

   The defaults of `fsbench run` are the settings above (6 runs, 256 readers, 176 burners,
   60 s, 4096 bytes, 256 MiB read whole by 16 readers, a sample a second).

4. Stop the instance (`sudo systemctl stop fsbench-ts`) and run the next release the same way.

5. The null file system, in a scope of its own, served until its standard input closes:

   ```text
   sleep infinity | sudo systemd-run --scope --unit=fsbench-null \
     fsbench null-fs --list tree.list --mountpoint /run/fsbench/null \
     --entry-timeout-ms 0 --attr-timeout-ms 0 --open explicit --threads 16 &
   sudo fsbench run --list tree.list --tree /run/fsbench/null --variant null --regime caches-off \
     --server-pid <pid of fsbench null-fs> --out results.jsonl
   ```

   (`caches-on`: `--entry-timeout-ms 32000000000 --attr-timeout-ms 32000000000`; `no-open`:
   the same and `--open none`.)

6. `fsbench report --results results.jsonl` prints the tables above.

## What this does not measure yet

- D state and FUSE waiting under a real komira build (named targets, wall time and the
  fraction of it spent waiting on FUSE): the load here is synthetic.
- `tierstore fs` with its kernel caches off: its timeouts are constants, so that regime is
  measured on the null file system alone.
- Latency inside the server: neither the fork nor `tierstore fs` 0.3.121 keeps a histogram per
  request, so every latency here is the client's (the call, end to end).
- The variants of the redesign (`tsfuse` with cloned readers, splice replies, io_uring): they
  run in this harness, against these series, when they exist.
