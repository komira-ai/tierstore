# A cache in front of a remote S3-compatible store

Keep a disk cache on a build machine in front of a MinIO or RustFS bucket that is far away or slow.

## What it is for

Build machines that read the same files again and again from a shared store. The store keeps the data; the cache holds what was used lately under a byte budget and evicts the least recently used objects first. This node takes no writes: it is a read cache.

```diagram
alt: Clients read from tierstore on the build machine. A read the local cache holds is answered from its disk. On a miss tierstore reads the remote S3-compatible store and the cache keeps a copy. When the cache fills it evicts the least recently used objects.
node clients 0 1 "Clients" "build jobs"
node tierstore 1 1 "tierstore" "build node" main
node cache 2 0.5 "local cache" "disk, LRU" hot
node store 2 1.5 "S3 store" "MinIO, RustFS" remote
edge clients tierstore "read"
edge tierstore cache "read"
edge tierstore store "read miss"
edge store cache "keep copy"
```

## The file

```toml
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9400"] }

[server]
proxy_id        = "tierstore-cache-1"
bucket          = "tierstore"
access_key_file = "/etc/tierstore-cache/access.key"

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"

[cold_store]
kind             = "s3"
bucket           = "tierstore"
endpoint         = "http://minio.example:9000"
credentials_file = "/etc/tierstore-cache/upstream-read.key"

[[policy]]
prefix = "cas/b/"
cache  = true
```

`[local_cache]` is the directory and the byte budget of the cache. `[cold_store]` is the remote store, reached over plain `http://`, path-style; give it a key that can only read. The store is a self-hosted S3-compatible store (MinIO, RustFS): Amazon S3 is not supported as a tier yet, and TLS to a tier is not built. `[[policy]]` names which keys are cached: here those under `cas/b/`, and at least one policy must say `cache = true`.

## Commands

There is no hot tier, so there is nothing to activate.

```sh
tierstore --config /etc/tierstore-cache/tierstore.toml --check-config
tierstore --config /etc/tierstore-cache/tierstore.toml
```

Read through the cache on loopback:

```sh
aws --endpoint-url http://127.0.0.1:9400 s3api get-object --bucket tierstore --key cas/b/example out
aws --endpoint-url http://127.0.0.1:9400 s3api head-object --bucket tierstore --key cas/b/example
```

## What to expect

- The first get of a key under `cas/b/` goes to the remote store and fills the cache; the next one is answered from the disk.
- When the cache passes `local_cache.high_water` (90 percent of the budget by default) a pass evicts the least recently used objects down to `local_cache.low_water` (75 percent). Reading an object keeps it.
- A key outside every policy that says `cache = true` is not kept in the cache.
- A write is refused with 403: the node is a read cache.

## Limits

- The remote store must be an S3-compatible store reached over `http://`: a MinIO or RustFS bucket, say. TLS to a store is not built, and Amazon S3 itself is not supported as a store yet.
- The store must give the strong read-after-write that [operations](operations.md#what-tierstore-requires-of-a-store) lists; tierstore does not test it.
