# Troubleshooting

## Exit statuses

| Status | Meaning | The unit |
|---|---|---|
| 0 | a clean drain, or `--check-config` or `--help` done | stays stopped |
| 1 | a start that failed: an address in use, a key file refused after parsing, the settings that depend on each other (see "Messages at start"), and a hot tier that cannot be reached when the proxy is opened (`opening the proxy: ...`). A cold store that cannot be reached is not one: tierstore does not test its stores | restarts after 30 seconds |
| 2 | a flag the parser refused (or a flag of the data path that went, which the message answers with its key), a start without `--config`, a file that does not load (`conditional_delete = false` for a `memory` or `dir` tier among it), a limit of the OCI front (its prefix, its certificate and key, see "OCI front"), a local cache budget the file system cannot hold (see "Build-node cache"), or two keys with one id | does not restart: a restart cannot mend it |
| 3 | the cache directory, once opened, could not be probed or its file system failed the cache check (a build node, or a node with `[local_cache]`) | does not restart: the same file system would answer the same way |

Messages go to stderr, prefixed `tierstore: `, and so to the journal: `journalctl -u tierstore`. None carries a secret.

## Messages at start

Settings that depend on each other. These are checked after the file has parsed, and a start that fails one exits with status 1, not 2, so the unit starts again every 30 seconds until the value is fixed:

```message
hot_store.low_water must be below hot_store.high_water
hot_store.high_water must not be above hot_store.critical
hot_store.usage_stale_after must be longer than hot_store.lease_ttl
server.body_idle_timeout must be longer than zero
server.s3_request_timeout must be longer than zero
server.bucket <name> is the name of a probe path
```

Fix the value; `journalctl -u tierstore` shows the message at every try. `--check-config` does not find these (see [configuration](configuration.md#the-flags-of-a-run)).

A key file:

```message
<flag> <path>: others can read it; make it mode 0600
<flag> <path>: is not owned by the user the server runs as
<flag> <path>: is a symbolic link; name the file itself
```

Run `chown tierstore` and `chmod 0600` on the file. The process refuses a file that its group can access too. For an unreadable or malformed file the message names the flag and the file and says what form it expects, never the content.

The same access key id on two keys: `the access key id "<id>" is used twice among server.access_key_file and server.read_only_key_files` (the flags it names are those of the keys that clash). Give each key an id of its own.

The configuration file:

```message
<key>: a secret is never in the file; give the path of the file that holds it (a key whose name ends in _file)
--config is needed: the settings are in a file (docs/configuration.md), and tierstore takes no flag for them
--<flag> was removed: the file given with --config holds it; <instead>
<key>: a node without [local_cache] keeps no cache: give it [local_cache]
<key>: the file needs both [hot_store] and [cold_store] (the storage-node shape), or [local_cache] and [cold_store] without [hot_store] (the build-node shape)
<key>: a node that owns the data takes writes, and they pass through the cache so that its entries stay right: leave it out or give true
<key>: must not be the directory of <tier>, inside it or around it: the tier would list the cache's files as objects
<key>: must be an absolute path (it is compared with the directories of the tiers)
<key>: must be an absolute path to be compared with <tier>'s directory
<key>: must not hold <key>: it is not a place for the cache's files
<key>: no [[policy]] covers the blobs under <prefix>, so every pull would go to the upstream; give a [[policy]] with prefix = <prefix> and cache = true
<key>: the blobs under <prefix> are covered by the [[policy]] with prefix <prefix>, which has cache = false, so every pull would go to the upstream; give a [[policy]] with prefix = <prefix> and cache = true
```

Move the setting into the file as a path; give the file one of the three shapes: a storage node's, a build node's, or a storage node's with a `[local_cache]` in front of it (all three tables). A cache needs a `[[policy]]` with `cache = true`, a `write_through` that is not `false` when the node has a hot store, and a `path` that is absolute and is not a tier's directory, inside it or around it (a `dir` tier's directory is absolute too). A message of a syntax error reads `line <N>, column <M>: <key>: ...` and never repeats the value.

The tiers:

```message
opening the proxy: <why>
binding <address>: <why>
```

tierstore does not test its stores at start. Opening the proxy reads and writes the hot tier's own state (its registration and its journal), so a hot tier that is down fails the start here with `opening the proxy: <why>`: check the endpoint, the network and the store; the unit tries again every 30 seconds. A cold store that is down does not fail a start: the first request that needs it fails (a client sees 503 `ServiceUnavailable`, see "Errors a client sees"). A store that does not meet what tierstore requires of it (see [operations](operations.md#what-tierstore-requires-of-a-store)) is not detected at all. `binding` is an address in use, or one that is not on this host. A server given several S3 or registry listen addresses (see [configuration](configuration.md)) starts only if it can bind every one: the message names the first that failed, and nothing is served.

An address given twice exits 2 before anything is bound; the message names the entries and the address:

```message
<key>: the address <address> is already used by <interface>
<key>: the address <address> is listed twice
<key>: it needs at least one address
```

`cold_store.skip_checks` and `server.backend_check_wait` are accepted and ignored, each with one warning line on stderr (see [configuration](configuration.md#deprecated-settings)): they belonged to the self-test that earlier releases ran, which is gone.

### OCI front

The params of an `oci` interface (`params.prefix`, `params.tls_cert_file`, `params.tls_key_file`, `params.addresses`) are checked at start. A refusal of the prefix, of a certificate or key that is not usable, or of the pair of params exits 2 and the unit does not restart; a certificate or key file that cannot be read, and an address that cannot be bound, exit 1 and restart every 30 seconds:

```message
params.prefix <quoted-prefix>: it must end in /
params.prefix <quoted-prefix>: it must be segments of letters, digits, . _ and -, separated by single /
params.prefix <quoted-prefix>: it overlaps a prefix the store keeps for itself
params.tls_cert_file and params.tls_key_file go together: give both, or neither
<flag> <path>: <why>
params.tls_cert_file and params.tls_key_file: <why>
```

`<quoted-prefix>` is the prefix as given, in double quotes. Give a prefix such as `oci/`, and give both TLS params or neither. `<flag> <path>: <why>` names the file: it is not PEM, holds no certificate, is a key any user can read (use mode 0640 or 0600), or is a certificate that is not valid now or does not name the address: `the certificate has no IP subjectAltName for <ip> (it names [...])`. Clients dial the front by IP address, so issue a certificate with an IP subjectAltName for each address of an `oci` interface (for `0.0.0.0` or `::`, any IP name serves). The last form, which names both keys, is a certificate and a key that do not make one TLS configuration, as when the key is not the certificate's: give the key that belongs to the certificate.

Once the front runs, a client whose TLS handshake fails is logged and dropped, and the front goes on:

```message
tierstore: oci: <peer>: TLS handshake: <error>
tierstore: oci: <peer>: TLS handshake took too long
```

The first is a client that does not trust the certificate, speaks plain HTTP to an HTTPS front, or sends garbage: the `<error>` says which. The second is a connection that did not finish the handshake in `server.oci_handshake_timeout` (10 seconds by default) (a port scan, a stalled client, or a path that drops packets); the connection is closed to free its slot. Neither needs a restart.

### Build-node cache

A `[local_cache]` is checked at start against its file system:

```message
local_cache.budget: <N> bytes is more than the <M> bytes of the file system at <path> less its free_floor of <P> percent
```

This exits 2 and the unit does not restart. Lower `local_cache.budget`, lower `local_cache.free_floor`, or give the cache a larger file system. A file system that does not report its size is not judged: the start prints a warning that the budget is not checked and goes on.

A cache directory that cannot be created or locked (missing and cannot be made, not writable by the user the server runs as, or on a read-only mount) fails earlier with `opening the local cache at <path>: <why>` and exits 1, so the unit restarts every 30 seconds: create it and `chown` it to that user, or mount it read-write.

The opened cache directory is then probed with a few files of its own:

```message
refusing to start: the local cache at <path>: <why>
refusing to start: the file system of the local cache at <path> failed the <check> check
```

Both exit 3 and the unit does not restart. The first is a probe that could not be made in the opened directory; `<why>` is the system's answer. The second names a question that the file system answered wrongly, one of `create-only`, `atomic-visibility`, `raw-bytes` or `st_nlink`: a file system that overwrites on a create-only write, shows a file before it is whole, does not return the bytes written, or does not report a link count of 2 for a file with a second hard link. Put the cache on a local file system that does these properly (not a network or overlay mount that does not).

## Errors a client sees

| Status and code | Why |
|---|---|
| 403 `InvalidAccessKeyId` | the key id is unknown, or the request's region is not `server.region` (the server's own, not a tier's) |
| 403 `SignatureDoesNotMatch` | the secret is wrong, or the request was changed after it was signed |
| 403 `RequestTimeTooSkewed` | the client's clock is more than 15 minutes from the server's |
| 403 `AccessDenied` | the key's role does not allow the call; see [keys and roles](keys.md) |
| 412 `PreconditionFailed` | the key already exists in either tier: puts are create-only |
| 413 `EntityTooLarge` | a body longer than `server.max_request_bytes` (or the presigning key's ceiling) |
| 400 `RequestTimeout` | a body that stalled for `server.body_idle_timeout` or came slower than `server.put_min_bytes_per_sec`; nothing is stored |
| 503 `SlowDown` | more than `server.max_concurrent_puts` bodies at once for longer than `server.put_permit_wait`, or a put that met a full tier; or, under the format phase `active-marks`, a delete of a key that another writer holds for longer than the lock wait (5 s), or whose hot DELETE ended unknown and is kept from a put for a minute; retry |
| 503 `ServiceUnavailable` | a write while the format object does not say `active` at epoch 2 (the text names `tierstore format-epoch --activate`; a store is activated once, see [operations](operations.md#rolling-out-the-entry-protocol)), or while `active-marks` records another marks bucket than this proxy's `hot_store.marks_bucket` (the text names both; set the key to the recorded name), or while it says `drain` or `drained` (writes are paused, and a rollback from the markers waits in `drain`, see [operations](operations.md#rolling-out-the-markers-and-going-back)); a put or delete that met a put, delete or copy of the same key in flight (retry; an entry left held by a proxy that is still beating is reaped after `hot_store.held_ttl`, an hour by default, and `tierstore entries ls` names its owner; one left by a process that has since restarted under the same proxy id is reaped at the reconciler's next step once the new process has beaten for `hot_store.departure`); a delete that waited for the reconciler's release of the key and did not get it before the request deadline; a tier could not be reached; a drain answers no request 503 (see "A connection ends during a restart") |
| 501 `NotImplemented` | a SigV4a (`STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD`) body, or a query parameter the front does not know |
| 400 `InvalidRequest` | an `aws-chunked` body that is not framed as S3 frames it, or has a line over `server.aws_chunked_max_line_bytes`; a trailer that `x-amz-trailer` did not announce |
| 400 `IncompleteBody` | an `aws-chunked` body that ended early or decoded to other than `x-amz-decoded-content-length` bytes |
| 403 `SignatureDoesNotMatch` | besides a bad request signature: a chunk or trailer of a signed `aws-chunked` body whose signature does not follow from the one before it; nothing is stored |

## The hot tier keeps filling

Check `/usagez` first. A 503 there means the reconciler is not publishing: look at its process (`journalctl`), at the cold tier's reachability and at the lease. If usage is fresh and still high, the cold tier is taking copies slower than clients write: raise `hot_store.copy_parallel` if the cold store has room for more at once, or give the hot tier a larger `hot_store.budget`. See [operations](operations.md) for what a full hot tier does.

## A connection ends during a restart

From the moment a stop begins every answer carries `Connection: close`, and at the end of the settle (`--drain-lb-wait`) the server closes each idle connection by shutting its end for writing: a client reads the end of the stream, not a reset, and opens a new connection. A client that sent a request in the one round trip before it read that end gets no answer: the server read the request, dropped it and counted it in `tierstore_drain_late_requests_total`. Do not retry such a request unless it is safe to repeat: a retried create-only put answers 412 whether its first try was stored or another writer stored the key. Only a connection that was idle for the whole settle can meet this, and the server cannot remove it; a count that is large after every restart means clients that send on a connection without looking at `Connection: close`. The OCI read front drains the same way, and counts its own connections in `tierstore_oci_drain_late_requests_total`. The counts reach their last value after the settle, when `/metricz` has closed with the S3 listener, so the process says them on stderr as it exits, a line for each front that counted one: `tierstore: <N> connections sent a request too late in the drain of the S3 front` (or `of the OCI front`). Each counts connections, once each, not requests.

## A reused connection is closed

The server closes a connection that has waited for its next request for longer than `server.header_timeout` (60 seconds by default; each request starts the wait again). It closes it without an answer: the client reads the end of the stream with nothing sent. A client whose pool still holds that connection finds it gone when it next uses it: its write fails with `BrokenPipe` or a connection reset, or it reads an end of stream where it expected an answer. This is not an error of the server, and it counts nothing. An SDK or a benchmark client that keeps an idle connection for longer than this meets it. Keep `server.header_timeout` above the time the clients keep an idle connection in their pool (it takes up to `1h`), or set the idle time of their pool below it. A client that retries must do so on a new connection, and only a request that is safe to repeat: the server may have closed the connection as the request was on its way (see "A connection ends during a restart").

## At drain deadline

`tierstore: <N> requests were cut at the drain deadline` on stderr means clients were still sending or reading when `--drain-deadline` ran out. Raise it, with `TimeoutStopSec=` in the unit.
