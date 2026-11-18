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

RESULTS-SUMMARY

## Setup

| | |
|---|---|
| Node | one build node of the farm: 2 x Xeon E5-4669 v4 (44 cores, 88 CPUs online), governor `schedutil`, kernel `7.0.0-42-generic`, `fuse` module defaults (`max_user_bgreq` 65535, `enable_uring` N) |
| Tree | a komira build tree (a root file system and the komira source): 20,219 regular files, 410,896,895 bytes, listed once with `fsbench list` (list SHA-256 `a01bea3dceeb...`, in every record) |
| Load | 256 reader processes, each going round its 1/256 of the list; 176 CPU burners (two per CPU, as the earlier measurement had); 60 s per run |
| Cycle | `stat`, `open`, a `read` of the first 4096 bytes, `close`, each timed on its own (client side) |
| Whole-file reads | after the cycles and with no burner: the largest files of the list up to 256 MiB (151 files), by 16 readers, cold (each file's pages dropped with `posix_fadvise(DONTNEED)` first) and then warm |
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

RESULTS-TABLES

RESULTS-READING

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
