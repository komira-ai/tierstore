# fsbench

The benchmark of the FUSE layer of `tierstore fs`, and a null file system to hold it against.
Linux only. Results of a run on a build node are in
[`docs/measurements`](../../docs/measurements); their raw records are in [`data`](data).

## What a run does

A series is `--runs` runs of one variant (what serves the mount) in one regime (how the kernel
caches its entries). One run is:

1. `--readers` reader processes read the file list and say they are ready; `--burners`
   processes spin, one thread each, to keep the CPUs busy as a build does.
2. For `--secs` seconds every reader goes round its share of the list (every `readers`-th file),
   and for each file makes one *cycle*: `stat`, `open`, a `read` of the first `--read-bytes`
   bytes, `close`, each call timed on its own into a latency histogram.
3. Meanwhile, once per `--sample-ms`, the harness counts the processes in D state and reads the
   requests waiting on the mount's FUSE connection (`/sys/fs/fuse/connections/<n>/waiting`).
   The CPU time of the server process (`--server-pid`), of its control group and of the node is
   read before and after.
4. The burners stop, and the first `--throughput-readers` readers read the largest files of the
   list (until `--throughput-mib` is reached; the last file may pass it) whole: first cold (each file's cached pages dropped with
   `posix_fadvise(DONTNEED)` before it is read), then warm (read again at once).
5. One JSON record of the run is appended to `--out` (format 1, `src/record.rs`): the inputs,
   the settings declared with `--setting KEY=VALUE`, the environment (kernel, CPUs and governor,
   `fuse` module parameters, the mount's line of mountinfo, the load at the start), the
   histograms and the figures.

`fsbench report --results <file>` prints the tables of a measurement document: for each series
the median of its runs and their least and most, the first run (the warm-up) left out.

## Commands

```text
fsbench list --tree <dir> --out <list>          # the fixed input: the regular files of a tree
fsbench null-fs --list <list> --mountpoint <dir> [--entry-timeout-ms N] [--attr-timeout-ms N]
                [--open explicit|none] [--threads N] [--allow-other]
fsbench run --list <list> --tree <dir> --variant <label> --regime <label> --server-pid <pid>
            --out <results> [--runs 6] [--readers 256] [--burners 176] [--secs 60]
            [--max-cycles 0] [--read-bytes 4096] [--throughput-mib 256] [--throughput-readers 16]
            [--sample-ms 1000] [--as-user UID:GID] [--setting KEY=VALUE]...
fsbench report --results <results> [--discard 1]
fsbench storm --mountpoint <dir> --out <results> --label <label> [--runs 3]
              [--transport dev|uring] [--uring-depth 5] [--uring-memory-mib 64] [--readers 1]
              [--park-us 60000] [--read buffered|direct] [--per-cpu N] [--storm-cpu 0]
              [--other-cpu N] [--secs 20] [--max-probes 0] [--sample-ms 100]
              [--setting KEY=VALUE]...
fsbench storm-report --results <results> [--discard 0]
```

`tsfuse-null-fs --transport uring` serves over `tsfuse`'s io_uring transport (V5): a ring and a
thread per possible CPU with `--uring-depth` entries each, and a session that asks for 32-page
requests, as an io_uring tree mount does. Where the session cannot use io_uring
(`fuse.enable_uring` off, more memory than `--uring-memory-mib`) it serves on `/dev/fuse`; the
line it prints when it unmounts says which (`transport`) and what the ring entries registered
(`uring_registered_bytes`).

`storm` is the storm test of the io_uring transport (`docs/design/fetch-cache-serve.md`, 5.2
item 4): it mounts a file system of its own on `tsfuse` whose READs of *cold* files are parked
for `--park-us` and then answered, and runs three phases of `--secs` each: `idle`; `one-cpu`,
`--per-cpu` threads (by default `ceil(1.5 x --uring-depth)`) pinned to `--storm-cpu` that read
cold files at offsets never read before; and `every-cpu`, as many on each CPU this process may
run on. Two probe threads, on `--storm-cpu` and on `--other-cpu`, `stat` a file over and over
(nothing is cached, so each is a request), and each `stat`'s latency is kept. The ring entries
held, the READs parked and the requests waiting on the connection are sampled. One JSON record
per run is appended to `--out` (`src/stormrec.rs`); `storm-report` prints its tables.

`null-fs` serves the files of a list as zeros on the FUSE library of `tierstore fs` with its
transport settings (same `fuser` commit, worker threads on one descriptor, `max_background`
1024, `FUSE_PARALLEL_DIROPS`), so the transport is measured apart from what `tierstore fs` does
per request. It has no passthrough, `READDIRPLUS` or negative entries. It mounts when it starts,
prints `mounted`, and unmounts when its standard input closes.

The tests: `cargo test -p fsbench`. The tests that mount FUSE or read the host's `/proc` and
`/sys` are the integration targets `nullfs_integration`, `harness_integration`,
`tsnull_integration` and `storm_integration`; they need `/dev/fuse` and `fusermount3`. Their
io_uring cases serve on io_uring where `fuse.enable_uring` is on, and check the fallback where it
is off. `baseline_doc` and `v5_doc` hold the measurement documents to the raw records in
[`data`](data).
