# A cache in front of a hot and a cold store

Run a disk cache, a hot directory and a cold S3-compatible store in one process.

## What it is for

A storage node that is also read heavily by the machine it runs on. Writes pass through the cache to the hot tier and are copied to the cold store in the background; reads are answered from the cache, then the hot tier, then the cold store.

```diagram
alt: Clients read and write through tierstore, which has three tiers in one process. The local cache answers reads. A write passes through the cache to the hot directory, and the hot directory is copied to the cold remote store in the background (dashed). A read that misses the cache and the hot directory is read from the cold store.
node clients 0 1 "Clients" "any S3 client"
node tierstore 1 1 "tierstore" "one process" main
node cache 2 0 "local cache" "disk, LRU" hot
node hot 2 1 "hot dir" "fast disk" hot
node cold 2 2 "cold store" "MinIO, RustFS" remote
edge clients tierstore "read/write" both
edge tierstore cache "read/write" both
edge cache hot "miss/write" both
edge hot cold "copy" dashed
```

## The file

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
endpoint         = "http://minio.example:9000"
credentials_file = "/etc/tierstore/cold.key"

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"

[[policy]]
prefix = "cas/b/"
cache  = true
```

The cold store is a self-hosted S3-compatible store (MinIO, RustFS) reached over `http://`: Amazon S3 is not supported as a tier yet, and TLS to a tier is not built.

With `[hot_store]`, `[cold_store]` and `[local_cache]` in one file the node owns the data and takes writes. `local_cache.write_through` is true whether it is left out or given, and `false` is refused: a write passes through the cache so that its entries stay right.

## Commands

```sh
tierstore --config /etc/tierstore/tierstore.toml
```

The first start of a new hot tier writes its format object itself, so writes work at once.

```sh
aws --endpoint-url http://127.0.0.1:9300 s3api put-object --bucket tierstore --key cas/b/example --body example
aws --endpoint-url http://127.0.0.1:9300 s3api get-object --bucket tierstore --key cas/b/example out
```

## What to expect

- A put is acknowledged when the hot tier has the object; the copy to the cold store follows in the background.
- A get under `cas/b/` is answered from the cache when it holds the object, and fills the cache from the proxy on a miss. The cache is a true LRU under its byte budget.
- The hot tier evicts the objects written (or copied back) longest ago, and only those that the cold store already holds. It is not an LRU.
- The node also listens on `127.0.0.1` at the port of each `s3` address, so processes on it reach the cache at `http://127.0.0.1:9300`.

## Limits

- The cold store must be an S3-compatible store reached over `http://` (a MinIO or RustFS bucket, say) or a directory. TLS to a tier is not built, and Amazon S3 itself is not supported as a tier yet.
- The cache path may not be the directory of a `dir` tier, nor inside one.
- A hot tier that is a directory uses tierstore's own flat layout and cannot serve an existing tree of files.
- The tiers are not tested at the start; the requirements of [operations](operations.md#what-tierstore-requires-of-a-store) are documented, not checked.
