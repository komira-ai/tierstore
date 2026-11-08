# Configure tierstore

tierstore reads one TOML file, given with `--config`. This page shows the smallest file that works, says what each key in it does, and lists every key of every table. Three complete examples follow the list.

## The smallest working file

The smallest working file has two directories as tiers: a hot one for new objects and a cold one where every object ends up. Save it as `/srv/tierstore/tierstore.toml`:

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
path   = "/srv/tierstore/hot"
budget = "1GiB"

[cold_store]
kind = "dir"
path = "/srv/tierstore/cold"
```

Each key, in one line:

| Key | What it says |
|---|---|
| `interface.type` | what this entry serves; `"s3"` is the S3 API |
| `interface.params` | where it listens: `addresses` is a list of `host:port` |
| `server.proxy_id` | the name of this process; keep it across restarts and give each process over the same tiers its own |
| `server.bucket` | the one bucket clients see |
| `server.access_key_file` | the key file: an access key id on the first line, the secret on the second; mode 0600 |
| `hot_store.kind` | the kind of the hot tier: `dir` here, a directory |
| `hot_store.path` | the directory of the hot tier, an absolute path; put it on the fast disk |
| `hot_store.budget` | how many bytes the hot tier may hold before the oldest objects that the cold tier has are removed |
| `cold_store.kind` | the kind of the cold tier |
| `cold_store.path` | the directory of the cold tier, an absolute path; put it on the big disk, and not at the hot tier's path |

Every other key keeps its default. Make the directories and the key file, check the file, activate the new store once, and start tierstore:

```sh
sudo install -d -o "$USER" /srv/tierstore/hot /srv/tierstore/cold
printf 'AKIDEXAMPLE\nexample-secret-change-me\n' > /srv/tierstore/access.key
chmod 0600 /srv/tierstore/access.key
tierstore --config /srv/tierstore/tierstore.toml --check-config
tierstore format-epoch --hot dir:/srv/tierstore/hot --activate --apply
tierstore --config /srv/tierstore/tierstore.toml
```

A new hot tier answers every write with 503 until `format-epoch --activate --apply` has run once; reads work. Run it once per store, not once per start. Then, from another terminal, put an object and get it back with the `aws` CLI:

```sh
export AWS_ACCESS_KEY_ID=AKIDEXAMPLE AWS_SECRET_ACCESS_KEY=example-secret-change-me AWS_DEFAULT_REGION=us-east-1
aws configure set default.s3.addressing_style path
echo hello > hello.txt
aws --endpoint-url http://127.0.0.1:9000 s3api put-object --bucket tierstore --key hello.txt --body hello.txt
aws --endpoint-url http://127.0.0.1:9000 s3api get-object --bucket tierstore --key hello.txt out.txt
cat out.txt
```

## All configuration options

Every table of the file, with each key's type, its default and what it does. The full reference, with the rules a start checks, is [Configuration](configuration.md). A shape of file has one of three forms: a hot and a cold tier, a local cache and a cold tier, or all three in one process; a file with one tier alone is refused.

## Examples

Three complete files, each with a diagram, the commands to run it, what to expect and where it stops:

- [A directory as an S3 bucket](examples/directory-as-bucket.md): a hot directory on a fast disk, a cold directory on a big one.
- [A cache in front of a remote S3-compatible store](examples/cache-in-front-of-s3.md): a disk cache that reads from a MinIO or RustFS bucket.
- [A cache in front of a hot and a cold store](examples/cache-hot-cold.md): all three tables in one process.
