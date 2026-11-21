# FUSE results, November 2026: the result tables, a real build's D state, and the feature table

The last report of the FUSE benchmark of `docs/design/fetch-cache-serve.md` (17.1, row T-B4 of
section 18). It closes the benchmark with three things the earlier reports left open:

- **The result tables**: every variant measured so far (V1 to V5, and the null control of each
  day), in one table read from the raw records of the three measurements, beside the outcome of
  each decision rule of 17.1.
- **D state and FUSE waiting under a real komira build**: a build of named komira targets on the
  farm, cold (every action runs), while each build node is sampled once a second for the threads
  in D state, the threads waiting on FUSE and the requests waiting on the node's `tierstore fs`
  connection.
- **The feature table**: what each FUSE library offers for asynchronous dispatch, cloned
  readers, splice, io_uring, resend (takeover) and the copies of a READ reply, read from each
  library's source at a pinned commit.

The measurements it draws on: `2026-11-fuse-baseline.md` (V1, V2), `2026-11-fuse-v3-v4.md` (V3,
V4), `2026-11-fuse-v5-storm.md` (V5 and the storm) and `2026-11-uring-takeover.md`.

**In short.**

- Every decision rule of 17.1 the null-file-system series can settle is settled. Replies stay on
  `/dev/fuse` with cloned readers (4 on these nodes), and io_uring stays off by default. READ
  replies go by splice from 16 KiB, with `SPLICE_F_MOVE` off. DONTNEED stays open (below).
- A cold build of `//src/arrow:arrow` and `//src/bench:bench` ran on the farm: 341 commands, all
  remote, in 4,818 s. Over that build, 0.59% of the sampled job threads that were running, in D
  state or asleep in FUSE were waiting on FUSE, nearly all of them on a request (0.57%) and
  almost none on a page (0.02%). The waits come in bursts: in 99% of a node's ticks at most 4 to
  53 threads waited, and the worst tick had 1,513.
- Of the six libraries read, only `tsfuse` and `fuse-backend-rs` have cloned readers, takeover
  and FUSE over io_uring. Only `tsfuse` replies to a READ by splice.

## The result tables

`fsbench summary` of the run records of the three measurements
(`crates/fsbench/data/2026-11-fuse-baseline.jsonl`, `2026-11-fuse-v3-v4.jsonl` and
`2026-11-fuse-v5.jsonl`; a test holds this document to them: `cargo test -p fsbench --test
results_doc`). A row is one series of one measurement: the median of its runs after the first,
with their least and most. Each measurement ran its own same-day control, so a variant is read
against the control of its own day (the `null` rows, and in V5 the `v3-r8` and `v3-r22` rows);
the measurement documents hold the other columns (p50, max, kernel time, the samples) and how
each series was run.

- **The variants**: `v1` and `v2` are `tierstore fs` 0.3.120 and 0.3.121 (the `fuser` fork, 16
  threads; `-t64` at 64 threads); `null` is the null file system on that fork; `v3-rN` is `tsfuse`
  on `/dev/fuse` with N readers (the mount's descriptor and its clones), replies by `writev`;
  `v4-rN` the same with READ replies by splice from 16 KiB, `v4m` with `SPLICE_F_MOVE`;
  `v5-dN` is `tsfuse` over io_uring at depth N.
- **The regimes**: `caches-off` (the library regime: timeouts 0, every `OPEN` answered),
  `caches-on` (long timeouts, every `OPEN` answered) and `no-open` (the production regime: long
  timeouts, `OPEN` answered `ENOSYS`).
- **The CPU columns** are the server process's `utime + stime` per 1000 cycles, and per GB of
  the cold whole-file reads (`-` for the baseline, whose records predate that figure).

| measurement | variant | regime | opens/s | stat p99 ms | open p99 ms | cold MB/s | warm MB/s | server CPU ms/1k | cold server CPU ms/GB |
|---|---|---|---|---|---|---|---|---|---|
| 2026-11-fuse-baseline | v1 | caches-on | 4137 (4021-4265) | 44.8 (43.3-45.9) | 60.0 (58.5-61.6) | 6085 (4898-7761) | 6206 (5209-7871) | 286 (285-294) | - |
| 2026-11-fuse-baseline | v2 | caches-on | 5088 (4829-5276) | 39.1 (37.0-42.2) | 45.9 (43.8-51.1) | 5066 (4727-7420) | 5416 (5006-7802) | 285 (283-292) | - |
| 2026-11-fuse-baseline | null | caches-off | 7299 (7034-8031) | 28.2 (26.9-29.2) | 29.0 (27.7-30.0) | 1211 (1152-1271) | 4996 (4644-5513) | 2086 (1936-2144) | - |
| 2026-11-fuse-baseline | null | caches-on | 255927 (221259-267845) | 0.02 (0.02-0.04) | 2.70 (2.67-3.72) | 1276 (1213-1387) | 7211 (5801-7503) | 60.4 (57.8-66.6) | - |
| 2026-11-fuse-baseline | null | no-open | 1002042 (953562-1022046) | 0.02 (0.02-0.03) | 0.03 (0.02-0.04) | 1393 (1375-1407) | 7285 (5391-7936) | 0.00 (0.00-0.00) | - |
| 2026-11-fuse-baseline | v1-t64 | caches-on | 3297 (3228-3408) | 51.1 (49.5-52.2) | 190 (181-202) | 4899 (4109-5390) | 6020 (4595-6483) | 343 (336-348) | - |
| 2026-11-fuse-baseline | v2-t64 | caches-on | 3658 (3355-4106) | 46.4 (41.2-50.1) | 150 (101-181) | 4597 (4571-5709) | 5335 (4638-6343) | 331 (322-347) | - |
| 2026-11-fuse-v3-v4 | null | caches-off | 8002 (7485-8759) | 26.9 (26.3-30.5) | 27.7 (27.1-31.1) | 1351 (1335-1409) | 4994 (4509-5387) | 1972 (1793-2105) | 1122 (991-1224) |
| 2026-11-fuse-v3-v4 | v3-r1 | caches-off | 2278 (2193-2297) | 103 (73.9-154) | 105 (76.0-160) | 1536 (1413-1647) | 6673 (6524-7463) | 438 (434-455) | 577 (554-642) |
| 2026-11-fuse-v3-v4 | v3-r2 | caches-off | 3974 (3898-4089) | 50.1 (42.2-85.5) | 51.1 (42.7-86.5) | 1772 (1748-1948) | 6261 (5306-7343) | 493 (488-511) | 694 (680-735) |
| 2026-11-fuse-v3-v4 | v3-r4 | caches-off | 5972 (5709-6584) | 34.9 (34.9-37.5) | 35.9 (35.9-39.1) | 1765 (1523-1948) | 6587 (6434-6909) | 654 (601-667) | 880 (805-1173) |
| 2026-11-fuse-v3-v4 | v3-r8 | caches-off | 6066 (5733-6446) | 35.4 (31.1-40.6) | 36.4 (31.9-41.7) | 1786 (1648-1915) | 6499 (5268-7011) | 1173 (1153-1244) | 1331 (1196-1420) |
| 2026-11-fuse-v3-v4 | v3-r22 | caches-off | 3461 (3106-3564) | 109 (95.9-213) | 111 (98.0-217) | 1501 (1352-1574) | 5354 (5069-5842) | 4912 (4632-5344) | 5595 (5278-6098) |
| 2026-11-fuse-v3-v4 | v4-r1 | caches-off | 2228 (1948-2409) | 73.9 (71.8-253) | 76.0 (73.9-261) | 1638 (1611-1837) | 6599 (5923-6974) | 448 (414-512) | 465 (419-489) |
| 2026-11-fuse-v3-v4 | v4-r2 | caches-off | 3954 (3889-3997) | 45.9 (43.8-62.1) | 46.4 (44.8-64.2) | 1957 (1705-2065) | 6683 (6228-6827) | 502 (482-512) | 521 (493-600) |
| 2026-11-fuse-v3-v4 | v4-r4 | caches-off | 6029 (5940-6257) | 34.9 (34.9-37.0) | 35.9 (35.9-38.0) | 1777 (1711-1947) | 6470 (6322-6584) | 645 (631-669) | 801 (694-829) |
| 2026-11-fuse-v3-v4 | v4-r8 | caches-off | 6347 (6079-6457) | 33.2 (31.6-34.9) | 34.3 (32.6-35.9) | 1680 (1623-1855) | 6445 (5411-6830) | 1161 (1116-1175) | 1308 (1178-1341) |
| 2026-11-fuse-v3-v4 | v4-r22 | caches-off | 3577 (3545-3594) | 85.5 (82.3-86.5) | 87.6 (84.4-88.6) | 1501 (1447-1623) | 5482 (5349-5707) | 4620 (4568-4763) | 6005 (5884-6177) |
| 2026-11-fuse-v3-v4 | v4m-r8 | caches-off | 6103 (5940-6306) | 34.9 (32.9-37.0) | 35.9 (33.8-39.1) | 1624 (1376-1704) | 6267 (5938-6452) | 1131 (1102-1138) | 1327 (1215-1587) |
| 2026-11-fuse-v3-v4 | null | caches-on | 235858 (221695-238985) | 0.03 (0.02-0.03) | 3.78 (2.97-3.92) | 1487 (1436-1514) | 7769 (7413-7988) | 65.3 (65.0-69.9) | 628 (624-656) |
| 2026-11-fuse-v3-v4 | v3-r1 | caches-on | 80590 (70814-81984) | 0.02 (0.02-0.03) | 5.47 (4.75-7.90) | 1643 (1634-1808) | 6232 (5781-7906) | 12.4 (12.2-14.1) | 535 (484-545) |
| 2026-11-fuse-v3-v4 | v3-r2 | caches-on | 135049 (125134-140143) | 0.03 (0.02-0.04) | 3.88 (3.00-4.01) | 1845 (1693-2040) | 7728 (7544-8513) | 14.7 (14.2-15.9) | 610 (559-749) |
| 2026-11-fuse-v3-v4 | v3-r4 | caches-on | 195313 (185232-203259) | 0.03 (0.03-0.04) | 3.42 (2.77-3.92) | 1847 (1814-2043) | 8168 (7844-8364) | 20.2 (19.0-21.2) | 768 (661-801) |
| 2026-11-fuse-v3-v4 | v3-r8 | caches-on | 182668 (179616-186978) | 0.03 (0.02-0.04) | 3.98 (3.65-4.23) | 1764 (1647-1958) | 7517 (7379-8355) | 41.2 (40.9-41.4) | 1103 (987-1178) |
| 2026-11-fuse-v3-v4 | v3-r22 | caches-on | 108297 (104181-111673) | 0.03 (0.02-0.03) | 7.90 (7.83-8.59) | 1575 (1442-1685) | 7629 (7002-8421) | 157 (149-164) | 4054 (3915-4189) |
| 2026-11-fuse-v3-v4 | null | no-open | 1027885 (969901-1030738) | 0.02 (0.02-0.04) | 0.02 (0.02-0.02) | 1471 (1454-1491) | 7212 (7129-7592) | 0.00 (0.00-0.00) | 633 (619-642) |
| 2026-11-fuse-v3-v4 | v3-r1 | no-open | 1006992 (995126-1051923) | 0.02 (0.02-0.03) | 0.02 (0.02-0.04) | 1843 (1627-1875) | 7776 (7341-8467) | 0.00 (0.00-0.00) | 465 (461-521) |
| 2026-11-fuse-v3-v4 | v3-r2 | no-open | 1035665 (1026708-1038144) | 0.03 (0.03-0.04) | 0.03 (0.03-0.03) | 1827 (1768-2113) | 7252 (6769-8633) | 0.00 (0.00-0.00) | 600 (549-652) |
| 2026-11-fuse-v3-v4 | v3-r4 | no-open | 996677 (978446-1058219) | 0.03 (0.03-0.04) | 0.03 (0.03-0.04) | 1784 (1534-2134) | 7731 (7347-7900) | 0.00 (0.00-0.00) | 856 (680-1047) |
| 2026-11-fuse-v3-v4 | v3-r8 | no-open | 905063 (891223-943989) | 0.02 (0.02-0.04) | 0.02 (0.02-0.03) | 1938 (1777-1991) | 7684 (7001-8320) | 0.00 (0.00-0.00) | 968 (931-1066) |
| 2026-11-fuse-v3-v4 | v3-r22 | no-open | 939594 (922679-944605) | 0.02 (0.02-0.02) | 0.02 (0.02-0.04) | 1638 (1390-1755) | 7653 (7197-8154) | 0.00 (0.00-0.00) | 3849 (3179-3933) |
| 2026-11-fuse-v3-v4 | v4-r1 | no-open | 928367 (890413-937675) | 0.02 (0.02-0.03) | 0.02 (0.02-0.04) | 2037 (1765-2055) | 7704 (7656-8112) | 0.00 (0.00-0.00) | 358 (344-414) |
| 2026-11-fuse-v3-v4 | v4-r2 | no-open | 996383 (915666-1003252) | 0.02 (0.02-0.04) | 0.03 (0.02-0.04) | 2163 (1850-2197) | 7791 (7451-8255) | 0.00 (0.00-0.00) | 456 (433-531) |
| 2026-11-fuse-v3-v4 | v4-r4 | no-open | 980637 (944111-1030220) | 0.02 (0.02-0.02) | 0.02 (0.02-0.03) | 1896 (1740-2114) | 7568 (7266-8386) | 0.00 (0.00-0.00) | 703 (559-787) |
| 2026-11-fuse-v3-v4 | v4-r8 | no-open | 1006439 (987027-1064858) | 0.02 (0.02-0.03) | 0.02 (0.02-0.02) | 1739 (1597-2034) | 7702 (7095-8367) | 0.00 (0.00-0.00) | 968 (773-1057) |
| 2026-11-fuse-v3-v4 | v4-r22 | no-open | 942566 (931386-988781) | 0.02 (0.02-0.03) | 0.02 (0.02-0.04) | 1698 (1455-1789) | 7716 (6737-7950) | 0.00 (0.00-0.00) | 3929 (3249-4036) |
| 2026-11-fuse-v3-v4 | v4m-r8 | no-open | 984978 (970228-1044336) | 0.03 (0.02-0.04) | 0.02 (0.02-0.02) | 1570 (1560-1753) | 7467 (7044-7713) | 0.00 (0.00-0.00) | 1238 (1126-1317) |
| 2026-11-fuse-v5 | v5-d5 | caches-off | 7202 (7143-7467) | 65.3 (60.6-67.6) | 65.8 (60.0-68.7) | 951 (875-968) | 6266 (6098-6627) | 4002 (3796-4155) | 1983 (1964-2192) |
| 2026-11-fuse-v5 | v3-r8 | caches-off | 5870 (5755-6246) | 50.1 (46.9-52.7) | 51.6 (48.5-53.7) | 1738 (1570-1947) | 6614 (6480-7105) | 889 (876-966) | 1382 (1294-1545) |
| 2026-11-fuse-v5 | v5-d16 | caches-off | 7158 (7105-7257) | 59.5 (53.7-65.3) | 60.0 (54.3-66.3) | 920 (890-960) | 5665 (4809-7035) | 4066 (3744-4145) | 2043 (1964-2155) |
| 2026-11-fuse-v5 | v3-r22 | caches-off | 3627 (3530-3860) | 95.9 (79.2-97.0) | 97.0 (81.3-98.0) | 1560 (1486-1640) | 6142 (5625-6360) | 4108 (3452-4262) | 5669 (5525-5888) |
| 2026-11-fuse-v5 | v5-d5 | no-open | 986371 (953308-1002815) | 0.02 (0.02-0.03) | 0.02 (0.02-0.03) | 1002 (928-1007) | 7949 (7507-8362) | 0.00 (0.00-0.00) | 1569 (1499-1629) |
| 2026-11-fuse-v5 | v3-r8 | no-open | 1024324 (1020543-1031857) | 0.02 (0.02-0.02) | 0.02 (0.02-0.03) | 2156 (1726-2248) | 8093 (7718-8796) | 0.00 (0.00-0.00) | 903 (870-1117) |
| 2026-11-fuse-v5 | v5-d16 | no-open | 1012195 (930383-1016909) | 0.02 (0.02-0.02) | 0.02 (0.02-0.03) | 943 (934-1004) | 8320 (7355-8538) | 0.00 (0.00-0.00) | 1643 (1564-1652) |
| 2026-11-fuse-v5 | v3-r22 | no-open | 1061415 (982293-1073606) | 0.02 (0.02-0.03) | 0.02 (0.02-0.03) | 1706 (1586-1809) | 8144 (6920-8819) | 0.00 (0.00-0.00) | 3682 (3593-3789) |

**The decision rules of 17.1, written before the data, and their outcomes:**

| rule | outcome | measured in |
|---|---|---|
| io_uring is the default only if it improves CPU per 1k requests or p99 by at least 20% against `/dev/fuse` with clones, keeps cold and warm MB/s within 5%, survives the storm, fits 64 MiB of registered memory per mount and allows takeover | **not met: `/dev/fuse` stays the default.** Against `v3-r8` of its day, `v5-d5` has 4.5 times the server CPU per 1000 cycles (4,002 against 889 ms), a 30% worse `stat` p99, 45% lower cold MB/s; a storm of parked reads on one CPU raises that CPU's `stat` p99 3,786 times; depth 16 needs 176 MiB; takeover is not possible on io_uring | `2026-11-fuse-v5-storm.md`, `2026-11-uring-takeover.md` |
| Splice replaces `pread` + `writev` above a threshold only where it wins on CPU at equal MB/s | **kept, from 16 KiB:** 19 to 25% less server CPU per cold GB at 1 and 2 readers, 9 to 18% (within the runs' spread) at 4, none at 8, never more up to 8 readers | `2026-11-fuse-v3-v4.md` |
| `SPLICE_F_MOVE` | **off:** no CPU saved; 28% more CPU per GB in the production regime | `2026-11-fuse-v3-v4.md` |
| The number of cloned readers is the knee of opens/s against CPU | **4 on these nodes (88 CPUs):** opens/s +2% from 4 to 8 readers (caches off) for 79% more server CPU, and the best of any count with the caches on | `2026-11-fuse-v3-v4.md` |
| DONTNEED stays on only if warm MB/s does not fall and the page cache falls | **not decided here** (below, "What this does not measure") | none |

**What the table says beyond the rules.** The redesign's transport is cheaper per request than
the fork's, not faster at its knee: with the caches off, `tsfuse` at 4 readers makes 5,972
opens/s at 654 ms of server CPU per 1000 cycles, the fork's null file system 8,002 at 1,972 ms (16
threads, same day); with the caches on, 195,313 against 235,858 opens/s at 20.2 against 65.3 ms.
In the production regime no cycle reaches any server (0.00 ms per 1000 cycles in every series),
so the 0.9 to 1.06 million cycles/s of every variant are the node's. `tierstore fs` itself (V1,
V2) spends about 285 ms per 1000 cycles where its own library's null file system spends 60 ms in
the same regime (`caches-on`): the time of `tierstore fs` is in what it does per request, not in
the transport (as the baseline found).

## D state and FUSE waiting under a real komira build

One cold build of two komira targets, run on the farm while every build node was sampled once a
second (the steps are in "How to run it again"). The farm's nodes serve the shipped mount:
`tierstore fs` 0.3.147 on the `fuser` fork, the same release on every node. So this is the
shipped transport under a real build, not `tsfuse` ("What this does not measure").

- **The build**: `./buck2 build --no-remote-cache //src/arrow:arrow //src/bench:bench` at komira
  `356b7651e`, after `./buck2 clean`. 341 commands ran, every one remote and none cached or
  local. After 4,818 s one test action (`test_codec_libraries`) was killed with exit code 137,
  and the build ended with exit code 3. Every other action of the build ran. The sampled window
  is the build's whole execution.
- **The nodes**: five build nodes, `n1` to `n5`, each with 88 CPUs, on kernels
  7.0.0-42-generic and 7.0.0-43-generic. Each node has one `tierstore fs` connection. The nodes
  also ran other builds' jobs in the window (`n4` had 37 jobs on average before this build
  started), so a node's jobs are not only this build's.
- **What a tick counts**: the threads in D state, split into those waiting on FUSE (`fuse_d`)
  and the rest. Of the jobs' threads it counts those running, those in D state and those asleep
  in FUSE. A FUSE wait is by request (`request_wait_answer` in the thread's wait channel or
  stack), on a page (a `fuse_` function with `folio_wait`), or other (another `fuse_` function).
  `tierstore fs`'s own reads of `/dev/fuse` are not waits. `D fault` counts the threads in D
  state in `filemap_fault` with no FUSE function on the stack. `waiting` is the connection's
  count of requests waiting for the server. **FUSE wait %** is the share of the jobs' active
  thread samples (running, in D state, or asleep in FUSE) that were waiting on FUSE, and **D %**
  is the share in D state.
- **Ticks**: a tick starts one second after the last one ended. On a loaded node one tick takes
  longer, so `n4` has 3,074 ticks in the build's 4,818 s. The first sampler, a pilot that printed
  every thread of every job, made about 180 MB per node per run and is not kept, which is why the
  run is `run2`.

`fsbench dstate-report --index crates/fsbench/data/2026-11-real-build/runs.json` (a test holds
this document to the run's data: `cargo test -p fsbench --test results_doc`):

| run | build s | nodes | kernels | settings |
|---|---|---|---|---|
| run2 | 4818 | 5 | 7.0.0-42-generic, 7.0.0-43-generic | commands=341 (cached: 0, remote: 341, local: 0), komira=356b7651e, outcome=one test action killed (exit 137), build exit 3, server=tierstore fs 0.3.147, targets=//src/arrow:arrow //src/bench:bench |

| run | node | phase | ticks | load mean | jobs mean | job threads mean | D mean | D max | fuse_d mean | fuse_d p99 | fuse_d max | D fault max | waiting mean | waiting p99 | waiting max | FUSE wait % | D % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| run2 | n1 | before | 106 | 0.36 | 0.00 | 0.00 | 0.08 | 2.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | - | - |
| run2 | n1 | build | 3771 | 7.77 | 11.6 | 974 | 3.15 | 1185 | 1.74 | 14.0 | 908 | 210 | 2.32 | 10.0 | 2401 | 0.72 | 1.16 |
| run2 | n1 | after | 88 | 11.9 | 8.27 | 967 | 3.83 | 287 | 0.92 | 70.0 | 70.0 | 0.00 | 5.30 | 424 | 424 | 0.40 | 1.58 |
| run2 | n2 | before | 107 | 0.37 | 0.00 | 0.00 | 0.03 | 2.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | - | - |
| run2 | n2 | build | 3874 | 6.28 | 9.63 | 746 | 0.65 | 386 | 0.31 | 4.00 | 262 | 26.0 | 0.73 | 5.00 | 721 | 0.17 | 0.24 |
| run2 | n2 | after | 96 | 6.00 | 2.39 | 174 | 0.14 | 3.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.02 | 1.00 | 1.00 | 0.00 | 0.05 |
| run2 | n3 | before | 110 | 0.46 | 0.00 | 0.00 | 0.02 | 1.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | - | - |
| run2 | n3 | build | 3951 | 5.05 | 8.04 | 578 | 0.80 | 732 | 0.55 | 4.00 | 548 | 4.00 | 1.22 | 5.00 | 856 | 0.34 | 0.40 |
| run2 | n3 | after | 100 | 3.46 | 1.00 | 7.79 | 0.20 | 2.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.20 | 2.00 | 3.00 | 0.00 | 0.00 |
| run2 | n4 | before | 89 | 44.6 | 36.6 | 595 | 0.90 | 32.0 | 0.11 | 3.00 | 3.00 | 0.00 | 0.47 | 9.00 | 9.00 | 0.08 | 0.53 |
| run2 | n4 | build | 3074 | 31.3 | 23.2 | 1640 | 11.1 | 2037 | 3.31 | 53.0 | 1513 | 363 | 8.24 | 274 | 1287 | 0.78 | 2.11 |
| run2 | n4 | after | 83 | 36.9 | 9.76 | 278 | 0.80 | 12.0 | 0.17 | 5.00 | 5.00 | 0.00 | 0.39 | 3.00 | 3.00 | 0.47 | 0.70 |
| run2 | n5 | before | 105 | 0.75 | 0.00 | 0.00 | 0.08 | 2.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | - | - |
| run2 | n5 | build | 3820 | 5.83 | 7.49 | 572 | 1.85 | 1298 | 0.98 | 7.00 | 868 | 63.0 | 0.60 | 6.00 | 350 | 0.75 | 1.10 |
| run2 | n5 | after | 101 | 2.15 | 1.00 | 5.00 | 0.13 | 1.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 |

| run | job thread samples | FUSE wait % | request % | page % | other % | D % | fuse_d max | waiting max |
|---|---|---|---|---|---|---|---|---|
| run2 | 3907415 | 0.59 | 0.57 | 0.02 | 0.00 | 1.18 | 1513 | 2401 |

**Reading it.**

- **FUSE costs a cold build little of its threads' time on the shipped mount.** Over the build,
  0.59% of the jobs' active thread samples on the five nodes were waiting on FUSE: 0.57% on a
  request and 0.02% on a page. Per node the share is 0.17% to 0.78%. The jobs' threads spent
  1.18% in D state, so about half of their D state was FUSE.
- **The waits come in bursts, not as a steady queue.** In 99% of a node's build ticks, 4 to 53
  threads waited on FUSE. The most in one tick was 1,513 on `n4` and 908 on `n1`, and up to
  2,401 requests waited on one connection (`n1`). These bursts are what a server with more
  readers, or with parked reads, would shorten. The table cannot say how long each burst
  lasted, because a tick only sees the waits it falls in.
- **Page waits are rare**: 0.02% against 0.57% by request. Almost every wait is for an answer
  from `tierstore fs`, not for a page of a file already being read. The server's answer time
  is what matters, not the page cache's reads.
- **D state outside FUSE** is page faults of mapped files (`D fault`, up to 363 threads in one
  tick on `n4`) and other kernel waits.
- **Before the build**, four nodes ran no jobs. `n4` ran other builds' jobs, and 0.08% of their
  samples waited on FUSE. **After it**, `n1` and `n4` still ran jobs, at 0.40% and 0.47%. The
  other three ran almost none and show no FUSE waits.

## The feature table

What each FUSE library offers, read from its source at the commit named (17.1: "a static feature
table ... read from source at pinned commits; unread cells stay empty"). Each cell was read by one
reader and checked against the cited lines by a second, who looked for what the first missed;
where the second corrected the first, the table has the correction. No library was built or run
for this table.

| library, at | asynchronous dispatch | cloned readers (`FUSE_DEV_IOC_CLONE`) | splice | FUSE over io_uring | resend, takeover | user-space copies of a READ reply |
|---|---|---|---|---|---|---|
| `fuser`, the fork `tierstore fs` ships (`komira-ai/fuser` `0d8f5da`: 0.18.0 and two commits) | yes: a reply is `Send + 'static`, completed from any thread; `n_threads` loops read requests | opt-in (`clone_fd`, off by default: the threads then share one descriptor) | no (the INIT flags are defined, never used) | no | yes: `Notifier::resend` sends `FUSE_NOTIFY_RESEND`; `Session::from_fd_initialized` serves an initialized descriptor without INIT (the fork's two commits) | 0: `ReplyData::data(&[u8])` goes to `writev` as an iovec |
| `fuser` upstream (`cberner/fuser` `d2170c5`: 0.18.0) | as the fork | as the fork | no | no (`FUSE_OVER_IO_URING` is in the list of refused capabilities) | no (`FUSE_HAS_RESEND` refused; `from_fd` always runs INIT) | 0, as the fork |
| `fuse3` (`Sherlock-Holo/fuse3` `35d62e0`: 0.10.0) | yes: `async fn` handlers, one task per request (tokio or async-io) | no (one descriptor; its reads are serialized by a mutex) | no (the splice flags are echoed in INIT, splice is never called) | no | no | 0, but the reply is an owned `Bytes` |
| `rfuse3` (crate 0.0.9, `.crate` SHA-256 `f6edadac8a81...`) | yes: one task per request, worker tasks and a bound on requests in flight | no (one descriptor; reads serialized by a mutex) | no (echoes the splice flags in INIT, never calls splice) | no | no | 0, but the reply is an owned `Bytes` |
| `polyfuse` (`ubnt-intrepid/polyfuse` `0301238`) | no: a request borrows the buffer it was read into; no runtime, no detached reply | a primitive only (`Device::try_ioc_clone`, called nowhere) | requests read by splice into a pipe (on by default) and then copied out; replies not spliced | no | no | 0: borrowed bytes go to `writev` |
| `fuse-backend-rs` (`cloud-hypervisor/fuse-backend-rs` `2b2c2fd`; `/dev/fuse`, not virtio-fs) | with the `async-io` feature (off by default): several request futures per worker thread; otherwise a handler writes its reply before it returns | yes: a cloned descriptor per worker (the default non-blocking mode `dup`s one descriptor instead) | no | yes, experimental, behind the `uring` feature (off by default): REGISTER and COMMIT_AND_FETCH | yes: `notify_resend`, and `restore_init` resumes a connection without INIT | 0 on its file-descriptor path (`write_from` reads into the reply buffer); 1 through `Write::write` |
| `tsfuse` (this repository at `7d2a834b`) | yes: `Handled::Parked`; a `Send + 'static` `Replier` completed from any task, on the descriptor the request came from | yes: one reader thread per cloned descriptor | READ replies from 16 KiB by splice through pooled pipes (`SPLICE_F_MOVE` off by default); requests are not read by splice | yes: REGISTER and COMMIT_AND_FETCH, with the fallback to `/dev/fuse` (not the default: the rule above) | yes on `/dev/fuse`: a `Handover` keeps every descriptor and `Session::resume` serves without INIT, then sends `FUSE_NOTIFY_RESEND`; not on io_uring | 0 on `/dev/fuse` (spliced, or read into a pooled buffer and written by `writev`); 1 on io_uring (the payload is copied into the ring entry's buffer) |

**Where each cell was read** (paths in each library's tree at the commit above; line numbers at
that commit):

- `fuser` fork: dispatch `src/reply.rs:104`, `src/session.rs:316`; clone `src/channel.rs:115-126`,
  `src/session.rs:352-363`; splice `src/ll/flags/init_flags.rs:23-28`, `src/channel.rs:35`,
  `src/channel.rs:134`; io_uring `src/ll/flags/init_flags.rs:92`; resend `src/notify.rs:117`,
  `src/session.rs:243`, `src/session.rs:280`; copies `src/reply.rs:208-209`,
  `src/ll/reply.rs:59`, `src/channel.rs:134`.
- `fuser` upstream: dispatch `src/reply.rs:105`, `src/session.rs:281-301`; clone
  `src/channel.rs:72-80`, `src/session.rs:322-330`; splice `src/channel.rs:31`,
  `src/channel.rs:92`; io_uring `src/lib.rs:160-161`; resend `src/lib.rs:162-164`,
  `src/session.rs:248`; copies `src/reply.rs:209`, `src/ll/reply.rs:58`.
- `fuse3`: dispatch `src/raw/session.rs:731`, `src/raw/session.rs:4035`; clone
  `src/raw/connection/tokio.rs:218`, `src/raw/connection/tokio.rs:343`,
  `src/raw/connection/tokio.rs:519`; splice `src/raw/session.rs:1101-1118`; resend
  `src/raw/session.rs:787`; copies `src/raw/reply.rs:168-171`, `src/raw/session.rs:2155`,
  `src/raw/session.rs:122`.
- `rfuse3`: dispatch `src/raw/session/mod.rs:2635`, `src/raw/session/mod.rs:508`,
  `src/raw/session/mod.rs:1037`; clone `src/raw/connection/tokio.rs:295`,
  `src/raw/connection/tokio.rs:653`; splice `src/raw/session/mod.rs:1459-1476`; resend
  `src/raw/session/mod.rs:1127`; copies `src/raw/reply.rs:153`, `src/raw/session/mod.rs:2652-2694`,
  `src/raw/connection/tokio.rs:689-691`.
- `polyfuse`: dispatch `src/session.rs:167-171`; clone `src/device.rs:23-33`,
  `src/session.rs:116-120`; splice `src/session.rs:58`, `src/buf.rs:223-236`, `src/init.rs:322`;
  io_uring `src/init.rs:298`; resend `src/init.rs:292`, `src/connect.rs:103`; copies
  `src/reply.rs:59`, `src/bytes.rs:487-493`.
- `fuse-backend-rs`: dispatch `crates/fuse-backend-fusedev/src/linux_session.rs:1090`,
  `crates/fuse-backend-fusedev/src/linux_session.rs:1401`; clone
  `crates/fuse-backend-fusedev/src/linux_session.rs:185-191`,
  `crates/fuse-backend-fusedev/src/async_serving.rs:98`; splice
  `crates/fuse-backend-core/src/abi/fuse_abi_wire.rs:297-313`; io_uring
  `crates/fuse-backend-fusedev/src/uring_session.rs:909`,
  `crates/fuse-backend-fusedev/src/uring_session.rs:980`, `crates/fuse-backend-fusedev/Cargo.toml:89`;
  resend `crates/fuse-backend-core/src/api/server/sync_io.rs:179`,
  `crates/fuse-backend-core/src/api/server/mod.rs:141`; copies
  `crates/fuse-backend-fusedev/src/lib.rs:177`, `crates/fuse-backend-fusedev/src/lib.rs:381-386`.
- `tsfuse`: dispatch `crates/tsfuse/src/fs.rs:81-101`, `crates/tsfuse/src/reply.rs:50-91`;
  clone `crates/tsfuse/src/dev.rs:51-67`, `crates/tsfuse/src/session.rs:330-337`; splice
  `crates/tsfuse/src/reply_path.rs:386-408`, `crates/tsfuse/src/reply_path.rs:486-496`,
  `crates/tsfuse/src/dev.rs:92-94`; io_uring `crates/tsfuse/src/uring/sys.rs:318-325`,
  `crates/tsfuse/src/uring/serve.rs:106-108`; resend `crates/tsfuse/src/session.rs:270-285`,
  `crates/tsfuse/src/session.rs:350-365`; copies `crates/tsfuse/src/dev.rs:136-141`,
  `crates/tsfuse/src/uring/queue.rs:126-147`.

**Reading the table.** Of the libraries read, `tsfuse` and `fuse-backend-rs` are the two with
cloned readers, takeover and FUSE over io_uring; `fuse-backend-rs` keeps io_uring behind an
experimental feature that is off by default, and neither it nor any library but `tsfuse` replies
to a READ by splice from the file. `fuse3` and `rfuse3` dispatch asynchronously but read one
descriptor under a lock, and tell the kernel at INIT that they take splice, which they never
use. `polyfuse` is the one that reads requests by splice, and then copies them out of the pipe.
The 0 in the last column means no library copies the caller's bytes before `writev`; what differs
is whether the file system must have the bytes in memory first (every library but `tsfuse` and
`fuse-backend-rs`'s file-descriptor path) or must hand over an owned buffer (`fuse3`, `rfuse3`).

## How to run it again

**The result tables**: from a build of this repository (`cargo build --release -p fsbench`),

```text
fsbench summary --results crates/fsbench/data/2026-11-fuse-baseline.jsonl \
  --results crates/fsbench/data/2026-11-fuse-v3-v4.jsonl \
  --results crates/fsbench/data/2026-11-fuse-v5.jsonl
```

**The real-build run.** On the build nodes nothing is installed and nothing of this repository
runs: each run below was one ssh session per node, as a user with `sudo`, running this sampler
of standard tools as root (`sudo -n sh -s -- <ticks> 1`, the text on standard input, its output
to a file on the machine that starts the build). It reads `/proc` and `/sys` and writes nothing on
the node:

```text
n=$1; iv=$2; i=0
echo "@node $(uname -r) $(nproc)"
echo "#mounts"; grep -E ' - fuse(\.[^ ]*)? ' /proc/self/mountinfo | awk '{print $3, $(NF-1)}' | sort -u
while [ "$i" -lt "$n" ]; do
  echo "@tick $(date +%s%N)"
  echo "#load"; cat /proc/loadavg
  echo "#waiting"; grep -H . /sys/fs/fuse/connections/*/waiting
  echo "#ps"; ps -eLo tid=,stat=,wchan:40=,cgroup:400= | awk '{ o="-"; if ($4 ~ /\/actions\/js-/) { o="job"; jt++; c=$4; sub(/.*\/actions\//,"",c); sub(/\/.*/,"",c); seen[c]=1 } else if ($4 ~ /tierstore-fs/) o="srv"; s=substr($2,1,1); if (s=="D" || (s=="R" && o=="job") || $3=="request_wait_answer") print $1, $2, $3, o } END { n=0; for (k in seen) n++; print "#count"; print jt+0, n }'
  echo "#stack"; grep -H -s -E 'fuse_|request_wait_answer|folio_wait|filemap_fault' /proc/[0-9]*/task/[0-9]*/stack
  i=$((i+1)); sleep "$iv"
done
echo "@end $(date +%s%N)"
```

The `awk` keeps the rows the parse reads and counts the jobs' threads and jobs; it drops each
job's identifier and every mount point, and the sampler prints no host name, so the texts in
`data/` name no host and no job. The runs on the machine that drives the build, from a checkout
of komira with the farm as its remote executor:

1. `./buck2 clean`, then `./buck2 cquery 'deps(//src/arrow:arrow + //src/bench:bench)'` (the
   analysis, so that what follows is the build's execution).
2. The samplers start on every node; 120 s later the build starts, `./buck2 build
   --no-remote-cache --console simple //src/arrow:arrow //src/bench:bench` (a cold build: no
   action result is taken from the cache, so every action runs on the farm and reads its inputs
   through the nodes' `tierstore fs`), its start and end taken as Unix nanoseconds around it.
3. 120 s after the build ends the ssh sessions are closed; each sampler ends at its next write,
   with no `@end`, so its last tick is left out.
4. Each node's text is compressed (`zstd -19 --single-thread`) into `data/2026-11-real-build/`,
   and `runs.json` names the run, the build's start and end, its settings and the nodes. The
   repository's banned-name test scans binary files as text too; with the default multi-threaded
   framing one compressed file happened to hold a banned word, and the single-threaded framing
   does not.

`fsbench dstate-report --index crates/fsbench/data/2026-11-real-build/runs.json` prints the
tables above.

## What this does not measure

- **`tsfuse` under a real build.** The tree mount on `tsfuse` (`TreeFs`, T-F8) is built and
  tested, but no mount process serves it until the tree mounts switch from `fuser` (row T-R of
  the design), and a job of the farm can only read a mount its node's `tierstore fs` serves. The
  real-build run therefore measures the shipped mount; the same run once T-R has switched a
  node is the comparison, and T10b's D-state figures cover it. (T-R then moved the node's mount
  onto `tsfuse` with the core's read path; `TreeFs` is served once a mount process attaches to
  the node's shared cache, section 23 of the design.)
- **`tsfuse` on the real core in the harness of 17.1.** Running `TreeFs` beside the node's own
  mount in `fsbench run` needs a node drained of jobs (a mount by hand on a serving node is not
  done); no node was drained for this report. The null-file-system series (V3 to V5) are what
  the decision rules were taken on.
- **A per-request latency histogram under the build.** The shipped server keeps none; the
  sampler sees a wait only when a tick falls in it, so a wait shorter than the time between ticks
  is mostly missed (the shares above are of what the ticks saw).
- **Which build a job belongs to.** The node's jobs are every build the farm ran in the window,
  this one's and others'; the before and after phases are the node without this build.
- **The DONTNEED rule and the page cache that splice costs a pack file** (5.2, 17.1): both need
  the real core's pack files under `fsbench run`, and are T10b's.
- **V6** (fuse3/rfuse3 and fuse-backend-rs serving the same list): optional in 17.1, and not
  run; the feature table above is what 17.1 asks of the libraries that are not run.
