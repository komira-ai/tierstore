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
```

`null-fs` serves the files of a list as zeros on the FUSE library of `tierstore fs` with its
transport settings (same `fuser` commit, worker threads on one descriptor, `max_background`
1024, `FUSE_PARALLEL_DIROPS`), so the transport is measured apart from what `tierstore fs` does
per request. It has no passthrough, `READDIRPLUS` or negative entries. It mounts when it starts,
prints `mounted`, and unmounts when its standard input closes.

The tests: `cargo test -p fsbench`. The tests that mount FUSE or read the host's `/proc` and
`/sys` are the integration targets `nullfs_integration` and `harness_integration`; they need
`/dev/fuse` and `fusermount3`.
