# FUSE over io_uring (V5) and the storm test, November 2026

The io_uring measurement of the FUSE benchmark (`crates/fsbench`, `docs/design/fetch-cache-serve.md`
17.1), and the storm test of 5.2 item 4. It answers the 17.1 decision rule: does the `tsfuse` io_uring
transport become the default for a mount?

- **V5**: `fsbench tsfuse-null-fs --transport uring`, the `tsfuse` null file system served over
  io_uring, at the default depth (5 entries per queue) and at 16.
- **V3 controls**: the same server on `/dev/fuse` with 8 and with 22 readers (the mount's
  descriptor and its clones), run between the V5 series on the same node in the same hour.
- **The storm**: `fsbench storm`, `ceil(depth x 1.5)` cold reads parked by the file system,
  issued from one CPU and then from every CPU at once, while a probe on the stormed CPU and a
  probe on another CPU `stat` a file over and over.

**In short.** io_uring does not become the default; `/dev/fuse` stays the transport of every mount,
as `tsfuse` ships today. Against `/dev/fuse` with 8 readers, V5 at depth 5 makes more cycles (7,202
against 5,870 cycles/s with caches off) but has a worse p99 (`stat` 65.3 against 50.1 ms) and costs
4.5 times the server CPU per 1000 cycles (4,002 against 889 ms). It reads cold files at 951 against
1,738 MB/s (-45%; the 64 MiB rule holds it to 128 KiB requests). It does not survive the storm: 8
reads parked for 60 ms on one CPU raise that CPU's `stat` p99 from 48 us to 181 ms (3,786 times),
where `/dev/fuse` keeps it within 1.08 times. Depth 5 registers 55.1 MiB and fits the 64 MiB rule;
depth 16 registers 176 MiB and does not. Takeover is not possible on io_uring (T-F2). All figures
are medians; the tables give the least and the most.

## Setup

| | |
|---|---|
| Node | one build node of the farm: 2 x Xeon E5-2699 v4 (2 NUMA nodes, 88 CPUs online), governor `schedutil`, kernel `7.0.0-42-generic`, `fuse` module defaults except `enable_uring` (below); the connection's `max_background` 1024 and `congestion_threshold` 768 |
| `enable_uring` | `N` on every farm node. The runner set it to `Y` before the first series and back to `N` after the last (the kernel reads it at a mount's INIT, so only a server that asks for io_uring gets it; the records hold `Y`) |
| Server | `fsbench tsfuse-null-fs` built from this branch: the files of the list served as zeros from one data file, every `stat`, `open` and `read` answered by `tsfuse`, writev replies. V5 asks for io_uring with `max_pages` 32 (the largest that fits 64 MiB at depth 5, 5.2 item 4) and keeps the one `/dev/fuse` reader the hybrid transport needs; V3 reads `/dev/fuse` with `max_pages` 256 |
| Load | the harness and tree of the baseline (`2026-11-fuse-baseline.md`): the same list of 20,219 files (`a01bea3dceeb...`), 256 reader processes, 176 CPU burners, 60 s per run, then the largest files up to 256 MiB read cold and warm by 16 readers, 8 rounds each |
| Runs | 6 per V5 and V3 series, the first left out; 3 per storm series, none left out |
| Order | every storm series, then the V5 and V3 series interleaved: in each regime `v5-d5`, `v3-r8`, `v5-d16`, `v3-r22` |
| Jobs | the node's job agent ran (no farm node was drained), and the runner counted no job process at the start or the end of any series; the load average at a series' start is the tail of the burners of the series before it |

**The regimes** (17.1): `caches-off`, the library regime (entry and attribute timeouts 0, `OPEN`
answered); `no-open`, the production regime (timeouts of 32,000,000 s, `OPEN` answered `ENOSYS`, so
the kernel opens without asking).

**The storm.** `fsbench storm` mounts its own file system (`tsfuse`, on the transport asked) with a
4 KiB probe file and a cold file of 1 TiB for each storm thread. A cold READ is parked for the given time (60 ms, the
cold-tier first-byte target of section 1, or 10 ms, the hot-tier one) and then answered; `LOOKUP` and
`GETATTR` are answered at once, with timeouts of 0, so every `stat` of a probe reaches the server.
The storm threads are pinned, `ceil(depth x 1.5)` per stormed CPU (8 at depth 5, 24 at depth 16;
`uring-d16-p60-s8` runs 8 at depth 16), and each reads 4 KiB of its own cold file over and over,
each time 1 MiB past the last, so every `read` is a cold READ: buffered (with `POSIX_FADV_RANDOM`,
so the kernel does not read ahead) or direct (`FOPEN_DIRECT_IO`). The storm CPU is CPU 1 and the other CPU 87 (on the other NUMA node). Each
run has three phases of 20 s: `idle` (the probes alone), `one-cpu` (the storm on CPU 1) and
`every-cpu` (the storm on all 88 CPUs). A phase's probes start once every storm thread has had a
read answered. Every 100 ms the runner samples the ring entries holding a request
(`uring_entries_parked`, `held`), the reads parked in the file system (`parked`) and the
connection's `waiting`. The `/dev/fuse` storms use 8 readers.

## Results: V5 and its controls

The tables are `fsbench report` of the raw records, `crates/fsbench/data/2026-11-fuse-v5.jsonl`
(a test holds this document to them: `cargo test -p fsbench --test v5_doc`).

Median of the runs kept (least-most); the first 1 run(s) of each series left out.

| variant | regime | opens/s | stat p50 ms | stat p99 ms | stat max ms | open p50 ms | open p99 ms | open max ms |
|---|---|---|---|---|---|---|---|---|
| v5-d5 | caches-off | 7202 (7143-7467) | 14.7 (14.4-15.1) | 65.3 (60.6-67.6) | 363 (231-457) | 15.0 (14.6-15.4) | 65.8 (60.0-68.7) | 352 (228-437) |
| v3-r8 | caches-off | 5870 (5755-6246) | 20.8 (18.2-21.1) | 50.1 (46.9-52.7) | 254 (220-300) | 21.4 (18.7-21.6) | 51.6 (48.5-53.7) | 202 (182-323) |
| v5-d16 | caches-off | 7158 (7105-7257) | 15.3 (15.1-15.8) | 59.5 (53.7-65.3) | 230 (185-338) | 15.5 (15.4-16.1) | 60.0 (54.3-66.3) | 234 (199-289) |
| v3-r22 | caches-off | 3627 (3530-3860) | 31.1 (30.3-32.4) | 95.9 (79.2-97.0) | 255 (229-288) | 31.9 (31.3-33.2) | 97.0 (81.3-98.0) | 287 (212-445) |
| v5-d5 | no-open | 986371 (953308-1002815) | 0.01 (0.01-0.01) | 0.02 (0.02-0.03) | 151 (83.0-325) | 0.01 (0.01-0.01) | 0.02 (0.02-0.03) | 91.0 (73.0-151) |
| v3-r8 | no-open | 1024324 (1020543-1031857) | 0.01 (0.01-0.01) | 0.02 (0.02-0.02) | 128 (82.0-242) | 0.01 (0.01-0.01) | 0.02 (0.02-0.03) | 127 (85.0-261) |
| v5-d16 | no-open | 1012195 (930383-1016909) | 0.01 (0.01-0.01) | 0.02 (0.02-0.02) | 99.0 (66.0-144) | 0.01 (0.01-0.01) | 0.02 (0.02-0.03) | 146 (66.0-159) |
| v3-r22 | no-open | 1061415 (982293-1073606) | 0.01 (0.01-0.01) | 0.02 (0.02-0.03) | 101 (96.0-129) | 0.01 (0.01-0.01) | 0.02 (0.02-0.03) | 94.0 (73.0-116) |

| variant | regime | cold MB/s | warm MB/s | server CPU ms/1k | cgroup kernel ms/1k | node CPU ms/1k | D mean | D max | waiting mean | waiting max |
|---|---|---|---|---|---|---|---|---|---|---|
| v5-d5 | caches-off | 951 (875-968) | 6266 (6098-6627) | 4002 (3796-4155) | 2685 (2515-2782) | 12188 (11747-12275) | 0.04 (0.00-0.28) | 2.00 (0.00-2.00) | 251 (246-252) | 262 (260-263) |
| v3-r8 | caches-off | 1738 (1570-1947) | 6614 (6480-7105) | 889 (876-966) | 673 (643-738) | 14984 (14078-15280) | 0.12 (0.04-0.25) | 2.00 (1.00-3.00) | 253 (249-254) | 265 (261-268) |
| v5-d16 | caches-off | 920 (890-960) | 5665 (4809-7035) | 4066 (3744-4145) | 2697 (2411-2813) | 12257 (12094-12356) | 0.13 (0.02-0.15) | 2.00 (1.00-2.00) | 247 (246-251) | 262 (261-263) |
| v3-r22 | caches-off | 1560 (1486-1640) | 6142 (5625-6360) | 4108 (3452-4262) | 3824 (3161-3973) | 24234 (22781-24906) | 0.04 (0.00-0.11) | 1.00 (0.00-2.00) | 253 (252-253) | 261 (260-264) |
| v5-d5 | no-open | 1002 (928-1007) | 7949 (7507-8362) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 89.2 (87.7-92.3) | 0.06 (0.06-0.08) | 1.00 (1.00-2.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) |
| v3-r8 | no-open | 2156 (1726-2248) | 8093 (7718-8796) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 85.9 (85.3-86.2) | 0.06 (0.02-0.08) | 1.00 (1.00-2.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) |
| v5-d16 | no-open | 943 (934-1004) | 8320 (7355-8538) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 86.9 (86.5-94.6) | 0.08 (0.02-0.19) | 1.00 (1.00-2.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) |
| v3-r22 | no-open | 1706 (1586-1809) | 8144 (6920-8819) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 82.9 (81.9-89.6) | 0.06 (0.04-0.16) | 1.00 (1.00-2.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) |

| variant | regime | cold MB/s | cold server CPU ms/GB | cold cgroup kernel ms/GB | warm MB/s | warm server CPU ms/GB | warm cgroup kernel ms/GB | rounds |
|---|---|---|---|---|---|---|---|---|
| v5-d5 | caches-off | 951 (875-968) | 1983 (1964-2192) | 0.50 (0.00-94.8) | 6266 (6098-6627) | 414 (405-419) | 145 (141-167) | 8.00 (8.00-8.00) |
| v3-r8 | caches-off | 1738 (1570-1947) | 1382 (1294-1545) | 1002 (956-1112) | 6614 (6480-7105) | 470 (447-475) | 321 (316-342) | 8.00 (8.00-8.00) |
| v5-d16 | caches-off | 920 (890-960) | 2043 (1964-2155) | 33.8 (11.2-111) | 5665 (4809-7035) | 396 (386-405) | 167 (150-170) | 8.00 (8.00-8.00) |
| v3-r22 | caches-off | 1560 (1486-1640) | 5669 (5525-5888) | 4969 (4796-5221) | 6142 (5625-6360) | 2500 (2416-2597) | 2342 (2242-2407) | 8.00 (8.00-8.00) |
| v5-d5 | no-open | 1002 (928-1007) | 1569 (1499-1629) | 577 (542-599) | 7949 (7507-8362) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 8.00 (8.00-8.00) |
| v3-r8 | no-open | 2156 (1726-2248) | 903 (870-1117) | 696 (684-844) | 8093 (7718-8796) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 8.00 (8.00-8.00) |
| v5-d16 | no-open | 943 (934-1004) | 1643 (1564-1652) | 624 (540-640) | 8320 (7355-8538) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 8.00 (8.00-8.00) |
| v3-r22 | no-open | 1706 (1586-1809) | 3682 (3593-3789) | 3124 (3072-3166) | 8144 (6920-8819) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 8.00 (8.00-8.00) |

| variant | regime | runs kept | errors | load at start | file list | server executable | settings |
|---|---|---|---|---|---|---|---|
| v5-d5 | caches-off | 5 of 6 | 0 | 293 (267-306) | a01bea3dceeb | fb1eecb7a724 | attr_timeout=0ms, entry_timeout=0ms, library=tsfuse (tierstore b20d4730), max_pages=32, open=explicit, readers=1, reply=writev, server=fsbench tsfuse-null-fs, transport=uring, uring_depth=5 |
| v3-r8 | caches-off | 5 of 6 | 0 | 267 (259-284) | a01bea3dceeb | fb1eecb7a724 | attr_timeout=0ms, entry_timeout=0ms, library=tsfuse (tierstore b20d4730), max_pages=256, open=explicit, readers=8, reply=writev, server=fsbench tsfuse-null-fs, transport=dev |
| v5-d16 | caches-off | 5 of 6 | 0 | 280 (269-290) | a01bea3dceeb | fb1eecb7a724 | attr_timeout=0ms, entry_timeout=0ms, library=tsfuse (tierstore b20d4730), max_pages=32, open=explicit, readers=1, reply=writev, server=fsbench tsfuse-null-fs, transport=uring, uring_depth=16 |
| v3-r22 | caches-off | 5 of 6 | 0 | 294 (286-330) | a01bea3dceeb | fb1eecb7a724 | attr_timeout=0ms, entry_timeout=0ms, library=tsfuse (tierstore b20d4730), max_pages=256, open=explicit, readers=22, reply=writev, server=fsbench tsfuse-null-fs, transport=dev |
| v5-d5 | no-open | 5 of 6 | 0 | 378 (349-380) | a01bea3dceeb | fb1eecb7a724 | attr_timeout=32000000000ms, entry_timeout=32000000000ms, library=tsfuse (tierstore b20d4730), max_pages=32, open=none, readers=1, reply=writev, server=fsbench tsfuse-null-fs, transport=uring, uring_depth=5 |
| v3-r8 | no-open | 5 of 6 | 0 | 414 (386-419) | a01bea3dceeb | fb1eecb7a724 | attr_timeout=32000000000ms, entry_timeout=32000000000ms, library=tsfuse (tierstore b20d4730), max_pages=256, open=none, readers=8, reply=writev, server=fsbench tsfuse-null-fs, transport=dev |
| v5-d16 | no-open | 5 of 6 | 0 | 382 (381-389) | a01bea3dceeb | fb1eecb7a724 | attr_timeout=32000000000ms, entry_timeout=32000000000ms, library=tsfuse (tierstore b20d4730), max_pages=32, open=none, readers=1, reply=writev, server=fsbench tsfuse-null-fs, transport=uring, uring_depth=16 |
| v3-r22 | no-open | 5 of 6 | 0 | 414 (384-415) | a01bea3dceeb | fb1eecb7a724 | attr_timeout=32000000000ms, entry_timeout=32000000000ms, library=tsfuse (tierstore b20d4730), max_pages=256, open=none, readers=22, reply=writev, server=fsbench tsfuse-null-fs, transport=dev |

| variant | regime | kernel | CPUs | governors | fuse parameters | mount | connection |
|---|---|---|---|---|---|---|---|
| v5-d5 | caches-off | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=Y, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:32 - fuse.fsbench-tsfuse fsbench-tsfuse ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |
| v3-r8 | caches-off | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=Y, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:32 - fuse.fsbench-tsfuse fsbench-tsfuse ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |
| v5-d16 | caches-off | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=Y, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:32 - fuse.fsbench-tsfuse fsbench-tsfuse ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |
| v3-r22 | caches-off | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=Y, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:32 - fuse.fsbench-tsfuse fsbench-tsfuse ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |
| v5-d5 | no-open | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=Y, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:32 - fuse.fsbench-tsfuse fsbench-tsfuse ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |
| v3-r8 | no-open | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=Y, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:32 - fuse.fsbench-tsfuse fsbench-tsfuse ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |
| v5-d16 | no-open | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=Y, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:32 - fuse.fsbench-tsfuse fsbench-tsfuse ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |
| v3-r22 | no-open | 7.0.0-42-generic | 88 | schedutil x88 | allow_sys_admin_access=N, enable_uring=Y, inval_wq=0, max_user_bgreq=65535, max_user_congthresh=65535 | ro,nosuid,nodev,relatime shared:32 - fuse.fsbench-tsfuse fsbench-tsfuse ro,user_id=0,group_id=0,default_permissions | congestion_threshold=768, max_background=1024 |

## Results: the storm

The tables are `fsbench storm-report` of the raw records,
`crates/fsbench/data/2026-11-fuse-storm.jsonl` (held by the same test). `p99 / idle p99` is a probe's
p99 over the p99 of the same probe in the run's idle phase; `held` is `uring_entries_parked`.

| label | transport | depth | registered MiB | park ms | read | threads per CPU | readers | runs kept | kernel | enable_uring |
|---|---|---|---|---|---|---|---|---|---|---|
| uring-d5-p60 | io_uring | 5 | 55.1 | 60.0 | buffered | 8 | 1 | 3 of 3 | 7.0.0-42-generic | Y |
| dev-r8-p60 | dev-fuse (NotAsked) | - | - | 60.0 | buffered | 8 | 8 | 3 of 3 | 7.0.0-42-generic | Y |
| uring-d5-p10 | io_uring | 5 | 55.1 | 10.0 | buffered | 8 | 1 | 3 of 3 | 7.0.0-42-generic | Y |
| dev-r8-p10 | dev-fuse (NotAsked) | - | - | 10.0 | buffered | 8 | 8 | 3 of 3 | 7.0.0-42-generic | Y |
| uring-d5-p60-direct | io_uring | 5 | 55.1 | 60.0 | direct | 8 | 1 | 3 of 3 | 7.0.0-42-generic | Y |
| uring-d16-p60 | io_uring | 16 | 176 | 60.0 | buffered | 24 | 1 | 3 of 3 | 7.0.0-42-generic | Y |
| uring-d16-p60-s8 | io_uring | 16 | 176 | 60.0 | buffered | 8 | 1 | 3 of 3 | 7.0.0-42-generic | Y |

| label | phase | probe | stormed | stats/s | stat p50 us | stat p99 us | stat max ms | p99 / idle p99 | stat errors | storm reads/s | storm errors | held mean | held max | parked max | waiting max |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| uring-d5-p60 | idle | storm-cpu | no | 40439 (39488-40722) | 23.7 (23.4-23.7) | 47.9 (46.3-55.0) | 3.46 (2.39-4.04) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.38 (0.35-0.40) | 2.00 (2.00-2.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| uring-d5-p60 | idle | other-cpu | no | 41227 (40934-41920) | 23.4 (22.9-23.7) | 40.2 (40.2-43.8) | 1.02 (0.24-1.60) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.38 (0.35-0.40) | 2.00 (2.00-2.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| uring-d5-p60 | one-cpu | storm-cpu | yes | 5.51 (5.51-5.51) | 181255 (181237-181404) | 181255 (181237-181404) | 181 (181-182) | 3786 (3296-3911) | 0.00 (0.00-0.00) | 82.8 (82.7-82.8) | 0.00 (0.00-0.00) | 5.05 (5.01-5.06) | 6.00 (6.00-6.00) | 5.00 (5.00-5.00) | 10.0 (10.0-10.0) |
| uring-d5-p60 | one-cpu | other-cpu | no | 60651 (60545-60836) | 16.2 (16.1-16.2) | 20.6 (20.6-20.6) | 0.55 (0.53-1.11) | 0.51 (0.47-0.51) | 0.00 (0.00-0.00) | 82.8 (82.7-82.8) | 0.00 (0.00-0.00) | 5.05 (5.01-5.06) | 6.00 (6.00-6.00) | 5.00 (5.00-5.00) | 10.0 (10.0-10.0) |
| uring-d5-p60 | every-cpu | storm-cpu | yes | 5.51 (5.51-5.51) | 181308 (181251-181377) | 181308 (181251-181377) | 181 (181-181) | 3789 (3293-3913) | 0.00 (0.00-0.00) | 7292 (7289-7292) | 0.00 (0.00-0.00) | 439 (439-439) | 440 (440-440) | 440 (440-440) | 706 (706-706) |
| uring-d5-p60 | every-cpu | other-cpu | yes | 5.51 (5.51-5.51) | 181390 (181270-181399) | 181390 (181270-181399) | 181 (181-181) | 4510 (4144-4513) | 0.00 (0.00-0.00) | 7292 (7289-7292) | 0.00 (0.00-0.00) | 439 (439-439) | 440 (440-440) | 440 (440-440) | 706 (706-706) |
| dev-r8-p60 | idle | storm-cpu | no | 19252 (14553-20072) | 52.0 (48.9-67.1) | 74.2 (73.2-101) | 1.99 (1.40-2.60) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| dev-r8-p60 | idle | other-cpu | no | 18960 (14895-19212) | 52.5 (50.4-66.0) | 75.3 (74.2-97.8) | 2.09 (0.42-4.05) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| dev-r8-p60 | one-cpu | storm-cpu | yes | 18500 (17332-19538) | 54.0 (49.4-55.6) | 81.4 (80.4-93.7) | 0.24 (0.23-1.01) | 1.08 (0.93-1.11) | 0.00 (0.00-0.00) | 133 (133-133) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 8.00 (8.00-8.00) | 10.0 (10.0-10.0) |
| dev-r8-p60 | one-cpu | other-cpu | no | 18417 (15809-18998) | 54.0 (50.9-60.7) | 80.4 (77.3-96.8) | 2.98 (0.17-3.02) | 1.04 (0.99-1.07) | 0.00 (0.00-0.00) | 133 (133-133) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 8.00 (8.00-8.00) | 10.0 (10.0-10.0) |
| dev-r8-p60 | every-cpu | storm-cpu | yes | 17894 (16919-18449) | 55.0 (51.5-55.0) | 90.6 (88.6-106) | 1.40 (0.25-2.74) | 1.21 (1.05-1.22) | 0.00 (0.00-0.00) | 11699 (11693-11706) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 704 (704-704) | 706 (706-706) |
| dev-r8-p60 | every-cpu | other-cpu | yes | 17688 (16809-18062) | 55.0 (52.5-55.6) | 92.7 (90.6-107) | 2.16 (0.32-3.99) | 1.20 (1.09-1.25) | 0.00 (0.00-0.00) | 11699 (11693-11706) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 704 (704-704) | 706 (706-706) |
| uring-d5-p10 | idle | storm-cpu | no | 40462 (39916-41251) | 23.7 (23.2-24.2) | 46.3 (45.8-46.3) | 2.78 (0.45-4.26) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.38 (0.33-0.43) | 2.00 (2.00-2.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| uring-d5-p10 | idle | other-cpu | no | 41415 (40409-41949) | 23.4 (22.9-24.2) | 40.7 (40.7-41.2) | 1.08 (1.02-4.56) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.38 (0.33-0.43) | 2.00 (2.00-2.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| uring-d5-p10 | one-cpu | storm-cpu | yes | 32.2 (32.2-32.3) | 31064 (31064-31064) | 31326 (31064-31326) | 33.0 (31.9-33.4) | 676 (670-684) | 0.00 (0.00-0.00) | 483 (483-484) | 0.00 (0.00-0.00) | 5.03 (4.99-5.04) | 6.00 (6.00-6.00) | 5.00 (5.00-5.00) | 10.0 (10.0-10.0) |
| uring-d5-p10 | one-cpu | other-cpu | no | 58788 (58212-60073) | 16.8 (16.3-16.8) | 21.6 (21.1-21.6) | 1.06 (1.02-1.11) | 0.52 (0.52-0.53) | 0.00 (0.00-0.00) | 483 (483-484) | 0.00 (0.00-0.00) | 5.03 (4.99-5.04) | 6.00 (6.00-6.00) | 5.00 (5.00-5.00) | 10.0 (10.0-10.0) |
| uring-d5-p10 | every-cpu | storm-cpu | yes | 32.6 (32.6-32.7) | 30802 (30802-31064) | 31326 (31326-31326) | 31.8 (31.7-34.5) | 676 (676-684) | 0.00 (0.00-0.00) | 42665 (42654-42676) | 0.00 (0.00-0.00) | 434 (433-434) | 440 (440-440) | 440 (440-440) | 706 (706-706) |
| uring-d5-p10 | every-cpu | other-cpu | yes | 32.4 (32.3-32.6) | 31064 (31064-31064) | 31326 (31326-31326) | 31.6 (31.5-34.0) | 770 (760-770) | 0.00 (0.00-0.00) | 42665 (42654-42676) | 0.00 (0.00-0.00) | 434 (433-434) | 440 (440-440) | 440 (440-440) | 706 (706-706) |
| dev-r8-p10 | idle | storm-cpu | no | 17843 (17835-18637) | 54.5 (52.0-55.0) | 87.6 (84.5-89.6) | 1.48 (0.36-5.03) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| dev-r8-p10 | idle | other-cpu | no | 18512 (18072-19201) | 53.0 (50.9-54.5) | 81.4 (80.4-86.5) | 0.49 (0.34-0.71) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| dev-r8-p10 | one-cpu | storm-cpu | yes | 18066 (17428-18668) | 53.0 (51.5-56.1) | 95.7 (90.6-97.8) | 0.34 (0.28-0.42) | 1.07 (1.04-1.16) | 0.00 (0.00-0.00) | 787 (787-789) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 8.00 (8.00-8.00) | 10.0 (10.0-10.0) |
| dev-r8-p10 | one-cpu | other-cpu | no | 19242 (17788-19821) | 50.4 (49.4-55.6) | 82.4 (75.3-86.5) | 0.29 (0.17-0.50) | 1.00 (0.92-1.03) | 0.00 (0.00-0.00) | 787 (787-789) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 8.00 (8.00-8.00) | 10.0 (10.0-10.0) |
| dev-r8-p10 | every-cpu | storm-cpu | yes | 15770 (15704-15961) | 60.2 (59.1-60.2) | 115 (114-115) | 0.42 (0.42-0.80) | 1.32 (1.29-1.35) | 0.00 (0.00-0.00) | 69137 (68797-69137) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 704 (704-704) | 706 (706-706) |
| dev-r8-p10 | every-cpu | other-cpu | yes | 16802 (16194-16866) | 56.6 (56.6-58.6) | 107 (106-112) | 2.05 (0.32-3.51) | 1.31 (1.30-1.32) | 0.00 (0.00-0.00) | 69137 (68797-69137) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 704 (704-704) | 706 (706-706) |
| uring-d5-p60-direct | idle | storm-cpu | no | 38906 (37953-39316) | 24.4 (24.2-24.4) | 54.5 (49.4-57.6) | 3.90 (2.04-5.54) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.33 (0.32-0.33) | 2.00 (2.00-2.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| uring-d5-p60-direct | idle | other-cpu | no | 39713 (39503-40122) | 23.9 (23.9-24.2) | 43.8 (42.2-46.3) | 1.05 (0.57-2.71) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.33 (0.32-0.33) | 2.00 (2.00-2.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| uring-d5-p60-direct | one-cpu | storm-cpu | yes | 5.51 (5.51-5.51) | 181060 (181021-181404) | 181060 (181021-181404) | 181 (181-183) | 3320 (3143-3672) | 0.00 (0.00-0.00) | 82.7 (82.7-82.7) | 0.00 (0.00-0.00) | 5.11 (5.08-5.14) | 6.00 (6.00-6.00) | 5.00 (5.00-5.00) | 10.0 (10.0-10.0) |
| uring-d5-p60-direct | one-cpu | other-cpu | no | 57046 (56890-57753) | 17.0 (17.0-17.3) | 22.1 (21.9-22.1) | 1.08 (1.03-1.53) | 0.50 (0.48-0.52) | 0.00 (0.00-0.00) | 82.7 (82.7-82.7) | 0.00 (0.00-0.00) | 5.11 (5.08-5.14) | 6.00 (6.00-6.00) | 5.00 (5.00-5.00) | 10.0 (10.0-10.0) |
| uring-d5-p60-direct | every-cpu | storm-cpu | yes | 5.51 (5.51-5.56) | 181404 (181326-181404) | 181404 (181326-181404) | 181 (181-182) | 3327 (3148-3672) | 0.00 (0.00-0.00) | 7293 (7293-7294) | 0.00 (0.00-0.00) | 439 (439-439) | 440 (440-440) | 440 (440-440) | 706 (706-706) |
| uring-d5-p60-direct | every-cpu | other-cpu | yes | 5.51 (5.51-5.51) | 181173 (181158-181404) | 181173 (181158-181404) | 181 (181-181) | 4144 (3910-4289) | 0.00 (0.00-0.00) | 7293 (7293-7294) | 0.00 (0.00-0.00) | 439 (439-439) | 440 (440-440) | 440 (440-440) | 706 (706-706) |
| uring-d16-p60 | idle | storm-cpu | no | 40315 (39920-40456) | 23.9 (23.9-23.9) | 47.4 (45.3-52.5) | 8.21 (3.07-8.34) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.36 (0.35-0.42) | 2.00 (2.00-2.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| uring-d16-p60 | idle | other-cpu | no | 41103 (40841-41201) | 23.7 (23.4-23.7) | 40.7 (40.7-41.2) | 2.01 (1.03-5.86) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.36 (0.35-0.42) | 2.00 (2.00-2.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| uring-d16-p60 | one-cpu | storm-cpu | yes | 5.54 (5.51-5.59) | 181404 (181404-181404) | 181404 (181404-181404) | 182 (182-182) | 3830 (3457-4004) | 0.00 (0.00-0.00) | 265 (265-265) | 0.00 (0.00-0.00) | 16.0 (16.0-16.1) | 17.0 (17.0-17.0) | 16.0 (16.0-16.0) | 26.0 (26.0-26.0) |
| uring-d16-p60 | one-cpu | other-cpu | no | 59419 (59242-59887) | 16.5 (16.5-16.5) | 21.1 (20.9-21.1) | 1.08 (1.06-1.10) | 0.51 (0.51-0.52) | 0.00 (0.00-0.00) | 265 (265-265) | 0.00 (0.00-0.00) | 16.0 (16.0-16.1) | 17.0 (17.0-17.0) | 16.0 (16.0-16.0) | 26.0 (26.0-26.0) |
| uring-d16-p60 | every-cpu | storm-cpu | yes | 21590 (20797-21645) | 25.2 (25.0-25.2) | 93.7 (91.6-93.7) | 54.5 (50.3-56.3) | 1.98 (1.79-2.02) | 0.00 (0.00-0.00) | 16917 (16908-16922) | 0.00 (0.00-0.00) | 1018 (1018-1018) | 1025 (1023-1025) | 1024 (1023-1024) | 2114 (2114-2114) |
| uring-d16-p60 | every-cpu | other-cpu | yes | 23378 (22790-24154) | 24.2 (23.9-24.2) | 92.7 (89.6-93.7) | 59.2 (55.4-60.5) | 2.27 (2.20-2.28) | 0.00 (0.00-0.00) | 16917 (16908-16922) | 0.00 (0.00-0.00) | 1018 (1018-1018) | 1025 (1023-1025) | 1024 (1023-1024) | 2114 (2114-2114) |
| uring-d16-p60-s8 | idle | storm-cpu | no | 37223 (36623-38260) | 25.7 (25.0-26.0) | 55.6 (55.0-56.1) | 8.53 (2.52-9.58) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.34 (0.34-0.43) | 2.00 (2.00-2.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| uring-d16-p60-s8 | idle | other-cpu | no | 38901 (38461-39019) | 24.4 (24.4-25.2) | 44.3 (43.3-44.8) | 1.05 (1.02-4.18) | 1.00 (1.00-1.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | 0.34 (0.34-0.43) | 2.00 (2.00-2.00) | 0.00 (0.00-0.00) | 2.00 (2.00-2.00) |
| uring-d16-p60-s8 | one-cpu | storm-cpu | yes | 37345 (36288-38299) | 25.7 (24.4-25.7) | 55.6 (51.5-59.6) | 0.33 (0.30-0.34) | 1.01 (0.92-1.07) | 0.00 (0.00-0.00) | 133 (133-133) | 0.00 (0.00-0.00) | 8.29 (8.22-8.32) | 10.0 (10.0-10.0) | 8.00 (8.00-8.00) | 10.0 (10.0-10.0) |
| uring-d16-p60-s8 | one-cpu | other-cpu | no | 38491 (38483-39347) | 24.7 (24.2-25.2) | 42.8 (42.2-46.3) | 1.03 (1.02-1.04) | 0.99 (0.94-1.05) | 0.00 (0.00-0.00) | 133 (133-133) | 0.00 (0.00-0.00) | 8.29 (8.22-8.32) | 10.0 (10.0-10.0) | 8.00 (8.00-8.00) | 10.0 (10.0-10.0) |
| uring-d16-p60-s8 | every-cpu | storm-cpu | yes | 33924 (33312-34500) | 26.0 (26.0-26.2) | 68.1 (64.3-71.2) | 0.30 (0.29-0.41) | 1.21 (1.17-1.28) | 0.00 (0.00-0.00) | 11673 (11670-11674) | 0.00 (0.00-0.00) | 702 (702-703) | 706 (706-706) | 704 (704-704) | 706 (706-706) |
| uring-d16-p60-s8 | every-cpu | other-cpu | yes | 34265 (34143-35491) | 26.0 (25.5-26.0) | 63.7 (58.6-63.7) | 1.29 (1.19-4.92) | 1.42 (1.36-1.44) | 0.00 (0.00-0.00) | 11673 (11670-11674) | 0.00 (0.00-0.00) | 702 (702-703) | 706 (706-706) | 704 (704-704) | 706 (706-706) |

## Reading

**The decision rule of 17.1**, against `/dev/fuse` with clones (`v3-r8`, the better control in
every column the rule names), in the library regime, where every request reaches the server:

| criterion | rule | V5 depth 5 against `v3-r8` | met |
|---|---|---|---|
| CPU per 1k requests | at least 20% lower | server CPU 4,002 against 889 ms (4.5 times), cgroup kernel time 2,685 against 673 ms (4.0 times); node CPU 12,188 against 14,984 ms (-19%, mostly the 176 burners divided by more cycles) | no |
| p99 | at least 20% lower (or the CPU above) | `stat` 65.3 against 50.1 ms (+30%), `open` 65.8 against 51.6 ms (+28%) | no |
| read MB/s | no more than 5% lower | cold 951 against 1,738 (-45%); warm 6,266 against 6,614 (-5.3%). In `no-open`: cold 1,002 against 2,156 (-54%), warm 7,949 against 8,093 (-1.8%) | no |
| the storm | metadata p99 does not grow | the stormed CPU's `stat` p99 3,786 times its idle p99 (below) | no |
| registered memory | at most 64 MiB per mount | 57,798,400 bytes (55.1 MiB) at depth 5; 184,954,880 (176 MiB) at depth 16 | depth 5 only |
| takeover | 5.2 item 5 | not possible on io_uring on Linux 7.0 (`2026-11-uring-takeover.md`) | no |

Against `v3-r22` the answer is the same: V5's p99 is lower (65.3 against 95.9 ms) and its server
CPU about equal (4,002 against 4,108 ms), but it reads cold files 39% slower (951 against 1,560 MB/s).
Depth 16 changes none of these (`stat` p99 59.5 ms, server CPU 4,066 ms, cold 920 MB/s) and
breaks the memory rule.

- **Cold reads and the memory rule pull against each other.** A cold READ of V5 carries at most
  128 KiB (`max_pages` 32), V3's up to 1 MiB, so V5 makes up to eight times the round trips per byte (inferred from the request sizes) and
  spends 1,983 against 1,382 ms of server CPU per cold GB. At `max_pages` 256 one entry per queue
  registers 88 MiB (5.2 item 4), over the rule at any depth; on these nodes no io_uring mount
  meets both the memory rule and the read rule.
- **Cycles.** V5 makes 23% more cycles than `v3-r8` with caches off (7,202 against 5,870
  cycles/s, `stat` p50 14.7 against 20.8 ms), at more server CPU and a worse tail. In `no-open`
  a cycle does not reach the server and every series is within 4% (986,371 to 1,061,415).

**The storm.** A request waits for an entry of the queue of the CPU it was issued from, in order
(K12). With 8 reads parked for 60 ms on a queue of 5 entries, 5 reads hold the entries (`held` 5
to 6 on the one-CPU storm) and the rest, and every `stat` of that CPU, wait behind them: the
stormed probe's `stat` takes 181 ms, about three parks, at every percentile (p50 equal to p99), and
the CPU's reads complete at 82.8/s, 5 entries per 60 ms. Parked for 10 ms, the same `stat` takes
31 ms. Direct reads change nothing (`uring-d5-p60-direct`). The probe on the other CPU is not
held up by the one-CPU storm: its p99 is lower than in the idle phase (21 against 40 us), which
this measurement does not explain. With every CPU stormed, every queue holds 5 entries (`held`
440 = 88 x 5) and both probes take 181 ms. On `/dev/fuse` the same storms leave the probes within
1.08 times (one CPU) and 1.20 to 1.32 times (every CPU, 704 storm threads running) of idle: a
parked READ holds no channel, and the 8 readers keep reading.

- **Depth does not help the storm as it is defined.** At depth 16 the storm is 24 reads per CPU,
  and 8 still wait: the one-CPU storm takes the stormed `stat` to 181 ms (`uring-d16-p60`). The
  every-CPU storm at depth 16 is held by the kernel's background limit, not the ring: buffered
  reads are background requests, and `max_background` (1024) keeps the rest of the 2,112 reads in
  the kernel (`waiting` 2,114), so 1,018 of the 1,408 entries are held and the probes still find
  free ones (p99 about 2 times idle; the slowest `stat` 55 to 59 ms). Only when the parked reads
  stay below the depth (`uring-d16-p60-s8`, 8 per CPU at depth 16) does the probe's p99 stay at
  idle (1.01 times on one CPU; 1.21 and 1.42 times on every CPU, near `/dev/fuse`'s 1.20 to 1.32), at 176 MiB.
- **What it means for the design.** A storm of `depth x 1.5` parked reads from one CPU is more
  than the CPU's entries, so on io_uring a `stat` from that CPU waits for parked reads to complete,
  at any depth: the storm of 5.2 item 4 cannot pass on io_uring while a parked READ takes longer
  than metadata. `/dev/fuse` passes it. This is what K12 predicts from the source, now measured.

## How to run it again

On a Linux node with `/dev/fuse` and kernel 7.0, as a user with `sudo`, from a build of this
repository (`cargo build --release -p fsbench`) and the list of the baseline's tree
(`fsbench list`, as in `2026-11-fuse-baseline.md`):

1. Let io_uring serve FUSE for the run, and set it back afterwards:

   ```text
   echo Y | sudo tee /sys/module/fuse/parameters/enable_uring
   ...
   echo N | sudo tee /sys/module/fuse/parameters/enable_uring
   ```

2. A V5 series: the server in a scope of its own, served until its standard input closes, then
   the run against its process:

   ```text
   sleep infinity | sudo systemd-run --scope --unit=fsbench-v5 \
     fsbench tsfuse-null-fs --list tree.list --mountpoint /run/fsbench/v5 --data zeros.bin \
     --entry-timeout-ms 0 --attr-timeout-ms 0 --open explicit --readers 1 --reply writev \
     --transport uring --uring-depth 5 --uring-memory-mib 64 &
   sudo fsbench run --list tree.list --tree /run/fsbench/v5 --variant v5-d5 --regime caches-off \
     --server-pid <pid of fsbench tsfuse-null-fs> --out results.jsonl --throughput-rounds 8 \
     --setting transport=uring --setting uring_depth=5 --setting max_pages=32
   ```

   Depth 16: `--uring-depth 16 --uring-memory-mib 1024`. The controls: `--transport dev
   --readers 8` (or 22). `no-open`: `--entry-timeout-ms 32000000000 --attr-timeout-ms
   32000000000 --open none`. `v5_doc` expects the settings `transport` on every record.

3. A storm series (the storm mounts its own file system):

   ```text
   sudo fsbench storm --mountpoint /run/fsbench/storm --out storm.jsonl --label uring-d5-p60 \
     --transport uring --uring-depth 5 --uring-memory-mib 64 --park-us 60000 --read buffered \
     --storm-cpu 1 --other-cpu 87
   ```

   `/dev/fuse`: `--transport dev --readers 8`; a park of 10 ms: `--park-us 10000`; direct reads:
   `--read direct`; another number of threads per CPU: `--per-cpu 8`.

4. `fsbench report --results results.jsonl` and `fsbench storm-report --results storm.jsonl`
   print the tables above.

## What this does not measure

- A real komira build on an io_uring mount (D state, FUSE waiting, wall time): with the rule
  failed on the null file system, a build run would not change the decision.
- A drained node: the node ran its job agent, and no job ran during the series. The V5 series and
  their controls ran in the same hour, so what the node did affects both.
- A kernel other than 7.0: a kernel whose ring lets a waiting request take another queue's entry,
  or a free entry of its own queue out of order, reopens the storm (K12), and one with a
  ring-aware RESEND reopens takeover.
