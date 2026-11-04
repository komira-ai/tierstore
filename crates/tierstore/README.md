# tierstore

A generic hot/cold object store over S3-compatible backends. Its normal dependencies include no
other crate of this workspace unless the `s3` feature is on.

- The library: the `Backend` trait (create-only put, `If-Match` put, ranged get, head, delete, list,
  multipart), an in-memory and a directory backend, `Proxy` (every write on the entry of its key, a
  get that asks for a cold hit to be copied back, a full hot tier sending puts to the cold tier),
  `EntryReconciler` (copies puts to the cold tier, finishes deletes, does the copy-backs, evicts what
  the cold tier holds, publishes usage) and `CachedStore`.
- Feature `s3`: `S3Backend`, a bucket of an S3-compatible store as a `Backend`.
- Feature `server` (turns `s3` on): `server`, the S3 API front of a proxy, and the `tierstore`
  binary that runs a proxy, that front and an entry reconciler in one process.
  `packaging/README.md` has the flags, the keys and the unit.

An S3 front over a hot and a cold tier. It serves one bucket, path-style, and checks SigV4 in the
`Authorization` header (and presigned GET and PUT URLs). The packaging notes, the flags and the key files
are in `packaging/README.md`; the roles of a key are in `src/server/keys.rs`.

The operator guide (install, keys and roles, configuration, operations, troubleshooting, limits) is in `docs/`; a test of the crate (`src/server/docs_tests.rs`) holds every page of it to the code, and the site's docs pages are made from it.

## Flags or a file

The settings that describe the data path are flags, or one TOML file given with `--config <path>`
(`[server]`, `[hot_store]`, `[cold_store]`, `[oci]` and `[[policy]]`; `src/server/config.rs`). The two make
the same settings and run the same code. `--print-config`, given the flags of a start, prints the
file that is equivalent to them. A file holds no secret, only the paths of key files, and refuses
a key or a table it does not know. The format, the table of the key of each flag and the rules are
in "The configuration file" in `packaging/README.md`; the flags stay supported along with the
file.

## When the puts are at their limit

At most `--max-concurrent-puts` put bodies and upload parts are taken at once. A request that
arrives when all are taken waits up to `--put-permit-wait` (`server.put_permit_wait`, 15s by
default) for one to end, without reading its body, and is taken if one ends in that time. If none
does, it is answered 503 `SlowDown` with `Retry-After: 1` and its body is never read. With a tier
that is slow to take a body, clients queue here for up to that long instead of retrying at once.
`0s` answers 503 at once. The requests that are refused for their head (411, 413) do not wait.

What waiting costs, as the code does it:

- A waiting request holds a connection and its task, and no permit and no body memory. It has
  been authenticated (a valid signature) before it waits. The number of waiters is bounded only by
  the number of connections the server accepts.
- A waiter counts as a request in flight. During a drain the server waits for it, so a drain can
  take up to `--put-permit-wait` longer than without waiting, and never longer than
  `--drain-deadline`.
- A waiter does not notice that its client went away: the connection is looked at again when the
  body is first read, after a permit is given.
- A client should retry the 503, and a connection that broke while it was still
  sending the body of a refused PUT, with jittered waits. A client should not retry
  429, 500, 502 or 504, which do not mean a full put queue.
- The 503 is sent before the body is read, so the front may close the connection under a client
  that is still sending a large body. A client that sees the reset instead of the 503 has to treat
  it as one.

## A build node: a read cache

A file with `[local_cache]` and `[cold_store]` and no `[hot_store]` runs the same binary as a read
cache in front of one upstream (`src/server/local_cache/`, the file's tables in
`src/server/config_cache.rs`): GET and HEAD of the keys a `[[policy]]` with `cache = true` holds are
served from a directory of raw files (`<path>/obj/<aa>/<bb>/<hex>`), filled from the upstream on a
miss, hashed as they are filled and evicted oldest-use first under a byte budget and a free-space
floor; every other call is refused 403. It has no entries and no lease. The keys of the table and
the rules are in "A build node" in `packaging/README.md`; `GET /metricz` shows its
counters.

With `write_through = true` in `[local_cache]`
the node also takes create-only puts, presigned PUTs, multipart uploads and deletes, and sends
each to the upstream (`src/server/local_cache/write.rs`) before it answers; a create-only put
under a `cache = true` policy is also kept on the node (`src/server/local_cache/keep.rs`), a
multipart upload is not. The roles of the keys apply as on a storage node. The start tests the upstream's
create-only and multipart-conditional answers with the self-test, so the upstream key must be one
that writes (exit status 3 if not). `tierstore --config <path> --check-config` checks a file as a
start does, without touching the network, and exits 0 and prints nothing, or prints the refusal and
exits 2.

## Metrics

`GET /metricz` on the S3 listener (not signed; a bucket cannot be called `metricz`) is the
Prometheus text of the counters of the process. All are counters since the process started.
`/metricz` renders whichever blocks the process has: the proxy's (a storage node, with a hot and
a cold tier; the block below), the OCI read front's, and the local cache's `tierstore_cache_*`
block (a build node, or a storage node with a cache in front of its proxy; see "Local cache"
below). A file with `[local_cache]`, `[hot_store]` and `[cold_store]` is one process with both the
proxy's block and the cache's.

The block of the proxy is one labelled family per measure, so one query serves every tier. The
labels have a few fixed values, never a key or a client: `tier` is `hot` or `cold` here (`local`
is the tier of the build node's cache), `op` is `get` or `head`.

| name | labels | meaning |
|---|---|---|
| `tierstore_hits_total` | `tier`, `op` | reads the tier had the object for |
| `tierstore_misses_total` | `tier`, `op` | reads the tier did not have it for (a hot miss goes on to the cold tier) |
| `tierstore_read_errors_total` | `tier`, `op` | reads the tier failed with anything but "not found" (the tier is down, refused, timed out) |
| `tierstore_reads_total` | `tier`, `op` | reads that reached the tier: hits, misses and errors. For `tier="cold"` these are the reads that had to go to the cold store |
| `tierstore_bytes_served_total` | `tier` | bytes of object data that gets answered, by the tier that gave them (the range, if one was asked) |
| `tierstore_bytes_read_total` | `tier` | bytes of object data gets read from the tier: for `cold`, the whole object for a get without a range and the range alone for a ranged get |
| `tierstore_promotions_total`, `tierstore_promoted_bytes_total` | none | always 0 (the proxy no longer copies into the hot tier) |
| `tierstore_promotions_skipped_total` | `reason` = `critical` (`full`, `raced` and `failed` stay at 0) | cold hits for which no copy-back was asked, by reason |

What the counts mean:

- An S3 GET reaches the proxy as one open read (the object's meta and its body in one call),
  counted under `op="get"` whatever its outcome: a key that exists nowhere, a conditional that
  answers 304 or 412 and a range that cannot be satisfied are each a hit or a miss at `op="get"`,
  and none is counted at `op="head"`. So `op="head"` counts the client's HEADs alone.
- A hit or a miss is counted when the tier answers, an error when it fails with anything but
  "not found"; an error is neither a hit nor a miss, so a tier that is down does not lower its
  hit rate and shows in `read_errors_total`.
- Bytes served are counted when the proxy hands them to the front, so a client that
  disconnects while they are sent still has them counted.
- `critical` is a skip because the hot tier is at or above its critical mark by the usage the
  reconciler published (the budget included), and also while the proxy takes the hot tier to be
  full because it answered an earlier put with "full" a moment ago. A read of a cold object that
  is not skipped asks for its copy-back on the entry of its key and copies nothing itself: the
  copy is the reconciler's, so the proxy counts no promotion. The releases before this one counted
  promotions, promoted bytes and the skips `full`, `raced` and `failed`; those series are still
  rendered, at 0, since the proxy no longer makes the copy.
- A ranged get while the hot tier is at or above its critical mark reads only the range from the
  cold tier: `bytes_read` is the range, no copy-back is asked, and a `critical` skip is counted.
- A head asks for no copy-back.

PromQL, over the reads that fetch data (`op="get"`):

```
# hot hit rate
sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m]))
  / sum(rate(tierstore_reads_total{tier="hot",op="get"}[5m]))
# the same with errors left out of the denominator
sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m]))
  / (sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m])) + sum(rate(tierstore_misses_total{tier="hot",op="get"}[5m])))
# share of gets answered by the cold tier
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
  / sum(rate(tierstore_hits_total{op="get"}[5m]))
# cold reads per second
sum(rate(tierstore_reads_total{tier="cold",op="get"}[5m]))
# errors per second of a tier
sum by (tier) (rate(tierstore_read_errors_total[5m]))
# bytes read from the cold tier per byte answered from it
rate(tierstore_bytes_read_total{tier="cold"}[5m]) / rate(tierstore_bytes_served_total{tier="cold"}[5m])
```

### Local cache

When the build-node cache (`local_cache`, see "A build node: a read cache" above) is configured,
its block of `tierstore_cache_*` series renders, after the proxy's and the OCI front's. Until those
series take the shared names, a query over every tier reads them as follows:

| shared series | `tierstore_cache_*` series |
|---|---|
| `tierstore_hits_total{tier="local",op="get"}` | `tierstore_cache_hits_total` |
| `tierstore_misses_total{tier="local",op="get"}` | `tierstore_cache_misses_total` |
| `tierstore_bytes_served_total{tier="local"}` | `tierstore_cache_bytes_served_local_total` |
| `tierstore_bytes_served_total{tier="cold"}` (the cache's upstream is the cold store) | `tierstore_cache_bytes_served_upstream_total` |

Its hit rate is `rate(tierstore_cache_hits_total[5m]) / (rate(tierstore_cache_hits_total[5m]) +
rate(tierstore_cache_misses_total[5m]))`. The cache's fills, skips and evictions have no
counterpart here.

## Roles of a key

| role | flag | may do |
|---|---|---|
| full | `--access-key-file` | everything the front serves |
| read-only | `--read-only-key-file` | get, head, list |
| presigning | `--presign-key-file` | sign presigned PUT URLs for one object each under a prefix |
| GC | `--gc-key-file` | a scoped key with list, delete and abort on one prefix: list, head, delete and abort uploads there |
| scoped | `--scoped-key-file` | read, create and delete under the prefixes of three lists |

The full, read-only and scoped keys also sign presigned GET URLs (`server::presign_get`): a client
that holds the URL and no key may GET or HEAD one object, with any `Range`, with the rights of the
key that signed it, until it expires (at most `--presign-max-expires`, 15 minutes by default). A URL
is a bearer credential: use TLS where that matters. See "Presigned GET URLs" in
`packaging/README.md`.

### Scoped keys

A scoped key's file is JSON of exactly `access_key_id`, `secret_access_key`, `read_prefixes`,
`write_prefixes` and `delete_prefixes`; the lists may be empty, but not all, and every prefix
ends in `/`, is not, or under, or over `_journal/` or `_tiering/`, and has neither as a segment.

A call whose key or listing `prefix` has `..` anywhere in it is refused, even inside a prefix.
The server library does not itself force the access key ids of its keys to differ (`ServerConfig`
is plain data); the command line does, and exits 2.

- `read_prefixes`: GetObject, HeadObject, ListObjectsV2 and ListMultipartUploads (a listing's
  `prefix` parameter must be inside one);
- `write_prefixes`: create-only PutObject (`If-None-Match: *` or no condition; `412` if the key
  exists) and the calls of a multipart upload (create, part, complete, abort);
- `delete_prefixes`: DeleteObject (not AbortMultipartUpload: a delete prefix cannot cancel an upload).

A scoped key never replaces (`If-Match`) and never copies (`x-amz-copy-source`). Everything else is
`403 AccessDenied`. Two uses:

- the frontend's pack store: `read_prefixes: ["cas/"]`, `write_prefixes: ["cas/f/<writer>/"]`,
  `delete_prefixes: []` (add the writer prefix to the delete list to let the frontend remove its
  self-test object and the object of a failed write);
- an append-only history store, `--history-objects 's3://<bucket>/history?endpoint=...&conditional-put=true'`
  (without the last parameter a retried chunk gets `412` for good): `read_prefixes`
  and `write_prefixes` of `["history/"]`, `delete_prefixes: []`.

See "Scoped keys" in `packaging/README.md` for the table of calls.

## HTTPS on the S3 front

On main (not in v0.0.41) `--tls-cert PATH --tls-key PATH` (the keys `server.tls_cert_file` and
`server.tls_key_file`) make every socket of the S3 front serve HTTPS. Without them it serves plain
HTTP, the default; off loopback the start then prints one warning line on stderr, naming the
non-loopback addresses. The certificate may name the front by a DNS name, by an IP address, or both
(a DNS name is enough whatever the listen address; with none, each listen address needs an IP name).
A key file that any user can read, a key that is not the certificate's, an expired certificate, one
of the two flags without the other (exit 2) and a file that cannot be read (exit 1) stop the start.
The files are read once; a new certificate takes a restart. A tier's `endpoint` is still `http://`.

## The OCI read front

With `--oci-listen ADDR` the binary also serves the objects of the store as a read-only container
registry (`server::oci`), so that podman and other registry clients pull an image by digest from the
same tiers. It runs in one of two modes:

- **HTTPS**, with `--oci-tls-cert PATH --oci-tls-key PATH`, at any address.
- **Plain HTTP**, with neither flag, at any address. A node's own container engine pulls from the
  front of its own store over loopback (127/8 or `::1`), so the traffic never leaves the host.
  Off loopback (`0.0.0.0` and `::` included) the front still starts, and prints one warning line
  on stderr saying it serves plain HTTP on a non-loopback address, since a plain connection can be
  read and changed on its path: give the certificate and key there. One of the two flags without
  the other is refused (exit 2).

The start line and `/metricz` say which mode runs.

In a file given with `--config`, the front is the optional `[oci]` table (`listen`, `tls_cert_file`,
`tls_key_file`, `prefix`, `max_connections`, `connection_lifetime`), with the rules of the flags
(`listen` is needed in it, the certificate and the key go together, `max_connections` is at least
1); the `--oci-*` flags are refused with the file, and `--print-config` writes them as the table.

The front runs on both shapes of the file. On a storage node it reads through the proxy. On a
build node (`[local_cache]` and `[cold_store]`, see above) the same `[oci]` table, with the same
keys and rules, starts the front beside the S3 listener over the node's local cache: a blob or a
manifest is a hit on the disk when the cache holds it, and a miss under a `[[policy]]` prefix with
`cache = true` is filled from the upstream, as an S3 GET is. A build node has no flag form, so its
front is set only in the file; the start line ends with ` oci-listen=<address> (http|https)`, and
`/metricz` shows the OCI block and the cache block.

- `GET` and `HEAD` of `/v2/`, `/v2/<name>/manifests/sha256:<hex>` and
  `/v2/<name>/blobs/sha256:<hex>`; anything else is 404, and any other method 405.
- An object is at `<--oci-prefix>blobs/sha256/<hex>` (`oci/` by default), manifests included. The
  repository name in a URL is checked against the distribution grammar and then ignored: one
  content-addressed namespace.
- Anonymous: no token is asked for and `Authorization` is ignored. Only digest-named objects under
  the prefix can be read.
- A manifest is checked against its digest before it is sent; a blob is hashed as it is read and cut
  short, not delivered whole, if it is not its digest. Tags are not served. A manifest is measured
  and then read in two store calls, so one replaced between them can be answered 500
  `DIGEST_INVALID` once; a retry succeeds.
- Memory is bounded by `--oci-max-connections` (64 by default) times the buffer a connection holds,
  up to 4 MiB (a blob chunk or a manifest).
- In HTTPS mode the certificate must carry an IP subjectAltName for the listen address (clients dial
  an IP), checked at start. The files are read once: a new certificate takes a restart. The
  connection limits, timeouts and drain are the same in both modes (a plain connection has no
  handshake to time).
- `GET /metricz` on the S3 listener has the counters.

Putting an image in, and pulling it with podman and js-oci, are in `packaging/README.md`
("The OCI read front"); the behaviour is specified by the tests in `tests/oci_front` (over real TLS and over plain HTTP, both
on loopback) and `src/server/oci`.
