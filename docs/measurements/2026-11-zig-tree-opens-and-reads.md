# The Zig tree read on the farm, opens and reads apart (komira-ai/tierstore#474)

komira-ai/tierstore#474 left two arms of `docs/measurements/2026-11-zig-tree-after-441-442.md` short of their targets:
- the first read of a new tree on the node that wrote it, 10-14 s, about what a cold read costs;
- a farm Mac's input fetch, which takes about 30 s more in about half the runs.

Neither the nodes' `/metricz` nor the Macs can be reached from the machine that runs the builds. So the reader now reports, per pass:
- the time of each file's open apart from its reads, summed over the files, with percentiles;
- the node's pressure stall information before and after the pass;
- the CPUs the job may use.

This page is the run of 2026-11-26, 17:08-18:42Z, with that reader. The release the nodes ran is not read here: no node or Mac was
asked anything. It is a release without the change of the pull request that adds this page. Every number is measured unless it
says inferred.

**Answer.**
- **Where a first pass's time goes.** On a node that does not hold the tree, it goes to the **opens**: 20-42 thread-seconds of
  them per first pass, against 2-8 of reads. The node is not short of CPU or I/O while it happens: its stall during the pass is 0.1-1.3 s
  of CPU and 0.0-0.7 s of I/O, on 88 CPUs.
  - The open is where `tierstore fs` asks the node for the file's span, and fills it when it is not held.
  - Most opens are short: median 0.12-0.59 ms.
  - A few wait seconds: the most is 2.2-7.0 s, the opens behind the fill of the 157 MB `zig` file and of other spans.
- **The writer's node, n=1** (tree L4, read 4, node C, 1-minute load 305).
  - The first pass took 10.48 s.
  - Its **reads** took 105.1 thread-seconds, against 2.2-8.0 on every other node, and the node stalled on I/O for 3.1 s during it.
  - So its bytes came from the disk, out of the pack the node holds whole, while every other node read spans it had just filled,
    still in the page cache. That is inferred from the read and I/O-stall figures.
  - Its opens took 24.5 thread-seconds. Every one of its spans was held, so that is time in the queue of the mount's fetch
    threads, which all the node's jobs share. This is the wait the pull request removes; it is inferred to be at most about 1.5 s of
    this pass, 24.5 thread-seconds over 16 threads.
- **Why komira-ai/tierstore#441 did not change this arm.** A farm job's input is a tree the mount provider places. Its files are
  read as spans of the pack (`span`), and that path already read a whole held pack without the store. The fill from a held pack
  that komira-ai/tierstore#441 added is in the fill of a blob of a tree of manifests, a path these trees do not take.
- **Farm Mac.** The slow mode is still there: 27.26 s and 55.46 s, against 6.06 s fast. Nothing here changes the Mac. What would
  settle it is unchanged: a sample on a Mac during a slow fetch, which cannot be taken from here.

## Linux, every read with a report

Classes as in the page before:
- "cold" is the first read of the tree on a node that is not its writer's.
- "warm" is a later read on a node that read it before.
- "writer's node" is the first read on the node that wrote it.
- "at once" marks two readers of one tree that ran side by side on one node: the second waits for the first one's fills.

Nodes are lettered A-F, the six Linux build nodes. Tree L1's writer ran on node E, L2's on node B, L3's on node C, and both L4's and
L5's on node C. "Opens" and "reads" are `open_sum_ns` and `read_sum_ns` of the report's first pass. The stall is the difference of
the `some ... total=` counters of `pressure_start` and `pressure_end`. Every report gives `cpus` 88.

| tree | run | class | node | load (1 min) | first pass (s) | opens, summed (thread-s) | reads, summed (thread-s) | open p50 / p99 / max (ms) | node stall cpu / io during it (s) | second pass (s) | its opens, summed | job |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| L1 | read 1 | cold | node D | 230.95 | 9.04 | 41.7 | 7.7 | 0.59 / 10.2 / 4398 | 0.7 / 0.1 | 1.71 | 2.7 | k689ac7f2-998666-0 |
| L3 | read 1 | cold | node D | 230.95 | 9.34 | 42.3 | 7.8 | 0.59 / 10.2 / 4339 | 0.8 / 0.1 | 1.79 | 3.0 | k689ac7f2-998668-0 |
| L1 | read 3 | warm | node D | 91.98 | 3.44 | 12.6 | 8.0 | 0.57 / 4.1 / 17 | 0.6 / 0.1 | 1.47 | 2.9 | k689ac7f2-999277-0 |
| L1 | read 4 | warm | node D | 83.20 | 2.67 | 8.1 | 4.1 | 0.38 / 2.8 / 11 | 0.2 / 0.0 | 2.12 | 4.1 | k689ac7f2-999330-0 |
| L3 | read 3 | warm | node D | 82.79 | 2.35 | 6.1 | 2.9 | 0.21 / 4.7 / 9 | 0.1 / 0.0 | 1.58 | 3.1 | k689ac7f2-998041-0 |
| L3 | read 4 | cold | node F | 102.51 | 6.86 | 35.6 | 7.9 | 0.49 / 16.9 / 2220 | 1.3 / 0.1 | 2.09 | 5.2 | k689ac7f2-998140-0 |
| L2 | read 2 | cold | node F | 98.21 | 6.69 | 19.6 | 5.8 | 0.47 / 12.1 / 3155 | 1.3 / 0.2 | 3.51 | 21.4 | k689ac7f2-998092-0 |
| L2 | read 3 | warm | node E | 269.81 | 1.88 | 2.6 | 2.3 | 0.15 / 0.8 / 5 | 0.1 / 0.0 | 1.78 | 2.0 | k689ac7f2-999512-0 |
| L4 | read 1 | cold | node A | 83.00 | 7.05 | 31.8 | 4.5 | 0.26 / 6.3 / 3599 | 0.2 / 0.3 | 1.95 | 1.6 | k689ac7f2-1006864-0 |
| L4 | read 2 | cold | node B | 107.33 | 10.63 | 29.7 | 2.2 | 0.16 / 12.9 / 7038 | 0.2 / 0.2 | 2.58 | 8.4 | k689ac7f2-1006865-0 |
| L4 | read 5 | warm | node B | 163.32 | 3.25 | 10.7 | 4.2 | 0.39 / 4.7 / 37 | 1.5 / 0.0 | 5.01 | 30.6 | k689ac7f2-1006185-0 |
| L4 | read 4 | writer's node | node C | 305.42 | 10.48 | 24.5 | 105.1 | 0.31 / 11.4 / 817 | 3.9 / 3.1 | 2.92 | 12.8 | k689ac7f2-1006198-0 |
| L4 | read 3 | cold | node D | 263.98 | 5.40 | 12.6 | 2.7 | 0.17 / 2.4 / 3164 | 0.1 / 0.2 | 2.11 | 9.3 | k689ac7f2-1006200-0 |
| L5 | read 5 | cold, with read 1 at once | node D | 102.30 | 10.15 | 36.5 | 2.2 | 0.12 / 0.3 / 6630 | 0.3 / 0.7 | 1.30 | 1.6 | k689ac7f2-1006863-0 |
| L5 | read 1 | cold, with read 5 at once | node D | 102.30 | 2.71 | 16.1 | 3.6 | 0.34 / 3.7 / 483 | 0.1 / 0.3 | 1.66 | 2.0 | k689ac7f2-1006888-0 |
| L5 | read 3 | cold, with read 4 at once | node A | 83.00 | 7.94 | 36.1 | 5.5 | 0.28 / 16.6 / 4077 | 0.2 / 0.4 | 2.74 | 10.2 | k689ac7f2-1006867-0 |
| L5 | read 4 | cold, with read 3 at once | node A | 83.00 | 7.90 | 35.8 | 5.8 | 0.30 / 15.1 / 4048 | 0.2 / 0.4 | 2.30 | 7.2 | k689ac7f2-1006866-0 |
| L5 | read 2 | warm | node A | 125.79 | 1.85 | 3.8 | 2.4 | 0.15 / 1.7 / 7 | 0.1 / 0.0 | 1.70 | 4.2 | k689ac7f2-1006181-0 |

| class | first pass, median [least-most], n |
|---|---|
| cold | 7.90 [2.71-10.63], n=11 |
| warm | 2.51 [1.85-3.44], n=6 |
| writer's node | 10.48, n=1 |

The 1-minute loads were 83-305. The cold median of the page before, 13.87 s, was taken at loads of 20-174, and the warm one there
at 108-174. These are not compared.

Not measured:
- **The writer's node, n=1.** Placement is not chosen. Of 18 reads with a report, one made its first read on its writer's node.
- **Runs without a report.** The connection to the farm broke three times: at 17:27Z (every build running), at about 17:41Z and
  at 18:22-18:24Z. These ended without a report: the three writers of a first attempt; L1 read 2, L2 reads 1 and 4, and L3 read 2; and all five
  readers of a sixth tree, L6. This machine also reached its limit of file watches with 18 buck2 daemons at once. Their nonces are in no table.

## Farm Mac

Input fetch is `input_materialization_duration_us` of the action in buck2's event log, the REAPI ActionResult's
`input_fetch_completed - input_fetch_start`. Macs are lettered as the job service names them; M1's writer ran on Mac B and M2's on
Mac C.

| tree | run | class | Mac | input fetch (s) | execution (s) | job |
|---|---|---|---|---|---|---|
| M1 | write | (writer) | Mac B | 9.24 | 2.15 | k689ac7f2-1002690-0 |
| M1 | read 1 | cold (second attempt; the first, on Mac B, was cut after 8 s) | Mac C | 27.26 | 1.90 | k689ac7f2-1003551-1 |
| M1 | read 3 | warm | Mac C | 6.06 | 1.93 | k689ac7f2-1003573-0 |
| M2 | write | (writer) | Mac C | 6.21 | 1.85 | k689ac7f2-1003574-0 |
| M2 | read 3 | writer's Mac | Mac C | 55.46 | 2.07 | k689ac7f2-1004461-0 |

M1 read 2 and M2 reads 1-2 ended in the farm's connection errors and have no figure.

## Commands

As in the page before, `local: 0` on every summary line that printed one:

```sh
buck2 build //measure/zig-toolchain:zig_tree -c ts_zig.write_nonce=<w> -c ts_zig.salt=<w> --materializations=none
buck2 build //measure/zig-toolchain:zig_tree_read -c ts_zig.write_nonce=<w> -c ts_zig.salt=<w> -c ts_zig.read_nonce=r<i>-<w> --show-output
```

- Trees L1-L3 were read four times in a row each, the three side by side.
- Trees L4-L6 were each read by five readers at once, each in its own buck2 daemon (`--isolation-dir`).
- M1 and M2 were read three times in a row with the `_macos` targets.
- Each job's node is from the job service's placement lines (`a job the history sized was placed job=<id> host=<node> class=<target>`),
  read-only. A writer was matched to its tree by the end of its job against the end of its build.
