# Configuring tierstore

This page is for an operator who is setting tierstore up: which few keys to set, a complete file for each shape in use, how to size the budgets, how to bring a new store into use, and what to cache. [Configuration](configuration.md) is the reference: it lists every key and its default, and every key left out of a file below has that default. Every block of this page loads through the same loader as the process, and every key and flag it names exists (a test of the crate holds the page to the code).

The process is started with the file and nothing else for the data path:

```sh
/usr/libexec/tierstore/tierstore --config /etc/tierstore/tierstore.toml --drain-lb-wait 10s --drain-deadline 15m
```

`--check-config` loads the file and reads the key files it names, binds nothing and asks no store, and exits 0 and prints nothing or prints the refusal and exits 2, so run it on a new file first. It does not make the checks of a start that need the disk and the stores (the water marks, the cache directory, the credentials of a tier); [Configuration](configuration.md#the-flags-of-a-run) lists what it leaves out:

```sh
/usr/libexec/tierstore/tierstore --config /etc/tierstore/tierstore.toml --check-config
```

## The keys to set

Most of the table of keys is tuning that has a default for a reason. A new file sets these, and only these:

| Key | What to put | Shape |
|---|---|---|
| `interface.type` and `params.addresses` | `"s3"` and the address or addresses to listen on: a private address on a storage node, `127.0.0.1` on a build node | all |
| `server.proxy_id` | a name of this process; give each process over the same tiers its own, and keep it across restarts | all |
| `server.bucket` | the one bucket clients see | all |
| `server.access_key_file` | the full key file | all |
| `hot_store.kind` and, for `s3`, `hot_store.bucket`, `hot_store.endpoint` or `hot_store.endpoints`, `hot_store.credentials_file` | the hot tier; for `dir`, `hot_store.path` | storage node, three tiers |
| `hot_store.budget` | the bytes the hot tier is meant to hold, below what the disk or bucket can hold (see "Sizing") | storage node, three tiers |
| `cold_store.kind` and the keys it needs, as for the hot tier | the cold tier. On a build node that chains to storage nodes this is the upstream | all |
| `local_cache.path` and `local_cache.budget` | the cache directory and its size | build node, three tiers |
| `[[policy]]` with `cache = true` | the prefixes to cache (see "What to cache") | build node, three tiers |

Everything else (the timeouts, the retries, the reconciler's intervals) is left at its default until a measurement says otherwise. A tuning value that you change is a line in the file with a reason beside it.

## A storage node with S3 tiers

A hot tier on SSD and a cold tier on spinning disks, each an S3 bucket of its own store (MinIO, for instance), and no cache. The hot store is the fast one and the cold store the large one; tierstore writes new objects to the hot bucket, copies them to the cold bucket, and evicts them from the hot bucket as it fills.

```toml
[[interface]]
type   = "s3"
params = { addresses = ["192.0.2.10:9000"] }

[server]
proxy_id            = "tierstore-1"
bucket              = "tierstore"
access_key_file     = "/etc/tierstore/access.key"
read_only_key_files = ["/etc/tierstore/daemons.key"]

[hot_store]
kind             = "s3"
bucket           = "tierstore-hot"
endpoint         = "http://s3-hot.example:9000"
credentials_file = "/etc/tierstore/hot.credentials"
budget           = "800GiB"
high_water       = 80
low_water        = 70
critical         = 95

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoint         = "http://s3-cold.example:9000"
credentials_file = "/etc/tierstore/cold.credentials"
```

The three water marks are written out at their defaults so that they are in front of you; see "Sizing". The hot bucket needs a second bucket beside it, the marks bucket (see "The marks bucket and the activation").

A second storage node over the same tiers is the same file with its own `server.proxy_id` and its own address. Both tiers are shared, not copied: see the next section.

## A build node with all three tiers in one process

One process on a build machine that owns the data and keeps a disk cache in front of its own proxy: `[local_cache]`, `[hot_store]` and `[cold_store]` in one file. The jobs on the machine read from the loopback address, hits come from the local disk, and a miss reads through the proxy.

Here the hot and the cold tier are S3 buckets of one store that is reached through three storage endpoints. `endpoints` in place of `endpoint` makes each request go to the next address in turn, and a request whose connection to its address cannot be made goes to the next one, up to `server.s3_endpoint_attempts` addresses (3 by default, one try for each of the three here).

```toml
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9300"] }

[server]
proxy_id                = "build-node-17"
bucket                  = "tierstore"
access_key_file         = "/etc/tierstore/access.key"
s3_endpoint_attempts    = 3

[hot_store]
kind             = "s3"
bucket           = "tierstore-hot"
endpoints        = ["http://storage-1.example:9000", "http://storage-2.example:9000", "http://storage-3.example:9000"]
credentials_file = "/etc/tierstore/hot.credentials"
budget           = "20TiB"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoints        = ["http://storage-1.example:9000", "http://storage-2.example:9000", "http://storage-3.example:9000"]
credentials_file = "/etc/tierstore/cold.credentials"

[local_cache]
path       = "/var/lib/tierstore-cache"
budget     = "200GiB"
high_water = 90
low_water  = 75
critical   = 95
free_floor = 10

[[policy]]
prefix = "cas/b/"
cache  = true
```

What to hold to in this shape:

- **One shared hot bucket.** Every build node of the site names the same hot bucket (and the same marks bucket) on the storage endpoints, each with its own `server.proxy_id`. Do not give each node a MinIO of its own: the hot tier is where a new object lands before the reconciler copies it, and where the nodes find each other's registrations and the lease, so a hot store that only one node reads is a copy that no other node sees, and that is lost with the node if it dies before the copy to the cold tier. The `hot_store.budget` is a figure of the bucket, so every node's file gives the same one.
- **The endpoints are one store.** The addresses of `endpoints` must be addresses of the same store (the three nodes of one MinIO cluster, say), so that any of them serves any request, the parts of a multipart upload and its completion included. Do not list the addresses of two stores.
- **Failover is for connections that could not be made.** A request that never reached an endpoint (refused, no route, name not resolved, connect timeout) is sent to the next one. A request that may have reached a store is not sent again by this key: the caller's retry goes to the next address.
- **The cache is a node's own.** `local_cache.path` is on the disk the jobs' scratch is on, not inside either tier's directory.
- **Writes pass through.** In this shape `local_cache.write_through` is true whether it is left out or given. A put under a cached prefix is also kept on the node.

## A build node chained to storage nodes

A build node that owns no data: a read cache on local disk in front of tierstores that are storage nodes. The file has `[local_cache]` and `[cold_store]` and no `[hot_store]`; the cold store is the upstream, here the S3 fronts of two storage nodes that share their tiers, read with a read-only key.

```toml
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9400"] }

[server]
proxy_id        = "build-cache-17"
bucket          = "tierstore"
access_key_file = "/etc/tierstore-cache/access.key"

[local_cache]
path                 = "/var/lib/tierstore-cache"
budget               = "200GiB"
free_floor           = 10
max_concurrent_fills = 4

[cold_store]
kind             = "s3"
bucket           = "tierstore"
endpoints        = ["http://storage-1.example:9000", "http://storage-2.example:9000"]
credentials_file = "/etc/tierstore-cache/upstream-read.key"

[[policy]]
prefix = "cas/b/"
cache  = true
```

- `cold_store.bucket` is the `server.bucket` of the storage nodes, and `cold_store.credentials_file` holds a key of theirs: a read-only key (`server.read_only_key_files` there) is enough, since the node refuses every call but GET and HEAD with 403 unless `local_cache.write_through` is `true`. With `write_through = true` the node sends writes to the upstream, which must then be read with a key that may write.
- The two addresses must be storage nodes over the same tiers, so that either serves any object. The cache fills from whichever answers.
- The upstream is not asked anything at start. A wrong address or key is found by the first miss, not by the start.
- Give the jobs scoped keys (`server.scoped_key_files`, see [Keys and roles](keys.md)) rather than the full key: anyone who may read through the front may read what the upstream's key can read.

## Sizing budgets and water marks

Every tier with a size has a budget and three water marks, percents of it. The budget is a number tierstore keeps to, not a capacity it discovers.

**The hot tier.** Set `hot_store.budget` to what the SSD or the bucket can hold with room to spare, since the store needs space beyond the objects (multipart parts in flight, the marks and entries, filesystem overhead). Above `hot_store.high_water` the reconciler evicts, oldest first, objects that the cold tier already holds, down to `hot_store.low_water`. Above `hot_store.critical` a read of a cold object is not copied back. At the budget a new object is written straight to the cold tier. For a budget of 800 GiB with the defaults:

| Mark | Share | Bytes | What happens |
|---|---|---|---|
| `hot_store.low_water` | 70% | 560 GiB | eviction stops here |
| `hot_store.high_water` | 80% | 640 GiB | eviction begins |
| `hot_store.critical` | 95% | 760 GiB | no copy-back of cold reads |
| `hot_store.budget` | 100% | 800 GiB | new objects go to the cold tier |

The gap between `high_water` and `low_water` (10% here, 80 GiB) is what one eviction frees: make it more than a burst of writes between two passes, or the tier reaches the budget before the eviction has finished. The start refuses `low_water` at or above `high_water`, and `high_water` above `critical`. Eviction only frees what the cold tier has already taken, which `hot_store.copy_parallel` bounds, so size the cold tier for the whole data set and watch it: if copies stall, nothing is evictable and the hot tier climbs to the budget.

**The cache.** Set `local_cache.budget` to the working set of the builds on the node (the content-addressed objects that consecutive builds read again), not to the whole disk. The budget may not be above the size of the file system less `local_cache.free_floor` (10% by default), and below that floor a pass evicts even under the budget. Above `local_cache.high_water` (90%) a pass evicts the least recently used objects down to `local_cache.low_water` (75%), and above `local_cache.critical` (95%) a fill is skipped. An object larger than the budget is never kept. The fills hold memory: at most `local_cache.max_concurrent_fills` chunks of `local_cache.fill_chunk` (8 MiB) at a time, 32 MiB with the defaults. If the count of misses served from the upstream and kept nowhere climbs, raise `local_cache.max_concurrent_fills`.

## The marks bucket and the activation

An `s3` hot tier has a second bucket, the marks bucket, named `<hot bucket>-marks` unless `hot_store.marks_bucket` says another name. Create it on the hot store before the first start, and give the hot credentials list, read, write and delete on it as on the hot bucket: a read that misses the hot tier reads the marker, and a bucket that does not exist makes a read of a key only the cold tier has answer 503. Every process over the hot bucket names the same marks bucket. A `dir` hot tier has none.

A new store takes no write until it is activated: reads are served and writes answer 503 with a message that names the verb. Activate it once, from a machine that is not a proxy, with the tier flags the unit has. The first run is a dry run that makes every check and writes nothing:

```sh
tierstore format-epoch --hot s3:tierstore-hot --hot-endpoint http://storage-1.example:9000 --hot-credentials-file /etc/tierstore/hot.credentials --activate
```

Read what it prints, then run it again with `--apply`. Within seconds every process over the tier takes writes. With several endpoints name any one of them; the verb makes a few calls. Moving a store onto the markers later (`--activate-marks`) is a planned pause of writes with its own order: [Rolling out the markers](operations.md#rolling-out-the-markers-and-going-back) has it, and so has the rule that the marks bucket exists before the release that reads it is rolled.

## What to cache

A build node or a node with a cache keeps a key only if the longest `[[policy]]` prefix that holds it says `cache = true`, and at least one policy must say so.

Put `cache = true` only on prefixes of content-addressed data: objects whose key is their digest, which are never rewritten. A kept object is never checked against the upstream again, so a key that is overwritten would be served stale for as long as the cache holds it. (`local_cache.revalidate_after` adds a check of the tag for hits older than a time; it is not in a release yet, see [limits](limits.md).) Leave the mutable prefixes (a ref, an index, a latest pointer) out; a prefix with no `cache = true` is read through to the upstream every time.

```toml fragment
[[policy]]
prefix = "cas/b/"
cache  = true

[[policy]]
prefix = "cas/c/"
cache  = true
```

Two non-empty prefixes may not nest, a prefix may not be listed twice, and none may lie under `_journal/`, `_tiering/` or `_entries/`. The prefixes above are examples: use the ones your clients write their content-addressed objects under.

## The key files

A key file is the access key id and the secret, in JSON or as two lines, owned by the account that runs tierstore with mode 0600 or 0400; a start with a key file that its group or others can access is refused. Which file does what:

| Key of the file | Who uses it |
|---|---|
| `server.access_key_file` | the full key: the clients that write |
| `server.read_only_key_files` | the daemons and jobs that only read |
| `hot_store.credentials_file` and `cold_store.credentials_file` | tierstore's own requests to its stores, not a client's key |

On a build node chained to storage nodes, `cold_store.credentials_file` holds a key that the storage nodes know. [Keys and roles](keys.md) says what each role may do, and covers the presigning, GC and scoped keys, rotation, and the file's forms.
