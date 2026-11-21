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

**In short.** At the settings `tierstore fs` ships with (16 threads), 0.3.121 makes 5,088 opens/s
against 4,137 for 0.3.120 (+23%; the least run of 0.3.121 is above the most of 0.3.120), at an open
p50 of 25.0 against 30.0 ms and a p99 of 45.9 against 60.0 ms, for the same server CPU per 1000
cycles (285 and 286 ms). The null file system, on the same library with the same timeouts and an
answered `OPEN`, makes 255,927 opens/s at an open p50 of 0.87 ms: the transport is not what costs
`tierstore fs` its 25 to 30 ms. All figures are medians of five runs; the tables below give the
least and the most.

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
| v1 | caches-on | 4137 (4021-4265) | 30.0 (29.0-31.1) | 44.8 (43.3-45.9) | 76.0 (75.0-82.7) | 30.0 (30.0-31.1) | 60.0 (58.5-61.6) | 154 (127-170) |
| v2 | caches-on | 5088 (4829-5276) | 24.0 (22.9-25.0) | 39.1 (37.0-42.2) | 85.5 (76.9-114) | 25.0 (24.2-26.3) | 45.9 (43.8-51.1) | 122 (99.1-128) |
| null | caches-off | 7299 (7034-8031) | 17.4 (15.7-18.0) | 28.2 (26.9-29.2) | 122 (117-175) | 18.0 (16.1-18.5) | 29.0 (27.7-30.0) | 158 (118-181) |
| null | caches-on | 255927 (221259-267845) | 0.01 (0.01-0.01) | 0.02 (0.02-0.04) | 28.1 (19.6-57.0) | 0.87 (0.82-0.99) | 2.70 (2.67-3.72) | 83.8 (74.5-101) |
| null | no-open | 1002042 (953562-1022046) | 0.01 (0.01-0.01) | 0.02 (0.02-0.03) | 95.3 (64.0-164) | 0.01 (0.01-0.01) | 0.03 (0.02-0.04) | 87.0 (60.0-107) |
| v1-t64 | caches-on | 3297 (3228-3408) | 33.2 (32.4-33.8) | 51.1 (49.5-52.2) | 93.1 (85.4-98.0) | 33.8 (33.8-34.9) | 190 (181-202) | 710 (641-766) |
| v2-t64 | caches-on | 3658 (3355-4106) | 30.3 (27.1-32.9) | 46.4 (41.2-50.1) | 87.2 (75.1-94.1) | 32.1 (30.0-33.8) | 150 (101-181) | 499 (363-654) |

| variant | regime | cold MB/s | warm MB/s | server CPU ms/1k | cgroup kernel ms/1k | node CPU ms/1k | D mean | D max | waiting mean | waiting max |
|---|---|---|---|---|---|---|---|---|---|---|
| v1 | caches-on | 6085 (4898-7761) | 6206 (5209-7871) | 286 (285-294) | 159 (157-164) | 21253 (20621-21865) | 0.21 (0.15-0.32) | 2.00 (1.00-2.00) | 282 (280-284) | 317 (311-334) |
| v2 | caches-on | 5066 (4727-7420) | 5416 (5006-7802) | 285 (283-292) | 161 (161-169) | 17289 (16661-18216) | 0.27 (0.24-0.37) | 2.00 (2.00-3.00) | 281 (280-282) | 316 (311-319) |
| null | caches-off | 1211 (1152-1271) | 4996 (4644-5513) | 2086 (1936-2144) | 1966 (1830-2027) | 12056 (10956-12508) | 0.11 (0.04-0.22) | 1.00 (1.00-2.00) | 253 (249-254) | 263 (262-264) |
| null | caches-on | 1276 (1213-1387) | 7211 (5801-7503) | 60.4 (57.8-66.6) | 57.6 (54.9-63.8) | 344 (328-398) | 0.17 (0.07-0.28) | 2.00 (1.00-2.00) | 417 (414-443) | 473 (465-476) |
| null | no-open | 1393 (1375-1407) | 7285 (5391-7936) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 87.8 (86.1-92.3) | 0.35 (0.16-0.56) | 2.00 (1.00-4.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) |
| v1-t64 | caches-on | 4899 (4109-5390) | 6020 (4595-6483) | 343 (336-348) | 166 (164-174) | 26680 (25799-27250) | 0.12 (0.07-0.16) | 1.00 (1.00-2.00) | 300 (296-302) | 336 (331-350) |
| v2-t64 | caches-on | 4597 (4571-5709) | 5335 (4638-6343) | 331 (322-347) | 165 (159-173) | 24050 (21423-26218) | 0.07 (0.04-0.16) | 1.00 (1.00-2.00) | 299 (297-302) | 331 (327-344) |

| variant | regime | runs kept | errors | load at start | file list | server executable | settings |
|---|---|---|---|---|---|---|---|
| v1 | caches-on | 5 of 6 | 0 | 214 (148-230) | a01bea3dceeb | e2be16acf886 | attr_timeout=32000000s, entry_timeout=32000000s, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=explicit, FOPEN_KEEP_CACHE; passthrough when offered, server=tierstore 0.3.120, threads=16 |
| v2 | caches-on | 5 of 6 | 0 | 215 (209-219) | a01bea3dceeb | 79f1fdaa8f82 | attr_timeout=32000000s, entry_timeout=32000000s, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=explicit, FOPEN_KEEP_CACHE; passthrough when offered, server=tierstore 0.3.121, threads=16 |
| null | caches-off | 5 of 6 | 0 | 260 (220-286) | a01bea3dceeb | 4bdbd5094dec | attr_timeout=0ms, entry_timeout=0ms, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=explicit, server=fsbench null-fs, threads=16 |
| null | caches-on | 5 of 6 | 0 | 275 (252-283) | a01bea3dceeb | 4bdbd5094dec | attr_timeout=32000000000ms, entry_timeout=32000000000ms, library=komira-ai/fuser 0d8f5da (fuser 0.18.0 + 2 commits), open=explicit, server=fsbench null-fs, threads=16 |
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

- **V1 against V2.** komira-ai/tierstore#343 takes away a lock that an open of a blob not yet
  pinned held across its round trip to the fill socket, and the shared lock an open of a pinned
  blob took. V2 is ahead at both thread counts: +23% opens/s at 16 threads, +11% at 64 (3,658
  against 3,297, open p99 150 against 190 ms). Both releases are slower at 64 threads than at
  16: more threads queue on the locks that remain (the backing table of passthrough, rewritten
  whole at each first open and last release, is one; see komira-ai/tierstore#343).
- **The earlier figures.** The synthetic measurement that found the stall gave 2,769 opens/s for
  0.3.119 and 28,199 for the fix, on a node that was running builds, through the node's own
  instance (with its whole backing table), at 64 threads and with a cycle of `open`, a 1-byte
  `read` and `close` (no `stat`). The 28,199 was of a build that also wrote the backing table
  from one thread outside the request locks; that change was taken out of
  komira-ai/tierstore#343 before it merged, so 0.3.121 is not that build. The series here are
  the baseline to compare with from now on.
- **Where the time goes.** With the same timeouts, a `stat` costs 24.0 ms (median) on
  `tierstore fs` 0.3.121 and 0.01 ms on the null file system, and an `open` 25.0 ms against
  0.87 ms; the
  requests waiting on the connection stay near 280, so about 256 readers are queued on the
  server at any moment. The null file system's `caches-off` series (7,299 opens/s, open p50
  18.0 ms, 2.1 s of server CPU per 1000 cycles) shows what a `LOOKUP`, a `GETATTR` and an `OPEN`
  per file cost through this library with 16 threads; `no-open` (1,002,042 opens/s, no server
  CPU) is the cost of a cycle that never reaches the server.
- **Reads.** `tierstore fs` reads whole blobs by passthrough here (kernel 7.0 offers it), so its
  cold reads (6,085 and 5,066 MB/s) are the kernel reading the node's cache files, and are not
  comparable with the null file system's (about 1,300 MB/s), whose every read reaches the server.
- **D state.** D state stays near zero in every series: under this load the readers wait for
  the server interruptibly (they show as `S`); the D counts of the loaded nodes are not
  reproduced by this load, which is one reason the measurement under a real build is still to do.
- **Load at start.** The one-minute load average when a run starts is the tail of the run before
  it (176 burners). No farm job ran on the node during any series kept here: the node's job
  agent was stopped before these series ran, and the runner counted no job process before or
  after each. A first collection of the four series `v1`, `v2`, `null caches-off` and
  `null caches-on`, which ran while the agent could still place jobs, was set aside and run
  again; it had put 0.3.120 and 0.3.121 level at 16 threads.

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

   The `null-fs` verb was removed from `fsbench` together with the `fuser` dependency; these
   commands run only with an `fsbench` built at the commit this report was made at, which still
   has it.

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
