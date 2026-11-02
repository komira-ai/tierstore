# The OCI read front

With `--oci-listen`, the process also serves the objects of the store as a read-only container
registry, over HTTPS or over plain HTTP (see Two modes), so that podman (and the
other clients of the OCI distribution API) pulls an image by digest from the same hot and cold tiers the S3 front writes to. It reads through the
proxy in the process: no S3 credential and no hop through the S3 listener. It is off unless
`--oci-listen` is given, and then nothing else about the process changes.

With `--config` the front is the `[oci]` table of the file and the `--oci-*` flags are refused
(see The configuration file, which lists the key of each flag below). `--print-config` writes
the front's flags as that table. A build node's file takes the same `[oci]` table (it has no flags):
the front then reads through the node's local cache, not a proxy (see The registry front).

| flag | what |
|---|---|
| `--oci-listen ADDR` | where the registry front listens. With `--oci-tls-cert` and `--oci-tls-key` it is HTTPS; with neither it is plain HTTP, at any address; off loopback (127/8 and `::1` are loopback; `0.0.0.0` and `::` are not) the start prints one warning line, and a certificate and key are advised. On main, not in v0.0.41: give the flag again for each further address; one front, one limit on connections and one drain serve them all, and an address given twice is refused (exit 2); the start line shows them as `oci-listen=<a>,<b> (https)` or `(http)`, and a plain front prints one warning line for each address off loopback |
| `--oci-tls-cert PATH`, `--oci-tls-key PATH` | both or neither (one without the other is refused at start, exit 2). The certificate chain (leaf first) and the private key, PEM. Clients dial an IP address, so the leaf must carry an IP subjectAltName for `--oci-listen` (for each of them, when it is given more than once; any IP subjectAltName, if the address is `0.0.0.0` or `::`); a certificate without one, one that is expired or not yet valid, a key file any user can read (the "other" mode bits; a group may), or an unreadable file, is refused at start. The files are read once: a new certificate takes a restart, so renew it before it expires |
| `--oci-prefix PREFIX` | the key prefix, ending in `/`, `oci/` if not given. A prefix inside `_journal/` or `_tiering/`, or one that is not plain path segments, is refused at start (exit 2) |
| `--oci-max-connections N` | connections open at once, 64 if not given; the others wait in the kernel's backlog. Memory is bounded by connections times the buffer a connection holds (up to 4 MiB), about 256 MiB at the default |
| `--oci-connection-lifetime D` | the longest a connection lives, 1h if not given; a request being answered is finished first |

A client has 10 seconds to finish the TLS handshake and then the S3 front's header timeout (a
minute) to send the head of a request, which is also how long an idle connection may wait for its
next request. A connection holds one of the `--oci-max-connections` slots from accept to close and
there is no limit per client address, so a host that opens that many connections and says nothing
can keep the others waiting: the host's firewall rules are the control for that. A client that takes no byte of an answer for `--body-idle-timeout`
is cut. A connection that reaches its lifetime finishes the answer it is sending for at most
`--drain-deadline`, and is cut then. On the drain the front goes on serving through `--drain-lb-wait`, takes no new connection,
lets the requests being answered finish, and closes what is left at `--drain-deadline`. The start
line on stdout gets ` oci-listen=<address> (https)` or ` (http)` after `listen=<address>`, and `/metricz`
has `tierstore_oci_front_info{mode="https"}` or `{mode="http"}`. In plain mode there is no handshake,
so the handshake time does not apply; the other limits are the same.

## Two modes

- **HTTPS**: with `--oci-tls-cert` and `--oci-tls-key`, at any address.
- **Plain HTTP**: with neither, at any address. A node's own container engine can pull from the
  front of its own tierstore over loopback, so the pull is a connection from the host to itself and
  the traffic never leaves it. A connection to any address beyond loopback can be read and changed on
  its way: the process starts, prints one warning line on stderr, and a certificate and key are
  advised there. Loopback is reachable by every local process, and by containers that
  share the host's network namespace; the front is anonymous in either mode.

A container engine that pulls over plain HTTP has to be told it may:

- podman: mark the registry insecure for that location in `registries.conf`, e.g.

  ```toml
  [[registry]]
  location = "127.0.0.1:9500"
  insecure = true
  ```

  or pull with `--tls-verify=false`. No `certs.d` directory is needed.
- js-oci needs to be allowed plain HTTP to a loopback registry address for such a pull; that is a
  setting of js-oci, not of tierstore.

What it serves, and nothing else (any other path is 404 and any method but GET and HEAD is 405
`UNSUPPORTED` with `Allow: GET, HEAD`, both with the OCI error document):

| request | answer |
|---|---|
| `/v2/` | 200, `{}`, `Docker-Distribution-API-Version: registry/2.0` |
| `/v2/<name>/manifests/sha256:<hex>` | the manifest (at most 4 MiB), with `Content-Type` from its `mediaType`, `Content-Length`, `Docker-Content-Digest` and an `ETag` of the digest. A tag is 404 `MANIFEST_UNKNOWN`: tags are not served |
| `/v2/<name>/blobs/sha256:<hex>` | the blob, streamed, `application/octet-stream`, with `Accept-Ranges: bytes`; one `Range` is 206 with `Content-Range`, one past the end 416 |

The `Content-Type` of a manifest is its `mediaType` when that is the OCI image manifest or index or
the Docker schema 2 manifest or manifest list. A manifest with none, or another, is served as an OCI
image manifest if it is `schemaVersion` 2 with a `config` object and a `layers` array, as an OCI
image index if it is `schemaVersion` 2 with a `manifests` array (an index may leave `mediaType`
out), and is 500 `MANIFEST_INVALID` with the reason otherwise.

A malformed digest (not `sha256:` and 64 lowercase hex digits) is 400 `DIGEST_INVALID`, a repository
name that is not valid by the distribution grammar is 400 `NAME_INVALID`, an object that is not in
the store is 404 `BLOB_UNKNOWN` or `MANIFEST_UNKNOWN`, a store that cannot be reached is 503, and
what the store said stays in the log.

## Keys, and one namespace

An object is at `<prefix>blobs/sha256/<64 lowercase hex digits>`, the same for a manifest, an index,
a layer and a config: manifests are blobs. The `<name>` in a URL is checked against the grammar and
then ignored, so there is one content-addressed namespace and any name pulls any digest. A key is
built only from a validated digest, so nothing outside the prefix can be named.

## Integrity

A manifest is read whole and its SHA-256 is checked against its digest before a byte is sent; one
that does not match is 500 `DIGEST_INVALID`, is never served and is counted. The size is asked
before the read, so a manifest replaced by an object of another size between the two can be
answered 500 `DIGEST_INVALID` once; a retry succeeds. A blob is hashed as it is
read, in chunks of 4 MiB, and its last chunk is held back until the hash is right: a blob that does
not match its digest ends the connection before the response is complete, so no client receives a
whole wrong body, and the mismatch is counted. That holds for a read of the whole blob, and for a
`Range` that covers all of it (which is still 206). A blob `HEAD` and a `Range` of part of a blob
report what the store holds and verify nothing; the client verifies the digest of what it
assembles. (A manifest `HEAD` reads and verifies the manifest.) A client that sends several ranges
(as a partial pull of a `zstd:chunked` or `estargz` layer does) is sent the whole blob with 200,
which the specification allows, and falls back to splitting it. `If-Range`, `If-None-Match` and
`Accept` are ignored.

## Anonymous reads

The front never asks for a token: a request with an `Authorization` header is served as one without.
Anyone who can connect can read any digest under the prefix, and nothing else. That is the intent:
the images in it are public (a public registry's images are the first thing put there), and the
network decides who can connect (the host's firewall rules). Do not put an image in
the prefix that is not meant to be read by whoever can reach the port.

## Putting an image in the store

Take the blobs of an OCI layout (the files under `blobs/sha256/` of a layout made with
`skopeo copy ... oci:<dir>` or `podman save --format oci-dir`) and put each, create-only, at
`<prefix>blobs/sha256/<hex>` through the S3 front with the full key: the layers, the config, the
manifest, and the index of a multi-platform image, all by the file's own name, which is its digest.
`index.json` of the layout is not put; the digest of the manifest (or index) to pull is in it, and
is what a client is given. A layer larger than `--max-request-bytes` (256 MiB if not given) needs the
flag raised, or a multipart upload, or a presigned PUT whose key has a `max_object_bytes` of up to
5 GiB (see [Presigned PUT URLs](presigned-and-gc-keys.md); the ceiling prefix must cover `oci/`). An object that is already
there is refused (412), which is a retry that is safe.

Check a put with `HEAD` on the front: the `Docker-Content-Digest` of a manifest is the digest of the
bytes it read, after they were checked.

## Pulling

Over plain HTTP on loopback there is no CA to trust; see Two modes for the client settings.

podman, from a host that trusts the CA of the certificate:

```sh
# the CA that issued --oci-tls-cert, in the directory named like the registry in the reference
# (the address, with :port when the reference has one)
install -D -m 0644 farm-ca.crt /etc/containers/certs.d/192.0.2.10/ca.crt
podman pull 192.0.2.10/library/app@sha256:<manifest digest>
```

The reference is by digest: a tag is refused. The repository name (`library/app`) is any name that
fits the grammar and means nothing to the store. A registry on another port than 443 is written
`192.0.2.10:8443/library/app@sha256:...` and its directory is `certs.d/192.0.2.10:8443`.

A client that allows only `https` registries on port 443 (or the port in the reference), and
registries at a private address only when told to, pulls the same references; allow the address in
that client's own settings.

## Counters

`GET /metricz` on the S3 listener (unsigned, like `/healthz`; a bucket cannot be called `metricz`)
shows, in the Prometheus text format, `tierstore_oci_requests_total{route,outcome}` (routes `ping`,
`manifest`, `blob`, `other`; outcomes `ok`, `partial`, `not_found`, `invalid`, `not_allowed`,
`range_not_satisfiable`, `failed`), `tierstore_oci_bytes_served_total` (bytes of blobs read for
responses), `tierstore_oci_digest_mismatches_total` and `tierstore_oci_front_info{mode="https"|"http"}` (1; the mode the front runs in). They are not on the registry listener,
which serves nothing else. Without `--oci-listen` the path says the front is not enabled.
