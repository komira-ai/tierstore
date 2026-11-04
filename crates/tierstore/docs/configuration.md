# Configuration

tierstore is configured by one TOML file given with `--config <path>`. The file is the only way to say what the data path is: which interfaces it serves, which tiers, which keys, which limits. A key that is left out has the default this page gives. A file holds no secret, only the paths of key files. The process takes no flag for any of it: the flags of earlier builds that did (--listen, --hot, --oci-listen and the rest) are refused at the start, with status 2 and a message that names the key of the file that replaces each, and there is no environment variable.

A file has one of three shapes, told apart by its tables: a storage node (a hot tier and a cold tier), a build node (a read cache on local disk in front of one upstream, `[local_cache]`), and a storage node with a cache in front of its own proxy (all three tables). Every shape has at least one `[[interface]]` of type `s3`.

## A file for a storage node

A hot tier on SSD in front of a cold tier on spinning disks, both directories:

```toml
[[interface]]
type   = "s3"
params = { addresses = ["192.0.2.10:9000"] }

[server]
proxy_id          = "tierstore-1"
bucket            = "tierstore"
access_key_file   = "/etc/tierstore/access.key"
read_only_key_files = ["/etc/tierstore/daemons.key"]

[hot_store]
kind   = "dir"
path   = "/srv/tierstore/hot"
budget = "800GiB"

[cold_store]
kind = "dir"
path = "/srv/tierstore/cold"
```

The same with S3 buckets for the tiers, and every other key left at its default spelled out:

```toml
[[interface]]
type   = "s3"
params = { addresses = ["192.0.2.10:9000"] }

[server]
proxy_id            = "tierstore-1"
bucket              = "tierstore"
region              = "us-east-1"
max_request_bytes   = 268435456
access_key_file     = "/etc/tierstore/access.key"
read_only_key_files = ["/etc/tierstore/daemons.key"]
presign_key_files   = ["/etc/tierstore/pack-presign.key"]
gc_key_files        = ["/etc/tierstore/pack-gc.key"]

[hot_store]
kind               = "s3"
bucket             = "tierstore-hot"
endpoint           = "http://s3-hot.example:9000"
region             = "us-east-1"
credentials_file   = "/etc/tierstore/hot.credentials"
budget             = "1TiB"
high_water         = 80
low_water          = 70
critical           = 95
copy_parallel      = 16
lease_ttl          = "30s"
reconcile_interval = "5s"
usage_stale_after  = "2m"
journal_linger     = "20ms"
journal_retention  = "24h"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoint         = "http://s3-cold.example:9000"
credentials_file = "/etc/tierstore/cold.credentials"
```

Start it with the file and nothing else but the flags of the run:

```sh
/usr/libexec/tierstore/tierstore --config /etc/tierstore/tierstore.toml --drain-lb-wait 10s --drain-deadline 15m
```

## A file for a build node

A read cache in front of one upstream, with no hot tier. The file has `[local_cache]` and `[cold_store]` and no `[hot_store]`; the cold store is the upstream, read with a read-only key:

```toml
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9400"] }

[server]
proxy_id        = "tierstore-cache-1"
bucket          = "tierstore"
access_key_file = "/etc/tierstore-cache/access.key"

[local_cache]
path       = "/var/lib/tierstore-cache"
layout     = "digest-v1"
budget     = "40GiB"
high_water = 90
low_water  = 75
critical   = 95
free_floor = 10
max_concurrent_fills = 4

[cold_store]
kind             = "s3"
bucket           = "tierstore"
endpoint         = "http://s3-cold.example:9000"
credentials_file = "/etc/tierstore-cache/upstream-read.key"

[[policy]]
prefix = "cas/b/"
cache  = true
```

How the cache behaves is in [operations](operations.md#a-build-node). The file is started as a storage node's is, with `--config` and the flags of the run.

## A file for a storage node with a cache

One process that owns the data and keeps a disk cache in front of its own proxy: the file has `[hot_store]`, `[cold_store]` and `[local_cache]`. The S3 front and the OCI front read from the cache; the cache reads from, and writes to, the proxy, which is a storage node's (both tiers, the journal, the reconciler). The tiers are not tested at the start, as a storage node's are not either:

```toml
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9300"] }

[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "/etc/tierstore/access.key"

[hot_store]
kind   = "dir"
path   = "/srv/tierstore/hot"
budget = "500GiB"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoint         = "http://s3-cold.example:9000"
credentials_file = "/etc/tierstore/cold.key"

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"

[[policy]]
prefix = "cas/b/"
cache  = true
```

The keys of each table are those of the storage node and of the build node. What differs: `local_cache.write_through` is true whether it is left out or given (the node owns the data and takes writes, which pass through the cache so that its entries stay right), and `false` is refused; the cache path (absolute, as the directory of a `dir` tier must be) may not be the directory of a `dir` tier, inside it or around it, nor around the lock file or the key files of an `oci` interface; `server.backend_check_wait`, `cold_store.skip_checks` and the reconciler's keys of `[hot_store]` keep their meaning (a build node refuses them). What the cache keeps, how a hand-over behaves and what to expect are in [operations](operations.md#a-storage-node-with-a-local-cache) and [limits](limits.md#a-storage-node-with-a-local-cache). The start line says `mode=storage-cache`.

## Types

- A duration is a string of a whole number and `ms`, `s`, `m` or `h`: `250ms`, `15m`.
- A size is an integer of bytes, or a string of a whole number and `B`, `KiB`, `MiB`, `GiB` or `TiB`: `"500GiB"`.
- A path is a string. A list of paths is an array.
- An address is a string, `"192.0.2.10:9000"` or `"[2001:db8::1]:9000"`. The `addresses` param of an interface takes one address or an array of them. Only an address named twice is refused by the check of the file. A wildcard and a specific address on one port (`0.0.0.0:9000` and `192.0.2.10:9000`) are refused by the system at bind on Linux (the start fails, naming the address); on macOS and the BSDs `SO_REUSEADDR` can let such sockets share the port, so do not rely on the refusal there.
- A number above 9223372036854775807 cannot go in a file.

## `[[interface]]`

An interface is a front over the same proxy: one entry for each place the process listens and what it serves there. The entry is `type` and `params`:

```toml fragment
[[interface]]
type   = "s3"
params = { port = 9300 }

[[interface]]
type   = "oci"
params = { addresses = ["127.0.0.1:5000"], tls_cert_file = "/etc/tierstore/oci.crt", tls_key_file = "/etc/tierstore/oci.key", prefix = "oci/" }
```

At least one entry has type `s3`. The entries of a type are served together: one server and one drain for all the `s3` entries (no limit on connections), and the same for the `oci` entries (with their one `max_connections`), on every address of the entries of that type. An address two entries listen on, or one entry lists twice, is refused, naming the entries (a port of 0, which asks the system for a port, is never the same as another). Each entry says where it listens with one of `params.port` and `params.addresses`, and not both.

| Key | Type, default | What |
|---|---|---|
| `interface.type` | `"s3"` or `"oci"`, required | what the entry serves. `"s3"` is the S3 API, `/healthz`, `/readyz`, `/usagez` and `/metricz`. `"oci"` is the read-only OCI registry front. `"fs"` is not served yet and is refused with a message that says so; any other value is refused |
| `interface.params` | table, required in effect | the params of the entry, below. An entry without `params.port` or `params.addresses` is refused |
| `params.port` | port, 1 to 65535 | listen on this port on the IPv4 wildcard, `0.0.0.0:<port>`. For loopback, another address or IPv6, give `params.addresses` |
| `params.addresses` | address or list of addresses | listen on exactly these. To listen on the wildcard of both families give `["0.0.0.0:<port>", "[::]:<port>"]`: an IPv6 address is bound for IPv6 only, so the two do not clash. A start that cannot bind every address is refused |
| `params.tls_cert_file` | path, `oci` only | the certificate chain (PEM), leaf first. It must carry an IP subjectAltName for each listen address. Both this and the key, or neither. An `s3` entry refuses it: the certificate of the S3 front is `server.tls_cert_file` |
| `params.tls_key_file` | path, `oci` only | the private key (PEM); a file any user can read is refused |
| `params.prefix` | string, `oci/`, `oci` only | the key prefix the registry reads, ending in `/`; not inside `_journal/`, `_tiering/` or `_entries/` |
| `params.max_connections` | count, 64 (at least 1), `oci` only | connections open at once |
| `params.connection_lifetime` | duration, `1h`, `oci` only | the longest a connection lives |

An `s3` entry takes no param but `params.port` and `params.addresses`. The `oci` entries make one registry front, which has one certificate, one prefix and one set of limits, so they must agree on `params.tls_cert_file`, `params.tls_key_file`, `params.prefix`, `params.max_connections` and `params.connection_lifetime`: an entry that differs from the first `oci` entry is refused, naming the param. With a certificate and key the registry is HTTPS; with neither it is plain HTTP, and off loopback the start prints one warning line for each such address. The front reads objects at `<prefix>blobs/sha256/<hex>` and serves GET and HEAD of `/v2/`, of manifests by digest and of blobs by digest. It is anonymous: anyone who can connect can read any digest under the prefix, so the network decides who may. Tags are not served. The certificate and key are read once; a new certificate takes a restart.

A file written for the release before still loads: its `listen` key of `[server]` (an address or a list) is read as an `s3` entry at those addresses, and its `[oci]` table (`listen`, which the table needs, and the same five params, with `listen` as the key of the addresses) as an `oci` entry. They are added after the entries the file lists, and every rule above holds for them, so an address that a legacy key and an entry both listen on is refused. A file that sets only these keys starts on the release before and on this one; a file with `[[interface]]` entries is refused by the release before, which does not know the table.

When the file has a `[local_cache]` table (a build node, or a storage node with a cache), the server also binds 127.0.0.1 on the port of each `s3` address, once for each distinct port, so that processes on the node reach it at `http://127.0.0.1:<port>`; the start is refused, naming loopback and the port, if that cannot be bound. Nothing is added for a port that already has `127.0.0.1` or the IPv4 wildcard `0.0.0.0` on it (the wildcard covers loopback, and a second bind would be refused), and an IPv6 address adds IPv4 loopback only, never `::1`. Without `[local_cache]` exactly the listed addresses are bound.

## `[server]`

| Key | Type, default | What |
|---|---|---|
| `server.proxy_id` | string, required | names this process's journal and lease in the hot tier; keep it across restarts and give each process over the same tiers its own |
| `server.slot` | port, 1 to 65535, none | The private port this instance runs as: the port of its `s3` interface, which the hand-over's unit `tierstore@<port>` is given in its own file (see the packaging README). It is the `slot` label of `tierstore_build_info` on `/metricz`, which the update hook's probe through the public port reads to know which instance answered. It binds nothing, and it is the same on a storage node and a build node. Without it the gauge has no `slot` label. `slot = 0` or a value that is not a port is refused (exit 2) |
| `server.bucket` | string, required | the one bucket clients see; not `healthz`, `readyz`, `usagez` or `metricz` |
| `server.region` | string, `us-east-1` | the region in the credential scope of a request's signature (not the tiers' regions) |
| `server.access_key_file` | path, required | the full key file |
| `server.tls_cert_file` | path, none | The certificate chain (PEM, leaf first) that makes the S3 front serve HTTPS; with neither this nor the key it serves plain HTTP, the default. Every socket of the front is HTTPS, a build node's added loopback socket included. The leaf must name the front: it needs a DNS subjectAltName (any, since the server cannot know which name a client dials) or else an IP subjectAltName for each address of an `s3` interface; it may have both. A leaf that is expired or not yet valid, or a certificate with no subjectAltName, is refused. Both this and the key, or neither. The files are read once, so a new certificate takes a restart. A tier's `endpoint` is still `http://` (TLS to a tier is not built yet), so a build node cannot use another node's HTTPS front as its upstream |
| `server.tls_key_file` | path, none | The private key (PEM) of `server.tls_cert_file`. A file that any user can read is refused (mode 0640 or 0600); a key that is not the certificate's is refused. Plain HTTP off loopback prints one warning line on stderr at start, naming the addresses and advising these two keys |
| `server.read_only_key_files` | paths, none | read-only key files |
| `server.presign_key_files` | paths, none | presigning key files |
| `server.gc_key_files` | paths, none | GC key files |
| `server.scoped_key_files` | paths, none | scoped key files |
| `server.presign_max_expires` | duration, `15m` | the longest expiry of any presigned URL, a GET or a PUT |
| `server.grant_key_files` | paths, none | deprecated, ignored: the read grant was removed (use a presigned GET URL). Accepted so that a file written for the release before still starts; the files are not read, and the server logs `tierstore: server.grant_key_files is ignored: the read grant was removed; use a presigned GET URL` |
| `server.grant_max_lifetime` | duration, none | deprecated, ignored, like `server.grant_key_files`; its warning names `server.grant_max_lifetime` |
| `server.max_request_bytes` | bytes, 268435456 (256 MiB) | the longest body of a request: the longest object and the longest part. A body over it is refused with 413 and nothing is stored |
| `server.max_concurrent_puts` | count, 32 | the most put bodies and parts taken at once; one more waits for one to end, up to `server.put_permit_wait` |
| `server.put_permit_wait` | duration, `15s` | How long a put that finds every put slot taken waits for one to end before it is answered 503 `SlowDown` with `Retry-After: 1`; `0s` answers at once. No byte of its body is read while it waits, and a waiter counts as in flight in a drain (never past `--drain-deadline`) |
| `server.put_min_bytes_per_sec` | bytes a second, 1024 | the least average rate of a body, counted over the time spent waiting for the client; 0 turns it off |
| `server.put_rate_grace` | duration, `30s` | the time spent waiting for the client before that rate is held |
| `server.body_idle_timeout` | duration, `30s` | how long a body or an answer may go without a byte; more than zero |
| `server.min_secret_bytes` | bytes, 16 | the fewest bytes of the secret of any access key (full, read-only, presigning, GC, scoped); a key file with a shorter secret refuses the start |
| `server.s3_request_timeout` | duration, `60s` | the limit on a request to an `s3` tier (both tiers); more than zero. Set it longer than the longest multipart completion your store takes |

## `[hot_store]` and `[cold_store]`

Both tables have `kind` and the keys it needs. `kind` is `memory` (no other key), `dir` (needs `path`) or `s3` (needs `bucket`, `endpoint` and `credentials_file`; `region` is optional). A key the kind does not take is refused.

| Key | Type, default | What |
|---|---|---|
| `hot_store.kind`, `hot_store.path`, `hot_store.bucket` | required | the hot tier |
| `cold_store.kind`, `cold_store.path`, `cold_store.bucket` | required | the cold tier. Multipart uploads go straight to it, so it must do them (all three kinds do) |
| `hot_store.endpoint` | `http://host[:port]`, required for `s3` | the hot tier's store. No path; TLS is not built yet |
| `hot_store.region` | string, `us-east-1` | the signing region of the hot tier's store |
| `hot_store.credentials_file` | path, required for `s3` | the key file of the hot tier's store |
| `cold_store.endpoint` | `http://host[:port]`, required for `s3` | the cold tier's store |
| `cold_store.region` | string, `us-east-1` | the signing region of the cold tier's store |
| `cold_store.credentials_file` | path, required for `s3` | the key file of the cold tier's store |
| `hot_store.conditional_delete` | boolean; `false` for `s3`, `true` for `memory` and `dir` | whether a conditional delete of the hot tier is sent as a DELETE with `If-Match` (`true`) or as a read of the tag before a plain DELETE (`false`). Set it `true` for a store that enforces `If-Match` on a DELETE, such as Amazon S3. `false` is refused for `memory` and `dir`, which delete conditionally in one atomic step. tierstore does not ask the store; see [operations](operations.md#conditional-delete) |
| `cold_store.conditional_delete` | boolean; as for the hot tier | the same, for the cold tier |

Only `[hot_store]` has these, since only a process with a hot tier has them:

| Key | Type, default | What |
|---|---|---|
| `hot_store.budget` | size, required | the bytes the hot tier is meant to hold. The water marks are shares of it |
| `hot_store.high_water` | percent, 80 | above this share of the budget, eviction begins |
| `hot_store.low_water` | percent, 70 | eviction goes on down to this share |
| `hot_store.critical` | percent, 95 | above this share a read of a cold object is not copied back (a put is not slowed; it goes to the cold tier only when the hot tier is full) |
| `hot_store.journal_linger` | duration, `20ms` | how long a batch of journal entries waits for more |
| `hot_store.journal_retention` | duration, `24h` | how long an applied journal object is kept |
| `hot_store.copy_parallel` | count, 16 | the most objects the reconciler copies at once |
| `hot_store.lease_ttl` | duration, `30s` | how long the lease object must stand unchanged, on another reconciler's own monotonic clock, before it takes the lease over; the holder renews every third of it |
| `hot_store.reconcile_interval` | duration, `5s` | how often the reconciler works; shorter than the lease |
| `hot_store.lock_file` | absolute path, none | The lock file of the node: the reconciler works only while this process holds an exclusive lock on it. A process that finds the lock held serves reads and writes through its own proxy and tries the lock again every second; the system frees it when the holder exits or crashes. If the file is replaced, the holder stops its reconciler and waits for the lock on the new file. Give every process of one node the same path, on a local file system, in a directory that nothing removes (`/var/lib/tierstore/`, not a directory under `/run`). The start refuses a path that is not absolute, one that names no file, and one that cannot be opened or is on a network file system. Without it the reconciler works as soon as the process starts |
| `hot_store.usage_stale_after` | duration, `2m` | how old the published usage may be before `/usagez` says 503; longer than the lease |
| `hot_store.heartbeat` | duration, `10s` | how often a proxy rewrites its registration `_tiering/proxies/<proxy id>` in the hot tier, and the longest one such call may take. Only the puts on journal entries (active format) depend on it: they are refused while the proxy is not live |
| `hot_store.departure` | duration, `60s` | the time a proxy that is not heard of stays live, less the margin: `Proxy::is_live` is true until the last acknowledged beat plus `departure` less `deadline_margin`. Only the puts on journal entries (active format) read it: they are refused while it is false. The journal path of a put, a delete and a read does not, and nothing else is refused or destroyed by it yet. The removal of registrations does not use it: that uses `registration_retention` |
| `hot_store.deadline_margin` | duration, `10s` | the time taken off `departure` for `is_live`, and the time `Bounded` must find left before it sends a destroying call, which it also cuts at that time. Only the puts on journal entries (active format) check that time, and only the heartbeat uses `Bounded` |
| `hot_store.registration_retention` | duration, `0s` | how long the registration of a proxy that heartbeated must stay unchanged before the reconciler removes it, when the proxy left no journal object. A registration with no heartbeat (the empty one of an older release) is never removed. `0s`, the default, removes none: a proxy of a release without the heartbeat that reopens a registration this release wrote never rewrites it, and the removal would take it for a departed owner's, so set a time only once no such proxy runs |

The water marks are percents from 1 to 100. The start refuses `low_water` at or above `high_water`, `high_water` above `critical`, a `reconcile_interval` at or above `lease_ttl`, and a `usage_stale_after` at or below `lease_ttl`. It also refuses a `heartbeat` or a `deadline_margin` of zero, a `departure` under six times `heartbeat`, a `deadline_margin` over a quarter of `departure`, and a `registration_retention` that is neither zero nor at least `departure`, naming the values.

## `[local_cache]`

The read cache of a build node, or of a storage node that keeps it in front of its own proxy (a file with `[hot_store]` too). A build node also serves its S3 port on 127.0.0.1 (see `[[interface]]`). A GET or HEAD of a key that a `[[policy]]` with `cache = true` holds is answered from the directory and filled from the upstream on a miss; every other call to the front but GET and HEAD is refused 403, unless `write_through` is `true`.

| Key | Type, default | What |
|---|---|---|
| `local_cache.path` | path, required | the directory of the cache, made if it is not there; put it on the file system that the jobs' scratch is on |
| `local_cache.layout` | string, `"digest-v1"` | the only layout: raw bytes, one file for each object |
| `local_cache.budget` | size, required | the bytes the cache is meant to hold. The marks are shares of it |
| `local_cache.high_water` | percent, 90 | above this share of the budget, a pass evicts |
| `local_cache.low_water` | percent, 75 | a pass evicts down to this share; below `high_water` |
| `local_cache.critical` | percent, 95 | above this share a fill is skipped; not below `high_water` |
| `local_cache.free_floor` | percent, 10 (0 to 99) | the share of the file system that is kept free: below it a pass evicts, even under the budget. The budget may not be above the size of the file system less this share |
| `local_cache.max_concurrent_fills` | count, 4 (1 to 64) | the most fills that run at once; a miss that finds them all running is served from the upstream and kept nowhere |
| `local_cache.max_concurrent_keeps` | count, 8 (1 to 64) | The most create-only puts copied into the cache at once, apart from the fills; a put that finds them all running succeeds and is not kept. A process that does not hold the lock of the cache directory (see [operations](operations.md#the-cache-lock-and-read-only-mode)) keeps no put at all |
| `local_cache.keep_wait` | duration, `5s` (above 0) | The longest a put waits, once the upstream has the object, for its copy to land in the cache; past it the put is answered and the copy finishes in the background |
| `local_cache.keep_deadline` | duration, `60s` (above 0, not below `keep_wait`) | The longest a copy takes in all, from the upstream's acknowledgement; a copy not done by then is abandoned and its stage file removed |
| `local_cache.write_through` | boolean, `false` | `false`: the node refuses every write with 403. `true`: it takes the writes its keys may do and sends each to the upstream, which must then be read with a key that may write; the upstream is not tested at start. In a file with `[hot_store]` too it is `true` whether left out or given, and `false` is refused |
| `local_cache.whole_fill_limit` | size, 64 MiB (1 byte to `budget`) | The most an object may be for a ranged read of it to fill the whole of it; a larger object is passed through and kept nowhere. A fill reads 8 MiB at a time whatever the limit, so raising it does not raise the memory of the fills |
| `local_cache.cache` | boolean, `true` | `false` is refused: leave `[local_cache]` out to run without a cache |
| `local_cache.scrub_rate` | rate, `"16MiB/s"` | a size and `/s`, such as `"50MiB/s"`: the rate at which the scrub re-reads cached objects and hashes them again; `"off"` or `"0B/s"` turns the scrub off (not in a release yet, see [limits](limits.md)) |
| `local_cache.scrub_min_round` | duration, `"1h"` | the least time between the starts of two scrub rounds, so a small cache is not read again and again; at least `"1s"`, or `"0s"` for none |

The upstream is `[cold_store]`, with any `kind`. In place of `endpoint` it may have a list `endpoints` of exactly one entry. With `write_through = true` the node takes create-only puts, presigned puts, multipart uploads and deletes, and sends each to the upstream before it answers; the keys' roles apply as on a storage node, and a put that replaces (`If-Match`) and a copy are still refused. A create-only put under a `cache = true` prefix is also kept on the node before it is answered, so its next read is a local hit; a copy that cannot be kept never fails the put. A build node leaves out `skip_checks` and the reconciler's keys and `server.backend_check_wait`. It may have an `oci` interface (an `[[interface]]` with `type = "oci"`): the registry front then reads through the cache, and the start refuses the file unless a `[[policy]]` with `cache = true` covers the blobs, the keys under `blobs/sha256/` below `params.prefix`.

## `[[policy]]`

`[[policy]]` has a `prefix` (empty for all the rest, or ending in `/`) and `cache`. Without `[local_cache]` a policy is read and checked and has no effect, and `policy.cache = true` is refused, since there is no cache. With `[local_cache]` (a build node, or a storage node with a cache) a key is kept if the longest prefix that holds it says `cache = true`, and at least one policy must say so. Two non-empty prefixes may not nest, a prefix may not be listed twice, and none may lie under `_journal/`, `_tiering/` or `_entries/`.

## The flags of a run

These describe one run, not the data path, and have no key:

| Flag | Default | What |
|---|---|---|
| `--config` | none | the file; a start without it is refused with status 2 |
| `--check-config` | off | with `--config`: load the file, read the key files it names, the S3 front's certificate and key if it has them, and for an `oci` interface check the prefix, the connection lifetime and the TLS files; then exit 0 and print nothing, or print the refusal and exit 2. It binds nothing and asks no store. What it does not check is listed below. Without `--config` it is a usage error |
| `--capabilities` | off | print what this build can do, one capability to a line, and exit 0 (a build with none prints nothing). It needs no file, and with any other argument it prints nothing on stdout, says `--capabilities takes no other flag` on stderr and exits 2 |
| `--drain-lb-wait` | `10s` | on SIGTERM, the settle: how long `/readyz` says 503 while the server goes on accepting and serving |
| `--drain-deadline` | `15m` | then how long it waits for requests in flight and for the connections it closes (a lingering close, see [operations](operations.md#drain-and-restart)) |

`--check-config` finds what the loader refuses (see "What the loader refuses"), a `bucket` that is a probe path name, a zero `body_idle_timeout`, a key file that is not owned by the user, is open to others or is a link, two keys with one id, and, in the file of a build node, a zero `s3_request_timeout`. It does not find what a start checks after that: the water marks out of order, `reconcile_interval` not below `lease_ttl`, `usage_stale_after` not above `lease_ttl`, the heartbeat values out of the relations above, or a zero `s3_request_timeout` in a storage node's file (a start exits 1 on each, and the unit tries again every 30 seconds); a hot tier that is down (the proxy's open fails, `opening the proxy`) or a store that does not do what tierstore requires of it (nothing checks); the cache directory and a `budget` over the size of its file system. A file that passes `--check-config` can still fail to start.

`--help` and `--version` are the usual: `--version` prints the version, `+` and the commit the binary was built from (`tierstore 0.3.0+0123456789ab`), and the start line ends with ` commit=<commit>`. The commit is twelve hex digits, or `unknown` for a build that had neither a git checkout nor a stamp.

## What the loader refuses

Each of these exits with status 2 and names the file and the key (a syntax error names the line and the column, never the value):

- a key or a table the format does not have, and a value of the wrong type;
- a key that holds a secret: keys and credentials are paths of files whose names end in `_file` or `_files`; `secret_access_key`, `access_key_id`, `password`, a name with `token` in it and the like are refused anywhere;
- a file that is none of a storage node's (`[hot_store]` and `[cold_store]`), a build node's (`[local_cache]` and `[cold_store]`, no `[hot_store]`) and a storage node's with a cache (all three tables);
- in a storage node's file with a cache, `write_through = false`, a relative cache `path`, `dir` tier directory, `lock_file` or `oci` key file, and a cache `path` that is a tier's directory, inside it or around it (or around the lock file or the key files of an `oci` interface);
- in the file of a build node or of a storage node with a cache, an `oci` interface whose blobs no `[[policy]]` with `cache = true` keeps, a `layout` other than `digest-v1`, `cache = false`, marks out of order, no `[[policy]]` with `cache = true`, and more than one entry in `endpoints`;
- a key a tier's `kind` does not take, a `dir` tier without `path`, an `s3` tier without `bucket`, `endpoint` or `credentials_file`, and a key only one tier has in the other (`budget` and the reconciler's keys belong to `[hot_store]`, `skip_checks` to `[cold_store]`);
- a water mark outside 1 to 100, and a budget, count or limit of zero;
- an `[[interface]]` that breaks the rules of its section above: a type that is not `s3` or `oci` (`fs` is named as not served yet), neither `params.port` nor `params.addresses` or both, a port of zero, an address listened on twice, a param its type does not take, only one of the certificate and the key, `params.max_connections = 0`, `oci` entries that disagree, and a file with no `s3` entry;
- a `[[policy]]` that breaks the rules above.

## Moving from flags

A unit that gave flags is moved by writing the file and changing `ExecStart=` to `--config <path>` with the flags of the run. Each key of `[server]`, `[hot_store]` and `[cold_store]` has the name of the flag that held it with `_` for `-`, the flags that were repeated are arrays (`server.read_only_key_files`), the budget flag is `hot_store.budget`, the flag that skipped a check is `cold_store.skip_checks`, the listen flags became the `s3` interfaces and the registry flags the params of an `oci` interface (the `listen` key and the `[oci]` table of a file stay readable, see `[[interface]]`). A start that still gives one of those flags exits with status 2 and a message that names its replacement.

## Not in a release yet

- **Several upstream endpoints.** A cache node listing more than one upstream, with failover between them.

## Deprecated settings

Two settings belonged to the start-up self-test that earlier releases ran. tierstore no longer tests its stores (see [what tierstore requires of a store](operations.md#what-tierstore-requires-of-a-store)), so they do nothing. They are still accepted, so that a file written for an earlier release starts, and each one given is one line on stderr at the start: `tierstore: cold_store.skip_checks is deprecated and ignored: tierstore no longer tests its stores at start-up; remove it`, and the same for `server.backend_check_wait`. Their values are not used: `skip_checks` takes any list of texts, and `backend_check_wait` is still parsed as a duration, as it always was, so a value that is not one is still refused.

| Key | What |
|---|---|
| `server.backend_check_wait` | a duration; ignored |
| `cold_store.skip_checks` | a list of texts; ignored. A build-node file still refuses both keys |
