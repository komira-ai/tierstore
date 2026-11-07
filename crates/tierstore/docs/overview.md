# What tierstore is

tierstore is an S3-compatible object store server that puts a small, fast tier in front of a large, slow one. Clients speak S3 to one bucket. New objects are written to the hot tier, copied to the cold tier in the background, and removed from the hot tier when it fills. It is a binary, `tierstore`, for Linux (x86_64, arm64) and macOS (Apple silicon). hostkeeper packages exist and update it through `tierstore update-hook`; there is no Homebrew formula, apt package or container image yet.

This guide is for the person who installs and runs it. It describes what is on the main branch of the source today. A piece that is planned and not in the code is described only under a heading titled "Not in a release yet", so you can tell the two apart.

> Every flag and key these pages describe is in the release built from main.

## The shape of a deployment

One `tierstore` process runs three things over the same pair of tiers:

- a proxy, which answers reads and takes writes;
- the S3 front of that proxy, which checks signatures and serves one bucket, path-style (and virtual-hosted-style for the domains of `server.domains`), over plain HTTP, or over HTTPS with `server.tls_cert_file` and `server.tls_key_file`;
- a reconciler, which copies written objects to the cold tier, applies deletes, evicts from the hot tier and publishes the hot tier's usage. Only one reconciler works at a time: it holds a lease in the hot tier, so any number of processes may run over the same tiers.

A tier is `memory` (lost when the process stops, for tests), `dir:<path>` (a directory of the local file system) or `s3:<bucket>` (a bucket of an S3-compatible store).

The intended v1 layout is a hot tier on SSD in front of a cold tier on spinning disks:

```text
client --S3--> tierstore --> hot tier  (dir: on SSD)
                         \-> cold tier (dir: on HDD, or an S3 bucket)
```

A write goes to the hot tier and is acknowledged once the entry of its key (a small object in the hot tier, `_entries/<digest of the key>`) says it landed. The reconciler later copies it to the cold tier. A read is served from the hot tier, else from the cold tier, and a read that found an object only in the cold tier asks the reconciler, on that entry, to copy it back into the hot tier. So the hot tier is also a cache of what was used lately. Writes are taken only once the store has been activated (see [operations](operations.md#rolling-out-the-entry-protocol)); reads never wait for it. When the hot tier is full, new objects are written straight to the cold tier.

## What it guarantees and what it does not

- On a store on the entry format (`active`) a put is create-only: a key that exists in either tier is refused with 412 `PreconditionFailed`. On a store on the marker format (`active-marks`) a plain put replaces the key, `If-None-Match: *` still creates only, and `If-Match` replaces only the object it names. Which one applies is the format phase of the hot tier ([operations](operations.md#the-format-object-and-the-pause-of-writes)).
- A put is acknowledged only after its entry says it landed in the hot tier. Losing the hot tier loses objects that were not yet copied to the cold tier.
- It does not scrub a storage node's objects: nothing re-reads stored bytes to find rot. See [limits](limits.md).

## Where to go next

- [Install](install.md): the tarball, the account, the directories and the unit.
- [Keys and roles](keys.md): which key may do what, and the file rules.
- [Configuration](configuration.md): every key of the file.
- [Operations](operations.md): probes, usage, drain, eviction, a full disk, metrics.
- [Troubleshooting](troubleshooting.md): the messages the process prints and what to do.
- [Limits](limits.md).

## Not in a release yet

These are planned for the v1 shape and are not in the code on main. Nothing here can be configured today.

- **Client retries on 503.** Clients retrying a 503 with jittered waits, and the server waiting briefly for a free put slot before it answers 503.
