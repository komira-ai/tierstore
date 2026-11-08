# A directory as an S3 bucket

Serve a hot directory on a fast disk and a cold directory on a big one as a single S3 bucket. No other service is needed.

## What it is for

A machine with a fast disk and a big slow one, and programs that speak S3. No other service is needed: both tiers are directories on this machine.

```diagram
alt: Clients send S3 requests to tierstore. A write goes to the hot directory. tierstore copies it to the cold directory in the background and removes old copies from the hot directory when it fills. A read the hot directory lacks is answered from the cold directory.
node clients 0 1.5 "Clients" "any S3 client"
node tierstore 1 1.5 "tierstore" "one S3 endpoint" main
node hot 2 1 "hot dir" "fast disk" hot
node cold 2 2 "cold dir" "big disk"
node gone 2 0 "freed" "space" ghost
edge clients tierstore "read/write" both
edge tierstore hot "write"
edge tierstore cold "read" dashed
edge hot cold "copy" dashed
edge hot gone "evict" dashed
```

## The file

```toml
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9000"] }

[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "/srv/tierstore/access.key"

[hot_store]
kind   = "dir"
path   = "/srv/fast/tierstore"
budget = "200GiB"

[cold_store]
kind = "dir"
path = "/srv/big/tierstore"
```

The two paths are two different directories, one on each disk. `hot_store.budget` is the bytes the hot directory may hold; past `hot_store.high_water` (80 percent by default) tierstore removes objects until usage is down to `hot_store.low_water` (70 percent).

## Commands

```sh
sudo install -d -o "$USER" /srv/fast/tierstore /srv/big/tierstore
printf 'AKIDEXAMPLE\nexample-secret-change-me\n' > /srv/tierstore/access.key
chmod 0600 /srv/tierstore/access.key
tierstore --config /srv/tierstore/tierstore.toml --check-config
tierstore format-epoch --hot dir:/srv/fast/tierstore --activate --apply
tierstore --config /srv/tierstore/tierstore.toml
```

The `format-epoch` step is needed once for a new hot tier: until it has run, every write is answered 503. Then use any S3 client, path-style, region `us-east-1`, with the key from `access.key`:

```sh
aws --endpoint-url http://127.0.0.1:9000 s3api put-object --bucket tierstore --key hello.txt --body hello.txt
aws --endpoint-url http://127.0.0.1:9000 s3api get-object --bucket tierstore --key hello.txt out.txt
```

## What to expect

- A put is acknowledged when the hot directory has the object. The copy to the cold directory follows in the background.
- A get is served from the hot directory; if only the cold directory has the object, it is read from there and a copy is put back in the hot directory.
- The hot tier evicts the objects that were written (or copied back) longest ago, and only those that the cold tier already holds. It is not an LRU: reading an object does not keep it.
- `/healthz`, `/readyz`, `/usagez` and `/metricz` answer on the same address.

## Limits

- A `dir` tier uses tierstore's own flat layout: names are percent-encoded and a 32-byte tag comes before each body. It cannot serve a tree of files that already exists, and the files in it are not meant to be read or edited by other programs. To serve existing files as a bucket, put them in through S3.
- The directories must be absolute paths, and two different ones.
- Both tiers are on this machine: losing the machine loses both. For a cold tier elsewhere, see [a cache in front of a hot and a cold store](examples/cache-hot-cold.md).
- The hot tier is a bounded number of bytes; the cold tier is not bounded by tierstore.
