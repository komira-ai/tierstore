# tierstore

tierstore is an S3-compatible object store. Clients speak S3 to one bucket; behind it are two tiers, a small fast hot tier
(an SSD directory or an S3 bucket) and a large cold tier (spinning disks or an S3 bucket). New objects are written to the
hot tier, copied to the cold tier in the background and evicted from the hot tier when it fills. A read-only OCI registry
front serves container blobs from the same objects, and a build-node mode puts a disk cache in front of one upstream store.
It is one binary, `tierstore`, for Linux (x86_64, arm64) and macOS (Apple silicon), licensed Apache 2.0.

## Quick start

This runs one process on one machine with two directories as the tiers. It takes about five minutes.

### 1. Download and verify

Take the tarball of your platform from the [releases page](https://github.com/komira-ai/tierstore/releases), with
`SHA256SUMS` from the same release. The platforms are `linux-x86_64`, `linux-arm64` and `macos-arm64`.

```sh
mkdir tierstore-try && cd tierstore-try
gh release download --repo komira-ai/tierstore \
  --pattern 'tierstore-*-linux-x86_64.tar.gz' --pattern SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
tar -xzf tierstore-*-linux-x86_64.tar.gz tierstore
./tierstore --version
```

On macOS take the `macos-arm64` file and check it with `grep -- '-macos-arm64\.tar\.gz$' SHA256SUMS | shasum -a 256 -c`.
The checksum proves the download is intact. To prove that this repository built and signed it, run the `gh attestation verify`
or `cosign` check in [the install guide](crates/tierstore/docs/install.md#verify).

### 2. Write a key file and a config file

A key file holds an access key id and a secret, one on each line, and must not be readable by group or others.

```sh
printf 'AKIDEXAMPLE\nexample-secret-change-me\n' > access.key
chmod 0600 access.key
mkdir hot cold
```

Write `tierstore.toml` in the same directory (`[[interface]]` says what the process serves: here the S3 API on loopback port 9000):

```toml
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9000"] }

[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "access.key"

[hot_store]
kind   = "dir"
path   = "hot"
budget = "1GiB"

[cold_store]
kind = "dir"
path = "cold"
```

### 3. Start it

```sh
./tierstore --config tierstore.toml
```

The process asks each tier the questions its guarantees rest on, then prints `tierstore <version> listen=127.0.0.1:9000`.
`curl http://127.0.0.1:9000/readyz` answers `ready`. Stop it with Ctrl-C; it drains and exits.

### 4. Put and get an object

The front serves one bucket, path-style, over plain HTTP, and refuses the checksum trailer that recent AWS clients send by
default, so tell the `aws` CLI to send checksums only when required:

```sh
export AWS_ACCESS_KEY_ID=AKIDEXAMPLE
export AWS_SECRET_ACCESS_KEY=example-secret-change-me
export AWS_DEFAULT_REGION=us-east-1
aws configure set default.s3.addressing_style path
aws configure set default.request_checksum_calculation when_required
aws configure set default.response_checksum_validation when_required

echo hello > hello.txt
aws --endpoint-url http://127.0.0.1:9000 s3api put-object --bucket tierstore --key hello.txt --body hello.txt
aws --endpoint-url http://127.0.0.1:9000 s3api get-object --bucket tierstore --key hello.txt out.txt
cat out.txt
```

A put is create-only: putting `hello.txt` again is refused with 412. The aws CLI is not part of tierstore's own tests, which
drive the front with an SDK; the settings above are those the SDK needs.

## The configurations

Every configuration is one process started with `--config <file>`. The file has the tables of the role it plays; every key
and its default is in the [configuration reference](crates/tierstore/docs/configuration.md).

### Storage node

A hot tier on SSD in front of a cold tier on spinning disks, with a read-only key for daemons that only read.

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
kind   = "dir"
path   = "/srv/tierstore/hot"
budget = "800GiB"

[cold_store]
kind = "dir"
path = "/srv/tierstore/cold"
```

Either tier may instead be `kind = "s3"` with a `bucket`, an `endpoint` and a `credentials_file`. An `[[interface]]` of type
`oci` adds the read-only OCI registry front.

### Build node

A read cache on local disk in front of one upstream, with no hot tier. The upstream is `[cold_store]`, read with a
read-only key, and `[[policy]]` says which key prefixes are kept.

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
endpoint         = "http://s3-cold.example:9000"
credentials_file = "/etc/tierstore-cache/upstream-read.key"

[[policy]]
prefix = "cas/b/"
cache  = true
```

### All-in-one (planned)

One process that is both: the storage node's two tiers and the build node's disk cache in front of them, for a single
machine. It is not in a release and has no configuration keys yet.

## Documentation

- [What tierstore is](crates/tierstore/docs/overview.md), [install](crates/tierstore/docs/install.md) (the account, the
  directories, the systemd unit, updates), [keys and roles](crates/tierstore/docs/keys.md).
- [Configuration reference](crates/tierstore/docs/configuration.md): every flag and every key of the file.
- [Operations](crates/tierstore/docs/operations.md), [troubleshooting](crates/tierstore/docs/troubleshooting.md),
  [limits](crates/tierstore/docs/limits.md), [metrics](crates/tierstore/docs/metrics.md).
- [CI and releases](docs/ci-and-release.md).
- The same guide is on the website under `crates/tierstore/site`.

## Contributing

The rules every change is written and reviewed against are in [AGENTS.md](AGENTS.md).

## Security

Report a vulnerability privately, as [SECURITY.md](SECURITY.md) describes. Do not open a public issue for it.
