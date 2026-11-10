# Configuration

tierstore is configured by one TOML file, given with `--config <path>`. A key that is left out has the default this page gives, and a file holds no secret, only the paths of key files. A file has one of three shapes, told apart by its tables; every shape has at least one `[[interface]]` of type `s3`, and a file with one tier alone is refused:

- **A storage node**: `[hot_store]` and `[cold_store]`. New objects go to the hot tier and are copied to the cold tier. See [Two directories as one S3 bucket](examples/directory-as-bucket.md).
- **A build node**: `[local_cache]` and `[cold_store]`, a read cache on local disk in front of one upstream. See [A cache in front of a remote S3-compatible store](examples/cache-in-front-of-s3.md).
- **A storage node with a cache**: all three tables in one process. See [A cache in front of a hot and a cold store](examples/cache-hot-cold.md).

Tiers today are directories or self-hosted S3-compatible stores (MinIO, RustFS) reached over `http://`. AWS S3 is not supported as a tier yet, and TLS to tiers is not built. Flags of earlier builds no longer set any of this; see [Moving from flags](configuration.md#moving-from-flags).

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
presign_key_files   = ["/etc/tierstore/presign.key"]
gc_key_files        = ["/etc/tierstore/gc.key"]

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
held_ttl           = "1h"
usage_stale_after  = "2m"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoint         = "http://s3-cold.example:9000"
credentials_file = "/etc/tierstore/cold.credentials"
```

`presign_key_files` and `gc_key_files` hold scoped keys: one signs presigned PUT URLs, the other lists and deletes under one prefix for garbage collection (see [keys](keys.md)). Leave both out if you do not use them. Start it with the file and nothing else:

```sh
/usr/libexec/tierstore/tierstore --config /etc/tierstore/tierstore.toml
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

One process that owns the data and keeps a disk cache in front of its own proxy: the file has `[hot_store]`, `[cold_store]` and `[local_cache]`. The S3 front and the OCI front read from the cache; the cache reads from, and writes to, the proxy, which is a storage node's (both tiers, the entries, the reconciler). The tiers are not tested at the start, as a storage node's are not either:

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

The keys of each table are those of the storage node and of the build node. What differs: `local_cache.write_through` is true whether it is left out or given (the node owns the data and takes writes, which pass through the cache so that its entries stay right), and `false` is refused; the cache path (absolute, as the directory of a `dir` tier must be) may not be the directory of a `dir` tier, inside it or around it, nor around the lock file, the key files of an `oci` interface or `server.handoff_socket`; `server.backend_check_wait`, `cold_store.skip_checks` and the reconciler's keys of `[hot_store]` keep their meaning (a build node refuses them). What the cache keeps, how a hand-over behaves and what to expect are in [operations](operations.md#a-storage-node-with-a-local-cache) and [limits](limits.md#a-storage-node-with-a-local-cache). The start line says `mode=storage-cache`.

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
| `interface.type` | `"s3"` or `"oci"`, required | what the entry serves. `"s3"` is the S3 API, `/healthz`, `/readyz`, `/usagez` and `/metricz`. `"oci"` is the OCI registry front (read-only unless `params.require_auth` is true). `"fs"` is refused with a message that says it is not an interface of this process: the virtual build file system is `tierstore fs`, a process of its own whose file has an `[fs]` table (see below); any other value is refused |
| `interface.params` | table, required in effect | the params of the entry, below. An entry without `params.port` or `params.addresses` is refused |
| `params.port` | port, 1 to 65535 | listen on this port on the IPv4 wildcard, `0.0.0.0:<port>`. For loopback, another address or IPv6, give `params.addresses` |
| `params.addresses` | address or list of addresses | listen on exactly these. To listen on the wildcard of both families give `["0.0.0.0:<port>", "[::]:<port>"]`: an IPv6 address is bound for IPv6 only, so the two do not clash. A start that cannot bind every address is refused |
| `params.tls_cert_file` | path, `oci` only | the certificate chain (PEM), leaf first. It must carry an IP subjectAltName for each listen address. Both this and the key, or neither. An `s3` entry refuses it: the certificate of the S3 front is `server.tls_cert_file` |
| `params.tls_key_file` | path, `oci` only | the private key (PEM); a file any user can read is refused |
| `params.prefix` | string, `oci/`, `oci` only | the key prefix the registry reads, ending in `/`; not inside `_journal/`, `_tiering/` or `_entries/` |
| `params.max_connections` | count, 64 (at least 1), `oci` only | connections open at once |
| `params.connection_lifetime` | duration, `1h`, `oci` only | the longest a connection lives |
| `params.require_auth` | boolean, `false`, `oci` only | `true`: clients must log in with an access key of the server (HTTP Basic, the id as user name and the secret as password); `false`: the front is anonymous. Refused with plain HTTP off loopback, since the secret would travel in the clear |
| `params.stage_read` | bytes, 1048576 (1 MiB; at least 1), `oci` only | the most bytes of a stored chunk that the end of a push reads in one call, so that a chunk of any size is held a piece at a time. On main, not in v0.0.41: v0.0.41 and before refuse a file with this param (and a file with `[[interface]]` entries at all) |
| `params.max_manifest_bytes` | bytes, 4194304 (4 MiB; at least 1), `oci` only | the largest manifest or index the front stores, serves or pulls from the upstream: a pushed body over it is answered 413, a stored manifest over it is answered 404 `MANIFEST_UNKNOWN`, and an upstream manifest (or token) over it is refused as the upstream's failure. On main, not in v0.0.41: v0.0.41 and before refuse a file with this param |
| `params.read_chunk_bytes` | bytes, 4194304 (4 MiB; at least 1), `oci` only | the most bytes asked of the store in one call when a blob is served, so a blob of any size is held a chunk at a time; a smaller value makes more calls and holds less. On main, not in v0.0.41: v0.0.41 and before refuse a file with this param |
| `params.echo_bytes` | bytes, 256 (at least 1), `oci` only | the longest piece of a request (a name, digest, tag or `n`) that an error answer repeats in its detail, cut at a character. On main, not in v0.0.41: v0.0.41 and before refuse a file with this param |
| `params.upstream` | table, `oci` only | the registry a miss is pulled from, described under `params.upstream` below. Not a key of the `[oci]` table of the release before |

An `s3` entry takes no param but `params.port` and `params.addresses`. The `oci` entries make one registry front, which has one certificate, one prefix and one set of limits, so they must agree on `params.tls_cert_file`, `params.tls_key_file`, `params.prefix`, `params.max_connections`, `params.connection_lifetime`, `params.require_auth`, `params.stage_read`, `params.max_manifest_bytes`, `params.read_chunk_bytes`, `params.echo_bytes` and `params.upstream`: an entry that differs from the first `oci` entry is refused, naming the param. With a certificate and key the registry is HTTPS; with neither it is plain HTTP, and off loopback the start prints one warning line for each such address. The front reads objects at `<prefix>blobs/sha256/<hex>` and serves GET and HEAD of `/v2/`, of manifests by digest and of blobs by digest. It is anonymous unless `params.require_auth` is true: anyone who can connect can read any digest under the prefix, so the network decides who may. With `params.require_auth` a request without a valid login is answered 401 with `WWW-Authenticate: Basic realm="tierstore"`; the full key, a read-only key and a scoped key with a read prefix over `<prefix>blobs/sha256/` may pull, and the full key and a scoped key with a write prefix over it may push (a key that may not push is answered 403). Without `params.require_auth` the front is read-only and every push is answered 405. A push is a blob upload, monolithic (`POST /v2/<name>/blobs/uploads/?digest=<digest>`) or chunked (`POST` to begin, `PATCH` for each chunk, `PUT ...?digest=<digest>` to end, `DELETE` to cancel), or `PUT /v2/<name>/manifests/<reference>`. A blob is checked against its digest in one pass as it is stored, with no limit on its size and no copy of it in memory, and nothing is stored if it does not match; a chunk that does not start where the upload is (its `Content-Range`, if it has one) is answered 416. The chunks of an upload are stored as objects under `<prefix>uploads/<id>/` until the upload ends or is cancelled, and are read back once when it ends, so a blob pushed in chunks is written twice; an upload that is neither ended nor cancelled keeps them, and nothing collects them. A manifest must name only blobs that are stored (400 `MANIFEST_BLOB_UNKNOWN` otherwise) and is stored by its digest; a tag in the place of the digest is then pointed at that manifest by writing the next version of the tag: a small object `<prefix>tags/<name>/<tag>/<inv>` (the `/` of a name written `~`) holding the digest, where `<inv>` is the 20 digits of `18446744073709551615` less the number of earlier versions, so that the newest version sorts first. No stored object is overwritten: the version is made with the store's create-only write, so of two pushes of one tag that overlap with different digests exactly one is taken and the other is answered 409 `TAG_CONFLICT` (its manifest is stored by digest all the same; the client pushes the tag again), and two pushes of one digest are both 201. A push of the digest the newest version already holds writes a version of its own all the same, since the newest may be another push's that is not yet acknowledged. Old versions are kept: nothing deletes them yet, so a tag that moves often has one 71-byte object per move, and the stores must list consistently with the writes before the listing (S3 and MinIO do). On a `dir:` tier a key whose file name would be over 255 bytes has a hashed file name (see `limits.md`), so a long name and tag work there too. A tag is pulled as `GET` or `HEAD` of `/v2/<name>/manifests/<tag>` (a listing of the tag's versions, a read of the newest, and then the manifest), and `GET /v2/<name>/tags/list` lists the tags of a name, paged with `n` and `last`; a name with no tag lists empty. Tags are not deleted. Basic was chosen over the token flow because the front then needs no token service, signing key or clock: `podman login` and `docker login` send the id and secret with each request, which is why TLS is needed off loopback. The certificate and key are read once; a new certificate takes a restart.

A file written for the release before still loads: its `listen` key of `[server]` (an address or a list) is read as an `s3` entry at those addresses, and its `[oci]` table (`listen`, which the table needs, and the same five params, with `listen` as the key of the addresses) as an `oci` entry. They are added after the entries the file lists, and every rule above holds for them, so an address that a legacy key and an entry both listen on is refused. A file that sets only these keys starts on the release before and on this one; a file with `[[interface]]` entries is refused by the release before, which does not know the table.

When the file has a `[local_cache]` table (a build node, or a storage node with a cache), the server also binds 127.0.0.1 on the port of each `s3` address, once for each distinct port, so that processes on the node reach it at `http://127.0.0.1:<port>`; the start is refused, naming loopback and the port, if that cannot be bound. Nothing is added for a port that already has `127.0.0.1` or the IPv4 wildcard `0.0.0.0` on it (the wildcard covers loopback, and a second bind would be refused), and an IPv6 address adds IPv4 loopback only, never `::1`. Without `[local_cache]` exactly the listed addresses are bound.

## `[server]`

| Key | Type, default | What |
|---|---|---|
| `server.proxy_id` | string, required | names this process's journal and lease in the hot tier; keep it across restarts and give each process over the same tiers its own |
| `server.slot` | port, 1 to 65535, none | The private port this instance runs as: the port of its `s3` interface. It is added as the `slot` label of `tierstore_build_info` on `/metricz`; the update hook does not read it. It binds nothing, and it is the same on a storage node and a build node. Without it the gauge has no `slot` label. `slot = 0` or a value that is not a port is refused (exit 2) |
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
| `server.domains` | list of domain names, none | the domains of virtual-hosted-style addressing: a request to `<bucket>.<domain>` takes the bucket from the host and the whole path as the key; every other host is path-style. Names, not URLs (no scheme, port or path); kept in lower case. The DNS names and the certificate are yours |
| `server.delete_concurrency` | count, 16 | the most keys of one DeleteObjects call (`POST /<bucket>?delete`) deleted at once; at least 1. The call is not limited by it, only how many of its deletes run together |
| `server.max_concurrent_puts` | count, 32 | the most put bodies and parts taken at once; one more waits for one to end, up to `server.put_permit_wait` |
| `server.put_permit_wait` | duration, `15s` | How long a put that finds every put slot taken waits for one to end before it is answered 503 `SlowDown` with `Retry-After: 1`; `0s` answers at once. No byte of its body is read while it waits, and a waiter counts as in flight in a drain (never past `--drain-deadline`) |
| `server.put_min_bytes_per_sec` | bytes a second, 1024 | the least average rate of a body, counted over the time spent waiting for the client; 0 turns it off |
| `server.put_rate_grace` | duration, `30s` | the time spent waiting for the client before that rate is held |
| `server.body_idle_timeout` | duration, `30s` | how long a body or an answer may go without a byte; more than zero |
| `server.aws_chunked_max_line_bytes` | bytes, 1024 | the longest line of an `aws-chunked` body that the server holds to read it: the header of a chunk, with its signature, and a line of the trailer. The data of a chunk is passed on as it arrives and is not held, so a chunk may be of any length. A body with a longer line is refused with 400 `InvalidRequest` |
| `server.min_secret_bytes` | bytes, 16 | the fewest bytes of the secret of any access key (full, read-only, presigning, GC, scoped); a key file with a shorter secret refuses the start |
| `server.s3_request_timeout` | duration, `60s` | the limit on a request to an `s3` tier (both tiers); more than zero. Set it longer than the longest multipart completion your store takes |
| `server.s3_connect_timeout` | duration, `5s` (1ms to 10m) | how long a connection to an `s3` tier (both tiers, or the upstream of a cache) may take to be opened |
| `server.s3_min_bytes_per_sec` | bytes a second, 1048576 | the slowest rate at which the body of a write to an `s3` tier is taken to go: a write may take `s3_request_timeout` plus its length at this rate; 0 gives every request `s3_request_timeout` however large its body |
| `server.s3_pool_idle_timeout` | duration, `15s` (1ms to 1h) | how long an unused connection to an `s3` tier is kept before it is dropped. Keep it clearly shorter than the idle close of the store and of any proxy before it; MinIO closes idle connections after 30s by default. A proxy that closes sooner, or a restart of the store, can still fail a request on a reused connection |
| `server.s3_read_retries` | count, 2 (0 to 8) | how many times a read of an `s3` tier that failed in transport (a connection closed after the request went out) is sent again; 0 sends it once. A read that timed out is never sent again |
| `server.s3_read_retry_wait` | duration, `50ms` (1ms to 10s) | the wait before the first resend of a read; each resend waits twice the one before |
| `server.s3_endpoint_attempts` | count, 3 (1 to 8) | how many times at most a request to an `s3` tier is sent when each send before it failed to connect to its address (refused, no route, name not resolved, connect timeout): the request never reached a store, so it is sent again to the next address of the tier's `endpoints` (to the same address on a new connection if the tier has one); 1 sends it once. A request that may have reached a store is not sent again by this key: a timeout after the connection was made, a connection reset or closed before the answer was complete, and any answer with a status. A body streamed from the client is not copied: it is sent again only if the connection was never made, so none of it was read |
| `server.header_timeout` | duration, `60s` (1ms to 1h) | how long a client has to send the head of a request, and so how long a connection may wait, idle, for its next request (S3 and OCI fronts); also how long the drain waits for a client to close a connection the server shut. Keep it above the time a client keeps an idle connection in its pool: a client that reuses a connection idle for longer finds it closed (see "A reused connection is closed" in the troubleshooting page) |
| `server.listen_backlog` | count, 1024 (1 to 65535) | the length of the queue of connections the system keeps for a listening socket that is not yet accepted from; the system may cut it to its own limit (`net.core.somaxconn` on Linux) |
| `server.accept_pause` | duration, `100ms` (1ms to 1m) | how long an accept loop (both fronts and the hand-off socket) waits after an accept that failed, so that a listener that keeps failing is not asked again at once |
| `server.door_grace` | duration, `3s` (1ms to 1m) | the longest the closing door of a draining front takes connections whose handshake is still in flight, and how long a new connection has to send its first request after the settle |
| `server.door_look` | duration, `10ms`, or `door_grace` if that is shorter (1ms to 10s, not over `door_grace`) | how far apart the two looks are that must both find no handshake in flight before the door closes |
| `server.put_pieces_queued` | count, 4 (1 to 4096) | how many pieces of a streamed put's body wait for the backend to take them; with the piece in hand and the one the backend has, all a put holds between the client and the backend |
| `server.oci_handshake_timeout` | duration, `10s` (1ms to 1h) | how long a client has to finish the TLS handshake with the OCI front before the connection is closed to free its slot |
| `server.s3_chunks_waiting` | count, 1 (1 to 1024) | how many chunks of a part wait for the request to an `s3` tier that sends them |
| `server.dir_copy_chunk` | bytes, 1 MiB (1 to 1 GiB) | the size of the pieces in which a `dir` tier joins the parts of a multipart upload |
| `server.dir_read_chunk` | bytes, 64 KiB (1 to 1 GiB) | the size of the chunks in which a `dir` tier gives a streamed read |
| `server.s3_single_put_max` | size, `5GiB` (1MiB to 5GiB) | the longest object of known length that an `s3` tier (both tiers, or the upstream of a cache) is written with one PUT, whose body is streamed and never held whole; a longer one, and a body of unknown length, is a multipart upload. 5GiB is the most S3 takes in one PUT. Lower it for a store or a proxy that takes a long PUT worse than several short ones |
| `server.s3_part_bytes` | size, `16MiB` (5MiB to 1GiB) | the size of a part of a multipart upload to an `s3` tier. One part is held in memory while it is sent, so a write holds this much; an object too large for 10,000 parts of this size gets larger parts |
| `server.drain_lb_wait` | duration, `10s` (`1s` on macOS) | on SIGTERM, the settle: how long `/readyz` says 503 while the server goes on accepting and serving, so that a load balancer sees it. On a loopback service there is none to wait for, which is why the macOS default is shorter. Given too as `--drain-lb-wait` (a flag of the releases before, still taken), a start that gets both is refused with status 2 |
| `server.drain_deadline` | duration, `15m` (`60s` on macOS) | then how long it waits for requests in flight and for the connections it closes (a lingering close, see [operations](operations.md#drain-and-restart)). Given too as `--drain-deadline`; both is refused as for `server.drain_lb_wait`. The unit's stop timeout is above the sum of the two (see [install](install.md)); if you raise either, raise it too |
| `server.handoff_socket` | path, none | The control socket of the hand-off of the listening sockets (see [Live update by hand-off of the sockets](operations.md#live-update-by-hand-off-of-the-sockets)): an absolute path of at most 103 bytes, in a directory only the node's user may write, and not `hot_store.lock_file`. With it a start takes the sockets of the instance that answers there, or binds its own where none answers, and then answers there itself. Left out, it is `handoff.sock` in the service's state directory when the service manager made one (systemd sets `STATE_DIRECTORY` for a unit with `StateDirectory=`, as hostkeeper's package has: `/var/lib/tierstore/handoff.sock`), and a process with no state directory binds its own sockets and makes no control socket. The default is held to the checks below as a named path is. Every shape reads it. On a node with `[local_cache]` it must not be inside `local_cache.path` (refused, exit 2), and for a build node `local_cache.path` must then be an absolute path, so that the two can be compared (refused, exit 2) |
| `server.handoff_wait` | duration, `10s` (1ms to 1h) | how long a start waits for the instance that answers on `server.handoff_socket` to hand its sockets over, from the connection to the end of the hand-off; a start that is not answered in that time fails (exit 1) and binds nothing. Refused without `server.handoff_socket` |

## `[hot_store]` and `[cold_store]`

Both tables have `kind` and the keys it needs. `kind` is `memory` (no other key), `dir` (needs `path`) or `s3` (needs `bucket`, `endpoint` or `endpoints`, and `credentials_file`; `region` is optional). A key the kind does not take is refused.

| Key | Type, default | What |
|---|---|---|
| `hot_store.kind`, `hot_store.path`, `hot_store.bucket` | required | the hot tier |
| `cold_store.kind`, `cold_store.path`, `cold_store.bucket` | required | the cold tier. Multipart uploads go to it when the hot tier is full, and it is where the reconciler copies them, so it must do them (all three kinds do) |
| `hot_store.endpoint` | `http://host[:port]`; for `s3`, this or `endpoints`, not both | the hot tier's store. No path; TLS is not built yet |
| `hot_store.endpoints` | list of `http://host[:port]`, at least one, none twice; for `s3`, this or `endpoint`, not both | the hot tier's store reached through several addresses: each request goes to the next address in turn (one counter for the tier, not one for each connection), signed for the `Host` it goes to, and each address keeps its own connections. The addresses must be one store: any of them has to serve any request, a multipart upload's parts and its completion included. A request whose connection to its address cannot be made goes to the next address (`server.s3_endpoint_attempts`), and any other failure fails as it does with one address; the caller's retry goes to the next. There is no marking of a down address. A list of one is the same as `endpoint`. The marks bucket of the hot tier uses the same addresses |
| `hot_store.region` | string, `us-east-1` | the signing region of the hot tier's store |
| `hot_store.credentials_file` | path, required for `s3` | the key file of the hot tier's store |
| `cold_store.endpoint` | `http://host[:port]`; for `s3`, this or `endpoints`, not both | the cold tier's store |
| `cold_store.endpoints` | list of `http://host[:port]`, as `hot_store.endpoints`; for `s3`, this or `endpoint`, not both | the cold tier's store reached through several addresses, in turn, as `hot_store.endpoints`. On a build node this is the upstream of the cache |
| `cold_store.region` | string, `us-east-1` | the signing region of the cold tier's store |
| `cold_store.credentials_file` | path, required for `s3` | the key file of the cold tier's store |
| `hot_store.conditional_delete` | boolean; `false` for `s3`, `true` for `memory` and `dir` | whether a conditional delete of the hot tier is sent as a DELETE with `If-Match` (`true`) or as a read of the tag before a plain DELETE (`false`). Set it `true` for a store that enforces `If-Match` on a DELETE, such as Amazon S3. `false` is refused for `memory` and `dir`, which delete conditionally in one atomic step. tierstore does not ask the store; see [operations](operations.md#conditional-delete) |
| `cold_store.conditional_delete` | boolean; as for the hot tier | the same, for the cold tier |

Only `[hot_store]` has these, since only a process with a hot tier has them:

| Key | Type, default | What |
|---|---|---|
| `hot_store.marks_bucket` | string, `<hot_store.bucket>-marks`; `s3` only | the bucket of the hot tier's store that holds the markers of the put-without-lock design (one object per key, named by the key). A proxy reads it only when the format object is unreadable so far or says `drain`, `drained`, `active-marks` or a phase this release does not know: on a hot miss, one `GET` of the key's marker beside the cold read, and in a listing, a page of marker names beside the hot page. Under `active-marks` it also writes and deletes the markers of its puts and deletes, so the credentials need write and delete access there before the phase is activated. While the format object says `active` (the entry protocol) or is missing, it makes no request to the bucket, which then need not exist and the hot tier's credentials need no access to it. Before the format leaves `active` the bucket must exist and the credentials must read and list it, or a read of a key only the cold tier has is answered 503. The start opens the second handle on it and refuses a name that is empty or is `hot_store.bucket`. A `dir` hot tier keeps its markers in the directory of its `path` with `-marks` added, and a `memory` one in a map of its own, so the key is refused for them; a `dir` hot tier has no handle yet, so reads no marker. The bucket is made by the operator, and the verbs of [Rolling out the markers](operations.md#rolling-out-the-markers-and-going-back) name it with `--marks-bucket` (the same name, or `<hot bucket>-marks`): `--activate-marks` records it in the format object, and a proxy whose name differs from the recorded one answers writes 503. Not for `[cold_store]` |
| `hot_store.budget` | size, required | the bytes the hot tier is meant to hold. The water marks are shares of it |
| `hot_store.high_water` | percent, 80 | above this share of the budget, eviction begins |
| `hot_store.low_water` | percent, 70 | eviction goes on down to this share |
| `hot_store.critical` | percent, 95 | above this share a read of a cold object is not copied back (a put is not slowed; it goes to the cold tier only when the hot tier is full) |
| `hot_store.copy_parallel` | count, 16 | the most objects the reconciler copies at once |
| `hot_store.lease_ttl` | duration, `30s` | how long the lease object must stand unchanged, on another reconciler's own monotonic clock, before it takes the lease over; the holder renews every `lease_renew` (a third of it unless that is set) |
| `hot_store.lock_file` | absolute path, none | The lock file of the node: the reconciler works only while this process holds an exclusive lock on it. A process that finds the lock held serves reads and writes through its own proxy and tries the lock again every `hot_store.lock_retry` (one second unless that is set); the system frees it when the holder exits or crashes. If the file is replaced, the holder stops its reconciler and waits for the lock on the new file. Give every process of one node the same path, on a local file system, in a directory that nothing removes (`/var/lib/tierstore/`, not a directory under `/run`). The start refuses a path that is not absolute, one that names no file, and one that cannot be opened or is on a network file system. Without it the reconciler works as soon as the process starts |
| `hot_store.lease_renew` | duration, a third of `lease_ttl` (1ms to 1h, below `lease_ttl`) | how often the holder renews the lease, on a timer of its own that runs while a step works. With `lease_margin` it must not be longer than `lease_ttl`; left out, it is a third of `lease_ttl` |
| `hot_store.lease_margin` | duration, a third of `lease_ttl` (0ms to 1h, below `lease_ttl`) | how much of `lease_ttl` the holder keeps in hand: it counts its lease as held until `lease_ttl` less this after its last renewal, and a step that starts with less than this left renews first, and starts no copy to the cold tier or delete with less than this left. Left out, it is a third of `lease_ttl` |
| `hot_store.idle_min` | duration, `1s` (1ms to 1h) | the wait of the reconciler's loop after a round that did nothing, the first time; it is also the wait while a live owner holds an entry. A round that changed something is followed by the next at once |
| `hot_store.idle_max` | duration, `10s` (1ms to 1h, not below `idle_min`) | the longest wait of the loop while nothing is to do; the wait doubles from `idle_min` to it. Keep it below `lease_ttl` less `lease_margin`, or a quiet holder lets its lease run down between rounds and renews it at the next |
| `hot_store.listing_page_size` | count, 100 (1 to 100000) | the objects asked for in one page of a listing of the entries, of the hot tier (the stray sweep and the usage scan) and of the open uploads of the cold tier by the reconciler. It replaces the retired `journal_page_size`, which is still accepted and ignored |
| `hot_store.stop_wait` | duration, `5s` (1ms to 1h) | the longest the reconciler waits, once the process is halted, for its lease to be given up; a hot tier that does not answer does not keep the process from leaving, and the lease then runs out by itself |
| `hot_store.lock_retry` | duration, `1s` (1ms to 1h) | how often a process that finds the `lock_file` held by another process tries the lock again |
| `hot_store.first_beat_poll` | duration, `1ms` (1ms to 1h) | how often the start looks at whether the proxy's first heartbeat was acknowledged, for at most one `heartbeat` period |
| `hot_store.usage_stale_after` | duration, `2m` | how old the published usage may be before `/usagez` says 503; longer than the lease |
| `hot_store.usage_refresh` | duration, `1s` (1ms to 1h) | how long the usage object that was read is used before it is read again, and how long the hot tier is taken to be full after it said so |
| `hot_store.compare_range` | size, `1MiB` (4KiB to 1GiB) | the most bytes of an object that one read of the reconciler's comparison of the two tiers holds |
| `hot_store.listing_heads` | count | no longer read: the key is still accepted, so a file that sets it loads, and it changes nothing (and the start says nothing of it) |
| `hot_store.abandoned_upload_after` | duration, `0s` (0s to 8760h) | how long a multipart upload may stand open in the hot tier before the reconciler's stray sweep (at takeover and daily) aborts it, by the start time the tier gives. The default `0s` aborts none: as in S3, an incomplete upload is kept, and its parts hold hot-tier space, until its client aborts it. Set it, for example `"168h"`, to opt in, as an S3 bucket owner does with an `AbortIncompleteMultipartUpload` lifecycle rule. Uploads are made in the hot tier when it has room (see [Multipart uploads](operations.md#multipart-uploads)) |
| `hot_store.upload_listing_calls` | count, 100 (1 to 100000) | the most calls to one tier that one listing of open uploads makes to fill a page that the filter of reserved keys cut short |
| `hot_store.retry_min` | duration, `1s` (1ms to 1h) | the delay before the reconciler tries again an entry whose item failed once; it doubles for each failure after the first |
| `hot_store.retry_max` | duration, `60s` (1ms to 1h) | the longest delay of a failing item; not below `retry_min` |
| `hot_store.stray_grace` | duration, `10m` (1ms to 8760h) | how old a hot object with no entry and no cold object must be before the stray sweep adopts it |
| `hot_store.stray_interval` | duration, `24h` (1ms to 8760h) | how often the stray sweep runs again after the one at takeover |
| `hot_store.stray_rate` | count, 100 (1 to 1000000) | the most cold lookups a second of the stray sweep |
| `hot_store.copy_back_ttl` | duration, `10m` (1ms to 8760h) | how long a requested copy-back may wait before it is dropped without a copy |
| `hot_store.copy_back_max` | count, 1000 (1 to 1000000) | the most copy-backs copied at a time; the rest are dropped without a copy |
| `hot_store.usage_interval` | duration, `10s` (1ms to 1h) | how often the reconciler publishes the usage figure to `_tiering/usage` |
| `hot_store.usage_scan_interval` | duration, `24h` (1ms to 8760h) | how often the hot tier is listed to measure its usage and pick the objects to evict, after the listing at takeover |
| `hot_store.scan_pages` | count, 100 (1 to 100000) | the most pages of that listing read in one step of the reconciler |
| `hot_store.candidate_limit` | count, 10000 (1 to 10000000) | the most eviction candidates kept from one listing, the oldest |
| `hot_store.tag_record_limit` | count, 100000 (1 to 100000000) | the most keys whose copy tags are remembered, so that the eviction can skip the compare |
| `hot_store.proxy_usage_stale_after` | duration, `30s` (1ms to 1h) | how long a proxy lets the usage object stand unchanged before it ignores it; longer than `usage_interval`. Not `usage_stale_after`, which is the limit of `/usagez` |
| `hot_store.format_refresh` | duration, `5s` (1ms to 1h) | how long a proxy uses the format object it read before it reads it again, so how soon a pause reaches a proxy that writes |
| `hot_store.entry_rounds` | count, 3 (1 to 100) | how many times a put or a delete looks again at an entry that vanished between its create and its read, before it answers 503 |
| `hot_store.delete_wait` | duration, `30s` (1ms to 1h) | the most time a delete waits for the reconciler to release an entry, as the sum of its sleeps; keep it under the S3 request timeout of the clients |
| `hot_store.delete_poll_min` | duration, `1s` (1ms to 1h) | the first sleep of that wait; each sleep after it is twice the one before; not above `delete_poll_max` or `delete_wait` |
| `hot_store.delete_poll_max` | duration, `10s` (1ms to 1h) | the longest sleep of that wait |
| `hot_store.held_ttl` | duration, `1h` (1ms to 8760h) | how long a held put or delete of a proxy that is still beating may stand unchanged before the reconciler reaps it, as it does at once for one whose proxy has departed, and at its next step for one whose proxy has restarted and no longer names the process that made the entry (the entry names that process's boot; the restarted proxy keeps naming the earlier boot in its registration for `hot_store.departure` of its heartbeats, in case that process is still draining beside it). A proxy that could not hand an entry over in any of its `step_attempts` tries (the hot tier failed its write each time) leaves it held, and every put and delete of that key answers 503 until it is reaped. Counted on the reconciler's own clock from the first step that sees the entry, and again from the start if the entry is written again. When set, it must be longer than `lease_ttl` and `departure`. A put that is still running when its entry is reaped is not acknowledged (its landing finds the entry changed and answers 503), so keep it above the longest put: a body at the rate floor of `put_min_bytes_per_sec` can take hours |
| `hot_store.emergency_cold_landing` | boolean, `false` | whether a plain put that the hot tier refuses with `Full` before it took a byte is written to the cold tier under a lock on the key's marker (the hot object, if there is one, is deleted and the put is acknowledged after that), and not answered 503. Only the format `active-marks` of the put-without-lock design has it, and this release does not write that format, so the key changes nothing yet. Off, because a landing is not ordered against a put of the same key that reached the hot tier (the residual R6' of the design: such a put can be acknowledged and then lose its object, or a reader can see the landed version, the put's, and the landed one again). Not for `[cold_store]` |
| `hot_store.step_attempts` | count, 4 (1 to 100) | the most tries of the last step of a put or a delete, the first included: the landing of a put (the replace that acknowledges it), the hand-over of a delete, and the removal of an entry. A try that the hot tier fails without a definite answer (unavailable, or an error that does not say whether it was done) is made again after a sleep, and a step that the hot tier refuses is not. Before each repeat of a replace the entry is read: if it carries the replace's own nonce the replace landed and is not sent again, and a put is acknowledged only for a replace that is known to have landed. 1 is no retry: a step that fails once leaves its entry held until `held_ttl` |
| `hot_store.step_retry_min` | duration, `100ms` (1ms to 1h) | the sleep before the second try of that step; each sleep after it is twice the one before; not above `step_retry_max`. No try starts once the proxy's registration no longer allows a call to start |
| `hot_store.step_retry_max` | duration, `2s` (1ms to 1h) | the longest sleep between two tries of that step |
| `hot_store.read_window` | duration, `5s` (1ms to 1h) | the longest a read that missed the hot tier may take between the start of its read of the key's marker and cold read and the end of the later one, and the longest the reads of a listing after its cold page (more marker names, the markers of keys only the cold tier lists) may take after that page began, before the read is made again; below `tombstone_hold`. Not used while the format object says `active` |
| `hot_store.read_retries` | count, 3 (1 to 100) | how many times such a read is made again before it is answered 503 |
| `hot_store.list_marker_pages` | count, 4 (1 to 100) | how many more pages of marker names one round of a listing reads to reach the last key it merged; a round that does not reach it keeps the keys the names reach and the listing goes on from there |
| `hot_store.want_retry` | duration, `60s` (1ms to 8760h) | how long a proxy waits before it asks again for the copy-back of a key it read from the cold tier. Used only in the phase `active-marks` |
| `hot_store.want_recent_limit` | count, 100000 (1 to 100000000) | the most keys a proxy remembers it asked a copy-back of; when the memory is full of asks still within `want_retry`, a read asks for none |
| `hot_store.copy_back_max_bytes` | size, `64MiB` (1B to 1TiB) | the largest object a whole read asks to have copied back into the hot tier, and the largest the leader copies back (a larger one is dropped, the lock of its key being held for the whole stream) |
| `hot_store.copy_concurrency` | count, 128 (1 to 100000) | how many keys the leader copies to the cold tier at once, once the store runs on markers (`put-without-lock.md`, 5.9). Nothing reads it before the activation |
| `hot_store.marks_page` | count, 1000 (1 to 100000) | how many markers one listing page of the marks bucket holds when the leader looks for markers to work |
| `hot_store.orphan_after` | duration, `2m` (1ms to 8760h) | how long a marker that says a put is in flight, with no hot object behind it, must have stood unchanged on the leader's clock before the leader clears it; more than the `reassert_after` of the puts (30 s), or a put that streams its body for longer would lose its marker |
| `hot_store.tombstone_hold` | duration, `2m` (1ms to 8760h) | how long the leader keeps the marker of a deleted key after it deleted or overwrote the cold object, so that a reader that began before cannot be served the old cold copy; not below `lease_ttl` plus `lease_margin` |
| `hot_store.lock_lease` | duration, `2m` (1ms to 8760h) | how long a lock may stand unchanged on the leader's clock before the leader reaps it though its holder is alive; not below 120 s (twice the refresh of a lock, 30 s, plus the 60 s a queued delete may still land) |
| `hot_store.evict_min_age` | duration, `10m` (1ms to 8760h) | how old the cold copy of a hot object must be, counted on the leader's clock from the cold write it made (a leader that did not see the copy made counts from its takeover), before the leader evicts the hot object once the store runs on markers (`put-without-lock.md`, 5.10); a key skipped for it is offered again at the listing the eviction then asks for in this time |
| `hot_store.heartbeat` | duration, `10s` | how often a proxy rewrites its registration `_tiering/proxies/<proxy id>` in the hot tier, and the longest one such call may take. The puts and deletes made on entries depend on it through `departure` |
| `hot_store.departure` | duration, `60s` | the time a proxy that is not heard of stays live, less the margin: `Proxy::is_live` is true until the last acknowledged beat plus `departure` less `deadline_margin`. The puts and deletes made on entries read it: one is refused with 503 while the proxy is not live, and a destroying call starts only if the margin is left. |
| `hot_store.deadline_margin` | duration, `10s` | the time taken off `departure` for `is_live`, and the time `Bounded` must find left before it sends a destroying call, which it also cuts at that time. The puts and deletes made on entries use `Bounded` |

The water marks are percents from 1 to 100. The start refuses `low_water` at or above `high_water`, `high_water` above `critical`, and a `usage_stale_after` at or below `lease_ttl`. It refuses a `tombstone_hold` below `lease_ttl` plus `lease_margin` and a `lock_lease` below 120 s. It refuses `retry_max` below `retry_min`, `step_retry_max` below `step_retry_min`, `delete_poll_max` or `delete_wait` below `delete_poll_min`, a `proxy_usage_stale_after` at or below `usage_interval`, a `held_ttl` that is set at or below `lease_ttl` or `departure`, and a value outside the range of its key, naming the key. It also refuses a `heartbeat` or a `deadline_margin` of zero, a `departure` under six times `heartbeat`, a `deadline_margin` over a quarter of `departure`, naming the values.

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
| `local_cache.max_concurrent_keeps` | count, 8 (1 to 64) | The most puts (create-only or plain) copied into the cache at once, apart from the fills; a put that finds them all running succeeds and is not kept. A process that does not hold the lock of the cache directory (see [operations](operations.md#the-cache-lock-and-read-only-mode)) keeps no put at all |
| `local_cache.keep_wait` | duration, `5s` (above 0) | The longest a put waits, once the upstream has the object, for its copy to land in the cache; past it the put is answered and the copy finishes in the background |
| `local_cache.keep_deadline` | duration, `60s` (above 0, not below `keep_wait`) | The longest a copy takes in all, from the upstream's acknowledgement; a copy not done by then is abandoned and its stage file removed |
| `local_cache.write_through` | boolean, `false` | `false`: the node refuses every write with 403. `true`: it takes the writes its keys may do and sends each to the upstream, which must then be read with a key that may write; the upstream is not tested at start. In a file with `[hot_store]` too it is `true` whether left out or given, and `false` is refused |
| `local_cache.whole_fill_limit` | size, 64 MiB (1 byte to `budget`) | The most an object may be for a ranged read of it to fill the whole of it; a larger object is passed through and kept nowhere. A fill reads 8 MiB at a time whatever the limit, so raising it does not raise the memory of the fills |
| `local_cache.pin_limit` | size, half the `budget` (0 to `budget`) | The most bytes of cached objects that trees of the virtual build file system may pin against eviction; a pin that would take the pinned bytes over it is refused and the object is kept by recency only. Pinned bytes stay charged to the budget. `0` refuses every pin |
| `local_cache.cache` | boolean, `true` | `false` is refused: leave `[local_cache]` out to run without a cache |
| `local_cache.scrub_rate` | rate, `"16MiB/s"` | a size and `/s`, such as `"50MiB/s"`: the rate at which the scrub re-reads cached objects and hashes them again; `"off"` or `"0B/s"` turns the scrub off (not in a release yet, see [limits](limits.md)) |
| `local_cache.scrub_min_round` | duration, `"1h"` | the least time between the starts of two scrub rounds, so a small cache is not read again and again; at least `scrub_tick` (`"1s"` unless that is set), or `"0s"` for none |
| `local_cache.pass_interval` | duration, `10s` (1ms to 1h) | how often the eviction pass runs |
| `local_cache.lock_interval` | duration, `1s` (1ms to 1h) | how often a cache that does not hold the lock of its directory tries it again, and one that holds it checks that the lock file is still the file at the path |
| `local_cache.scrub_tick` | duration, `1s` (1ms to 1h) | the time between two ticks of the scrub. `scrub_rate` stays a rate in bytes a second: each tick reads that rate times this, rounded up and at least a byte. `scrub_min_round` is counted in ticks, rounded up to a whole tick, and not below one |
| `local_cache.listing_calls` | count, 100 (1 to 1000000) | the most pages of the upstream that one listing reads to make up for the names it leaves out (the reserved ones); a listing that needs more is answered 503 |
| `local_cache.forget_limit` | count, 65536 (1 to 100000000) | the most keys a read-only cache remembers it deleted or replaced through the upstream; past it the cache is emptied when it takes the lock |
| `local_cache.fill_chunk` | size, `8MiB` (4KiB to 1GiB) | the bytes a fill reads from the upstream at a time; the memory of a fill, whatever `whole_fill_limit` is |
| `local_cache.keep_queue_bytes` | size, `4MiB` (1B to 1GiB) | the most bytes of a put's body that may wait for the disk before its copy is dropped (counted as `slow`), so that the upstream never waits for the disk; the keeps that run at once hold at most `max_concurrent_keeps` times this |
| `local_cache.fill_socket` | path, `fill.sock` in the service's state directory | The Unix socket over which the filesystem process asks this node to put blobs in its cache (see [operations](operations.md#the-fill-socket)): an absolute path of at most 103 bytes, not `server.handoff_socket`, and not inside `local_cache.path` (which must then be absolute; refused, exit 2). Left out, it is `fill.sock` in the service's state directory when the service manager made one (`/var/lib/tierstore/fill.sock` for hostkeeper's package), and a process with no state directory makes no fill socket. A build node's key: a storage node with a cache refuses it (not in a release yet, see [operations](operations.md#not-in-a-release-yet)) |
| `local_cache.fill_socket_concurrency` | count, 16 (1 to 1024) | the most fills the fill socket runs at once, a pool of its own that `max_concurrent_fills` does not draw on; a request that finds them all running waits for one, up to `fill_socket_wait`. A storage node with a cache refuses it |
| `local_cache.fill_socket_wait` | duration, `60s` (1ms to 1h) | the longest a request of the fill socket waits for a permit of its pool before it is answered `busy`. A storage node with a cache refuses it |
| `local_cache.revalidate_after` | duration, unset (never; `0s` checks every hit) | how old a hit may be, counted from when its entry was filled or last checked, before it is checked against the upstream with one HEAD that compares the tag the entry was filled with and the size with the HEAD's (also over two tiers) and never reads the body. The same tag and size: the hit is served from the disk. Another tag or size, or no object: the entry is dropped and the read goes upstream as a miss (over two tiers, where a tier's tag is its own, an object that the other tier answers for since the fill may have another tag and is read once more). The HEAD fails: the cached copy is served, the failure is logged and counted, and the next hit asks again. An entry found on the disk at start is checked once by its first hit. Unset, a hit never calls the upstream (not in a release yet, see [limits](limits.md)) |
| `local_cache.head_before_put` | boolean, `true` | `true`: a create-only put asks the upstream for the key (one HEAD) before it reads the body (a plain put never does), so that an upstream which refuses a key it holds only after the whole body has arrived is not sent a large blob it has already; an object that is there is refused at once. `false`: the put goes to the upstream as it is, and its answer (with the tag of the object, where the upstream gives one) is the cache's version of the copy. Set it to `false` where the upstream is a proxy whose format is `active-marks`, which refuses a key it holds before it reads the body: the put then costs the upstream its own requests and not three more |

The upstream is `[cold_store]`, with any `kind`. In place of `endpoint` it may have a list `endpoints`, whose addresses the upstream requests go to in turn. With `write_through = true` the node takes create-only puts, plain puts, presigned puts, multipart uploads and deletes, and sends each to the upstream before it answers; the keys' roles apply as on a storage node, and a put that replaces on a tag (`If-Match`) and a copy are still refused. A plain `PUT` (no `If-None-Match`) is sent to the upstream as a plain put, with no HEAD before it, and replaces the object where the upstream replaces: a proxy whose format is `active-marks`, or an S3 bucket (a proxy on the entry protocol creates only, and refuses a key it holds with 412, as it does for any plain put). A put under a `cache = true` prefix, create-only or plain, is also kept on the node before it is answered, so its next read is a local hit; a plain put forgets the node's older copy of the key, also when the upstream refuses it, and keeps the new one under the tag the upstream answered. A copy that cannot be kept never fails the put; two puts of one key that run at once on the node, one of them plain, keep no copy, and the next read fills it. A build node leaves out `skip_checks` and the reconciler's keys and `server.backend_check_wait`. It may have an `oci` interface (an `[[interface]]` with `type = "oci"`): the registry front then reads through the cache, and the start refuses the file unless a `[[policy]]` with `cache = true` covers the blobs, the keys under `blobs/sha256/` below `params.prefix`.

## `[fs]`

The file of `tierstore fs`, the virtual build file system of a build node (see [the virtual build file system](operations.md#the-virtual-build-file-system)). It is read with `tierstore fs --config <path>`; it is a file of its own, not the file of the node's `tierstore` (which has no `[fs]` table), and it holds no secret. A key it has that this release does not know is a warning on stderr. On a Mac the file may also have an `[fs.nfs]` table (below). Three keys are needed, and two of them repeat what the node's `tierstore` was told, so that the two files agree.

| Key | Type, default | What |
|---|---|---|
| `fs.mountpoint` | path, `/run/tierstore/fs` (absolute) | where the one FUSE mount is made, before the job runtime starts; made if it is not there. Jobs are given `<mountpoint>/trees/<digest>` |
| `fs.runtime_dir` | path, `/run/tierstore-fs` (absolute) | the state that outlives the process: the table of tree numbers, the attachments, the record of the kernel's handshake (`init`) and the backing table (`backing`). The unit's `RuntimeDirectory=`, kept across restarts |
| `fs.control_socket` | path, `control.sock` in `fs.runtime_dir` (absolute, at most 103 bytes) | the Unix socket clients attach and detach on; mode 0660 |
| `fs.client_group` | group name or number, required | the group of the control socket. The process must be a member of it |
| `fs.owner` | `"uid:gid"`, required | the user and group id every entry of every tree reports: the host id of the user the jobs run as, so that a job may change what it copies up |
| `fs.fill_socket` | path, required (absolute) | the fill socket of the node's `tierstore`: its `local_cache.fill_socket` |
| `fs.cache_path` | path, required (absolute) | the cache directory of the node's `tierstore`: its `local_cache.path`. It must not be on a stacked file system (overlayfs, eCryptfs) |
| `fs.fetch_timeout` | duration, `60s` (1ms to 1h) | how long an answer of the fill socket is waited for, and, in whole seconds, how long a closed fill socket is retried (the node's `tierstore` is being updated) before the open fails |
| `fs.threads` | count, 16 (1 to 1024) | the threads that serve the kernel's requests, which read one `/dev/fuse` descriptor between them; an open that waits for a fill holds one |
| `fs.passthrough` | `true` or `false`, `true` | whether to ask the kernel for passthrough (reads go from the cache file to the job inside the kernel) when it offers it and the process holds `CAP_SYS_ADMIN`. `false` forces the mode of Linux 6.8: cached opens, every read the page cache misses answered by the process. See [operations](operations.md#the-virtual-build-file-system) |
| `fs.max_backing_ids` | count, 65536 (1 to 1048576) | the most backing ids live at once, one for each blob that some open file is served from; an open of a blob that has none, while this many are live, is a cached open (reads reach the process) and so is every open of that blob until those are closed; no open waits for a release, which needs a free worker. Only passthrough uses them |
| `fs.handoff_socket` | path, `handoff.sock` in `fs.runtime_dir` (absolute, at most 103 bytes) | the Unix socket the running instance offers its mount on (mode 0600, its user only), and a new instance asks on at its start. See [crash recovery and the hand-over](operations.md#crash-recovery-and-the-hand-over-of-the-mount) |
| `fs.handoff_wait` | duration, `120s` (1s to 1h) | how long a hand-over may last: for the new instance, from its connect to the running instance's `done`; for the running instance, the time it gives the new one to ask, to say `ack` and, once it has stopped, to say `ready` before it reads requests again. The wait for `ready` starts at `done` and covers the new instance's whole start (it makes its attachments again, reading the manifests through the fill socket), so set it above that time on a node with many attached trees: a new instance still starting when it ends is not the one that serves, and its `ready` fails |
| `fs.recover_wait` | duration, `30s` (1s to 1h) | after a restart on a kernel that cannot send requests again (before Linux 6.9): how long the connection is watched for requests no one answers before it is aborted, so that the jobs that wait on them fail. Set it above the longest time the node's builds keep the mount busy without a moment of rest, and not under `fs.fetch_timeout` |

### `[fs.nfs]`

On a Mac `tierstore fs` has no FUSE: it serves the same trees read-only over NFSv3 on a loopback socket and mounts the export itself with `mount_nfs` (on Linux the table is read and ignored). The table is optional, and every key has a default. A Mac's `fs.mountpoint` must be a directory the mount user owns (`mount_nfs` of a loopback export then needs no root); the process refuses to mount on one that someone else owns, and says who does. It mounts with `vers=3,tcp,ro,rdirplus,locallocks,actimeo=3600,hard` and the port twice: `ro` so that no read ends in a `SETATTR`, one hour of caching because everything below a tree's root is immutable, and `hard` so that a client waits for a server that restarts. A name that was absent when it was first asked stays absent in the client's cache for that hour, so a tree must be attached before a job looks for it.

| Key | Type, default | What |
|---|---|---|
| `fs.nfs.listen` | `"ip:port"`, `127.0.0.1:12049` | where the NFS and MOUNT protocols are served, on one port. Must be an IPv4 loopback address. Port 0 takes a free port |
| `fs.nfs.mount` | `true` or `false`, `true` | whether the process makes the mount. `false` serves only (a mount made by hand, or kept from an earlier instance) |
| `fs.nfs.mount_user` | user name, the user of the process | the user the mount is made as, who must own `fs.mountpoint`. A process that is root runs the mount through `sudo -n -u`; one that is neither this user nor root refuses |
| `fs.nfs.mount_wait` | duration, `30s` (1s to 1h) | how long a mount that fails is tried again |
| `fs.nfs.mount_retry` | duration, `500ms` (1ms to 1m) | the pause between two tries |
| `fs.nfs.fd_cache_shards` | count, 64 (1 to 4096) | the shards of the cache of open cache files, each with a lock of its own |
| `fs.nfs.fd_cache_per_shard` | count, 64 (1 to 65536) | the open files one shard keeps; the oldest is closed first. Shards times this is the most descriptors the cache holds, and the process needs a limit of open files above that (the launchd job sets 65536; with the default 256 of a shell, a read that cannot open a cache file is `NFS3ERR_IO`) |
| `fs.nfs.allowed_uids` | list of user ids, the mount user and the user of `fs.owner` | the users whose requests (`AUTH_SYS`) are served; any other, and a request with no user, is `NFS3ERR_ACCES`. This is a fence against other local users, not authentication: any local process can send any user id to the loopback socket |
| `fs.nfs.jukebox_after` | duration, `5s` (1ms to 10m) | how long a read waits for a fill before the client is told to ask again (`NFS3ERR_JUKEBOX`); the fill goes on |

```toml
[fs]
client_group = "builders"
owner = "501:20"
fill_socket = "/var/lib/tierstore/fill.sock"
cache_path = "/usr/local/tierstore/cache"
mountpoint = "/Users/builder/tierstore"

[fs.nfs]
mount_user = "builder"
allowed_uids = [501]
```

The mount is made with `allow_other` and `default_permissions` (the kernel checks the modes, which are 0644, 0755 for an executable file or a directory, and 0777 for a link), `max_background` 1024 and, with passthrough, `max_stack_depth` 1, and, where the kernel offers them, `FUSE_CACHE_SYMLINKS`, `FUSE_PARALLEL_DIROPS`, `FUSE_DO_READDIRPLUS` and `FUSE_READDIRPLUS_AUTO`. An example:

```toml
[fs]
client_group = "builders"
owner = "1000:1000"
fill_socket = "/var/lib/tierstore/fill.sock"
cache_path = "/var/cache/tierstore"
```

## `params.upstream`

On main, not in v0.0.41. A registry the OCI front pulls from when its store has no such manifest or blob (a pull-through cache). It is an inline table in the params of an `oci` interface, for example `params = { addresses = ["127.0.0.1:5000"], upstream = { url = "https://registry.example" } }`. A storage node's file takes it, with or without `[local_cache]`; a build node's file is refused with it, because a pull is stored and a build node takes no writes.

**This makes tierstore an outbound client.** Without the param the process only accepts connections for the registry front. With it, a request for an object the store lacks makes the process open a connection of its own to the upstream's address, and the host it runs on must be allowed to reach it. Nothing else of tierstore dials out for the front. A connection is made for each request and carries a plain `GET` (or a `HEAD` to re-check a tag): nothing is pooled and no state is kept between pulls.

| Key | Type, default | What |
| --- | --- | --- |
| `params.upstream.url` | string, needed | `https://host[:port]` or `http://host[:port]`, with no user name, path or query |
| `params.upstream.username` | string | the user name to log in with; with `password_file`, or neither |
| `params.upstream.password_file` | path | the file that holds the password or token of `username`: owned by the user the server runs as, mode 0600 (any group or other permission bit is refused), not a link; its text without the line end is the password. The password is never in the configuration file |
| `params.upstream.ca_file` | path | PEM certificates the upstream's certificate may be issued by, besides the system's; read once at the start |
| `params.upstream.timeout` | duration, `30s` | how long the upstream has to answer a request, and to send the next piece of a body; the time a whole body takes is not bounded |
| `params.upstream.max_redirects` | count, 4 | the most redirects one request follows |
| `params.upstream.recheck_timeout` | duration, `5s` | how long the whole re-check of a tag the store has may take, its `HEAD`, a login and a `GET` of a moved manifest together; past it the stored tag is served |

For example, a registry at `registry.example` that wants a login is `url = "https://registry.example"` with `username` and `password_file`, as keys of the `upstream` table of the `oci` interface.

How it behaves:

- The login answers only a challenge (401) that the configured `url` itself sends: a 401 that comes after a redirect to another host or port is not answered, and the pull fails with 502. A `Bearer` challenge from `url` names a token `realm`; the realm is trusted with the login as in the registry token flow, over https or to a loopback address, and a realm that is neither is asked without the login.
- A manifest by digest, a manifest by tag and a blob are pulled, as `GET /v2/<name>/manifests/<reference>` and `GET /v2/<name>/blobs/<digest>` of the upstream, with the `<name>` of the request. The store is still one namespace: an object that is in it is served for any name, and the upstream is asked only for what the store lacks. `tags/list` is the store's own and is not pulled.
- A manifest is read whole (at most 4 MiB), must be a manifest, and is stored by the digest its bytes hash to; one asked for by digest that does not hash to it is refused. A tag is then pointed at the manifest. A blob is streamed into the store and hashed in the same pass, with no limit on its size and no copy of it in memory, and nothing is stored if it does not hash to its digest.
- The client's request waits while the object is pulled and stored, and is then answered from the store as any other (a `Range` too; a `HEAD` of a blob that is not there pulls it). A later request never reaches the upstream. A client that goes away does not stop a blob being stored. Two requests for one missing object at once each pull it.
- A tag that is in the store is re-checked on each `GET` or `HEAD` of it: the process sends a `HEAD` of the manifest to the upstream (with the same login, `timeout` and redirects) and reads its `Docker-Content-Digest`. If that is the digest the newest version of the tag holds, the stored manifest is served and no `GET` is made. If it differs, the manifest is pulled by the tag, stored, and the next version of the tag is written (versions are immutable and the newest wins; the old ones stay), and that manifest is served. If the `HEAD` fails (the upstream cannot be reached, answers late, answers 429, 5xx or any status but 200 or 404, or sends no valid digest, or the re-check as a whole takes longer than `recheck_timeout`), or the moved manifest cannot be pulled or stored, the stored tag is served and `tierstore_oci_tag_recheck_failures_total` counts it. A pull by digest is never re-checked. A repository that is pushed to and pulled through has the upstream as the authority for its tags: a tag moved by a push stays while the upstream has the same digest or no such tag (404), and is moved to the upstream's digest by the next pull of the tag if the upstream's differs. A file with no `upstream` re-checks nothing.
- The login follows the upstream's challenge. `Basic` is answered with the user name and password. `Bearer` is answered with a token fetched from the challenge's realm, logged in with the same user name and password (or anonymously, if there is none), and used for that pull only. A password is sent only over HTTPS or to a loopback address: the start refuses a login that would go in the clear, and a token realm that is neither is asked with no password. Redirects are followed and carry no login.
- The client is told 404 when the upstream has no such object, and 502 with the code `UNAVAILABLE` for any other failure of the upstream (it cannot be reached, refuses the login, answers late, sends bytes that are not the object, or cuts a body); what the upstream said is on stderr and not in the answer. A store that cannot take what was pulled answers as it does for any write it cannot make.

The start refuses an `upstream` table whose password file others can read, that is missing or empty, whose `ca_file` holds no certificate, or whose address cannot work; `--check-config` reads the password file and the `ca_file` as the start does.

## `[[policy]]`

`[[policy]]` has a `prefix` (empty for all the rest, or ending in `/`) and `cache`. Without `[local_cache]` a policy is read and checked and has no effect, and `policy.cache = true` is refused, since there is no cache. With `[local_cache]` (a build node, or a storage node with a cache) a key is kept if the longest prefix that holds it says `cache = true`, and at least one policy must say so. Two non-empty prefixes may not nest, a prefix may not be listed twice, and none may lie under `_journal/`, `_tiering/` or `_entries/`.

| Key | Type, default | What |
|---|---|---|
| `policy.prefix` | string, required | the keys the policy is about: empty for all the rest, or ending in `/` |
| `policy.cache` | boolean, `false` | `true`: keep the keys under the prefix in the local cache |

## What is not a key

A value of the code is a key when it is a tuning choice: an interval, a timeout, a limit, a back-off, a retry count or a buffer size that an operator may have a reason to change. These are not keys, because they are not choices of this program:

- the limits of the S3 protocol that the code holds to: the 1024-byte longest key, 1000 keys to a listing page, 5 MiB to 5 GiB for a part, 10000 parts to an upload, the 604800-second longest presigned expiry, the 15-minute clock skew SigV4 allows, and the 64-byte block of the HMAC;
- the limits of the OCI distribution format: the 255-byte longest repository name and the 128-byte longest tag;
- the sizes of the formats the program writes: the 32-byte tag of a directory tier's file, the file-system limits of a directory tier's names and mode bits;
- the bounds a grant token and a key file are held to (a token's size, the number of keys of a grant, the shortest grant secret), which keep hostile input from using memory.

These tuning values are fixed in this release and have no key: the intervals of the cache's passes and of the scrub, the buffer sizes of the OCI front, and the timeouts of the update hook's calls.

## The flags of a run

These describe one run, not the data path, and have no key. `--check-config` and `--capabilities` are for the host updater's hook and are not in `--help`; an operator writes the file and enables the service:

| Flag | Default | What |
|---|---|---|
| `--config` | none | the file; a start without it is refused with status 2 |
| `--check-config` | off | with `--config`: load the file, read the key files it names, the S3 front's certificate and key if it has them, and for an `oci` interface check the prefix, the connection lifetime and the TLS files; then exit 0 and print nothing, or print the refusal and exit 2. It binds nothing and asks no store. What it does not check is listed below. Without `--config` it is a usage error |
| `--capabilities` | off | print what this build can do, one capability to a line, and exit 0 (a build with none prints nothing). It needs no file, and with any other argument it prints nothing on stdout, says `--capabilities takes no other flag` on stderr and exits 2 |
| `--drain-lb-wait` | the key's | kept for the units of the releases before: the same as `server.drain_lb_wait`, not in `--help`; refused when the file sets the key too |
| `--drain-deadline` | the key's | kept as above for `server.drain_deadline` |

`--check-config` finds what the loader refuses (see "What the loader refuses"), a `bucket` that is a probe path name, a zero `body_idle_timeout`, a key file that is not owned by the user, is open to others or is a link, two keys with one id, and, in the file of a build node, a zero `s3_request_timeout`. It does not find what a start checks after that: the water marks out of order, `usage_stale_after` not above `lease_ttl`, the heartbeat values out of the relations above, or a zero `s3_request_timeout` in a storage node's file (a start exits 1 on each, and the unit tries again every 30 seconds); a tier that is down; the cache directory and a `budget` over the size of its file system. A file that passes it can still fail to start.

`--help` and `--version` are the usual: `--version` prints the version, `+` and the commit the binary was built from (`tierstore 0.3.0+0123456789ab`), and the start line ends with ` commit=<commit>`. The commit is twelve hex digits, or `unknown` for a build that had neither a git checkout nor a stamp.

## Settings that were removed

The entry protocol has no per-node journal and no reconciler interval, so eight settings of `[hot_store]` are gone: `journal_linger`, `journal_retention`, `reconcile_interval`, `registration_retention`, `journal_batch_entries`, `journal_page_size`, `copy_rounds` and `delete_rounds`. A file that still has one of them loads: the key is accepted, its value is not used, and the start says once for each key, `tierstore: hot_store.<key> is ignored: the journal and its reconciler were removed`. In `[cold_store]` the key is refused, as it was, since only `[hot_store]` has it. The reconciler now waits `hot_store.idle_min` (one second) after a round that did nothing, doubling to `hot_store.idle_max` (ten seconds), and not at all after a round that changed something. The size of its listing pages, which `journal_page_size` once set, is `hot_store.listing_page_size`.

## What the loader refuses

Each of these exits with status 2 and names the file and the key (a syntax error names the line and the column, never the value):

- a value of the wrong type in a key this release has (a key or a table it does not have is not refused, see the next section);
- a key that holds a secret: keys and credentials are paths of files whose names end in `_file` or `_files`; `secret_access_key`, `access_key_id`, `password`, a name with `token` in it and the like are refused anywhere;
- a file that is none of a storage node's (`[hot_store]` and `[cold_store]`), a build node's (`[local_cache]` and `[cold_store]`, no `[hot_store]`) and a storage node's with a cache (all three tables);
- in a storage node's file with a cache, `write_through = false`, a relative cache `path`, `dir` tier directory, `lock_file` or `oci` key file, and a cache `path` that is a tier's directory, inside it or around it (or around the lock file or the key files of an `oci` interface);
- in the file of a build node or of a storage node with a cache, an `oci` interface whose blobs no `[[policy]]` with `cache = true` keeps, a `layout` other than `digest-v1`, `cache = false`, marks out of order, and no `[[policy]]` with `cache = true`;
- an `s3` tier with both `endpoint` and `endpoints`, with an empty `endpoints` or with an address in it twice, a key a tier's `kind` does not take (`endpoints` included), a `dir` tier without `path`, an `s3` tier without `bucket`, `endpoint` (or `endpoints`) or `credentials_file`, and a key only one tier has in the other (`budget` and the reconciler's keys belong to `[hot_store]`, `skip_checks` to `[cold_store]`);
- a water mark outside 1 to 100, and a budget, count or limit of zero;
- an `[[interface]]` that breaks the rules of its section above: a type that is not `s3` or `oci` (`fs` is named as a process of its own), neither `params.port` nor `params.addresses` or both, a port of zero, an address listened on twice, a param its type does not take, only one of the certificate and the key, `params.max_connections = 0`, `oci` entries that disagree, and a file with no `s3` entry;
- a `[[policy]]` that breaks the rules above.

## Keys this release does not know

A key or a table that this release does not have is not a fault. The loader reads the file without it, and says on stderr, once for each such key, with the table and the name of the key (a table of the top level, or a table inside a table, is one key, named by its own name, and what is in it is not read):

```text
tierstore: config key <table>.<key> is not known to this release and is ignored
```

This is so that a downgrade works: a later release adds an optional key, an operator sets it, and the release before still starts from the same file, without the setting. The warning names only the key, never a value. This holds from the release that introduced it: a release before it still refuses an unknown key, so a file with a key of a later release is downgrade-safe only to a release that has this rule. A key that this release has but whose value is of the wrong type is still refused, and so is a key of an `[[interface]]` entry (its `type` and `params`) that the release does not have, and so is an unknown key whose name says it holds a secret (see above). `--check-config` prints the same warnings and exits 0.

## Moving from flags

A unit that gave flags is moved by writing the file and changing `ExecStart=` to `--config <path>` with the flags of the run. Each key of `[server]`, `[hot_store]` and `[cold_store]` has the name of the flag that held it with `_` for `-`, the flags that were repeated are arrays (`server.read_only_key_files`), the budget flag is `hot_store.budget`, the flag that skipped a check is `cold_store.skip_checks`, the listen flags became the `s3` interfaces and the registry flags the params of an `oci` interface (the `listen` key and the `[oci]` table of a file stay readable, see `[[interface]]`). A start that still gives one of those flags exits with status 2 and a message that names its replacement.

## Not in a release yet

- **Random choice and least-in-flight choice between the endpoints of a tier.** The addresses of `endpoints` are used in turn; a request moves to the next address only when its connection could not be made (`server.s3_endpoint_attempts`).

## Deprecated settings

Two settings belonged to the start-up self-test that earlier releases ran. tierstore no longer tests its stores (see [what tierstore requires of a store](operations.md#what-tierstore-requires-of-a-store)), so they do nothing. They are still accepted, so that a file written for an earlier release starts, and each one given is one line on stderr at the start: `tierstore: cold_store.skip_checks is deprecated and ignored: tierstore no longer tests its stores at start-up; remove it`, and the same for `server.backend_check_wait`. Their values are not used: `skip_checks` takes any list of texts, and `backend_check_wait` is still parsed as a duration, as it always was, so a value that is not one is still refused.

| Key | What |
|---|---|
| `server.backend_check_wait` | a duration; ignored |
| `cold_store.skip_checks` | a list of texts; ignored. A build-node file still refuses both keys |
