# tsfuse: the kernel behaviour the design assumes, checked on the farm kernel (T-F0)

Row T-F0 of `docs/design/fetch-cache-serve.md` (section 18): every Linux behaviour the design labels
**[K]** (kernel behaviour from source knowledge, not yet checked), and the kernel facts it states
without a label in 4.8, 5.1, 5.2 and 5.3, checked against the kernel the build nodes run. Each
claim below has a verdict, the source lines that decide it, the farm run that shows it (where one
can run), and the change to the design. The changes are applied to `fetch-cache-serve.md` in the
same change as this document.

Verdicts: **confirmed** (the kernel does what the design says), **changed** (true in substance, but
the design's wording or a detail it builds on is wrong and is corrected here), **refuted** (the
kernel does not do it). Evidence: **run** (a test of this repository shows it on the farm kernel)
or **source** (read from the kernel's source only, with the reason it was not run).

## The kernel

The build nodes run Ubuntu's `linux-hwe-7.0` kernel: `7.0.0-42-generic` on five nodes and
`7.0.0-43-generic` on one. It is Linux 7.0.14 with Ubuntu's patches. Every source line below is of
Ubuntu's git tag `Ubuntu-hwe-7.0-7.0.0-42.42_24.04.1`
(`git.launchpad.net/~ubuntu-kernel/ubuntu/+source/linux/+git/noble`), not of upstream v7.0: the
two differ in most files of `fs/fuse` (stable backports of the io_uring header copy, the ring
readiness barriers, an `abort_on_kill` for a synchronous INIT, re-checks of `FR_SENT` in the
interrupt and resend paths). Where upstream v7.0 differs at a cited line, the entry says so. Its
`include/uapi/linux/fuse.h` is byte-identical to the pinned `crates/tsfuse/abi/fuse.h` (protocol
7.45).

Facts read on all six build nodes (read only; nothing was changed on a node):

| Fact | Value on every node |
|---|---|
| `fuse.enable_uring` (module parameter, mode 0644) | `N` |
| `fs.fuse.max_pages_limit` | 256 |
| `fs.fuse.default_request_timeout`, `fs.fuse.max_request_timeout` | 0, 0 |
| `fs.pipe-max-size` | 1048576 |
| `kernel.io_uring_disabled` | 0 |
| `vm.memfd_noexec` | 0 |
| possible CPUs (one FUSE io_uring queue each) | 88 (`0-87`) |
| fuse `max_user_bgreq`, `max_user_congthresh` | 65535, 65535 |

## How the claims were checked

**Source.** Each claim was traced twice in the Ubuntu tree, and every cited line was checked against
it; the lines below are `file:line` of the Ubuntu tree.

**Run.** The checks that can run on `/dev/fuse` with no `unsafe` code are integration tests of
`tsfuse` that speak the protocol raw, through the harness `crates/tsfuse/tests/kernel/mod.rs`: they
answer INIT with the flags a check asks for, read each request on a chosen cloned descriptor and
write each reply to a chosen descriptor, and wait only on kernel events (a request read, a reply
written, a caller's system call returning), never on a timer. The five targets are
`kernel_dev_integration`, `kernel_takeover_integration`, `kernel_interrupt_integration`,
`kernel_cache_integration` and `kernel_mm_integration` (21 tests). On a build node of each kernel
build (`-42` and `-43`) all 21 passed 10 times in a row as root (mounts by `mount(2)`), and once as
an unprivileged user (mounts by `fusermount3`, the path CI's runner takes); no mount was left
behind. To run them again on a build node:

```
cargo test -p tsfuse --test kernel_dev_integration --test kernel_takeover_integration \
  --test kernel_interrupt_integration --test kernel_cache_integration --test kernel_mm_integration
```

**Not run, and why.** FUSE over io_uring (K10 to K14) and the `mmap` half of K24 are from the source
only. `fuse.enable_uring` is `N` on every node and is set at boot only (5.2 item 4), and a ring
needs `io_uring_setup`, `io_uring_enter`, `io_uring_register` and `mmap`, which are `unsafe` in Rust
(also through `rustix`); AGENTS.md R2 allows no new `unsafe`. The run of K10 to K14 waits for a node
booted with `fuse.enable_uring=1` and for a scoped exception to R2, which the io_uring transport
itself (T-F5) needs as well ("What is left", below).

## Summary

| Id | Claim (design section) | Verdict | Evidence |
|---|---|---|---|
| K1 | A reply is matched on the processing queue of the clone it is written to (5.2 item 2) | confirmed | run |
| K2 | Clones share one input queue: one request wakes every reader (5.2 item 2) | confirmed | run |
| K3 | The kernel's verdict on a malformed reply (5.2, 5.3 item 2) | changed | run |
| K4 | RESEND requeues the requests of every kept clone (5.2 item 5) | changed: the reply carries bit 63 | run |
| K5 | Closing a clone ends its requests; a kept descriptor keeps them (5.2 item 5) | confirmed | run |
| K6 | RESEND's preconditions and exactly-once completion (5.2 item 5) | changed | run |
| K7 | INTERRUPT delivery and the `EAGAIN` answer (5.2 item 3) | changed: `EAGAIN` goes to the clone holding the request | run |
| K8 | A SIGKILLed build frees its parked read (5.2 item 3) | changed: not for readahead READs | run (the uninterruptible wait: source) |
| K9 | FORGET delivery; FORGET and INTERRUPT stay on `/dev/fuse` in io_uring mode (5.2 item 4) | confirmed | source |
| K10 | io_uring registration, INIT-then-register order (5.2 item 4) | changed | source |
| K11 | Hybrid delivery in io_uring mode (5.2 item 4) | changed | source |
| K12 | Parked reads hold ring entries; the storm size (5.2 item 4) | changed | source |
| K13 | Registered memory per mount (5.2 item 4) | changed: 88 MiB at depth 1 with `max_pages` 256 | source |
| K14 | No takeover on io_uring (5.2 item 5) | changed in detail, conclusion stands | source |
| K15 | A reply by splice, and a wrong length (5.3 item 2) | changed | run |
| K16 | `FUSE_SPLICE_MOVE` falls back to a copy (5.3 item 2) | changed | source |
| K17 | The default pipe and `F_SETPIPE_SZ` (5.3 item 2) | changed: a short count, not a failure | run |
| K18 | `NO_OPEN_SUPPORT` keeps the page cache, the first handle included (5.3 Opens) | confirmed | run |
| K19 | `FOPEN_KEEP_CACHE` across opens (5.3 Opens) | confirmed | run (other invalidations: source) |
| K20 | Without `ASYNC_READ` the kernel serialises reads (5.2 INIT flags) | changed | source; READ size measured |
| K21 | A READ is at most `max_pages` pages, 1 MiB (section 2) | changed: buffered READs are at most 128 KiB | run |
| K22 | A request timeout can abort the connection (5.2 item 4) | changed | source and node facts |
| K23 | Background limits (5.2 INIT) | confirmed | source |
| K24 | `F_SEAL_FUTURE_WRITE` forbids new writable mappings and `write(2)` (5.1) | changed: shared mappings only | run (writes), source (mappings) |
| K25 | `F_SEAL_SHRINK`, `F_SEAL_GROW`, `F_SEAL_SEAL`, `MFD_ALLOW_SEALING` (5.1) | changed | run (`vm.memfd_noexec` 1 and 2: source) |
| K26 | DONTNEED on a dirty page does nothing (4.8) | changed | run (synced cells); dirty cells measured |

## Go or no-go per feature

| Feature | Verdict | Condition |
|---|---|---|
| Cloned `/dev/fuse` readers, each reply on its reading clone | go | K1, K2: run |
| Takeover by `FUSE_NOTIFY_RESEND`, the supervisor keeping every descriptor | go | the reply carries the resent unique (bit 63); handlers tolerate a second execution (K4 to K6) |
| INTERRUPT through the token table | go | `EAGAIN` on every clone in turn, never `ENOSYS`; a buffered READ gets none (K7, K8) |
| `NO_OPEN_SUPPORT` for the tree mount | go | no `FOPEN_KEEP_CACHE` fallback needed (K18) |
| `FOPEN_KEEP_CACHE` for the put mount | go | K19 |
| Replies by splice | go | pipes sized before use; `SPLICE_WRITE` is not checked by the kernel (K15, K17) |
| `SPLICE_F_MOVE` | measure (V4) | it moves only readahead READs' pages and empties the pack file's cache of them (K16) |
| `max_pages` 256 | go as a bound | a READ touches at most two cells; the tree mount's READs are at most 128 KiB (K20, K21) |
| State memfd sealed with `F_SEAL_FUTURE_WRITE` | go | made with `MFD_ALLOW_SEALING \| MFD_NOEXEC_SEAL`, sealed before any other mapping (K24, K25) |
| DONTNEED on `durable` cells | go (policy (a) stays the default) | from a thread that may block; counts bytes asked for (K26) |
| Request timeouts | off on every node | `tsfuse` never sends one (K22) |
| FUSE over io_uring | no-go as the default, not run | not enabled on any node; 88 MiB per mount at depth 1 with `max_pages` 256; no takeover (K10 to K14) |

## The findings

### K1. A reply is matched on the processing queue of the clone it is written to: confirmed (run)

**Kernel.** A read moves the request from the connection's input queue to the processing queue of
the reading descriptor's own `fuse_dev` (`fs/fuse/dev.c:1419`, `1526-1527`); every clone is its own
`fuse_dev` with its own processing queue (`dev.c:2610`, `inode.c:1630`). A write looks the unique up
only in the processing queue of the descriptor written to (`dev.c:2212`, `2244`) and returns `ENOENT`
when it is not there (`dev.c:2246`), changing nothing. The request stays on the reading clone's
processing queue (not the input queue) until that clone answers it, a RESEND moves it (K4), or that
clone's open file is released (K5).

**Run.** `kernel_dev_integration`: `a_reply_on_another_clone_is_refused_enoent_and_the_request_waits_for_its_own_clone`
(a LOOKUP read on clone `a`, answered on clone `b` and on the mount's descriptor: `ENOENT` both, the
caller still waits; on `a`: accepted; a second reply on `a`: `ENOENT`);
`parked_reads_on_two_clones_complete_out_of_order_each_on_its_own_clone`.

**Design.** Unchanged; 5.2 item 2 names the errno and the queue the request waits on.

### K2. Clones share one input queue, so one request wakes every reader: confirmed (run)

**Kernel.** `poll` on any clone waits on the connection's input-queue wait queue
(`dev.c:2421-2422`) and reports readable when the shared queue holds anything (`dev.c:2424-2428`);
queuing a request calls `wake_up` (`dev.c:335`), which wakes every non-exclusive waiter, and an
`epoll` registration (what `AsyncFd` uses) is non-exclusive. A blocking `read(2)` waits exclusively
(`dev.c:1452`), but `tsfuse`'s readers are non-blocking: every one wakes, one reads, the others get
`EAGAIN` (`dev.c:1450-1451`). The input queue's lock is the connection's (`dev.c:1445`); only the
processing queues are per clone.

**Run.** `one_request_makes_every_clone_poll_readable_and_only_one_clone_reads_it` (four
non-blocking clones: all four poll readable for one request, one reads it, three read `EAGAIN`).

**Design.** Unchanged.

### K3. The kernel's verdict on a malformed reply: changed (run)

**Kernel**, `fuse_dev_do_write` (`dev.c:2207-2300`), by case:

| Reply | `write(2)` | The request |
|---|---|---|
| shorter than the 16-byte header (source) | `EINVAL` (`dev.c:2217`) | untouched |
| `len` field not the number of bytes written | `EINVAL` (`dev.c:2225-2226`) | untouched, answerable |
| `error` above 0 or at most -512 | `EINVAL` (`dev.c:2238`) | untouched, answerable |
| unique not on this clone's processing queue (unknown, already answered, another clone's) | `ENOENT` (`dev.c:2244-2247`) | untouched |
| `error` not 0 with a payload, or (source) a payload of the wrong size | `EINVAL` (`dev.c:2279-2290`) | **ended: its caller gets `EIO`** |
| unique 0 | a notification, code in `error` (`dev.c:2232`) | none |
| INTERRUPT reply (unique with bit 0) | see K7 | |

**Run.** `a_reply_with_a_wrong_length_or_an_errno_out_of_range_is_refused_einval_and_the_request_stays_answerable`;
`an_error_reply_with_a_payload_is_refused_einval_and_fails_the_caller_with_eio`. The rows marked (source) have no test; the notification row is K6's.

**Design.** 5.2 states the table's two consequences: an error reply is the bare header (the `tsfuse`
encoder already cannot build another), and a refused write is never taken to mean the request is
gone (with `EINVAL` it may be ended or answerable, by the case above).

### K4. RESEND requeues the requests of every kept clone: changed (run)

**Kernel.** `fuse_resend` walks every device of the connection (`dev.c:2026`) and moves each one's
processing queue (`dev.c:2031-2035`) to the head of the shared input queue (`dev.c:2060`), setting
`FUSE_UNIQUE_RESEND`, bit 63, in each unique (`dev.c:2054`). Any clone then reads them
(`dev.c:1418`); the reply is looked up by the unique exactly as read again (`dev.c:2244`). Ubuntu
also clears `FR_SENT` under the queue's lock and drops a queued INTERRUPT of a resent request
(`dev.c:2034`, `2056-2057`); upstream v7.0 does neither.

**Run.** `resend_brings_back_reads_parked_on_every_kept_clone_with_bit_63_set_in_their_unique`
(READs parked on two clones whose readers then close while a `dup` of each stays open; RESEND on the
mount's descriptor; both READs read again with bit 63 set and the same arguments; the reply under the
original unique: `ENOENT`; under the unique as read: accepted, on the descriptor that read it, and
on the old clones' kept descriptors: `ENOENT`; both callers get their bytes, no `EIO`).

**Design.** 5.2 item 5 said "resent requests are answered under their original unique id on the
descriptor that holds them". Corrected: under the unique as read again (the original with bit 63
set), on the descriptor that read the resent copy. (`tsfuse::abi::UNIQUE_RESEND` already documents
this.)

### K5. Closing a clone ends its requests; a kept descriptor keeps them: confirmed (run)

**Kernel.** `fuse_dev_release` runs on the last close of an open file (a `dup` or an `SCM_RIGHTS`
copy is the same open file). It moves that device's processing queue aside (`dev.c:2576`) and ends
each request with `ECONNABORTED` (`dev.c:2579`, `2440`); the connection is aborted only when its last
device closes (`dev.c:2582-2584`). A process's death is its descriptors closing, so a supervisor that
keeps a descriptor of every clone keeps every in-flight request.

**Run.** `a_dup_kept_by_a_supervisor_keeps_a_parked_read_answerable_after_the_reader_closes`;
`closing_the_only_descriptor_of_a_clone_ends_its_parked_read_with_econnaborted` (the caller gets
`ECONNABORTED`, 103, not `EIO`; another clone then serves the next caller).

**Design.** Unchanged; 5.2 item 5 names the errno.

### K6. RESEND's preconditions and exactly-once completion: changed (run)

**Kernel.** `FUSE_HAS_RESEND` is only offered at INIT (`inode.c:1510`): the kernel never reads it from
the reply, and `fuse_resend` checks no flag. A RESEND is a 16-byte notification (`dev.c:2145-2146`)
that always succeeds (`dev.c:2064-2068`), with nothing in flight too; on a disconnected connection
it moves nothing (`dev.c:2021`). Completion is once: a request ends on its first accepted reply
(`dev.c:490`) and leaves the processing queue then. Execution is not: a request the old server
carried out but did not answer is resent and carried out again.

**Run.** `resend_with_nothing_in_flight_is_accepted_and_resends_nothing`; the K4 test (a second
reply to a resent request: `ENOENT`).

**Design.** 5.2 item 5 states that resent requests may already have been carried out, so a handler
that is not idempotent (none of the tree mount's) must say so.

### K7. INTERRUPT delivery and the `EAGAIN` answer: changed (run)

**Kernel.** An INTERRUPT is queued on the connection's own interrupt list (`dev.c:358-359`; Ubuntu
re-checks `FR_SENT` there, upstream v7.0 does not) and is read, before any other request, by
whichever clone reads next (`dev.c:1463-1466`): 48 bytes, its own unique the interrupted request's
with bit 0 set (`dev.c:1274`), its body the interrupted unique. Its reply is looked up, like any
reply, in the processing queue of the clone it is written to, by the interrupted request's unique
(`dev.c:2244`); there `ENOSYS` turns INTERRUPT off for the whole connection (`dev.c:2260-2261`) and
`EAGAIN` queues the INTERRUPT again (`dev.c:2262-2263`). Written to any other clone, the `EAGAIN`
is refused with `ENOENT` and the INTERRUPT is lost. A narrow race between a signal and the read of
its request (`dev.c:567-571` against `dev.c:1529-1534`) can deliver the same INTERRUPT twice.

**Run.** `an_interrupt_answered_eagain_is_queued_again_only_when_written_to_the_clone_holding_the_request`
(an INTERRUPT read on clone `b` for a READ held by clone `a`: `EAGAIN` on `b` is `ENOENT` and nothing
is queued again; on `a` it is accepted and the same INTERRUPT is read again);
`a_killed_caller_of_a_parked_read_queues_an_interrupt_and_ends_once_the_read_is_answered` (read on
another clone than the READ's).

**Design.** 5.2 item 3 said "an INTERRUPT that arrives before its request is answered `EAGAIN`".
Corrected: answered `EAGAIN` on every clone in turn (only the one holding the request takes it), a
repeated INTERRUPT is idempotent, and INTERRUPT is never answered `ENOSYS`. `tsfuse` already does
this (`transport.rs`, `Every`); its crate documentation said "on the device that read the
INTERRUPT", now corrected (`lib.rs`, `tokens.rs`).

### K8. A SIGKILLed build frees its parked read: changed (run)

**Kernel.** A caller whose request the server has read and not answered is woken by any signal,
marks the request interrupted and queues an INTERRUPT (`dev.c:560-571`); with a fatal signal it then
waits, uninterruptibly, until the reply (`dev.c:574-601`): a killed build does not exit until its
request is answered (Ubuntu's `abort_on_kill`, `dev.c:583-585`, is set only on a synchronous INIT,
`inode.c:1552`). A READ sent by readahead under `FUSE_ASYNC_READ`, which the tree mount asks for, is
a background request (`file.c:1060`) with no waiter: the reader waits on the page's lock killably,
so SIGKILL frees it at once, no INTERRUPT is sent, and the READ stays with the server until it is
answered. At its exit the killed process's close sends FLUSH, which is forced (`file.c:497`) and
waits for its reply.

**Run.** `a_killed_caller_of_a_parked_read_queues_an_interrupt_and_ends_once_the_read_is_answered`
(a direct READ: the INTERRUPT names it and is read on another clone; answering the READ `EINTR`
lets the child end, killed by signal 9; that it cannot end before is from the source above);
`a_killed_reader_of_a_parked_readahead_read_exits_and_leaves_the_read_parked_uninterrupted` (under
`ASYNC_READ`: no INTERRUPT names the READ up to the child's exit-time FLUSH; once the FLUSH is
answered the child ends; the READ's reply is still accepted after).

**Design.** 5.2 item 3 said "a SIGKILLed build frees its parked read". Corrected: a killed build
waits for the server's answer to any request the server has read, which the INTERRUPT asks for; a
READ of a buffered read (the tree mount's, under `ASYNC_READ`) gets no INTERRUPT, its reader does
not wait for it (it exits once its exit-time FLUSH is answered), and the parked READ stays until the fill completes or fails. The server answers FLUSH at
once.

### K9. FORGET, and FORGET and INTERRUPT in io_uring mode: confirmed (source)

**Kernel.** FORGETs are queued on the connection's forget list (`dev.c:343-351`) and read after
INTERRUPTs, alternating with requests, eight requests to sixteen forget reads
(`dev.c:1469-1474`); several are read as one `FUSE_BATCH_FORGET` (`dev.c:1398-1401`). No reply is
expected (one is refused with `ENOENT`). In io_uring mode the queue operations keep FORGET and
INTERRUPT on `/dev/fuse` (`dev_uring.c:1488`, `1494`). Not run: the claim that matters is the
io_uring one (see the reason above).

**Design.** The "[K, uncertain]" of 5.2 item 4 ("Hybrid") is now from source; see K11.

### K10. io_uring registration and the INIT-then-register order: changed (source)

**Kernel.** `enable_uring` is a run-time writable parameter (`dev_uring.c:16`); the kernel offers
`FUSE_OVER_IO_URING` at INIT only while it is on (`inode.c:1523-1524`) and grants it if the reply
asks for it (with `FUSE_INIT_EXT`, it is in `flags2`) and the parameter is still on
(`inode.c:1455-1456`): a mount granted it keeps it whatever the parameter becomes. A REGISTER
needs a ring made with `IORING_SETUP_SQE128` (`dev_uring.c:1248-1249`) and INIT already answered on
`/dev/fuse` (`EAGAIN` before). There is one queue per **possible** CPU (`dev_uring.c:240`; 88 on the
build nodes), and the ring carries requests only once every queue has an entry
(`dev_uring.c:1046-1068`, `1105`); until then the mount's ordinary requests wait in the kernel
(`dev.c:179`). Any failed REGISTER turns io_uring off for the connection (`dev_uring.c:1289`), and
after the ring is ready that wedges the mount: requests still go to the ring and every commit is
refused.

**Design.** 5.2 item 4 states the order (INIT on `/dev/fuse`, then a REGISTER on every possible CPU's
queue, none of which may fail), the `SQE128` ring, and that the parameter is fixed at boot by policy,
not by the kernel.

### K11. Hybrid delivery in io_uring mode: changed (source)

**Kernel.** In io_uring mode requests go to the ring (`dev_uring.c:1495`) except FORGET,
`BATCH_FORGET` and INTERRUPT, which stay on `/dev/fuse` (`dev_uring.c:1488`, `1494`); requests
already queued, or forced (INIT, RELEASE, FLUSH, DESTROY), before the ring is ready, and requests
resent by RESEND, also come through `/dev/fuse` (`dev.c:753`, `2060`). A ring request's reply is a
commit; written to `/dev/fuse` it is `ENOENT` (it is not on any clone's processing queue), and so is
an INTERRUPT's `EAGAIN` for it: in io_uring mode the server must hold an INTERRUPT whose request has
not reached it yet instead of answering `EAGAIN`. A ring entry carrying a request that takes no
reply (a FORGET forced by `readdirplus`, `readdir.c:288`) must still be committed, or its caller
waits.

**Design.** 5.2 item 4's "Hybrid" bullet states this rule in place of "INIT, early requests,
INTERRUPT and FORGET may still arrive".

### K12. Parked reads hold ring entries; the storm size: changed (source)

**Kernel.** A request goes to the queue of the CPU its caller runs on (`dev_uring.c:1365`) and waits
there, in order, for that queue's next free entry (`dev_uring.c:1409-1414`); it never moves to
another queue. A held entry has no timeout unless a request timeout is set (K22). A REGISTER of a new
entry does not take a request already waiting (`dev_uring.c:1098`): only a commit or a new request
does.

**Design.** 5.2 item 4's storm test is per queue: `depth + 1` parked reads issued from one CPU stall
that CPU's metadata and no other's; "`depth x queues x 1.5` parked reads" spread over all queues
tests the wrong thing.

### K13. Registered memory per mount: changed (source)

**Kernel.** Each entry registers a 288-byte header (`include/uapi/linux/fuse.h:1251-1281`) and a
payload of at least `max(8192, max_write, max_pages x 4 KiB)`, fixed by the mount's first REGISTER
(`dev_uring.c:253-254`, `1164`), on each of the 88 queues. The memory is the server's own,
unpinned until touched.

| `max_pages` (payload) | depth 1 | depth 5 |
|---|---|---|
| 256 (1 MiB, the tree mount's `InitConfig::tree`) | 88 MiB | 440 MiB |
| 32 (128 KiB) | 11 MiB | 55 MiB |
| 2 (8 KiB) | 0.7 MiB | 3.4 MiB |

**Design.** 5.2 item 4's 64 MiB rule cannot be met with `max_pages` 256 at any depth on these nodes.
Since a buffered READ is at most 128 KiB anyway (K21), an io_uring tree mount uses `max_pages` 32,
which allows a depth of 5; the formula in 5.2 is corrected (queues = possible CPUs).

### K14. No takeover on io_uring: changed in detail, the conclusion stands (source)

The T-F2 spike (`docs/measurements/2026-11-uring-takeover.md`) read upstream v7.0. On the Ubuntu
tree: RESEND still walks only `/dev/fuse` devices (`dev.c:2026`), so a request held by a dead
server's ring entry is not resent, and the conclusion (no takeover on io_uring) stands. Two details
differ: when the dead server's ring is cancelled, Ubuntu frees the entries waiting for a request
(`dev_uring.c:527-540`) where upstream kept them, and ends a request caught mid-dispatch with
`ECANCELED` (`dev_uring.c:1337-1353`), an error the build sees. And "no other process can commit"
is too strong: a commit by another process is accepted if it knows the queue and the request's
unique and the dead process's buffer addresses are valid in it (`dev_uring.c:1006-1015`); a
takeover would need the supervisor to own those buffers.

**Design.** 5.2 item 5 adds that a server killed under load on io_uring makes some in-flight requests
fail with `ECANCELED`; the measurement document has a note pointing here.

### K15. A reply by splice, and a wrong length: changed (run)

**Kernel.** `fuse_dev_splice_write` first counts the bytes in the pipe: fewer than the length asked
is `EINVAL` before anything is taken from the pipe or the request looked up (`dev.c:2350-2355`).
Otherwise it takes those buffers and writes them as one reply, with the cases of K3 (a `len` field
not equal to the length: `EINVAL`, the request answerable, the pipe's bytes consumed). The
`FUSE_SPLICE_WRITE` INIT flag is only offered (`inode.c:1500`); the kernel never checks it.

**Run.** `a_read_answered_by_splice_from_a_pipe_reaches_the_caller` (a 256 KiB READ answered by a
header written into a sized pipe, the data spliced from a memfd, and one splice into the device; with
and without `SPLICE_WRITE` in the INIT reply);
`a_reply_spliced_from_a_pipe_too_small_for_it_is_refused_einval_and_the_request_stays_answerable`.

**Design.** 5.3 item 2 states that the pipe is drained after any refused splice and the reply built
again, and 5.2 lists `SPLICE_WRITE` as offered, not negotiated.

### K16. `FUSE_SPLICE_MOVE` falls back to a copy: changed (source)

**Kernel.** `SPLICE_F_MOVE` is honoured only for READs sent by readahead (`page_replace`,
`file.c:1046`; `dev.c:2276-2277`, `2393-2394`), only for whole page-sized destination pages, and only
when the source buffer can be stolen; otherwise it copies. Where a steal succeeds, the page leaves
the pack file's page cache (a copy saved, and in effect a DONTNEED of that page). The INIT flag is
not checked.

**Design.** 5.3 item 2's "[K, uncertain]" sentence is replaced by this; V4 measures both the copy
saved and the pack file's page cache lost.

### K17. The default pipe and `F_SETPIPE_SZ`: changed (run)

**Kernel.** A pipe starts with 16 buffers (`include/linux/pipe_fs_i.h:5`), 64 KiB of pages. A splice
of more than fits into it moves a short count (and a further one blocks, or is `EAGAIN` with
`SPLICE_F_NONBLOCK`); it does not fail. `F_SETPIPE_SZ` rounds up to a power of two; a process without
`CAP_SYS_RESOURCE` is held to `fs.pipe-max-size` (`fs/pipe.c:55`, 1 MiB on the nodes) and a per-user
soft limit (`fs/pipe.c:809-811`).

**Run.** `a_reply_spliced_from_a_pipe_too_small_for_it_is_refused_einval_and_the_request_stays_answerable`
(the 256 KiB data splice into a default pipe moves a short count).

**Design.** 5.3 item 2 said "the default 64 KiB pipe fails a 256 KiB splice". Corrected: it moves a
short count; every pipe is sized with `F_SETPIPE_SZ` to the largest reply plus the header before use,
and the fill loop never blocks on a full pipe.

### K18. `NO_OPEN_SUPPORT` keeps the page cache, the first handle included: confirmed (run)

**Kernel.** An OPEN answered `ENOSYS` sets the connection's `no_open` (`file.c:171`); from then on no
OPEN is sent (`file.c:134`). A handle opened without OPEN, and the one whose OPEN was answered
`ENOSYS`, get `FOPEN_KEEP_CACHE` (`file.c:151`, kept since only a successful OPEN replaces it,
`file.c:160`), so no open drops the file's pages (`file.c:291-292`). No RELEASE is sent for them
(`file.c:113`). FLUSH is still sent at every close (`file.c:497`) until answered `ENOSYS` once. The
kernel does not read `NO_OPEN_SUPPORT` from the INIT reply: the `ENOSYS` is the mechanism.

**Run.** `no_open_support_stops_open_after_one_enosys_and_keeps_the_page_cache_for_every_later_open`
(one OPEN, answered `ENOSYS`; the second whole read of the file sends no OPEN and no READ; no
RELEASE).

**Design.** 5.3 "Opens" drops the "if not, the library returns `FOPEN_KEEP_CACHE`" branch for the tree
mount; every OPEN that arrives before the first `ENOSYS` lands is answered `ENOSYS` too.

### K19. `FOPEN_KEEP_CACHE` across opens: confirmed (run)

**Kernel.** A successful OPEN without `FOPEN_KEEP_CACHE` drops the file's pages
(`file.c:291-292`). Pages are also dropped when GETATTR or LOOKUP reports another size
(`inode.c:380-382`) and, with `FUSE_AUTO_INVAL_DATA`, another mtime (`inode.c:384`); these two are
from the source, not run.

**Run.** `keep_cache_keeps_the_page_cache_across_opens_and_an_open_without_it_drops_it` (with the flag
the second whole read sends no READ; without it, READs again).

**Design.** 5.3 adds that the tree mount returns one stable size and mtime per node and does not ask
for `FUSE_AUTO_INVAL_DATA`.

### K20. Without `ASYNC_READ` the kernel serialises reads: changed (source; READ size measured)

**Kernel.** Without `FUSE_ASYNC_READ` the READs of a readahead are sent one at a time by the reading
thread, each waited for (`file.c:1064`): nothing is in flight when `read(2)` returns. With it they
are background requests (`file.c:1060`), counted against `max_background` (K23). A buffered READ is
at most the readahead window, which the kernel offers at INIT (`inode.c:1496`, 128 KiB) and the
reply can only lower (`inode.c:1342`).

**Run.** No test asserts the synchronous sending as such; the page-cache checks of
`kernel_cache_integration` rely on it (they count READs right after their caller's `read(2)` returns,
and the counts hold run after run). The `NO_OPEN_SUPPORT` check asserts that every buffered READ is at
most the readahead window the kernel offered, and measures the largest: 131072 bytes on both kernel
builds.

**Design.** 5.2's INIT list said "`ASYNC_READ` (without it the kernel serialises reads per file)";
corrected to the above.

### K21. A READ is at most `max_pages` pages, 1 MiB: changed (run)

**Kernel.** `max_pages` is the INIT reply's, capped by the `fs.fuse.max_pages_limit` sysctl read at
mount (`inode.c:1000`, `1392-1394`; 256 by default, `inode.c:42`), and 32 when the reply does not
carry `FUSE_MAX_PAGES` (`fuse_i.h:37`, `inode.c:999`). A direct READ is split at `max_pages` pages of
the caller's buffer; a buffered READ is at most the readahead window (K20). The kernel keeps
`max_write` as asked (at least 4096, `inode.c:1471-1472`), and a `/dev/fuse` read buffer must hold
`80 + max_write` bytes (`dev.c:1437`).

**Run.** `a_direct_read_is_split_into_reads_of_max_pages_capped_by_the_sysctl` (a 4 MiB direct read
into an aligned buffer: four 1 MiB READs at 256 pages, sixteen of 256 KiB at 64 pages, and four of
1 MiB when 1024 pages are asked for); the K20 measurement (buffered READs of at most 128 KiB).

**Design.** Section 2 ("the FUSE read size"): a READ is at most 1 MiB, so it touches at most two
cells; the tree mount, whose reads are all buffered under `NO_OPEN_SUPPORT`, sees READs of at most
128 KiB. `tsfuse`'s comment on `InitConfig` (it said the kernel lowers `max_write` to `max_pages`
pages) is corrected.

### K22. A request timeout can abort the connection: changed (source and node facts)

**Kernel.** With both sysctls at 0 and no timeout in the INIT reply, no request is ever timed out
(`inode.c:1295-1296`): this is the farm's state, so a parked read waits for as long as the server
holds it. When an administrator sets `fs.fuse.default_request_timeout` (used when the reply sends 0)
or `fs.fuse.max_request_timeout` (a cap, which also replaces 0), the server cannot opt out
(`inode.c:1299-1301`); the minimum is 15 s, and a request older than it, whether the server has read
it or not, aborts the whole connection (every request in flight ends `ECONNABORTED`). A RESEND does
not reset a request's age. Not run: a check would have to set a node's sysctl.

**Design.** 5.2 item 4 and T0 E record both sysctls (0 and 0 on every node) and state that `tsfuse`
never sends a request timeout.

### K23. Background limits: confirmed (source)

**Kernel.** Without values in the INIT reply `max_background` is 12 and `congestion_threshold` 9
(`inode.c:66-69`, `987-988`); the reply sets each separately (`include/uapi/linux/fuse.h:919-920`).
For an unprivileged mount they are capped by `max_user_bgreq` and `max_user_congthresh` (65535 on
the nodes, `inode.c:1260-1262`). Under `ASYNC_READ` every buffered READ parked in the server is a
background request, so `max_background` bounds the parked READs of a mount; past it a new readahead
waits in the kernel, killably, with no error.

**Design.** 5.2 adds `max_background` and `congestion_threshold` to the INIT list (`tsfuse` sets 1024
and 768).

### K24. `F_SEAL_FUTURE_WRITE` forbids new writable mappings and `write(2)`: changed (run, mapping by source)

**Kernel.** The seals are the inode's (`mm/memfd.c:213`), so every descriptor of the memfd, a reopen
through `/proc/self/fd` included, sees them. After `F_SEAL_FUTURE_WRITE` a write is `EPERM`
(`mm/shmem.c:3317-3318`), and so is a hole punch (`mm/shmem.c:3712-3713`); a new `MAP_SHARED`
mapping with `PROT_WRITE` is `EPERM` (`mm/memfd.c:385-386`); a read-only shared mapping made after
the seal loses `VM_MAYWRITE` (`mm/memfd.c:392`), so `mprotect` cannot make it writable (`EACCES`); a
`MAP_PRIVATE` writable mapping is allowed (`mm/memfd.c:378`): its writes are copy-on-write and never
reach the memfd. The server's mapping made before the seal stays writable, and so would any shared
mapping made before it.

**Run.** `a_future_write_sealed_memfd_refuses_every_write_and_resize_and_keeps_its_bytes` (`write`,
`pwrite` in place and a hole punch are `EPERM`, also through a read-write reopen from
`/proc/self/fd`; the bytes read back). The mapping cases need `mmap`, not called here; they run on the store's state memfd in
`tierstore-entry` (see "What is left").

**Design.** 5.1 said "forbids new writable mappings": corrected to "new writable shared mappings";
the server seals before the descriptor leaves it and before any other mapping of it; the conformance
test (T-F7) maps `MAP_SHARED`.

### K25. `F_SEAL_SHRINK`, `F_SEAL_GROW`, `F_SEAL_SEAL` and `MFD_ALLOW_SEALING`: changed (run)

**Kernel.** Shrinking under `F_SEAL_SHRINK` and growing under `F_SEAL_GROW` are `EPERM`
(`mm/shmem.c:1340-1342`), `fallocate` past the end too (`mm/shmem.c:3744-3745`); the same size, and
`fallocate` inside it, are allowed. A seal added after `F_SEAL_SEAL`, or through a descriptor not
open for writing, is `EPERM`. A memfd starts sealed against sealing; `MFD_ALLOW_SEALING` clears that,
and so does `MFD_NOEXEC_SEAL`, which also sets `F_SEAL_EXEC` (`mm/memfd.c:488-493`); where
`vm.memfd_noexec` is 1 the kernel adds `MFD_NOEXEC_SEAL` by itself, and where it is 2 a memfd without
it is refused (`mm/memfd.c:350-361`).

**Run.** The K24 test (shrink, grow and `fallocate` past the end: `EPERM`; same size and inside:
allowed; `F_SEAL_SEAL` then a seal: `EPERM`; a seal through a read-only reopen: `EPERM`);
`a_memfd_made_without_allow_sealing_takes_no_seal`.

**Design.** 5.1 creates the state memfd (and the counter memfd) with `MFD_ALLOW_SEALING |
MFD_NOEXEC_SEAL`, which works at every `vm.memfd_noexec` setting (run at 0, the setting of every
node; 1 and 2 from the source), and sizes it before sealing.

### K26. DONTNEED on a dirty page does nothing: changed (run for synced cells, measured for dirty ones)

**Kernel.** `posix_fadvise(DONTNEED)` first starts writeback of the range's dirty pages without
waiting for it (`mm/fadvise.c:114`, `mm/filemap.c:437`), ignoring its result, then drops only the
pages that are clean, not under writeback, unlocked, unmapped and otherwise unreferenced, and wholly
inside the range (`mm/fadvise.c:121-157`, `mm/truncate.c:330-336`). Pages held by a pipe (a splice
in flight) are referenced and stay (`mm/filemap.c:3031`). A synced cell is not dropped if it is
mapped, in a pipe, being read, or in a large folio that crosses the range's edge. The call can block
while it starts writeback.

**Run.** `dontneed_drops_the_synced_cells_of_its_range_and_no_other` (four cells written by four
aligned 1 MiB writes and synced: DONTNEED on cell 1 drops cell 1, at least 1 MiB read from disk again,
and keeps cells 0, 2 and 3, no disk read; DONTNEED on the file drops every cell);
`dontneed_on_dirty_cells_succeeds_and_loses_nothing`, which measured on both kernel builds that
DONTNEED right after the writes dropped 0 of 4 MiB. Residency is read from the disk reads a read costs
(`read_bytes` of `/proc/thread-self/io`); `preadv2(RWF_NOWAIT)` is not a residency probe on this
kernel, since `RWF_NOWAIT` sets `IOCB_NOWAIT` but not `IOCB_NOIO` (`include/linux/fs.h:344`,
`3438-3456`) and a miss then starts readahead (`mm/filemap.c:2696-2702`).

**Design.** 4.8 said "because DONTNEED on a dirty page does nothing". Corrected: DONTNEED starts the
writeback of dirty pages (unbatched I/O, not durability) and keeps them; it drops only clean pages
nobody holds, so the server issues it only on `durable` cells, from a thread that may block, and
`fadvise_dontneed_bytes_total` counts bytes asked for, not dropped.

## What changes in the design

Applied to `docs/design/fetch-cache-serve.md`: section 2 (the FUSE read size, K21),
4.8 (DONTNEED, K26), 5.1 (the state memfd, K24 and K25), 5.2 (INIT flags K15, K20, K23; item 2 K1 and
K3; item 3 K7 and K8; item 4 K10 to K13 and K22; item 5 K4 to K6 and K14), 5.3 (Opens K18 and K19;
reads K15 to K17), and the T-F0 row and the "what was not run" notes. The measurement
`docs/measurements/2026-11-uring-takeover.md` has a note pointing to K14.

## What is left

- **The io_uring run (K10 to K14).** It needs a build node booted with `fuse.enable_uring=1` (a
  drained node and a reboot, by the node's operator) and a scoped exception to AGENTS.md R2 for the
  ring's `mmap` and `io_uring_*` calls, which the io_uring transport (T-F5) needs in any case. The
  checks to run then: REGISTER before INIT (`EAGAIN`) and after; the ring carries nothing until every
  possible CPU's queue has an entry; one failed REGISTER turns io_uring off for the connection; a
  parked READ holds its entry and requests from that CPU wait behind it; FORGET and INTERRUPT arrive
  on `/dev/fuse`; a server killed under load (`ECANCELED`, K14). The io_uring transport's own
  test, `tests/kernel_uring_integration.rs` (T-F5), runs part of this on a node with the parameter
  on (set for the run, the kernel reads it at INIT, K10): REGISTER after INIT on every possible
  CPU's queue (88), requests answered by COMMIT_AND_FETCH and read by the queues' threads, a parked
  READ holding its entry until another thread completes it, and an INTERRUPT arriving on
  `/dev/fuse` and answered by a commit of the READ's entry. The rest is still to run.
- **The mapping half of K24** needs no exception: it runs on the store's own state memfd in
  `tierstore-entry` (`a_writable_shared_mapping_of_the_passed_state_memfd_fails_with_eperm`, which
  maps through `vm-memory`'s mapping and writes no `unsafe` of its own), and the conformance suite
  (T-F7, `docs/design/tsfuse-conformance.md`, item 11) counts it there.
