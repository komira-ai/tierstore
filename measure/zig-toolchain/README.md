# The Zig toolchain, written and read back through the build farm

A repeatable measurement of one tree: the Zig 0.12.0 toolchain, 15,189 files and 306,729,395 bytes in 988 directories (144 files are
empty, the largest is the 156,925,448-byte `zig` binary), as the writer's totals and the reader's report give them. It is written as the output directory of a build action, and read back as the
input of a later action, both as ordinary farm jobs. Both actions run remotely (`Commands: N (cached: N, remote: N, local: 0)`).

| target | what its action does |
|---|---|
| `:zig_tree` | the writer: copies the toolchain that buildrules unpacks from the pinned release archive (`buildrules//rules/pins:zig`) into its output directory `zig`, with `tree_copy` |
| `:zig_tree_read` | the reader: reads and hashes every file of `:zig_tree`'s output, its input, with `tree_read`, and outputs the report `zig_tree_read.json` |
| `:zig_tree_macos`, `:zig_tree_read_macos` | the same two actions on a farm Mac (below) |
| `:tree_copy_unit`, `:tree_read_unit`, `:coverage`, `:coverage_floor` | the programs' unit tests, and their line coverage held to `coverage_floor.txt` |

## Running it

Each run needs a new action key, or the action cache answers it and no job runs. The key is changed by a nonce in the action's
environment and nowhere else, so the command and the output stay the same (`defs.bzl`). The action cache is never bypassed.

```sh
buck2 build //measure/zig-toolchain:zig_tree -c ts_zig.write_nonce=<new value> --materializations=none
buck2 build //measure/zig-toolchain:zig_tree_read -c ts_zig.write_nonce=<the write's value> -c ts_zig.read_nonce=<new value> --show-output
```

`--materializations=none` keeps buck2 from downloading the 307 MB tree to the machine that runs it. The reader's report is the one
file it outputs.

| setting | effect |
|---|---|
| `ts_zig.write_nonce` | a new value runs the writer again |
| `ts_zig.salt` | gives every file new bytes (below); empty, the copy is byte for byte |
| `ts_zig.read_nonce` | a new value runs the reader again on the same tree |

### The salt

A byte-for-byte copy holds blobs the farm already has: every action that uses this Zig reads them, so a node's cache may hold them
before the tree is first read. With a salt, `tree_copy` replaces the last 16 bytes of each non-empty file (all of a shorter one) with
the first bytes of SHA-256(salt, the file's digest) (`saltTail` in `tree_copy.zig`). Paths, sizes, file permission bits and the empty
files are unchanged (directories are made with the default mode), and two files of equal bytes stay equal, so the tree has the same
shape and the same duplicates. Its non-empty blobs are on no node until a job of the measurement reads them, which makes a cold read checkable from the job records alone: the first read of a salted
tree on a host that is not the writer's host, and that no earlier read of that tree ran on.

### On a Mac

`:zig_tree_macos` and `:zig_tree_read_macos` run the programs, built for `aarch64-macos`, on the Mac execution platform, which
buildrules registers only where `[buildrules_re] macos_aarch64_properties` is set. They are built only when named.

## What the numbers are, and their clocks

| from | what | clock |
|---|---|---|
| the job service's record of the job | the host it ran on; when it was queued, placed, started and ended | the job service's |
| the ActionResult's execution metadata (REAPI `GetActionResult` of the action's digest) | the durations the task in the job measured: `input_fetch` (`fetch_ms`), `execution` (the command), `output_upload` (`pack_ms`: the outputs walked, hashed and written as one pack, in one pass). The instants are laid end to end back from the time the frontend read the pack, so only the durations are measurements | the node's monotonic clock (durations); the frontend's clock (the end) |
| buck2's event log (`buck2 log show`) | the action's wall time as the client saw it, and the writer's stderr (its totals) | the client's |
| the reader's report | the wall time it started; nanoseconds to list the tree's root and every directory; per pass, the time to the first file read whole and to the last; per pass, each file's time split into its open (`open_sum_ns`, from the call of `open` to its return) and its reads (`read_sum_ns`, the calls of `read` without the hashing), summed over the files, with the percentiles of the opens (`open_p50_ns`, `open_p90_ns`, `open_p99_ns`, `open_max_ns`) and of the whole files (`file_p50_ns`, `file_p99_ns`, `file_max_ns`), and the largest file's time (`largest_ns`); per pass, the node's pressure stall information (`pressure/cpu`, `pressure/io`, `pressure/memory`) just before and just after it (`pressure_start`, `pressure_end`), whose `total=` counters, in microseconds, say how long the node's tasks stalled on each during the pass; the tree's digest; the CPUs the program may run on (`cpus`); the node (`boot_id`, the host name, the load average, the mount the tree is on) | the node's |

The commit, the rename by which the job's put directory stores the pack, returns only once the object store has acknowledged it, and
the job ends after it. No record holds its duration, so it is the job's duration minus `fetch`, `execution` and `output_upload`.

The reports are one JSON object each, with `"version": 1`; a change to them adds fields and renames none, so recorded runs stay
comparable. A tree with an entry that is neither a file, a directory nor a symbolic link is refused by both programs.

The reader reads with 16 threads (`threads`), twice (`passes`): the second pass, in the same job on the same mount, reads with the
node's caches as the first left them.

## The programs

`tree_copy.zig` and `tree_read.zig` hold the programs. `tree_copy_main.zig` and `tree_read_main.zig` only hand the process's arguments
to them. The tests read no system path: `tree_read` takes `--proc <dir>`, and its tests give it a directory they make.
