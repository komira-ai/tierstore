# tierstore metrics and PromQL reference

This page lists every series tierstore exports, says exactly what increments each one and what it
does not count, and gives the PromQL queries used to measure tierstore. It is written for operators who scrape a node and for anyone who repeats those measurements.

Every series is on `GET /metricz`, on the S3 listener, in the Prometheus text format. The path is
not signed, like `/healthz`, and a bucket cannot be called `metricz`. Point a scrape job at it:

```yaml
scrape_configs:
  - job_name: tierstore
    metrics_path: /metricz
    static_configs:
      - targets: ["localhost:9000"]
```

## The two deployments

**v1: a build-node cache in front of the cold store.** One tierstore process runs on a build node
with a local directory as a read-through cache over one upstream tierstore. It has no hot tier, no
entries and no reconciler. A read the cache holds is answered from disk; any other read goes to
the upstream, and a key the cache is set to keep is filled into the directory on the way. This is
the only tierstore hop in v1, and its series describe the cache's own work and its calls to the
upstream.

**v2: storage nodes with hot and cold tiers.** A storage node runs a proxy over a hot store and a
cold store. A read asks the hot tier first, and on a miss the cold tier. A get without a range that finds the
object only in the cold tier reads the whole object, gives it to the client as it arrives, and, when the hot tier has room, asks for its copy into the hot tier on the entry of its key (the reconciler makes the copy, so the proxy counts none); a ranged get reads only its range and asks for nothing. Its series are one labelled family per measure, with the tier as a
label, so one query serves every tier.

A process runs a storage node, a build-node cache, or both. A combined node (`[hot_store]`,
`[cold_store]` and `[local_cache]` in one file) exposes both blocks, the proxy's and the cache's, on its
`/metricz`. A process without one component has none of that component's series. A series that is
absent has not been counted at zero: a query over it returns nothing, not 0.

## Reading the series

- Every series except the gauges named as such is a counter since the process started. It never goes down while the process runs, and it restarts at 0 when the process does. Use `rate()` or `increase()` over a window at least four scrape intervals long; the queries on this page use `5m` for a 15 s scrape.
- Labels have a few fixed values each. None is a key, a bucket or a client, so the number of series stays small however busy the node is.
- Sum a ratio's numerator and denominator separately (`sum(rate(a)) / sum(rate(b))`), never average per-series ratios.

## The build-node cache (v1)

Every name in this section starts with `tierstore_cache_`; the text below shortens a name to what follows the prefix (`hits_total` for `tierstore_cache_hits_total`) and the tables give it whole. The text of the cache's series has no `# TYPE` line, so Prometheus stores them as untyped; `rate()` works on them anyway. An S3 GET reaches the cache as a HEAD (for the size and the conditions) and then a GET. These series count the GET, so a GET of a missing key, which the HEAD before it answers 404, is not in them.

### Cache reads and bytes

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_hits_total` | counter | none | a GET of an object the cache holds is answered from disk, whole or ranged |
| `tierstore_cache_misses_total` | counter | none | a GET of a key the cache is set to keep is not answered from disk: the key is not held, or its file could not be read. |
| `tierstore_cache_bytes_served_local_total` | counter | none | bytes of the answers that counted as hits |
| `tierstore_cache_bytes_served_upstream_total` | counter | none | bytes of every answer that was not a hit: a pass-through read, and the read from disk that follows a fill |

What they do not count:

- A fill that the file system asks of the cache (see [operations](operations.md#fills-for-the-file-system)), in this process, is counted as any fill is: in `fills_total`, in `fill_failures_total` under its reason, in `fills_skipped_total` when it finds no room, and in `upstream_reads_total` and `upstream_bytes_read_total`. It is a read of no client, so it counts no hit, miss or bytes served.
- A key the cache is not set to keep (outside every keep policy, or under a reserved prefix) is passed through to the upstream. It counts bytes under `bytes_served_upstream` and is neither a hit nor a miss.
- A miss counts once for each reader. Readers that share one fill each count a miss, and the fill counts once.
- A ranged GET of a key that is not held first asks the upstream for the size. If that HEAD fails (the key does not exist, the upstream is down) the GET counts no miss. A ranged GET of an object over the whole-object limit counts a miss and is passed through: it fills nothing and counts bytes under `bytes_served_upstream`. Any other GET of a key that is not held counts its miss before the fill starts, so a failed fill still shows as a miss. A file that could not be read from disk is a miss on the paths that count one.
- A range past the end of an object the cache holds is an error of the client: it is neither a hit nor a miss.
- A GET of a missing key never reaches the GET: the HEAD before it answers 404. So the hit and miss counters are rates of GETs that went on to read data.
- Bytes are counted when the cache hands them to the front, so a client that disconnects while they are sent still has them counted.
- `bytes_served_upstream` means "answered after going upstream", not "fetched". The read that follows a fill is served from the file the fill just wrote, and counts here although it came from disk. The bytes the cache fetched from the upstream are not exported yet.
- A ranged miss of an object at or under the whole-object limit fills the whole object, then answers the range. Its bytes served are the range; it fetches the object.

### Cache fills, failures and eviction

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_fills_total` | counter | none | a fill wrote a new object into the cache |
| `tierstore_cache_fills_skipped_total` | counter | none | a fill was not started or not kept and nothing went wrong: every fill slot was running, or there was no room |
| `tierstore_cache_fills_skipped_busy_total` | counter | none | of the skipped fills, those that found every fill slot taken |
| `tierstore_cache_fill_failures_total` | counter | `reason`: `hash_mismatch`, `upstream_error`, `short_body`, `local_io`, `changed`, `aborted` | a fill ended without leaving an object in the cache |
| `tierstore_cache_evictions_total` | counter | `reason`: `budget`, `floor` | an object was evicted |
| `tierstore_cache_pinned_skips_total` | counter | none | an eviction pass passed over an object because it has more than one hard link |
| `tierstore_cache_pin_skips_total` | counter | none | an eviction pass passed over an object because a tree pins it (see `local_cache.pin_limit`) |
| `tierstore_cache_scrubbed_objects_total` | counter | none | the scrub hashed an object again |
| `tierstore_cache_scrubbed_bytes_total` | counter | none | the bytes the scrub read for those objects |
| `tierstore_cache_scrub_mismatches_total` | counter | none | the scrub dropped an object whose bytes no longer matched its digest |
| `tierstore_cache_scrub_errors_total` | counter | none | the scrub could not judge an object because a read failed, or its check panicked |
| `tierstore_cache_discarded_at_start_total` | counter | none | a file was removed at start: a file the scan did not find whole, or what a fill left in the stage directory at the last stop |

The reasons of a failed fill:

- `hash_mismatch`: a key named by its content hashed to another name. The readers get an error and nothing is kept.
- `upstream_error`: the upstream failed with anything but "not found". A fill whose object does not exist counts nothing here.
- `short_body`: the upstream gave fewer bytes than the size it had said. The readers are served from the upstream.
- `local_io`: the cache directory could not be written.
- `changed`: the object was written over while it was read. Only keys not named by their content are checked for this.
- `aborted`: the fill task panicked or was cancelled before it ended. The readers that waited get a 503, and the failure counts once for the fill.

`fills_skipped_total` and `fill_failures_total` do not overlap: a short body, a local write error
and a changed object are failures, not skips. `evictions_total` counts objects, not bytes, and
`budget` is the share of an eviction pass that brings the cache under its low water mark while
`floor` is the share that restores the free space of the file system. `pinned_skips_total` goes
up once for each object with more than one hard link that a pass reaches, and `pin_skips_total`
once for each object that a tree pins (`local_cache.pin_limit`) that a pass reaches. A pass walks the
objects oldest first and stops once it has freed enough, so it does not reach every such object,
and each keeps rising while an object that the passes reach stays linked or pinned.
`discarded_at_start_total` is a counter that is set once, when the cache is opened, and stays
the same for the rest of the process.

### Cache gauges

These four are gauges, measured when `/metricz` is read.

| name | type | what it is |
|---|---|---|
| `tierstore_cache_bytes` | gauge | bytes of objects in the cache |
| `tierstore_cache_objects` | gauge | objects in the cache |
| `tierstore_cache_fs_free_bytes` | gauge | free bytes of the file system of the cache directory |
| `tierstore_cache_staging_files` | gauge | files in the stage directory now: fills that are running |

### The prefetch queue

Blobs queued to be filled before a read asks for them (see [operations](operations.md#the-prefetch-queue)). A fill of the queue is also counted as any fill is, in `fills_total` and `upstream_reads_total`.

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_hydrate_blobs_total` | counter | `outcome`: `present`, `joined`, `queued`, `too_large`, `busy`, `off`, `read_only`, `too_many_blobs`, `too_many_bytes`, `reserved_key`, `access_denied` | a blob was handed to the queue: the cache held it, a fill of it was queued or running, it was queued, it is larger than the whole budget, the queue was full, `prefetch_concurrency` is 0, or the cache is read-only; or a blob of a tree was not handed over: the call had queued `hydrate_max_blobs` (this ends the walk), the blob would take it past `hydrate_max_bytes` (it is left out and the walk goes on), its `source.key` is under `_journal/`, `_tiering/` or `_entries/` (the walk goes on), or its `source.key` is outside the read prefixes of the scoped key of the call (`access_denied`; the walk goes on) |
| `tierstore_cache_prefetch_fills_total` | counter | none | a worker's fill left its blob in the cache |
| `tierstore_cache_hydrate_trees_total` | counter | `outcome`: `expanded`, `refused`, `cut`, `access_denied` | a tree of a call was walked to its end, was refused (a manifest could not be fetched or read, or the library refused it or the tree it makes), was cut (the walk stopped at a bound of the call, or because its hold ended, or never began because the call had reached one), or was not expanded because the scoped key of the call may not read its manifest (`access_denied`) |
| `tierstore_cache_prefetch_fill_failures_total` | counter | `reason`: `upstream`, `not_found`, `bad_digest`, `no_space`, `changed`, `expired`, `other` | a queued blob was dropped or its fill left nothing: `no_space` is a blob that would take the cache past its high water mark when its turn came (or past `critical` when it started), `expired` one that waited longer than `prefetch_queue_ttl`; a blob the cache held by its turn is dropped and counted nowhere |
| `tierstore_cache_prefetch_promotions_total` | counter | none | a read of a queued blob started it before a worker did |
| `tierstore_cache_fill_joins_total` | counter | `path`: `mount`, `s3` | a fill the file system asked of the cache (`mount`) or a read of the S3 front joined a fill that was running instead of starting one; a read that started a queued blob counts a promotion, not a join |
| `tierstore_cache_fill_socket_waiting` | gauge | none | fills the file system asks of the cache (blob, manifest and range fills) waiting now for a permit of `fill_socket_concurrency`; counts down when the request gets a permit, is answered `busy` or is dropped |
| `tierstore_cache_fill_socket_running` | gauge | none | fills that hold a permit of `fill_socket_concurrency` now (the size of the pool less its free permits) |
| `tierstore_cache_upstream_reads_in_flight` | gauge | none | HEADs and GETs the cache has sent the upstream and not had answered: a GET counts until its body has been read; a call that is dropped counts down. Requests waiting in `tierstore_cache_fill_socket_waiting` while this is 0 for a long time are a stall of the cache itself (see [fills for the file system](operations.md#fills-for-the-file-system)) |
| `tierstore_cache_fill_socket_busy_total` | counter | none | fills the file system asked of the cache answered `busy` because no permit of `fill_socket_concurrency` came free within `fill_socket_wait` |
| `tierstore_cache_prefetch_queue_depth` | gauge | none | blobs waiting for a worker now |
| `tierstore_cache_holds_open` | gauge | none | holds open now (see [holds](operations.md#holds)): opened and not yet released or expired |
| `tierstore_cache_extent_entries` | gauge | none | the packs the cache holds only some ranges of (entries by extents, in `extents-v1/` beside `obj/`); `tierstore_cache_objects` does not count them |
| `tierstore_cache_extent_bytes` | gauge | none | the bytes those entries have filled, each counted once; they are in `tierstore_cache_bytes`, in the budget and in the marks |
| `tierstore_cache_extent_fills_total` | counter | none | ranges filled from the upstream and hashed against the digest the caller gave |
| `tierstore_cache_extent_fill_bytes_total` | counter | none | the bytes those fills added to the cache (a byte a held range has is not added again) |
| `tierstore_cache_extent_fill_failures_total` | counter | `reason`: `upstream`, `mismatch`, `short`, `no_space`, `busy`, `read_only`, `io`, `changed`, `aborted` | a fill of a range left nothing: the upstream failed or has no such pack, the bytes did not hash to the digest, the upstream gave another length, the range does not fit the budget or the critical mark, no permit of the file system's fill pool came free in `fill_socket_wait`, this process does not hold the lock of the cache directory, the directory could not be written, the entry was removed or replaced while it was read, or the task did not end |
| `tierstore_cache_extent_joins_total` | counter | none | a request for a range joined a fill of the same range that was running |
| `tierstore_cache_extent_held_total` | counter | none | a request for a range found it in the cache (an entry by extents or the whole pack) and asked the upstream nothing |
| `tierstore_cache_extent_promotions_total` | counter | none | an entry filled to the whole pack and was moved into `obj/` as an ordinary object |
| `tierstore_cache_extent_evictions_total` | counter | none | an entry was removed by the eviction pass (`tierstore_cache_evictions_total` counts objects only) |
| `tierstore_cache_hydrate_requests_total` | counter | none | a hydrate call was accepted (see [the hydrate call](operations.md#the-hydrate-call)): the hold was opened and the expansion of its trees started |
| `tierstore_cache_hold_handovers_total` | counter | none | trees of a hold whose pins moved to an attachment, at the first pin of a mount for the tree (see [holds](operations.md#holds)); a tree attached before the hold asked for its blobs moves nothing and is not counted |

### The cache mode

A build node's cache is read-write when its process holds the lock of the cache directory and read-only when another process does (see [operations](operations.md#the-cache-lock-and-read-only-mode)).

| name | type | labels | value |
|---|---|---|---|
| `tierstore_cache_mode` | gauge | `mode`: `read_only`, `read_write` | one series for each mode: 1 for the mode the cache is in now, 0 for the other |

In a read-only process the families of fills, evictions and pinned skips stay at 0, `tierstore_cache_staging_files` is 0, and `tierstore_cache_discarded_at_start_total` is 0 (a read-only start deletes nothing); it counts what the process deletes when it takes the lock. `tierstore_cache_bytes` and `tierstore_cache_objects` are the scan the process made until it takes the lock. Two processes over one directory count the same directory and the same file system twice in `tierstore_cache_bytes`, `tierstore_cache_objects` and `tierstore_cache_fs_free_bytes`: read the gauges of the directory from the process whose mode is `read_write`. The counters of hits, misses and bytes served are each process's own, and their sum is right.

### The check of an old hit

With `local_cache.revalidate_after` set, a hit older than it is checked by one HEAD to the upstream (see [configuration](configuration.md)); without it the series stay at 0.

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_revalidations_total` | counter | `result`: `same`, `changed`, `gone`, `failed` | a check of a hit ended: `same`, the upstream holds the version the entry stands for and the hit was served from the disk; `changed`, it holds another version and the entry was dropped; `gone`, it has no such object and the entry was dropped; `failed`, the HEAD failed and the cached copy was served |

### Cache writes

These series count the writes of a cache node that sends them to the upstream (`local_cache.write_through`). Without it, the cache takes no writes and they stay 0.

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_writes_through_total` | counter | none | the upstream took a whole-body create-only or plain put (not a completion) |
| `tierstore_cache_write_exists_total` | counter | none | a put was answered "already exists" |
| `tierstore_cache_keeps_expired_total` | counter | none | a put was answered before the copy kept on the node finished, because the copy took longer than the wait |
| `tierstore_cache_write_keep_skipped_total` | counter | `reason`: `budget`, `io`, `busy`, `head`, `deleted`, `slow`, `deadline`, `read_only` | the upstream took a put and the node did not keep a copy (`head`: the upstream's answer to the put gave no tag and the HEAD after it failed or gave another size; `deleted`: the object was deleted or replaced meanwhile, or another put of the key ran at the same time as a plain put); `read_only` is a node whose cache does not hold the lock of its directory, or lost it while the copy was made (see [operations](operations.md#the-cache-lock-and-read-only-mode)) |

A key that the HEAD before a create-only put finds is answered "already exists" without a put reaching the upstream, and counts in `write_exists_total` as the upstream's own "already exists" does.

## The hot and cold tiers (v2)

The text of these series has no `# TYPE` line, so Prometheus stores them as untyped; `rate()` works on them anyway.

One labelled family per measure. `tier` is `hot` or `cold`, and `op` is `get` or `head`. The
value `local` of `tier` is reserved for the build node's cache.

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_hits_total` | counter | `tier`, `op` | the tier had the object |
| `tierstore_misses_total` | counter | `tier`, `op` | the tier answered "not found" (a hot miss goes on to the cold tier) |
| `tierstore_read_errors_total` | counter | `tier`, `op` | the tier failed with anything but "not found": it is down, refused, or timed out |
| `tierstore_reads_total` | counter | `tier`, `op` | a read reached the tier: its hits, misses and errors added up. For `tier="cold"`, the reads that had to go to the cold store |
| `tierstore_bytes_served_total` | counter | `tier` | bytes of object data that gets answered, by the tier that gave them, after a range is applied |
| `tierstore_bytes_read_total` | counter | `tier` | bytes of object data that gets read from the tier. For `cold`, the whole object for a get without a range, and the range alone for a ranged get |
| `tierstore_promotions_total` | counter | none | always 0: the proxy no longer copies an object into the hot tier (kept so a query on it keeps a sample) |
| `tierstore_promoted_bytes_total` | counter | none | always 0, for the same reason |
| `tierstore_promotions_skipped_total` | counter | `reason`: `critical`, `full`, `raced`, `failed` | a get found the object in the cold tier and asked for no copy-back. Only `critical` counts; `full`, `raced` and `failed` are always 0 |

What they do not count, and how to read them:

- An S3 GET reaches the proxy as one open read (the object's meta and its body in one call), counted under `op="get"` whatever its outcome: a key that exists in neither tier, a conditional request answered 304 or 412 and a range that cannot be satisfied are each a hit or a miss at `op="get"`, and none of them is counted at `op="head"`. So `op="head"` counts the clients' HEADs alone. A GET answered 304 or 412 has counted its hit and its bytes read and served although the body was never sent, and one answered 416 has counted its hit and zero bytes read and served; with the hot tier at its critical mark it counts a `critical` skip, which is counted when the read opens, before the body is known to be unread.
- An error is neither a hit nor a miss. A tier that is down does not lower its hit rate; it shows in `read_errors_total`. A hot-tier error ends the read: the cold tier is not asked, so a hot outage does not show as cold reads.
- Bytes are counted when the proxy hands the read to the front (for a read of the hot tier, when it opens, from the window it covers), so a client that disconnects while they are sent still has them counted. A range that cannot be satisfied counts the hit (cold, if the hot tier missed) but zero bytes read and zero bytes served, unlike a conditional response, whose resolved window can hold bytes.
- A ranged GET that misses the hot tier asks the cold tier for the range alone and asks for no copy-back, so its `bytes_read` and `bytes_served` are both the range. While the hot tier is at its critical mark it also counts a `critical` skip.
- A HEAD asks for no copy-back.
- `critical` is a skip because the hot tier is at or above its critical mark by the usage the reconciler last published, and also while the proxy takes the hot tier to be full because it answered an earlier put with "full" a moment ago. It is the one reason that counts: a read that is not skipped asks for the copy-back with a create-only entry in a task of its own and copies nothing, so the proxy no longer counts promotions, promoted bytes or the reasons `full`, `raced` and `failed`: those series of the earlier releases are still rendered, at 0. The copy-backs are the entry reconciler's work.
- A cold body that fails part way counts a cold error as well as its cold hit (the hit is counted when the read opens), whether the read was of the whole object or of a range.

### Tier queries

All of these use a `5m` window. Every `sum` is outside the `rate`. Shorten the window to see a
change sooner and lengthen it to smooth a quiet node; keep it at least four scrape intervals.

### Tier hit rates

The hot tier, over the reads that fetch data:

```promql
sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m]))
  /
(sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m]))
  + sum(rate(tierstore_misses_total{tier="hot",op="get"}[5m])))
```

The cold tier is asked only after a hot miss, so its hit rate is the share of those that the
cold store had:

```promql
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
  /
(sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
  + sum(rate(tierstore_misses_total{tier="cold",op="get"}[5m])))
```

To count errors as non-hits, divide by `tierstore_reads_total` for the tier instead.

### Cold-read share of GETs

Every GET asks the hot tier first, so the share of GETs that had to read the cold tier is the
cold reads over the hot reads:

```promql
sum(rate(tierstore_reads_total{tier="cold",op="get"}[5m]))
  /
sum(rate(tierstore_reads_total{tier="hot",op="get"}[5m]))
```

A hot error ends the read before the cold tier, so it is in the denominator and not the
numerator. For the share of GETs that the cold tier answered, use its hits:

```promql
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
  /
sum(rate(tierstore_hits_total{op="get"}[5m]))
```

### Tier read amplification

Bytes read from the tier divided by bytes served.

For the cold tier, and then for all gets:

```promql
sum(rate(tierstore_bytes_read_total{tier="cold"}[5m]))
  /
sum(rate(tierstore_bytes_served_total{tier="cold"}[5m]))
```

```promql
sum(rate(tierstore_bytes_read_total[5m]))
  /
sum(rate(tierstore_bytes_served_total[5m]))
```

On the hot tier the two byte counters are equal, so the second query is 1 plus the extra bytes
that whole-object cold reads cost.

### Reads with no copy-back asked per cold hit

```promql
sum by (reason) (rate(tierstore_promotions_skipped_total[5m]))
  /
ignoring (reason) group_left
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
```

This is the share of cold hits that asked for no copy-back because the hot tier was at its
critical mark.

## On `/metricz` today

`/metricz` shows the blocks of the sources a process has, one after the other: the proxy's (the
hot and cold tier series above), then the OCI read front's, then the cache's. A source a process
does not have shows nothing. When it has none, the whole text is one line, "this server has no
proxy, no OCI read front and no local cache". The OCI read front, when it is enabled, exports
these series:

| name | type | labels | what it counts |
|---|---|---|---|
| `tierstore_oci_requests_total` | counter | `route`: `ping`, `manifest`, `blob`, `tags`, `upload`, `other`; `outcome`: `ok`, `created`, `accepted`, `no_content`, `partial`, `not_found`, `invalid`, `not_allowed`, `range_not_satisfiable`, `unauthorized`, `denied`, `failed`, `conflict` | requests, by the status they were answered with (`ok` 200, `created` 201, `accepted` 202, `no_content` 204, `partial` 206, `not_found` 404, `invalid` 400, 411 or 413, `not_allowed` 405, `range_not_satisfiable` 416, `unauthorized` 401, `denied` 403, `failed` 5xx, `conflict` 409). The `upload` route is the paths of blob uploads; a manifest `PUT` is counted under `manifest` |
| `tierstore_oci_bytes_served_total` | counter | none | bytes of blobs read for responses |
| `tierstore_oci_digest_mismatches_total` | counter | none | objects whose bytes were not the digest they are named by |
| `tierstore_oci_tag_recheck_failures_total` | counter | none | a pull of a stored tag whose re-check against the upstream registry failed or took longer than `recheck_timeout`, so the stored tag was served (see [configuration](configuration.md#paramsupstream)); 0 on a front with no `upstream` |
| `tierstore_oci_front_info` | gauge | `mode`: `https`, `http` | always 1; the label is the mode the front runs in |

### The drain

After the blocks of the sources, every `/metricz` shows one series of the server itself, with a
`# TYPE` line, whatever sources the node has:

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_drain_late_requests_total` | counter | none | in a drain, the server had shut its end of a connection for writing and its peer wrote after that: counted once for each such connection, not for each read |

It is 0 until a drain. After a restart it is the number of connections whose client sent a
request within one round trip of the end of the stream: the request was read, dropped and not
answered (see "Drain and restart" in [operations](operations.md)). A request that was on the wire
before the server shut its end is served and is not counted.

The OCI read front drains the same way, and its block has one more series that counts the same
thing for the front's own connections (the series above counts only those of the S3 listener):

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_oci_drain_late_requests_total` | counter | none | in a drain, the OCI front had shut its end of a connection for writing and its client wrote after that: counted once for each such connection, not for each read |

The two together are the requests that came too late in a drain.

### The build

Every `/metricz`, with or without the front, ends with one more series:

| name | type | labels | what it says |
|---|---|---|---|
| `tierstore_build_info` | gauge | `version`: the crate version; `commit`: the commit the binary was built from, twelve hex digits, or `unknown` outside a git checkout; `slot`: the port `server.slot` names, only when `server.slot` is given (otherwise the gauge has no such label) | always 1; says which build answered |

## Not in a release yet

Everything below is on main and not in a released tierstore yet; this page is updated when it
ships. Do not build alerts on these series until they are in a release.

### v1: changes in the next release

The series of "The build-node cache (v1)" above change as follows, and the cache gains the series
of the sections after this one. They all start with `tierstore_cache_`.

- Each family gets one `# TYPE` line, before its samples: a counter, or a gauge for `tierstore_cache_bytes`, `tierstore_cache_objects`, `tierstore_cache_fs_free_bytes`, `tierstore_cache_staging_files`, `tierstore_cache_prefetch_queue_depth`, `tierstore_cache_holds_open`, `tierstore_cache_fill_socket_waiting`, `tierstore_cache_fill_socket_running`, `tierstore_cache_upstream_reads_in_flight` and `tierstore_cache_mode`. `tierstore_cache_discarded_at_start_total` is declared a counter that is set once, when the cache is opened.
- A read of a kept key that the cache does not serve is one miss on every path, counted right after the lookup and before any call the read itself makes to the upstream (only the HEAD that checks an old hit, with `local_cache.revalidate_after`, comes before the lookup): a ranged GET whose size HEAD to the upstream then fails, a ranged GET of an object over the whole-object limit, a fill that then fails, and every miss of a read-only cache. So a ranged GET whose HEAD fails now counts a miss. A range past the end of a held object is still neither a hit nor a miss.
- The bytes the cache fetched from the upstream are `tierstore_cache_upstream_bytes_read_total`, in the next section.

### v1: the cache's calls to the upstream

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_upstream_reads_total` | counter | `op`: `get`, `head`, `list`, `list_uploads` | the cache sends one such call to the upstream |
| `tierstore_cache_upstream_bytes_read_total` | counter | none | bytes that the cache's upstream GETs brought back |
| `tierstore_cache_head_hits_total` | counter | none | a HEAD the cache is asked for is answered from its index: a client's HEAD, or the S3 front's HEAD before a GET |
| `tierstore_cache_head_misses_total` | counter | none | the same HEADs, answered by the upstream |

What they do not count:

- A fill reads the upstream in ranged chunks, so `op="get"` counts calls, not objects: an object of 350 bytes filled in chunks of 100 is four GET calls. A pass-through read (a key the cache does not keep, a range over the whole-object limit, the rest of a read after a fill that was given up) is read in chunks of the same size, one GET each.
- `op="head"` counts the cache's own HEADs as well as the clients': the size before a ranged fill, the HEAD of a fill before its GETs and, for a key not named by its content, a second one after them, the HEAD that checks an old hit (`local_cache.revalidate_after`), the two HEADs that hold a pass-through read to one version (before the first chunk and after the last: every pass-through read without a range makes them, and a ranged one whose range is over one chunk), the HEAD before a create-only put, and the HEAD after a put is acknowledged only when the upstream's answer to the put gave no tag (an S3 upstream's answer has the `ETag`, so a put to one makes no HEAD after it). `head_hits_total` and `head_misses_total` count only the HEADs the cache is asked for, which include the HEAD that the S3 front sends before each GET. So `head_hits_total + head_misses_total` is a lookup rate, not a rate of HEAD requests from clients.
- A HEAD of an old hit that the check finds the same, or cannot make, is a head hit; one the check finds changed or gone is a head miss, answered by the check's HEAD. A HEAD of a held key that a read-only cache finds changed on disk (the holder of the directory put the object again) is a head miss, answered by an upstream HEAD. A HEAD of a key the cache does not keep is a miss, and one of a reserved key is refused and is neither.
- A listing without pages counts one `list` call, and a paged listing counts one for each page it asks the upstream for, the pages that make up for reserved keys left out included. Each page of `list_uploads` counts one. A listing under a reserved prefix asks the upstream nothing and counts nothing.
- Not counted at all: deletes, creating and aborting uploads, sending a part, listing the parts of an upload, the start-up probe, and retries made inside a backend wrapper.
- `upstream_bytes_read_total` counts a GET that returned. The bytes of a GET that failed are not counted, and the bytes of a GET that returned are counted even if the fill then fails (a short body, a hash mismatch, a changed object) or is dropped.

### v1: writes through the cache

These series count the writes of a cache node that sends them to the upstream
(`local_cache.write_through`). Without it, the cache takes no writes and they stay 0.

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_upstream_puts_total` | counter | none | the upstream accepted a create-only or plain put, or the completion of a multipart upload |
| `tierstore_cache_upstream_put_bytes_total` | counter | none | bytes of the puts the upstream accepted, and of each part it accepted |
| `tierstore_cache_upstream_put_failures_total` | counter | `reason`: `exists`, `unavailable`, `full`, `bad_digest`, `other` | a put or a completion reached the upstream and failed, or the HEAD before a put failed |

The reasons of a failed put:

- `exists`: the upstream's create-only put or completion found the object (412).
- `unavailable`: the upstream did not answer.
- `full`: the upstream had no room (507).
- `bad_digest`: the body did not hash to the name of the object. When the upstream had already taken the put (it stopped reading the body before its end), it counts as a put, with the bytes the upstream was given, and as a `bad_digest` failure.
- `other`: any other failure.

A body that fails part way, such as that of a client that goes away, fails the put, and counts under the reason of its error (`unavailable` for a client that went away).

What they do not count:

- Before sending a create-only put, the cache sends the upstream a HEAD, unless `local_cache.head_before_put` is `false`; a plain put sends none. A key the HEAD finds is answered "already exists" without a put reaching the upstream: it counts in `write_exists_total` only, not as `put_failures` with reason `exists`. That reason counts the races the HEAD did not see.
- A HEAD before a put that fails with anything but "not found" fails the put although no put was sent. It counts under `put_failures` with the reason its error maps to (`unavailable`, `full`, `other`), and in `upstream_reads_total{op="head"}`.
- A part that fails, a delete, an abort and a part's retry are not failures here. A part that is accepted counts its bytes even if the upload is never completed, and a client's retry of an accepted part counts them twice.
- A completion counts one put and no bytes of its own: the bytes are those of its parts.
- A put that failed at the upstream, or at the HEAD before it, counts no bytes, whatever part of its body was sent. The one put the client is told failed that counts bytes is a `bad_digest` put the upstream had already taken (above).
- `writes_through_total` counts whole-body puts only, `upstream_puts_total` counts those and completions, and `write_exists_total` counts the HEAD's answer and the upstream's together.

### Failed puts and the short objects they leave

Two families count the puts of an object that fail, and what is done about an object of the wrong
length a failed put can leave in the upstream S3 store (some stores keep a PUT whose body was cut
short as an object of the bytes that came, 0 of them for a body that failed before its first byte).

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_s3_put_failures_total` | counter | `reason`: `timeout`, `incomplete`, `bad_digest`, `too_large`, `busy`, `exists`, `upstream`, `other` | a `PUT` of an object on the S3 listener whose body the front streams (no `If-Match`, no `If-None-Match` but `*`) was answered with an error, by the code of the answer |
| `tierstore_cache_upstream_short_objects_total` | counter | `result`: `removed`, `left`, `failed` | a single PUT the cache sent its S3 upstream got no answer (its body or its connection failed) and the one HEAD of the key after it found an object of another length than the put declared, or could not be made |

The reasons of a failed put on the front:

- `timeout`: no byte of the body came within `server.body_idle_timeout`, or the body came slower than `server.put_min_bytes_per_sec` (`RequestTimeout`).
- `incomplete`: the body ended before its length or could not be read, as when the client goes away (`IncompleteBody`).
- `bad_digest`: the body is not the digest its request named (`BadDigest`, `XAmzContentSHA256Mismatch`, `InvalidDigest`).
- `too_large`: the body is over the limit (`EntityTooLarge`).
- `busy`: the server was taking as many bodies as it may, or the store had no room (`SlowDown`).
- `exists`: a create-only put of a key that exists (`PreconditionFailed`).
- `upstream`: the store did not answer, or failed (`ServiceUnavailable`, `InternalError`).
- `other`: any other refusal.

The results of the look after a single PUT that got no answer:

- `removed`: the object carried the token that PUT sent as its metadata `x-amz-meta-write-token`, so the PUT made it; it was deleted on its entity tag.
- `left`: the object did not carry that token: it was there before the put began (the store refused a create-only PUT on it), or another writer made it. It is left as it is.
- `failed`: the HEAD or the DELETE failed, or the object had no entity tag; an object of the wrong length may have stayed.

An object of the length the put declared, or no object, counts nothing. A put the store answered
(even with an error) is not looked after, and a body sent as a multipart upload never leaves an
object of a failed put. The series stay 0 for an upstream that is not an S3 store.

### The memory bound

Every `/metricz` shows four series of the bound on the process's anonymous memory
(`server.memory_bound`, see [operations](operations.md#the-memory-bound)), after the failed puts,
each with a `# TYPE` line, whatever sources the node has:

| name | type | labels | what it says |
|---|---|---|---|
| `tierstore_memory_anon_bytes` | gauge | none | the anonymous resident memory of the process at the last look (a look a second; on Linux `RssAnon` of `/proc/self/status`, which leaves out the page cache and mapped files; on macOS the peak resident size); 0 when the memory is not watched |
| `tierstore_memory_bound_bytes` | gauge | none | the bound, `server.memory_bound`; 0 when there is none. The mark is half of it |
| `tierstore_memory_degraded` | gauge | none | 1 from a look at or over the mark until one under seven eighths of it, and from the bound on; else 0 |
| `tierstore_memory_bound_exits_total` | counter | none | exits at the bound, counted in the file `memory-bound-exits` of the service's state directory, so across restarts of the node (both instances of the template share it, and each exit adds one to what the file says then); this process's own exit is in it from the look that reached the bound. Without a state directory it counts this process alone |

Alert on `tierstore_memory_degraded` (or on `/memoryz`, which is 503 at the same time) before the
bound, and on an increase of `tierstore_memory_bound_exits_total` after it:

```promql
tierstore_memory_degraded == 1
increase(tierstore_memory_bound_exits_total[1h]) > 0
```

### Build node: request to start

A build node whose cache has a tree store and serves jobscheduler's mount provider (the
`mount-provider` interface) times each job from its request to the start of its command, for the
launch budget of the fetch, cache and serve design (`docs/design/fetch-cache-serve.md`, sections 1
and 12: under 200 ms, aiming for 100). The family is in the cache's block, with a `# TYPE` line, on
every process with a cache, and stays at zero on one that serves no provider.

| name | type | labels | what it says |
|---|---|---|---|
| `tierstore_request_to_start_seconds` | histogram | `le` (on `_bucket`): `0.005`, `0.01`, `0.025`, `0.05`, `0.1`, `0.2`, `0.5`, `1`, `2.5`, `5`, `10`, `30`, `60`, `120`, `300`, `900`, `+Inf` | one sample per run: the seconds from the first `Prepare` of any of the run's sources to the first lookup or listing that `tierstore fs` serves inside any of the run's trees |
| `tierstore_request_to_start_seconds_bucket` | counter | `le` | the runs sampled at or under `le` seconds |
| `tierstore_request_to_start_seconds_sum` | counter | none | the seconds of every sample, to the millisecond |
| `tierstore_request_to_start_seconds_count` | counter | none | the runs sampled |

The two ends:

- **Request.** jobscheduler's agent sends `Prepare` for a job's sources the moment it has the job. The first `Prepare` of a run that the process takes starts its span; a `Prepare` of another source of the run, or the same one again, does not move it.
- **Start.** The agent opens each tree's root, and the kernel looks the roots up when it mounts the job's overlay; neither reads below a root. The first lookup in a directory of one of the run's trees, or listing of one, is the container runtime making the job's root, or the command itself. The file system reports it to the node's cache, in this process, so the request it serves does not wait for the report.

What it does not count:

- A run whose command never looks inside its trees, and a run released or swept first.
- A run whose `Prepare` this process did not take. This happens when the process started after it, when the run's request was forgotten because 4096 runs were kept at once (the oldest is forgotten first), or when an `fs` report was lost. A report is lost when 1024 reports wait at once, or when the cache is not answering the mount.
- A tree placed through the trees API rather than the provider.

A report that waits while the cache is not answering the mount (an update of the node's tierstore) ends its
span when the node takes it, so such a span is longer than the job's real start.

The span is on the node's wall clock: a span across a step of the clock back is 0. Fraction of
runs within the budget, over a day:

```promql
sum(increase(tierstore_request_to_start_seconds_bucket{le="0.2"}[1d])) / sum(increase(tierstore_request_to_start_seconds_count[1d]))
```

The 99th percentile, over an hour:

```promql
histogram_quantile(0.99, sum by (le) (rate(tierstore_request_to_start_seconds_bucket[1h])))
```

### Build node: tree lists the mount provider refused

The mount provider reads a tree's L2 sidecar, `cas/l2/<id>`, and refuses a tree it cannot mount
through tierstore with `tree list not supported: <reason>` (`ListVersionUnsupported`,
`docs/design/fetch-cache-serve.md`, 3.5), on which the CAS service takes its miss path. The two
families are in the cache's block, with a `# TYPE` line, on every process with a cache, and stay
at zero on one that serves no provider.

| name | type | labels | what it says |
|---|---|---|---|
| `tierstore_list_refused_total` | counter | `reason`: `v1_list`, `missing_sidecar`, `unknown_cell_digest`, `unknown_version` | the preparations refused because the tree has no sidecar (`missing_sidecar`) or the object at the sidecar's key is a version 1 or 2 list (`v1_list`) or an L2 list of a major version or a cell digest function this release does not read |
| `tierstore_l2_sidecar_missing_total` | counter | none | the preparations refused because the tree has no sidecar: the same count as `reason="missing_sidecar"`, the one the rollout watches |

What it does not count:

- A sidecar that is there and refused as malformed (another tree's, a projection that does not hash to the id, a cell shift other than 20): the provider's log line names it.
- A `Mount` asked again of a refused source: a preparation is counted once.

Trees refused for want of a sidecar, over a day, which must stay at zero for 7 days before the CAS
service stops writing version 2 lists:

```promql
sum(increase(tierstore_l2_sidecar_missing_total[1d]))
```

### Build node: tree attach on a lookup

A lookup of `trees/<digest>` that no run has attached reads the tree's manifests (or the one tree
list it is) and attaches it
(see [the virtual build file system](operations.md#the-virtual-build-file-system)). `tierstore fs`
reports to the node's cache how long each such attach took, where the manifests of
each read of a tree came from and how many levels and rounds of fills each read took, and the node's
cache counts the reports in its block; the node also times each fill of a manifest it makes itself.
The families are on every process with a cache, with a `# TYPE` line, and stay at zero on a node
whose mount reports nothing. The buckets of the histograms are `0.001`, `0.002`, `0.005`, `0.01`, `0.025`,
`0.05`, `0.1`, `0.25`, `0.5`, `1`, `2.5`, `5`, `10`, `30`, `60`, `300` and `+Inf` seconds.

| name | type | labels | what it says |
|---|---|---|---|
| `tierstore_fs_tree_attach_seconds` | histogram | `outcome`: `ok`, `absent`, `failed`; `le` on `_bucket` | one sample per lookup that attached a tree, or tried to: the seconds from the lookup's start to its answer on the mount's clock, to the millisecond. `absent` is a tree whose root manifest the upstream does not have (`ENOENT`); `failed` is any other that could not be read or numbered (`EIO`) |
| `tierstore_fs_tree_attach_seconds_bucket` | counter | `outcome`, `le` | the attaches of that outcome at or under `le` seconds |
| `tierstore_fs_tree_attach_seconds_sum` | counter | `outcome` | their seconds |
| `tierstore_fs_tree_attach_seconds_count` | counter | `outcome` | the attaches of that outcome |
| `tierstore_fs_manifest_fetch_total` | counter | `result`: `cache`, `fill`, `error` | a manifest a read of a tree fetched: read from the cache directory, filled through the cache and then read, or not had (the fill failed, or the file was not there after it). A read of a tree attached for a run, or read again into memory, counts too |
| `tierstore_fs_manifest_fill_seconds` | histogram | `le` on `_bucket` | one sample per manifest a read asked the cache for, whatever the answer: the seconds of the call on the mount's clock, to the millisecond |
| `tierstore_fs_manifest_fill_seconds_bucket` | counter | `le` | the fills at or under `le` seconds |
| `tierstore_fs_manifest_fill_seconds_sum` | counter | none | their seconds |
| `tierstore_fs_manifest_fill_seconds_count` | counter | none | the fills |
| `tierstore_fs_tree_levels_total` | counter | none | the levels of the trees read from their manifests (a tree that is a root alone has 1), summed over the reads; one `levels` report per read |
| `tierstore_fs_tree_fill_rounds_total` | counter | none | the rounds of fills those reads waited for one after another: one for each level in which a read filled a manifest, and one for each fill of a tree it asked, summed |
| `tierstore_cache_manifest_fill_seconds` | histogram | `via`: `fill`, `tree`; `le` on `_bucket` | one sample per fill of a manifest this node made: the seconds from the request to its answer on the node's own clock (a permit of the file system's fill pool, the upstream's answers, the write into the cache), to the millisecond. `fill` is a fill of a manifest asked by the file system or by a read of the S3 front; `tree` is a manifest filled for a fill of a tree |
| `tierstore_cache_manifest_fill_seconds_bucket` | counter | `via`, `le` | those fills at or under `le` seconds |
| `tierstore_cache_manifest_fill_seconds_sum` | counter | `via` | their seconds |
| `tierstore_cache_manifest_fill_seconds_count` | counter | `via` | those fills |

A read of a tree whose root manifest the cache does not hold first asks the node to fill the tree's
new manifests itself (a fill of a tree, see [fills for the file system](operations.md#fills-for-the-file-system)): the node
goes down the tree in its cache, a level at a time, and answers once, so the call is made once
for the tree. The read then fetches the manifests of a level of the tree at once, up to 16, before
the level below, filling through the cache what it does not find there, so the attach of a tree whose manifests
the cache does not hold takes about one fill for each level, not one for each directory. The fills of a level
overlap, so their seconds add up to more than the attach took.

What they do not count: a report that was lost, as a `started` is lost (above); a lookup of a tree
that was attached already; a placement's tree, which is read whole with no manifest. Each attach is
also said on stderr, with the levels of the read, the fills of each and the slowest of them, and
the `fill-tree` if the read asked one: `tierstore fs: lookup of <digest>: ok in 12 ms; 40
manifests: 40 from the cache, 0 filled, 0 not had; 4 levels; fills by level: 0 0 0 0, slowest ms
by level: 0 0 0 0; fill-tree ok in 9 ms, 8 ms at the node, fills by level: 1 1 1 0`. Any other answer
is said as `fill-tree failed in <ms> ms: <words>`.

The share of the manifests read that needed a fill of the read's own, over an hour (a manifest the
node filled for a fill of a tree is read from the cache, and counts as `cache`):

```promql
sum(increase(tierstore_fs_manifest_fetch_total{result="fill"}[1h])) / sum(increase(tierstore_fs_manifest_fetch_total[1h]))
```

The median attach, over an hour:

```promql
histogram_quantile(0.5, sum by (le) (rate(tierstore_fs_tree_attach_seconds_bucket{outcome="ok"}[1h])))
```

The rounds of fills an attach waited for, on average, over an hour: one for each
level with a new manifest where the node fills them one at a time, about one where it fills the
tree's new manifests itself:

```promql
sum(increase(tierstore_fs_tree_fill_rounds_total[1h])) / sum(increase(tierstore_fs_tree_attach_seconds_count[1h]))
```

What the file system's call adds to a fill of a manifest, over an hour: the mount's mean time for its
fills, less the node's for the same fills.

```promql
sum(increase(tierstore_fs_manifest_fill_seconds_sum[1h])) / sum(increase(tierstore_fs_manifest_fill_seconds_count[1h])) - sum(increase(tierstore_cache_manifest_fill_seconds_sum{via="fill"}[1h])) / sum(increase(tierstore_cache_manifest_fill_seconds_count{via="fill"}[1h]))
```

### Build node: the mount's descriptors

`tierstore fs` shares one descriptor per cache file (a blob or a pack) across all its open handles,
and at each look for idle trees (every quarter of `fs.tree_idle_secs`, at most a minute apart: a
minute by default) reports to the node's cache how many cache files it holds open and
how many opens failed for lack of descriptors since its previous report (the cache file could not
be opened, or the call of the cache could not be made). The families are on
every process with a cache, with a `# TYPE` line, and have no series until a mount reports. The
node keeps at most 64 mounts; a report of a 65th mount is answered `ok` and dropped, so a client
that makes up mount ids cannot grow the node's memory. The fs unit's descriptor limit is 1,048,576.

| name | type | labels | what it says |
|---|---|---|---|
| `tierstore_fs_cache_descriptors` | gauge | `mount`: the mount's id | the cache files that mount's fs process held open at its last report |
| `tierstore_fs_open_descriptor_failures_total` | counter | `mount` | opens of files of that mount that failed for lack of descriptors (`EMFILE`, `ENFILE`), summed over its reports. It keeps counting across restarts of the mount process, since each report carries only the failures since the previous one |

What they do not count: a report that was lost, as a `started` is lost (above), so the gauge may be
a minute old and a failure whose report was lost is never counted; the store process's own
descriptors; descriptors that are not cache files, such as the copies of the FUSE device, the
splice pipes and the sockets.

An alert when a mount holds more than nine tenths of the limit:

```promql
tierstore_fs_cache_descriptors > 0.9 * 1048576
```

Any open that found no descriptor in the last hour:

```promql
increase(tierstore_fs_open_descriptor_failures_total[1h]) > 0
```

### Build node: the opens of files

`tierstore fs` times every open of a file of a tree, from the arrival of its request to its answer
on the mount's clock, to the millisecond, and counts the opens waiting for an answer. Over FUSE that
is an `OPEN`, answered at once by the reader that read it or parked until the node answers a fill;
over NFS, which has no open, it is the fill of the file's descriptor that the first read of the file
starts, from that read's arrival. The mount reports the counts to the node's cache at
each look for idle trees (every quarter of `fs.tree_idle_secs`, at most a minute apart: a minute by
default) and when its readers stop for a hand-over or a restart, as running totals of the process;
the node adds what each process's totals grew by since its previous report, so a report that was
lost loses nothing the next one carries, and the two processes of a hand-over, reporting in turns,
are each counted once. The families are on every process with a cache, with a `# TYPE` line, and
stay at zero on a node whose mount reports nothing. The buckets are those of the attach histogram
above. The node keeps at most 64 mounts, as for the descriptors.

| name | type | labels | what it says |
|---|---|---|---|
| `tierstore_fs_open_seconds` | histogram | `outcome`: `ok`, `error`; `source`: `cached`, `filled`; `le` on `_bucket` | one sample per open of a file of a tree: the seconds from its request's arrival to its answer. `ok` is an open answered with a handle, `error` one answered with an error (`EIO` for a fill that failed, `EISDIR`, `EROFS` and the like). `filled` is an open that asked the node for the file's bytes (a fill of its blob, or the span of a pack) and waited for the answer; `cached` is one that had them without asking the node |
| `tierstore_fs_open_seconds_bucket` | counter | `outcome`, `source`, `le` | the opens of that outcome and source at or under `le` seconds |
| `tierstore_fs_open_seconds_sum` | counter | `outcome`, `source` | their seconds |
| `tierstore_fs_open_seconds_count` | counter | `outcome`, `source` | the opens of that outcome and source |
| `tierstore_fs_opens_waiting` | gauge | none | the opens begun and not yet answered at the mount's last report, summed over the mounts |

What they do not count: a report that was lost, as a `started` is lost (above), so the gauge may be
a minute old and the histogram behind by the opens since the last report that arrived; a read of a
file that is open, which is answered from the descriptor it holds; a read of a handle the process
before a hand-over gave out, which opens the file again for that read. A node of an earlier release
answers the report `refused unknown request`, which the mount ignores, and no open waits for it: the
report is sent by a thread of its own, and an open only adds to counters of its process.

A high p99 of `source="filled"` with a growing `tierstore_fs_opens_waiting` is a build that waits on
the node's fills: the processes held in an open are those opens (see
[operations](operations.md#the-virtual-build-file-system)).

The 99th percentile of an open over five minutes, by source:

```promql
histogram_quantile(0.99, sum by (source, le) (rate(tierstore_fs_open_seconds_bucket[5m])))
```

### Build node: the mapped tree lists

`tierstore fs` reads the tree of a placement that is a version 2 tree list from a read-only mapping
of its image file, one mapping per list however many of its trees show it, shared with the node's
tree store where the mount runs in the node's process. It keeps the bytes of those mappings, in
whole pages and each list once, under `fs.tree_list_ram_bytes` (512 MiB by default): before a list
is attached it releases, the least recently used first, the lists that no run attaches and no
request is reading, and a list that still does not fit is mapped past the cap and counted (see
[operations](operations.md#the-virtual-build-file-system)). The mount reports to the node's cache at
each look for idle trees (every quarter of `fs.tree_idle_secs`, at most a minute
apart: a minute by default) and when its readers stop for a hand-over or a restart, the lists mapped
in its process now and their bytes, the cap, and as running totals of the process the lists it
released and those it mapped past the cap; the node adds what each process's totals grew by since
its previous report, so a report that was lost loses no release the next one carries. The families
are on every process with a cache, with a `# TYPE` line, and have no series until a mount reports.
The node keeps at most 64 mounts, as for the descriptors.

| name | type | labels | what it says |
|---|---|---|---|
| `tierstore_fs_tree_lists_mapped` | gauge | `mount`: the mount's id | the tree lists mapped in that mount's process at its last report, each once (the tree store's too, where it is in that process) |
| `tierstore_fs_tree_list_mapped_bytes` | gauge | `mount` | the bytes of those mappings, in whole pages: what is budgeted against the cap |
| `tierstore_fs_tree_list_ram_bytes` | gauge | `mount` | the cap, `fs.tree_list_ram_bytes`, or the one the control socket set since the start (reported at once) |
| `tierstore_fs_tree_list_releases_total` | counter | `mount`; `why`: `cap`, `idle` | the lists that mount released: `cap` to make room for a list under the cap, `idle` at the sweep for idle trees |
| `tierstore_fs_tree_list_over_cap_total` | counter | `mount` | the lists mapped past the cap because every list mapped was held (a run attaches it, or a request reads it) |

What they do not count: a report that was lost, so a gauge may be a minute old; the trees of
manifests, whose memory is in the line of the trees in memory
([operations](operations.md#the-virtual-build-file-system)); the lists the tree store of the node
maps where the mount runs in a process of its own (`tierstore fs`), which are mapped, and counted
nowhere, in the node's process. A release of a list the tree store also holds (its placement still
has a lease or another holder) frees no bytes until the store lets it go too. A node of an earlier
release answers the report `refused unknown request`, which the mount ignores.

A mount whose held lists alone pass its cap, over the last hour:

```promql
increase(tierstore_fs_tree_list_over_cap_total[1h]) > 0
```

### Build node: the requests of the mount

`tierstore fs` times every request its frontend answers, from the request's arrival to its answer
on the mount's clock, to the millisecond, by its kind, for the requests by kind and their latency
of the Q-B measurement (`docs/design/virtual-build-fs.md`, section 11). Over FUSE a request is
one the session hands the frontend, by its opcode, answered at once by the reader that read it or
parked until the node answers; over NFS it is a call of the protocol layer into the file system,
so the attributes a LOOKUP or a READ also reads are a `getattr` of their own. The mount reports the
counts to the node's cache, by a call in the process, at each look for idle trees (every quarter of
`fs.tree_idle_secs`, at most a minute apart: a minute by default) and when its readers stop for a
hand-over or a restart, as running totals of the process, and only when it answered a request
since its previous report; the node adds what each process's totals grew by, as for the opens
above, so a report that was lost loses nothing the next one carries. The family is on every
process with a cache, with a `# TYPE` line, and stays at zero on a node whose mount reports
nothing. The buckets are those of the attach histogram above. The node keeps at most 64 mounts, as
for the descriptors.

| name | type | labels | what it says |
|---|---|---|---|
| `tierstore_fs_request_seconds` | histogram | `kind`: `lookup`, `getattr`, `access`, `open`, `read`, `readdir`, `readlink`, `other`; `le` on `_bucket` | one sample per request of the mount's frontend: the seconds from its arrival to its answer, whatever the answer. `lookup` is FUSE's and NFS's LOOKUP; `getattr` FUSE's GETATTR and NFS's GETATTR and the attributes NFS reads for its other procedures; `access` NFS's ACCESS; `open` FUSE's OPEN; `read` FUSE's and NFS's READ; `readdir` FUSE's READDIR and READDIRPLUS and NFS's READDIR and READDIRPLUS; `readlink` FUSE's and NFS's READLINK; `other` any other request: a RELEASE, an OPENDIR, a STATFS, NFS's FSINFO, a change refused with `EROFS`, an opcode the frontend does not serve, a request too short for its opcode |
| `tierstore_fs_request_seconds_bucket` | counter | `kind`, `le` | the requests of that kind at or under `le` seconds |
| `tierstore_fs_request_seconds_sum` | counter | `kind` | their seconds |
| `tierstore_fs_request_seconds_count` | counter | `kind` | the requests of that kind |

What it does not count: a report that was lost, so the histogram may be behind by the requests
since the last report that arrived; the requests of the put directory (`put/`); FORGET and
BATCH_FORGET, which have no answer; what the FUSE session or the NFS protocol layer answers itself
with no call into the file system (INIT, INTERRUPT, an opcode the session does not know, NFS's
NULL and an unknown procedure); a parked request that is never answered. A node of an earlier
release answers the report `refused unknown request`, which the mount ignores, and no request
waits for it: the report is sent by a thread of its own, and a request only adds to counters of
its process.

The 99th percentile of a request over five minutes, by kind:

```promql
histogram_quantile(0.99, sum by (kind, le) (rate(tierstore_fs_request_seconds_bucket[5m])))
```

### Build node: the attaches and detaches of runs

`tierstore fs` times each `attach <digest> for <run>` and each `detach <run>` of its control
socket, from the request to its answer on the mount's clock, to the millisecond, and splits each
attach into three parts that add up to it, the `attach_ms` and `detach_ms` of the Q-B measurement
(`docs/design/virtual-build-fs.md`, section 11): the **fetch**, to the end of the last manifest
the attach's read of the tree fetched from the cache directory or filled through the cache
(its `fill-tree` included); the **flatten**, from there to the end of the read (the entries made
of the manifests, or the tree list mapped); and the **append**, from there to the answer (the
tree's number in the table of tree numbers, the tree kept in memory, the attachments file
written). A tree in memory already, or one another request is reading, or one read again into
memory under the number it has, has its whole read under the fetch; an attach refused before its
read (a digest or a run that is not one, a process being replaced) has no fetch and no flatten.
The mount reports each to the node's cache, by a call in the process, when it ends; the families are on
every process with a cache, with a `# TYPE` line, and stay at zero on a node whose mount reports
nothing. The buckets are those of the attach histogram above.

| name | type | labels | what it says |
|---|---|---|---|
| `tierstore_fs_run_attach_seconds` | histogram | `outcome`: `ok`, `failed`; `le` on `_bucket` | one sample per attach of a tree for a run: the seconds from the request to its answer. `ok` is an attach that answered the run's path, `failed` one refused, whatever the reason |
| `tierstore_fs_run_attach_seconds_bucket` | counter | `outcome`, `le` | the attaches of that outcome at or under `le` seconds |
| `tierstore_fs_run_attach_seconds_sum` | counter | `outcome` | their seconds |
| `tierstore_fs_run_attach_seconds_count` | counter | `outcome` | the attaches of that outcome |
| `tierstore_fs_run_attach_part_seconds` | histogram | `part`: `fetch`, `flatten`, `append`; `outcome`: `ok`, `failed`; `le` on `_bucket` | one sample of each part per attach of a tree for a run, under the attach's outcome: the seconds of that part. A failed attach has the parts it ran, and 0 for those it did not |
| `tierstore_fs_run_attach_part_seconds_bucket` | counter | `part`, `outcome`, `le` | those parts at or under `le` seconds |
| `tierstore_fs_run_attach_part_seconds_sum` | counter | `part`, `outcome` | their seconds |
| `tierstore_fs_run_attach_part_seconds_count` | counter | `part`, `outcome` | those parts: the attaches of that outcome |
| `tierstore_fs_run_detach_seconds` | histogram | `outcome`: `ok`, `failed`; `le` on `_bucket` | one sample per detach of a run (and per run that `detach-all-except` detaches): the seconds from the request to its answer, the unpins of the trees it freed included. `ok` is a detach that ended the run's attachments, `failed` one of a run that attached nothing, or refused while the process is being replaced |
| `tierstore_fs_run_detach_seconds_bucket` | counter | `outcome`, `le` | the detaches of that outcome at or under `le` seconds |
| `tierstore_fs_run_detach_seconds_sum` | counter | `outcome` | their seconds |
| `tierstore_fs_run_detach_seconds_count` | counter | `outcome` | the detaches of that outcome |

What they do not count: a report that was lost, as a `started` is lost (above); an attach of a tree
on a lookup, which is `tierstore_fs_tree_attach_seconds`. A node of an earlier release answers
the reports `refused unknown request`, which the mount ignores.

The share of an attach's time that is its fetch, over an hour:

```promql
sum(increase(tierstore_fs_run_attach_part_seconds_sum{part="fetch",outcome="ok"}[1h])) / sum(increase(tierstore_fs_run_attach_seconds_sum{outcome="ok"}[1h]))
```

## Cache queries (v1)

All of these use a `5m` window. Every `sum` is outside the `rate`. Shorten the window to see a
change sooner and lengthen it to smooth a quiet node; keep it at least four scrape intervals.
The queries over `tierstore_cache_upstream_bytes_read_total`, `tierstore_cache_upstream_reads_total`
and the `upstream_put` series need the release that adds them (see "Not in a release yet"); the
others work on a released tierstore.

### Query: cache hit rate (v1)

The v1 local cache, from the cache's own series. Errors are not in either term, so a failing
upstream does not lower the rate:

```promql
sum(rate(tierstore_cache_hits_total[5m]))
  /
(sum(rate(tierstore_cache_hits_total[5m])) + sum(rate(tierstore_cache_misses_total[5m])))
```

The same by bytes, the share of answered bytes that came from the local directory:

```promql
sum(rate(tierstore_cache_bytes_served_local_total[5m]))
  /
(sum(rate(tierstore_cache_bytes_served_local_total[5m]))
  + sum(rate(tierstore_cache_bytes_served_upstream_total[5m])))
```

### Query: one dashboard for both, the v1 cache as tier="local"

The shared series have `tier` and `op` labels and the cache's do not. `label_replace` adds the
label, and `or` joins the two shapes, so one panel, `sum by (tier)`, reads either deployment.
Hit rate by tier:

```promql
(
  sum by (tier) (rate(tierstore_hits_total{op="get"}[5m]))
  or
  label_replace(sum(rate(tierstore_cache_hits_total[5m])), "tier", "local", "", "")
)
/
(
  sum by (tier) (rate(tierstore_hits_total{op="get"}[5m]) + rate(tierstore_misses_total{op="get"}[5m]))
  or
  label_replace(
    sum(rate(tierstore_cache_hits_total[5m]) + rate(tierstore_cache_misses_total[5m])),
    "tier", "local", "", ""
  )
)
```

Bytes served by tier:

```promql
sum by (tier) (rate(tierstore_bytes_served_total[5m]))
or
label_replace(sum(rate(tierstore_cache_bytes_served_local_total[5m])), "tier", "local", "", "")
```

The mapping, series by series:

| shared series | the cache's series |
|---|---|
| `tierstore_hits_total{tier="local",op="get"}` | `tierstore_cache_hits_total` |
| `tierstore_misses_total{tier="local",op="get"}` | `tierstore_cache_misses_total` |
| `tierstore_bytes_served_total{tier="local"}` | `tierstore_cache_bytes_served_local_total` |

Only these three map to `tier="local"`. The cache's `upstream` series are not local: they
measure the cache's calls to the cold store, so on a build node they are the cold-side numbers
and map to `tier="cold"`:

| shared series | the cache's series | how they differ |
|---|---|---|
| `tierstore_bytes_read_total{tier="cold"}` | `tierstore_cache_upstream_bytes_read_total` | the cache's counts the chunks of fills and pass-through reads that returned, whole objects or not |
| `tierstore_reads_total{tier="cold",op="get"}` | `tierstore_cache_upstream_reads_total{op="get"}` | the cache's counts calls, and a fill makes one per chunk, so it is not a count of objects |
| `tierstore_reads_total{tier="cold",op="head"}` | `tierstore_cache_upstream_reads_total{op="head"}` | the cache's includes its own HEADs (fills, puts) |

Cold-side bytes read, one query for both deployments:

```promql
sum by (tier) (rate(tierstore_bytes_read_total{tier="cold"}[5m]))
or
label_replace(sum(rate(tierstore_cache_upstream_bytes_read_total[5m])), "tier", "cold", "", "")
```

The write-through counters (`upstream_puts`, `upstream_put_bytes`, `upstream_put_failures`) are
also toward the cold store and have no shared counterpart. `tierstore_cache_bytes_served_upstream_total`,
the fills and the evictions have none either, and `tierstore_cache_head_hits_total` counts
the HEADs the cache is asked for (the S3 front's HEAD before each GET included), not `op="head"` of the shared family. The `local` values count a GET once per reader at the cache, and the `hot` and `cold`
values once per GET at the proxy, so compare the rates, not the totals.

### Query: cache read amplification (v1)

Bytes fetched from the upstream divided by bytes served. For the v1 cache, over all answers, and over the
answers that went upstream:

```promql
sum(rate(tierstore_cache_upstream_bytes_read_total[5m]))
  /
(sum(rate(tierstore_cache_bytes_served_local_total[5m]))
  + sum(rate(tierstore_cache_bytes_served_upstream_total[5m])))
```

```promql
sum(rate(tierstore_cache_upstream_bytes_read_total[5m]))
  /
sum(rate(tierstore_cache_bytes_served_upstream_total[5m]))
```

The second is the fill amplification: it is above 1 when ranged misses fill whole objects, and
close to 1 when clients read whole objects. It is not exactly 1 for whole reads, because the read
that follows a fill counts its bytes under `bytes_served_upstream` and the fill's chunks under
`upstream_bytes_read`, and a fill that fails counts bytes fetched and none served.

### Query: write-through put failure rate by reason (v1)

Failures per second, by reason:

```promql
sum by (reason) (rate(tierstore_cache_upstream_put_failures_total[5m]))
```

As a share of the puts attempted (accepted, or failed at the upstream or at the HEAD before the
send):

```promql
sum by (reason) (rate(tierstore_cache_upstream_put_failures_total[5m]))
  /
ignoring (reason) group_left
(sum(rate(tierstore_cache_upstream_puts_total[5m]))
  + sum(rate(tierstore_cache_upstream_put_failures_total[5m])))
```

The denominator counts a bad-digest put that the upstream took twice, once as a put and once as
a failure, and does not count puts refused because the HEAD before the send found the key (see
"v1: writes through the cache"). A completion that fails with "exists" counts in `put_failures` with reason `exists` and not in
`write_exists_total`, which only whole-body puts feed. The share above counts such a completion as
a failure, and the share below leaves it out, so they differ slightly when completions race. For the share of puts refused because the key exists, add
`write_exists_total`:

```promql
sum(rate(tierstore_cache_write_exists_total[5m]))
  /
(sum(rate(tierstore_cache_upstream_puts_total[5m]))
  + sum(rate(tierstore_cache_write_exists_total[5m]))
  + sum(rate(tierstore_cache_upstream_put_failures_total{reason!="exists"}[5m])))
```
