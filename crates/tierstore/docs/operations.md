# Operations

## Starting

Start the unit by hand (see [install](install.md)). Before it opens the proxy or binds the address, the process runs the start-up self-test (below). When it is ready it prints one line on stdout:

```text
tierstore <version> listen=192.0.2.10:9000
```

With the OCI front on, the line gets ` oci-listen=<address> (https)` or ` (http)` after the listen address. With more than one address of a kind (on main, not in v0.0.41) the value is every address, in the order given, separated by commas with no space: `listen=0.0.0.0:9000,[::]:9000`. The self-test's progress and every other message go to stderr, and so to the journal.

## Probes

Four paths of the S3 listener answer a GET or HEAD without a signature, so a bucket cannot have any of these names.

| Path | Answer | Use it for |
|---|---|---|
| `/healthz` | 200 `ok` while the process runs | liveness |
| `/readyz` | 200 `ready` until the drain begins, then 503 `draining` | a load balancer's health check |
| `/usagez` | 200 with the age of the hot tier's published usage, or 503 with the reason | alerting |
| `/metricz` | 200 with the counters, in the Prometheus text format (see [Metrics](operations.md#metrics)) | scraping |

`/readyz` does not look at the usage: one stopped reconciler would otherwise make every node unready. `/usagez` is for alerting, not for a load balancer.

On main, not in v0.0.41, the 200 of a build node says which mode its cache is in: `ready read-write` or `ready read-only` (see [the cache lock](operations.md#the-cache-lock-and-read-only-mode)). The status is the same in both: a read-only node serves.

## Usage

The reconciler publishes the hot tier's usage every pass. `/usagez` says:

```text
usage published <N>s ago
no usage yet, <N>s since start
usage of the hot tier is stale: published <N>s ago, limit <M>s
usage of the hot tier is missing: none published in <N>s, limit <M>s
```

The first two are 200 and the last two 503. The limit is `--usage-stale-after` (`hot_store.usage_stale_after`, 2m by default). A server that starts before the reconciler's first pass is 200 for that long. Puts ignore a usage older than 30 seconds, as if the hot tier were not full, so a stopped reconciler means a hot tier that is never taken to be full. Puts are not refused; alert on `/usagez`.

## Eviction and the water marks

The hot tier's size is measured against `--hot-bytes` (`hot_store.budget`). Three shares of it matter:

- Above `--high-water` (80% by default) the reconciler evicts the oldest objects that the cold tier already holds, until the hot tier is down to `--low-water` (70%). It never evicts an object it has not copied.
- Above `--critical` (95%) a read of an object that is only in the cold tier is served but not copied back into the hot tier.
- At the budget, or when the hot tier says it is full, new objects are written straight to the cold tier.

Eviction frees the hot tier only as fast as the cold tier takes copies, which `--copy-parallel` bounds. If the cold tier is slow or down, copies stall, nothing becomes evictable, and the hot tier climbs to the budget.

## The reconciler's lease

One reconciler works at a time: the one that holds `_tiering/lease` in the hot tier. It lists `_entries/`, copies what the puts leave to the cold tier, finishes the deletes handed over to it, copies a cold object into the hot tier for each copy-back a read asked for, evicts, reaps the entries of owners that departed, and publishes the usage. Beyond the lease and the usage it publishes it keeps nothing in the tiers: what it knows it reads again from the entries, so a reconciler that is stopped or loses the lease loses nothing but its place in a listing.

While it works, the holder renews the lease on a timer of its own, every third of `--lease-ttl`. A step that has less than a third of `--lease-ttl` left on its lease (the margin) does not start a copy to the cold tier or a delete, and the item waits for the next step. The lease is not a fence: a step that loses the lease (a renewal failed because the hot tier did not answer it, or another reconciler took the lease) may still finish a copy beside the new holder. The copy is create-only and the entry is changed on its content tag, so the new holder finds what was done and does not do it twice.

Another reconciler takes the lease over when it has seen the lease object unchanged for `--lease-ttl` by its own monotonic clock, counted from the first time it looked. The clocks of two machines are never compared, so they may differ by any amount. A process that has just started therefore waits up to `--lease-ttl` before it takes a lease that a crashed holder left; a holder that stops normally gives the lease up, and the next step of another takes it at once.

An eviction deletes on the content tag the copy found (`If-Match` where the store enforces it), so an object that was put again with other bytes after the copy is kept. A reconciler that took over from another has no tag for the objects the other copied: it first checks that the cold tier holds a real object of the key with the same size and the same bytes as the hot object. The bytes are compared (both objects read in ranges of at most 1 MiB, each read with its content tag, which must be the one the delete uses, so a key replaced and put back around the read is not deleted), and not the tags across tiers, since tiers of different kinds tag the same bytes differently. If it does not, the hot object stays, since it may be the only copy, and it remains a candidate that the next step asks about again. A tag names content: an object put again with the same bytes has the same tag.

## Reservations

A reservation is an object under a key in the cold tier that holds the key without being an object of yours: 56 bytes, marked by the user metadata `tierstore-reservation` (on a `dir` tier, by the owner-execute bit of the file). It exists so that the cold tier, which every proxy shares, decides which of two concurrent puts of a key wins. This release only reads around reservations; **nothing in it writes one**, so a cold tier written only by this release holds none.

What this release does with one that is there:

- A read, a head and a listing do not show it: a get or head is `NotFound` (counted as a miss of the cold tier), a listing leaves it out (a cold object of 56 bytes that the hot listing does not show costs one HEAD at every listing: a listing gives no version of an object, so no answer is kept), and a build node's cache treats it as absent.
- A delete removes it only if the hot tier holds the key; a bare one belongs to a put in flight and is left.

A raw listing of the cold bucket (`aws s3 ls`) shows reservations as 56-byte objects. Do not delete one by hand while a put runs.

**Upgrade order.** The release that writes reservations must not run beside a release that does not read them: an older reader serves the 56 bytes as the object, and an older listing shows them. Upgrade every proxy, reconciler and build node to a release that reads reservations (this one) before any node runs a release that writes them. The auto-release cannot enforce it, and nothing at run time does; going back to a release before this one after a writer has run is not supported.

## What a full disk does

A `dir` hot tier whose disk is out of space, or an `s3` hot tier that answers 507, is a full hot tier: the put that met it goes to the cold tier where nothing of its body was taken yet, and the tier is taken to be full for a moment. A put that had already streamed some of its body into the hot tier fails, and the client's retry goes to the cold tier. Reads use both tiers throughout. The hot tier is used again once the usage falls. An object written straight to the cold tier has its entry like any other, and the reconciler finds it in the cold tier and has nothing to copy.

If the cold tier is a full disk, puts that reach it fail. A tier that is full at start still starts; the start-up checks that need a write are skipped, with a line that says so. Size `--hot-bytes` below the real capacity of the SSD: the budget is what the process compares with, not what the file system has.

## Drain and restart

On SIGTERM or SIGINT (what `systemctl stop` sends) the process drains:

1. `/readyz` answers 503 at once, and every answer from then on carries `Connection: close`, so that a client does not send its next request on a connection that is about to end;
2. for `--drain-lb-wait` (10s), the settle, the server goes on accepting connections and serving everything, so that a load balancer sees the 503 and a connection that was routed here a moment ago is served, not reset;
3. at the end of the settle it closes each connection that has no request on the wire, and stops accepting connections without resetting one the kernel has already set up for it: it takes and serves the connections whose handshake is over or still in flight, and closes its socket once none is left, 3 seconds at most (always the 3 seconds once the host's network namespace has answered any connection with a SYN cookie, and on any system but Linux, where it cannot see every handshake). Send new connections elsewhere before the stop (the update hook moves its redirect off an instance before it stops it): a connection that comes as the socket closes is reset. A new connection that has not sent its first request yet is given the same 3 seconds to begin it, and that request is served, with `Connection: close`; a `--drain-deadline` shorter than that wins. Each connection it closes gets a lingering close: it shuts its end for writing, so that the client reads a clean end of stream, and goes on reading until the client closes its end, or for 60 seconds at most (the time an idle connection is let to wait for its next request, which has no flag), and then closes;
4. a request that is on the wire at that moment (begun, and not answered in full) is served, with `Connection: close`, never answered 503, and its connection is closed in the same way once the answer is out;
5. it waits for the requests in flight and for the connections to close, at most `--drain-deadline` (15m) in all;
6. it gives up the reconciler's lease and exits 0.

A request that arrives within one round trip of the end of the stream, on a connection that was idle since before the drain began, cannot be answered: the client sent it before it read the end of the stream. The server reads and drops it and counts the connection in `tierstore_drain_late_requests_total`. A client that follows `Connection: close` never meets this; a client that retries such a request must be sure the request is safe to repeat, which a create-only put is not. This drain, the lingering close and the metric are on main, not in v0.0.41; v0.0.41 and before answer every request on a connection that is open after the settle 503 `ServiceUnavailable` with `Connection: close`.

The OCI read front (`--oci-listen`) drains the same way and at the same moments, on main and not in v0.0.41: from the stop its answers carry `Connection: close`, it goes on accepting and serving for `--drain-lb-wait`, a blob being sent when the settle ends is sent in full, and then each of its connections gets the same lingering close, for 60 seconds at most. A connection that lingers still holds its place under `--oci-max-connections`, and one still open at `--drain-deadline` is cut and counted in `tierstore: <N> OCI connections were cut at the drain deadline`. A client that writes on an OCI connection after its end of stream is counted in `tierstore_oci_drain_late_requests_total`, beside the S3 listener's count. v0.0.41 and before answer on the OCI front without `Connection: close` during the settle and close its connections with a plain close at the end of it, which can reset a client that sent its next request.

A request cut at the deadline is counted on stderr: `tierstore: <N> requests were cut at the drain deadline`. So, on main, are the connections that sent a request too late, as the process exits: `tierstore: <N> connections sent a request too late in the drain of the S3 front`, and the same with `the OCI front`, a line only for a front that counted one. These are the last values of `tierstore_drain_late_requests_total` and `tierstore_oci_drain_late_requests_total`, which count connections: they can rise from the start of the drain, but reach their last value after the settle, when `/metricz` has closed with the S3 listener. Another process over the same tiers takes the reconciler's lease once it is given up or has expired (`--lease-ttl`). State is in the tiers, not the process, so a restart needs nothing but its flags.

To restart for a new binary or a new key file, replace the file, then `systemctl restart tierstore`; run more than one process, with ids of their own, if clients must not see the pause. The update hook (`tierstore update-hook`) does that for a host updater; see [the update hook](update-hook.md).

On main, not in v0.0.41, `--lock-file` (`hot_store.lock_file`) says which of the processes over one node reconciles. A process that holds an exclusive lock on the file runs the reconciler and takes the lease. A process that finds the lock held serves reads and writes through its own proxy at once and does not touch the reconciler's state in the hot tier, the lease included, until it holds the lock; it tries every second. The system frees the lock when the holder exits or crashes, so a process started beside the one that is draining reconciles as soon as that one has exited and given its lease up. Give both processes the same file, on a local file system, in a directory that nothing removes: a directory under `/run` that systemd removes with the unit would let a third process make a new file and lock it beside the second. The holder looks before each pass at whether the path still leads to the file it locked; if the file was replaced, it gives its lease up and waits for the lock on the new file. A start whose lock file cannot be opened, is a link or is on a network file system exits 1. Without the flag the reconciler works as soon as the process starts, as before.

## The format object and the pause of writes

The hot tier's object `_tiering/format` says what a proxy may do. A proxy reads it at start, after each heartbeat and, at most every 5 seconds (the proxy's `format_refresh`), before a write; a read that fails leaves what it last read. A read is never paused.

- `{"epoch":2,"phase":"active"}`, or a later epoch: writes are made on entries (below).
- `{"phase":"drain"}` or `{"phase":"drained"}`: every proxy answers a put, a delete and the multipart calls that write with 503 `ServiceUnavailable` and the text `writes are paused while the store changes format; retry later`, and goes on serving reads.
- No object, an `active` with no epoch or an epoch below 2, or a phase this release does not know: writes are answered 503 with the text `writes wait until the format object says active at epoch 2 (tierstore format-epoch --activate); reads are served`.

So a store this release has not been activated on takes no write, and a new store is activated once: `tierstore format-epoch --activate --apply` (see below). A node whose hot tier is `memory` is the exception: the tier is new with the process and has no earlier release to drain, so the node writes the active format into it at start.

The verb `tierstore format-epoch` (below) writes and removes this object, with checks.

The release that reads this object (the bridge release) must run on every node before anything writes `_tiering/format`: a node of an earlier release does not read it, and goes on writing.

Once the object says `{"epoch":2,"phase":"active"}`, a proxy makes every put on the journal entry of the key (`_entries/<sha256 of the key>` in the hot tier): a put, a streamed or presigned put, and the completion of a multipart upload. The entry decides which of two puts of one key wins; the loser is answered 503 while the winner is in flight, and 412 once it has landed. A put of a new key is answered 503 while the cold tier cannot be asked, because the cold tier is where "this key does not exist yet" is decided. A proxy puts on entries only while its heartbeat is acknowledged. How many times a put looks again at an entry that vanished between its create and its read is the `entry_rounds` field of `ProxyConfig` (3), not a flag of the binary yet. A delete is made on the entry too: the cold tier's object is deleted at once and then the hot tier's, each call once. A delete that finds no entry removes its own entry afterwards. A delete that finds a landed put or a requested copy-back hands the entry to the reconciler (`(DEL, PENDING)`) and is acknowledged. A delete that finds a put the reconciler is copying, or a delete handed over (a retry of one that failed), hands the entry over and then waits until the reconciler has released it, so that a copy that was in flight cannot outlive the delete; if the entry is still there after the wait it answers 503 (both tiers are deleted, and the retry waits again). A delete that finds a copy-back the reconciler has sealed changes nothing yet and waits in the same way until the entry is released, and then deletes both tiers; if the entry is still there after the wait it answers 503 and nothing was deleted. A delete that finds a put or a delete of another client in flight, or any other sealed entry, is answered 503 and changes nothing. The wait is the `delete_wait` field of `ProxyConfig`: 30 seconds in all, reading the entry after sleeps of 1 second doubling to 10 seconds (`wait`, `poll_min`, `poll_max`), not flags of the binary yet. The reconciler of the same release works the entries from the same moment (copies what a put leaves, finishes a handed-over delete, evicts, and does a copy-back), as [Rolling out the entry protocol](operations.md#rolling-out-the-entry-protocol) says.

## The write pause of a format change: format-epoch and entries

Two verbs of the binary run the one planned pause of writes and look at the journal entries of keys. They are on main, not in a release yet. Each is a short-lived process, answered before the server's flags are read, that takes the hot tier by the server's flags (`--hot`, `--hot-endpoint`, `--hot-region`, `--hot-credentials-file`, `--s3-request-timeout`). They read and write only tierstore's own objects in the hot tier (`_tiering/format`, `_tiering/proxies/`, `_entries/`); they never ask a store how it behaves, so what the stores must do is documented, not checked. The credential they run with needs create, read, replace and delete on those prefixes.

`tierstore format-epoch` takes exactly one of four actions. Run from a machine that is not a proxy, with the same clock discipline as the proxies, since a heartbeat's age is the difference of that machine's clock and the hot tier's time of the write.

- `--check` prints the format object (its phase and epoch), each registration with how it is classified, and the number of open entries, then says whether `--drain` would be accepted. It writes nothing. It exits 1 with the refusal if `--drain` would be refused.
- `--drain` writes `{"phase":"drain"}` and waits, with no time limit, until the format says `drained`, reading it every `--poll` (5 seconds) and printing what it waits for: the reconciler of the bridge release (the one before this release) that holds the lease writes `drained` once every journal is applied. This release's reconciler never writes `drained`, so `--drain` is for a store that still has journals to apply, with the bridge release running. Interrupt it to stop waiting; the pause stays. Run it again to wait again: a format that already says `drain` is not written over. If the format is at an epoch, the epoch is kept.
- `--cancel` resumes writes: it deletes the format object, or, if the format was at an epoch before the drain, writes `active` with that epoch back. It refuses when the format is not `drain` or `drained`.
- `--activate` writes `{"epoch":2,"phase":"active"}` over `drained`, after the checks below. On a store that has no format object (a new store) it creates the object instead, and refuses if the store has a `_journal/` object of a release before the entries: such a store is drained by the bridge release first. It does not rotate the write credential; it prints that the operator does, and that processes of the new release get the new credential first.

`--drain`, `--cancel` and `--activate` are held: without `--apply` they make every check, print what they would write, and write nothing. A write is over the content tag that was read, so a format object changed by someone else in between is refused, not overwritten.

A registration (`_tiering/proxies/<id>`) says whether its process is alive only if its body has a protocol number and the object is rewritten on every beat. A registration is live when it has a protocol number and was written less than `--beats` (3) times `--beat` (5 seconds) ago, silent when it has one and was not, and unlabelled when its body has none, as the registration of a release before the heartbeat. A silent or unlabelled registration says nothing of whether its process is gone, and a process of an old release that goes on writing during the pause is what the pause is meant to rule out. So `--drain` and `--activate` refuse until the operator has looked at the machines and named each such id on `--departed` (repeat the flag or separate ids by commas). `--departed` refuses an id that has no registration and an id that is live. `--activate` also refuses a live registration below protocol 2.

Before the pause: tell the owners of the clients that write (builds, image pushes), since writes answer 503 for as long as the drain takes, which grows with the number of keys to copy and does not end while the cold tier is down. Run `--check` first, with nothing at stake. After `--drain` has printed `drained`, roll the new release over the proxies, then `--activate`. To go back before `--activate`, `--cancel`.

`tierstore entries ls` prints a line for each open entry (`_entries/<sha256 of the key in hex>`): the name, the operation (`PUT`, `DEL`, `CB`, `EVICT`), the state (`HELD`, `PENDING`, `SEALED`), the owner (`-` for none), the whole seconds since the entry was last written, and the key, quoted and escaped. An entry whose body cannot be read is listed with the reason. `tierstore entries show --key <key>` prints the entry of one key, or says there is none. Both only read.

## Rolling out the entry protocol

This release has the entry protocol only: the per-node journals, their checkpoints and the journal reconciler are gone. It reads no `_journal/` object and writes none, and it takes no write until the format object says `{"epoch":2,"phase":"active"}`. So it is rolled over a store after the bridge release has drained the journals, and the store is activated once. There is no canary, because the epoch is one object that every node reads: the whole fleet changes protocol at once, after one planned pause of writes.

What a node of this release does about the epoch:

- At start, and then after every heartbeat (`--heartbeat`, 10 seconds), a proxy reads `_tiering/format` and keeps what it says. A write or a read that finds the kept answer older than 5 seconds reads it again. So a node takes up the epoch within a few seconds of its being written, a node that starts after it is written takes it up at its first beat, and a node that cannot read the object keeps what it last read.
- Its registration says protocol 2 (the release before says 1). `--activate` refuses while a live registration says less, so a node still on the old release is found before anything is written.
- Its reconciler does nothing, and takes no lease, until the epoch is active. From then on it works the entries: a round, then the next at once while a round changes something, and otherwise after a wait of one second that doubles up to ten while nothing is to do. The wait is not a flag: `--reconcile-interval` is gone. A hot object that an old journal described and no entry names is adopted by the stray sweep once it is older than ten minutes.
- The settings of the entry reconciler (its waits, retry delays, sweep and usage intervals, copy-back limits) and of the entry puts and deletes (looks at an entry, the delete's wait) are fixed to their defaults in this release; `--departure`, `--lease-ttl`, `--copy-parallel` and `--hot-bytes` apply to them. Keys for the rest come in a later release.

The order, once, in a window agreed with the owners of the clients that write:

1. Before anything: the pre-flight of the design. A lifecycle rule `AbortIncompleteMultipartUpload` on the cold bucket; the hot credential allowed `_entries/*` and `_tiering/proxies/*`; on a versioned hot bucket, expire noncurrent versions after one day, since every put leaves a created and a deleted entry. Every node runs the bridge release (it reserves `_entries/`, heartbeats and answers `drain` with 503). Run `tierstore format-epoch --check` and read it.
2. Tell the owners of the clients that write. Writes answer 503 from the next step until step 5, which takes as long as the journals take to apply and does not end while the cold tier is down; reads are served throughout.
3. `tierstore format-epoch --drain --apply`. It waits until the format says `drained`, which the reconciler of the bridge release writes once every journal is applied. Name on `--departed` each registration the check lists as silent or unlabelled once you have looked at its machine.
4. Roll this release over every node (the update hook hands the sockets over, so no connection is refused). A node of this release serves reads and answers writes 503 while the format says `drained`.
5. `tierstore format-epoch --check`, then `tierstore format-epoch --activate --apply`. Within seconds every proxy puts and deletes on entries, and writes are accepted again. This ends the pause.
6. Rotate the write credential as `--activate` prints, so that a process of an older release that is restarted later cannot write: new-release processes get the new credential first. An old binary started after the epoch writes journals nobody reads; that is not supported.
7. Watch `tierstore entries ls`: entries appear while a put or a delete is in flight and are gone within seconds. An entry that stays names its owner; one whose owner has departed is reaped by the reconciler after `--departure`.

A new store has no journal to drain: install this release, run `tierstore format-epoch --activate --apply` once, and the nodes take writes within seconds.

To go back before step 5, `tierstore format-epoch --cancel --apply` and roll the bridge release back over the nodes: a node of this release answers writes 503 while the format is not active. Going back after step 5 is not in this release.

**The old objects.** The `_journal/<proxy id>/` objects and the `_tiering/checkpoint/` objects of the releases before stay in the hot bucket, untouched, so that a rollback to the bridge release still finds them; they only take space. The release after this one is to delete them. Do not delete them by hand before then.

## The start-up self-test

Before it serves, the process asks each tier, under a probe key in `_tiering/selftest/` that is new for each start and deleted afterwards, the questions its guarantees rest on: a create-only put refuses a key that exists, an `If-Match` put replaces only on the right content tag, a range read gives the range, a head has `Last-Modified`, a listing honors `start-after`, and on the cold tier a multipart completion is create-only. A tier that answers one wrongly exits the process with status 3, naming the tier, the check and the answer. A tier that cannot be reached is asked again, with pauses from one second doubling to thirty, for `--backend-check-wait` (5m), and then the start exits 1.

After the checks an S3 tier is asked one more question, which is not a check and never fails the start: does the store enforce `If-Match` on a DELETE? The probe writes a key under the same prefix, sends a DELETE with a wrong tag, reads the key, sends a DELETE with the right tag, reads it again, and deletes it. The tier writes one line to stderr, `tierstore: <tier>: conditional delete: if-match` when the store refused the wrong tag and took the right one, and `tierstore: <tier>: conditional delete: head-then-delete` when the store answered and did otherwise, for a store that takes the header and deletes anyway (some S3-compatible stores do) or one that refuses it. When a request of the probe failed, so that the store was not asked (a 503, a timeout, a dropped connection), the line is `tierstore: <tier>: conditional delete: head-then-delete (could not ask: <error>)`: the tier works the same way as in the line above, but the store may enforce the header, and a restart asks again. A conditional delete is then one DELETE with `If-Match` in the first case, and in the second a HEAD, a comparison of the content tags and a plain DELETE, so another writer's replacement of the key between the two requests would be deleted. A directory or in-memory tier compares and removes in one step and writes no line. The proxy and the reconciler make conditional deletes: an eviction deletes the hot copy, and a reconciler removes a cold object, on the content tag it found. A guarantee that rests on conditional deletes holds only once every process over the tiers runs a version that makes them; during a rolling upgrade a process of an older version deletes without a tag.

The same prefix can hold probe keys that a start which was killed left behind. A start deletes the ones older than an hour, at most 100 keys each time, and no new delete starts after 10 seconds; a request already sent can take up to `--s3-request-timeout`. When it has run it writes one line to stderr: `tierstore: <tier>: swept N stale probe keys in T ms`, with `(more left)` when a limit stopped it; the rest is deleted at a later start. If the sweep cannot read the time of the store or list the keys, it writes a `could not …` note instead of that line. A storage node does this after the checks and before it binds its address, so it can wait about those 10 seconds for each tier, plus the request in flight. A build node with `write_through` does it after it has printed its ready line, beside the server, so the sweep never delays readiness.

`--skip-backend-check multipart-conditional=<reason>` leaves out the last check, for a store you have shown to be safe another way; the reason is written to stderr at every start. No other check can be skipped.

## A build node

A build node (the file has `[local_cache]`, see [configuration](configuration.md#a-file-for-a-build-node)) serves the S3 front as a read cache. What it keeps: a key is kept if the longest `[[policy]]` prefix that holds it says `cache = true` (and it is not under `_journal/`, `_tiering/` or `_entries/`). A GET or HEAD of a kept object is answered from the disk with no call to the upstream. A GET of one that is not there is read from the upstream in chunks of 8 MiB into a staging directory, hashed as it passes, and linked into place only when it is whole; the misses of one key share one fill. A ranged GET of a miss fills the whole object if it is at most 64 MiB, and otherwise passes the range through and keeps nothing. A fill that fails never fails the reader for the cache's reasons: the reader is answered from the upstream. Every call but GET and HEAD is refused 403 `AccessDenied`.

Put `cache = true` only on prefixes whose objects are never rewritten, the content-addressed ones: a kept object is never checked against the upstream again. Anyone who may read through the front may read what the upstream's key can read, so give clients scoped keys.

Memory is bounded for the fills: at most `local_cache.max_concurrent_fills` chunks of 8 MiB are held. A miss that finds every slot taken is served from the upstream and kept nowhere; if the count of such skips climbs, raise the key. A response is held whole in memory, as for any backend.

Eviction keeps the cache under `local_cache.budget`: above `high_water` percent of it, or when the free space of the file system is under `free_floor` percent, a pass (every 10 seconds) removes the least recently used objects down to `low_water` percent and until the free space is back at the floor. The free-space floor only triggers a pass; it does not stop a fill. An object that another tree has linked is skipped, counted and stays charged to the budget. A fill is skipped when the object is larger than the budget or would take the cache above `critical`.

At start the process makes the directory, removes what a fill left half done, reads the kept objects back (sizes only; an object whose sidecar does not say what the file holds is deleted), and asks the file system four questions with a probe object. A file system that answers one wrongly exits the process with status 3, naming the check. It then asks the upstream for a key that is not there: 404 or any object is an answer, and anything else exits 1. On SIGTERM it drains as a storage node does.

### The cache lock and read-only mode

On main, not in v0.0.41. Two processes may have one cache directory open at once, for example a new binary started beside the one that is draining. Only one of them writes to it. The directory has a lock file, `.lock`, and the process that holds an exclusive lock on it is read-write: it fills, keeps, evicts and records the use of objects. A process that finds the lock held is read-only. A lone process takes the lock before it binds the address, so it is read-write from its first answer; the system frees the lock when the holder exits or crashes.

A read-only process serves what its scan of the directory found, from the disk, and sends every other read and every write to the upstream. It makes and removes nothing in the directory and stamps no file: it does not empty the staging directory or delete a half-published object at start, skips the start-up probe of the file system, deletes nothing when a read of a kept object fails (the read is answered from the upstream), and starts no fill, no keep of a put, no eviction pass and no write of file times. A create-only put that reaches it with `write_through` goes to the upstream and succeeds, with no copy made in the directory; it is counted as `read_only` in `tierstore_cache_write_keep_skipped_total`. An object the holder keeps after the scan is read from the upstream until the process takes the lock. The counters of fills and evictions (`fills_total`, `evictions_total`, `fill_failures_total`) stay at 0 in a read-only process; hits, misses and bytes count.

With `write_through`, a delete or put sent to a read-only process goes to the upstream, and from then on the process reads that key from the upstream. The files the holder has for it stay on the disk, because the process may not remove them; the process notes the names, at most 65536 of them. When it takes the lock it removes those files after its scan and before it is read-write, so a deleted or replaced object is never served from the files the holder left. If more than 65536 keys were deleted or replaced, the process does not know which, and it removes every object file when it takes the lock and starts with an empty cache. One window is left: the holder itself still serves its own cached copy of such an object on the connections it is draining, until it exits. It ends with the holder's drain (`--drain-deadline` at most).

A cache that loses its lock (the lock file was replaced, see below) stops writing at once: a fill or the copy of a put (a keep) that is running publishes nothing, and the put is counted as `read_only`, and neither touches the staging files of the process that holds the directory now, which are named with a prefix of their own. The loss is noticed by the one-second try, so for up to that second a fill or a keep still believes it owns the directory. A fill or keep that has linked its object into the directory when it finds the loss removes the data file and the sidecar it linked (they are its own: no other process makes those names), so the object is not left where the new holder does not index it. What is left of the window is a whole, valid object that is on the disk for the moments until that removal.

The process tries the lock every second. When it takes it, it empties the staging directory, scans the directory again (deleting what is not a whole object) and only then is read-write; stderr says so, and the first line of the start says which mode it started in. `/readyz` says the mode in its body and `tierstore_cache_mode` in the metrics (see [metrics](metrics.md)); the status of `/readyz` does not depend on it. A holder looks at each try whether `.lock` still leads to the file it locked: a file that was removed or replaced is a lock that no other process looks at, so the holder lets it go, is read-only, and takes the new file when it can. A lock file that is a link, or on a network file system, stops the start (exit 1); a try that fails later is said on stderr and made again.

## A storage node with a local cache

On main, not in v0.0.41. A file with `[hot_store]`, `[cold_store]` and `[local_cache]` (see [configuration](configuration.md#a-file-for-a-storage-node-with-a-cache)) runs one process that owns the data and keeps a disk cache in front of its own proxy: the S3 front and the OCI front read through the cache, and the cache reads from the proxy. What the cache keeps is as for a build node: a key under a `[[policy]]` with `cache = true`. A hit is answered from the cache directory with no call to either tier (the hot tier is skipped); a miss reads through the proxy, which reads the hot tier and, for an object only the cold tier has, the cold tier. The cache fills with ranged reads, and a ranged read of an object only the cold tier has is served from the cold tier and not copied up; the copy-up described above is for proxy reads without a range.

Writes pass through the cache to the proxy, so its entries stay right: a create-only put goes to the proxy, which answers `AlreadyExists` for a key either tier has, and under a `cache = true` prefix the object is also kept; a presigned put goes to the proxy as a hot-only put, so the one-winner rule holds; a delete drops the cache's entry, deletes through the proxy (both tiers and the journal) and drops the entry again; a multipart upload passes to the proxy and is not kept (the first read fills it). The cache's versions of an object are compared by size, because an object's tag is the tag of the tier that answers and moves when the proxy copies it between tiers.

The start tests both tiers as a storage node does, opens the proxy, then opens the cache directory: its own lock, the file-system probe if this process holds the lock, and the budget against the file system less `free_floor`. It does not run the cache's write self-test of an upstream (its upstream is the proxy). The cache has its own lock, `<path>/.lock`, beside the reconciler's `lock_file`; a process started beside a draining one serves at once from the proxy, its cache is `ready read-only` until the holder exits (see [the cache lock](operations.md#the-cache-lock-and-read-only-mode)), and its reconciler waits for the lock file or the lease. Put the cache path in a directory of its own, not inside either tier's directory, and, with a `dir` hot tier, on another file system: the cache then does not duplicate the hot tier's blocks on one disk, and its `free_floor` eviction does not react to the hot tier filling it. The loader checks the paths by component, with `.` and `..` resolved by text and not through links; the cache path and the directory of a `dir` tier must be absolute.

The delete and one-winner guarantees of a delete or a put that races a copy-back depend on the fixes to the proxy and the reconciler that make a delete conditional on what it found. They hold once every process over the tiers runs a version that has those fixes; while processes of different versions share the tiers, they do not.

## Metrics

`GET /metricz` on the S3 listener (unsigned) answers 200 in the Prometheus text format. On main it has the hit, miss, byte and cold-read counters of the hot and cold tiers, the counters of the OCI read front, such as `tierstore_oci_requests_total`, and the counters of the cache of a build node, all named `tierstore_` and a word; without `--oci-listen` it says the front is not enabled, and a node with no cache says so on a line of its own. The tier counters are on main and not in v0.0.40. The full list of metric names, their labels and queries to alert on is in [metrics](metrics.md).

## Handing out read URLs

A job that holds no key reads its objects with presigned GET URLs. The service that starts the job signs one URL for each object with a key that may read (a scoped key limited to the job's prefix is the usual choice; see [keys](keys.md)) and gives the job the URLs. Each URL is good for at most `--presign-max-expires` (15 minutes by default), so sign it when the job starts, and again for a job that runs longer. To fetch a large object in parallel, the job sends several `GET`s of the one URL, each with its own `Range` header, and puts the parts together: the range is not signed, so every part passes. A URL is a bearer credential; do not log it, and use TLS where the path is not trusted. A URL the server refuses is always a 403, and its code says why: `AccessDenied` (the URL has expired, its method is not GET or HEAD, it asks for more than one object or carries a parameter beyond the SigV4 ones and `x-id`, `host` is not a signed header, the key may not read the object, or the request has no signature at all); `InvalidAccessKeyId` (the key id is unknown, is a GC key, or the region is not the server's); `SignatureDoesNotMatch` (a wrong secret, or the URL was changed after it was signed, its method and path included); `AuthorizationQueryParametersError` (`X-Amz-Expires` is zero or over `--presign-max-expires`); `RequestTimeTooSkewed` (`X-Amz-Date` is more than 15 minutes ahead of the server's clock); `AuthorizationHeaderMalformed` (a SigV4 parameter is missing, repeated or not well formed). The first thing to check is the clock of the signing service against the server's.

## Not in a release yet

- **Waiting for a put slot** (on main, not in v0.0.41). A put over `--max-concurrent-puts` waits up to `--put-permit-wait` (15 s) for a slot before it is answered 503 `SlowDown`. In v0.0.41 it is answered 503 at once.
