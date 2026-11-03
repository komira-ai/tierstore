# Troubleshooting

## Exit statuses

| Status | Meaning | The unit |
|---|---|---|
| 0 | a clean drain, or `--print-config` or `--help` done | stays stopped |
| 1 | a start that failed: a tier that could not be reached, an address in use, a key file refused after parsing, and the flags that depend on each other (see "Messages at start") | restarts after 30 seconds |
| 2 | a flag the parser refused, a file that does not load, a limit of the OCI front (its prefix, its certificate and key, see "OCI front"), a local cache budget the file system cannot hold (see "Build-node cache"), or two keys with one id | does not restart: a restart cannot mend it |
| 3 | a tier answered the start-up self-test wrongly, or the local cache directory is unusable or its file system failed the cache check (a missing or unwritable directory too) | does not restart: the same store would answer the same way |

Messages go to stderr, prefixed `tierstore: `, and so to the journal: `journalctl -u tierstore`. None carries a secret.

## Messages at start

Flags that depend on each other. These are checked after the flags and the file have parsed, and a start that fails one exits with status 1, not 2, so the unit starts again every 30 seconds until the value is fixed (a file's message names the key where a flag's names the flag):

```message
--low-water must be below --high-water
--high-water must not be above --critical
--reconcile-interval must be shorter than --lease-ttl
--usage-stale-after must be longer than --lease-ttl
--body-idle-timeout must be longer than zero
--s3-request-timeout must be longer than zero
--bucket <name> is the name of a probe path
```

Fix the value; `journalctl -u tierstore` shows the message at every try. `--check-config` does not find these (see [configuration](configuration.md#the-flags-that-stay-flags)). In a file these name `hot_store.low_water`, `hot_store.high_water` and so on.

A key file:

```message
<flag> <path>: others can read it; make it mode 0600
<flag> <path>: is not owned by the user the server runs as
<flag> <path>: is a symbolic link; name the file itself
<flag> <path>: the secret must be at least 32 bytes long
```

Run `chown tierstore` and `chmod 0600` on the file. The process refuses a file that its group can access too. For an unreadable or malformed file the message names the flag and the file and says what form it expects, never the content.

The same access key id on two keys: `the access key id "<id>" is used twice among --access-key-file and --read-only-key-file` (the flags it names are those of the keys that clash). Give each key an id of its own.

The configuration file:

```message
<key>: a secret is never in the file; give the path of the file that holds it (a key whose name ends in _file)
--config cannot be given with <flags>: the file holds what they hold (only --drain-lb-wait and --drain-deadline go with it)
<key>: a node without [local_cache] keeps no cache: give it [local_cache]
<key>: the file needs both [hot_store] and [cold_store] (the storage-node shape), or [local_cache] and [cold_store] without [hot_store] (the build-node shape)
<key>: a node that owns the data takes writes, and they pass through the cache so that its entries stay right: leave it out or give true
<key>: must not be the directory of <tier>, inside it or around it: the tier would list the cache's files as objects
<key>: must be an absolute path (it is compared with the directories of the tiers)
<key>: must be an absolute path to be compared with <tier>'s directory
<key>: must not hold <key>: it is not a place for the cache's files
<key>: is in a file with [hot_store] too, which has no flag form
<key>: no [[policy]] covers the blobs under <prefix>, so every pull would go to the upstream; give a [[policy]] with prefix = <prefix> and cache = true
<key>: the blobs under <prefix> are covered by the [[policy]] with prefix <prefix>, which has cache = false, so every pull would go to the upstream; give a [[policy]] with prefix = <prefix> and cache = true
```

Move the setting into the file as a path, or take the flag out of the command line; give the file one of the three shapes: a storage node's, a build node's, or a storage node's with a `[local_cache]` in front of it (all three tables). A cache needs a `[[policy]]` with `cache = true`, a `write_through` that is not `false` when the node has a hot store, and a `path` that is absolute and is not a tier's directory, inside it or around it (a `dir` tier's directory is absolute too). A message of a syntax error reads `line <N>, column <M>: <key>: ...` and never repeats the value.

The tiers:

```message
tier unreachable: <tier>: <error>
opening the proxy: <why>
binding <address>: <why>
```

`tier unreachable` is a store that did not answer for `--backend-check-wait`: check the endpoint, the network and the store. The process asks again with growing pauses before it gives up, so a store that is slow to come up after a reboot does not fail a start. `binding` is an address in use, or one that is not on this host. On main, not in v0.0.41, a server given several S3 or registry listen addresses (see [configuration](configuration.md)) starts only if it can bind every one: the message names the first that failed, and nothing is served.

An address given twice (on main, not in v0.0.41) exits 2 before anything is bound; the message names the flag or the key and never the address:

```message
<flag> is given the same address twice
<key>: the same address is listed twice
<key>: it needs at least one address
```

A tier that answers a self-test question wrongly exits 3 and names the tier, the check and what it answered: the store overwrites on a create-only put, ignores `If-Match`, answers a range with the whole object, or completes a multipart upload over an existing object. Use a store that does these properly. `--skip-backend-check multipart-conditional=<reason>` is for the last only.

### OCI front

The flags of the OCI front (`--oci-prefix`, `--oci-tls-cert`, `--oci-tls-key`, `--oci-listen`) are checked at start. A refusal of the prefix, of a certificate or key that is not usable, or of the pair of flags exits 2 and the unit does not restart; a certificate or key file that cannot be read, and an address that cannot be bound, exit 1 and restart every 30 seconds:

```message
--oci-prefix "<prefix>": it must end in /
--oci-prefix "<prefix>": it must be segments of letters, digits, . _ and -, separated by single /
--oci-prefix "<prefix>": it overlaps a prefix the store keeps for itself
--oci-tls-cert and --oci-tls-key go together: give both, or neither
<flag> <path>: <why>
--oci-tls-cert and --oci-tls-key: <why>
```

Give a prefix such as `oci/`, and give both TLS flags or neither. `<flag> <path>: <why>` names the file: it is not PEM, holds no certificate, is a key any user can read (use mode 0640 or 0600), or is a certificate that is not valid now or does not name the address: `the certificate has no IP subjectAltName for <ip> (it names [...])`. Clients dial the front by IP address, so issue a certificate with an IP subjectAltName for each `--oci-listen` address (for `0.0.0.0` or `::`, any IP name serves). The last form, which names both flags, is a certificate and a key that do not make one TLS configuration, as when the key is not the certificate's: give the key that belongs to the certificate.

Once the front runs, a client whose TLS handshake fails is logged and dropped, and the front goes on:

```message
tierstore: oci: <peer>: TLS handshake: <error>
tierstore: oci: <peer>: TLS handshake took too long
```

The first is a client that does not trust the certificate, speaks plain HTTP to an HTTPS front, or sends garbage: the `<error>` says which. The second is a connection that did not finish the handshake in 10 seconds (a port scan, a stalled client, or a path that drops packets); the connection is closed to free its slot. Neither needs a restart.

### Build-node cache

A `[local_cache]` is checked at start against its file system:

```message
local_cache.budget: <N> bytes is more than the <M> bytes of the file system at <path> less its free_floor of <P> percent
```

This exits 2 and the unit does not restart. Lower `local_cache.budget`, lower `local_cache.free_floor`, or give the cache a larger file system. A file system that does not report its size is not judged: the start prints a warning that the budget is not checked and goes on.

The cache directory is then probed with a few files of its own:

```message
refusing to start: the local cache at <path>: <why>
refusing to start: the file system of the local cache at <path> failed the <check> check
```

Both exit 3 and the unit does not restart. The first is a directory that cannot be used: it is missing and cannot be made, not writable by the user the server runs as, or on a read-only mount; `<why>` is the system's answer. Create it and `chown` it to that user, or mount it read-write. The second names a question that the file system answered wrongly, one of `create-only`, `atomic-visibility` or `raw-bytes`: a file system that overwrites on a create-only write, shows a file before it is whole, or does not return the bytes written. Put the cache on a local file system that does these properly (not a network or overlay mount that does not).

## Errors a client sees

| Status and code | Why |
|---|---|
| 403 `InvalidAccessKeyId` | the key id is unknown, or the request's region is not `--region` (the server's own, not a tier's) |
| 403 `SignatureDoesNotMatch` | the secret is wrong, or the request was changed after it was signed |
| 403 `RequestTimeTooSkewed` | the client's clock is more than 15 minutes from the server's |
| 403 `AccessDenied` | the key's role does not allow the call; see [keys and roles](keys.md) |
| 412 `PreconditionFailed` | the key already exists in either tier: puts are create-only |
| 413 `EntityTooLarge` | a body longer than `--max-request-bytes` (or the presigning key's ceiling) |
| 400 `RequestTimeout` | a body that stalled for `--body-idle-timeout` or came slower than `--put-min-bytes-per-sec`; nothing is stored |
| 503 `SlowDown` | more than `--max-concurrent-puts` bodies at once for longer than `--put-permit-wait`, or a put that met a full tier; retry |
| 503 `ServiceUnavailable` | a tier could not be reached; in v0.0.41 and before also a request on a connection that was open when the drain's settle ended. On main a drain answers no request 503 (see "A connection ends during a restart") |
| 501 `NotImplemented` | an `aws-chunked` or trailing-checksum body, CRC-64/NVMe, or `encoding-type=url` on a listing; set the SDK's checksum calculation to "when required" |

## The hot tier keeps filling

Check `/usagez` first. A 503 there means the reconciler is not publishing: look at its process (`journalctl`), at the cold tier's reachability and at the lease. If usage is fresh and still high, the cold tier is taking copies slower than clients write: raise `--copy-parallel` if the cold store has room for more at once, or give the hot tier a larger `--hot-bytes`. See [operations](operations.md) for what a full hot tier does.

## A connection ends during a restart

On main, not in v0.0.41. From the moment a stop begins every answer carries `Connection: close`, and at the end of the settle (`--drain-lb-wait`) the server closes each idle connection by shutting its end for writing: a client reads the end of the stream, not a reset, and opens a new connection. A client that sent a request in the one round trip before it read that end gets no answer: the server read the request, dropped it and counted it in `tierstore_drain_late_requests_total`. Do not retry such a request unless it is safe to repeat: a retried create-only put answers 412 whether its first try was stored or another writer stored the key. Only a connection that was idle for the whole settle can meet this, and the server cannot remove it; a count that is large after every restart means clients that send on a connection without looking at `Connection: close`. The OCI read front drains the same way, and counts its own connections in `tierstore_oci_drain_late_requests_total`. The counts reach their last value after the settle, when `/metricz` has closed with the S3 listener, so the process says them on stderr as it exits, a line for each front that counted one: `tierstore: <N> connections sent a request too late in the drain of the S3 front` (or `of the OCI front`). Each counts connections, once each, not requests.

## At drain deadline

`tierstore: <N> requests were cut at the drain deadline` on stderr means clients were still sending or reading when `--drain-deadline` ran out. Raise it, with `TimeoutStopSec=` in the unit.
