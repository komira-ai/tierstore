# Spike: can an io_uring FUSE mount survive a restart of its server? (T-F2)

The question of `docs/design/fetch-cache-serve.md`, 5.2 item 5: when the process serving a mount
dies and a supervisor keeps the connection's descriptors, does a new process get back the
requests that were in flight on io_uring ring entries, as it gets back those on `/dev/fuse`
clones through `FUSE_NOTIFY_RESEND`?

**Answer: no, on the farm kernel (Linux 7.0).** A request held by a ring entry is not resent and
cannot be answered by another process. It stays unanswered until the connection is aborted
(the caller then gets `ECONNABORTED`) or the caller is killed. io_uring stays
takeover-incapable: takeover (T-F6) is `/dev/fuse` only, and a mount that must survive a process
update uses `/dev/fuse` (5.2 item 4 already says so).

**How this was answered.** By reading the source of the farm kernel's FUSE module, `fs/fuse/` of
Linux v7.0 (`dev.c`, `dev_uring.c`, `dev_uring_i.h`); label [K], from the source, not a run.
It was not run on a node because `fuse.enable_uring` is `N` on all six farm nodes (read from
`/sys/module/fuse/parameters/enable_uring` on each, kernel `7.0.0-42-generic`), and turning it on
changes kernel state on nodes running jobs (the design sets it at boot, never at runtime). The run
that confirms it is conformance item 7 on the io_uring transport, on a node with
`enable_uring=1` (T-ENV, T-F7).

## The evidence, function by function

1. **RESEND sees only `/dev/fuse` devices.** `fuse_resend()` (`dev.c`, called for
   `FUSE_NOTIFY_RESEND`) walks `fc->devices` and splices each device's `fud->pq.processing`
   lists back to the pending queue, marking each request `FUSE_UNIQUE_RESEND`. A request handed to
   a ring entry is not on those lists: it is on the ring queue's own `queue->fpq.processing`
   (`dev_uring.c`), which `fuse_resend()` never touches.
2. **The server's death cancels its ring commands, not their requests.** When the server's
   io_uring instance goes away, each registered command gets `IO_URING_F_CANCEL`
   (`fuse_uring_cancel()`). An entry that is `FRRS_AVAILABLE` (waiting for a request) is completed
   with `-ENOTCONN` and moved to `ent_in_userspace` with no command. An entry in `FRRS_USERSPACE`
   (its request was given to the dead server) is left as it is, its `fuse_req` still on
   `queue->fpq`.
3. **Nobody else can answer such a request.** `fuse_uring_commit_fetch()` finds the request by
   `commit_id` (the request's unique) on `queue->fpq` and copies the reply from `ent->headers` and
   `ent->payload`: user addresses of the process that registered the entry. A new process does not
   know the unique (it was in the dead process's ring buffer), and a commit from it would read those
   addresses in its own address space (`copy_from_user` faults with `EFAULT`, or reads unrelated
   memory), so the request would end with an error or a wrong reply, never the right one.
4. **The request ends only with the connection.** `fuse_uring_entry_teardown()`, which ends an
   entry's request with `-ECONNABORTED`, runs from `fuse_uring_stop_queues()`, which runs from
   `fuse_uring_abort()` on a connection abort. `fuse_dev_release()` ends only the device's own
   `fud->pq` requests when a descriptor closes; a supervisor that holds the descriptor keeps the
   connection, and with it the stuck request, alive.
5. **New requests can be served again.** `fuse_uring_register()` reuses an existing ring and
   queue and adds a new entry, so a new process that registers entries serves requests that arrive
   after it. Only the requests in flight at the death are lost; that is what takeover must not lose.

## What changes in the design

- 5.2 item 5 and conformance item 7: the io_uring answer is recorded as "no on Linux 7.0", and item 7
  runs on `/dev/fuse`; on io_uring it checks that a restart is refused (the mount is served by
  `/dev/fuse`), not that it survives.
- Row T-F6: takeover is `/dev/fuse` only.
- A new farm kernel reopens the question: a kernel that adds a ring-aware RESEND (walking
  `queue->fpq`, or re-sending to a newly registered entry) is a new pinned header, and this spike is
  read again against that kernel's source.
