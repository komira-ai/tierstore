# tierstore-blackbox

The black-box test of a released `tierstore`: the binary of the package under test, started with a
node file of the farm's shape and driven only from outside, as the job agent drives its mount
provider. None of the node's runtime is linked in (the driver links the tree-list format, to write
trees, and the provider's messages): it sees the node's provider socket, its mount, the `/metricz`
of its S3 front, its process (`/proc/<pid>/status`, signals) and the files of its cache directory.

Two programs:

- `tierstore-blackbox`, the driver. It runs the cases below, each against a node of its own in
  `<scratch>/<case>`, prints `PASS <case>: ...` or `FAIL <case>: ...` for each, and exits 0 when
  every case passed, 1 when one failed, 2 when its flags are refused.
- `fake-s3`, the node's upstream: the loopback S3 service of `objstore_s3::s3::fake` as a program
  of its own, with a control socket that stalls or fails every request under a key prefix
  (`src/fake.rs` has its commands). It serves on `--listen <address>`, or a free loopback port. The node is pointed at it by its file (`[cold_store] endpoint`).

## The node

A build node of the farm's shape (`src/config.rs`): a local cache over an S3 upstream, the
`mount-provider` interface and the `fs` interface (the FUSE mount, served in the node's process),
the S3 front on a free loopback port. It has the rig's own paths, user and group, and no name,
address or key of the farm. Every key of `[trees]` is the node's default, but `max_placements` in
`t2-placements` (set to the value the case holds, 1,024 by default, the node's own default too) and
in `t3-stall` (above the Prepares that arrive, so that every one is placed).

The mount is made with `allow_other`, which tierstore always asks for and which a user other than
root gets only by `/etc/fuse.conf`: the driver runs as root, in a container with `/dev/fuse` and
`CAP_SYS_ADMIN`. `start-without-fs` alone has no `fs` interface in its node file, and so no mount
provider either (tierstore refuses a file with a provider and no `fs` interface), and needs
neither: it runs as an ordinary user with no capabilities and no network.

What each case needs of the host it runs on:

| case | mount (`/dev/fuse`, and `CAP_SYS_ADMIN` or a setuid `fusermount3` with `user_allow_other`) | more |
|---|---|---|
| `start-without-fs` | no | nothing: an ordinary user, the package unpacked |
| `t1-cycles`, `t2-placements` | yes | - |
| `t3-stall`, `upstream-errors` | yes | the node's pid, for its resident memory |
| `restart` | yes | to kill the node and start it again on the same files |
| `disk-full` | yes | a small file system of its own for the cache (`--small-disk`) |

## The cases

A tree is written into the upstream as a CAS service writes one: its files in one pack under
`cas/p/`, its version 2 tree list under `cas/l/<id>` and the L2 sidecar under `cas/l2/<id>`
(`src/tree.rs`). The agent's source is `(blackbox-agent, js.<case>-<n>, 0)`, no attempt, as the
agent on main names it. A read is every file's bytes against their SHA-256, the link and the empty
file. Where a case asks again as the agent does, a Mount answered "unavailable" is followed, a second
later, by a Prepare and a Mount again, for at most the agent's budget for a tree (120 s,
`--mount-provider-wait`'s default).

| case | what it does | passes when | the failure it is the test of |
|---|---|---|---|
| `start-without-fs` | a node with no `fs` interface and no mount provider, started first: its `/readyz`, a read through its S3 front of a pack in the upstream, then a Prepare | `/readyz` answers 200; the front serves the pack's bytes from the upstream; the Prepare has no answer | a package that cannot start, or serve its cache, as an ordinary user without its mount |
| `t1-cycles` (T1) | 2,100 cycles, one after another, of Prepare, Mount, read, Release of a tree of 16 files of 4 KiB, at the node's defaults | every step of every cycle succeeds at its first ask; no placement is left 60 s after the last Release | placements that a Release does not free: the cycle after `trees.max_placements` (1,025) is refused |
| `t2-placements` (T2) | 1,024 sources prepared, mounted and read (held, as running jobs hold their trees), then a burst of 64 more Prepares, a Mount of each, then the Releases of the held 1,024 | each Mount of the burst is "unavailable" for too many placements (none succeeds, none is a refusal that fails the job); within 60 s of the Releases every source of the burst has its tree; no placement is left 60 s after | a Release that leaves the place counted while the mount holds the tree (komira-ai/tierstore#619) |
| `t3-stall` (T3) | the reads of every pack stalled for 300 s while 3,000 Prepares of new sources of one tree of 16,384 files arrive evenly over the first 60 s; the lists and sidecars are served, so the node places each tree and queues its reads | the resident memory, read every 5 s, is under its idle size plus 1 GiB at every sample and grows by at most 32 MiB over the stall's last 60 s; the stall held requests of the node; the queue gauges (`tierstore_provider_prefetch_queued_bytes`, `tierstore_cache_prefetch_queue_depth`, `tierstore_provider_preparing`) are 0 within 120 s of the stall's end | no bound on the heap of the reads queued while the upstream stalls (komira-ai/tierstore#613) |
| `restart` | the node killed (SIGKILL) right after a Prepare's answer, and again after a second source's placement is on disk, each time started again on the same files | each source, asked again as the agent asks, gets its tree, read as the tree, within the agent's budget | a node that forgets or refuses a source it was asked to prepare before a crash |
| `upstream-errors` | every request to the upstream answered 503 for 60 s, while 8 sources ask for a tree not yet in the cache once a second | every answer meanwhile is "unavailable" with a reason (the start waits): never a tree, never a refusal that fails the job; the resident memory stays under idle plus 1 GiB; once the upstream serves, every source gets its tree | an upstream failure turned into a failed job, or into memory that grows while the starts wait |
| `disk-full` | the cache on a file system of its own (`--small-disk`), filled to its last byte by a ballast file; 5 cycles of a tree not in the cache; the ballast removed; one more source | each answer is a tree read as the tree, a read that fails as the job sees it (an error of the mount, never other bytes), or a refusal with its reason; each Release succeeds; the node lives through it; once the disk has room, the new source gets its tree | a full disk that crashes the node, answers nothing, or serves wrong bytes |

Every case also fails when the node is not running at its end, and when it does not end as asked
(status 0, or by the SIGTERM) when its rig stops it. Each call on the provider's socket waits as
long as the agent's does (Prepare 10 s, Mount 30 s, Release 5 s) and is "no answer" after that; a
node gets 60 s for its start line. A tree that comes after its case's deadline fails the case as
one that does not come. `t3-stall` reads the three queue gauges, which a release without them
fails. Each wait is a bound on a condition
the case looks for (once a second, or every 5 s for the memory), never a pause whose length is the
test.

The numbers are the plan's; the flags `--cycles`, `--max-placements`, `--burst`, `--prepares`,
`--arrival-secs`, `--stall-secs`, `--stall-files`, `--memory-bound-mib`, `--errors-secs` and
`--sample-secs` change them, for a quick run by hand.

## Running it

The workflow `blackbox.yml` runs every case, by hand or from another workflow, against the
`.deb` of a published release: it builds the two programs, downloads the release's `.deb` and
`SHA256SUMS`, checks the one against the other, unpacks the package, and runs the driver in a
container on a hosted runner, with `/dev/fuse`, `CAP_SYS_ADMIN` and a tmpfs of 256 MiB as the small
disk:

```sh
gh workflow run blackbox.yml -f tag=v0.3.206
```

By hand, the same container:

```sh
cargo build --locked --release -p tierstore-blackbox
dpkg-deb --extract tierstore_<version>_amd64.deb pkg
docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
    --tmpfs /small:size=256m --volume "$PWD/pkg:/pkg:ro" \
    --volume "$PWD/target/release:/bb:ro" --volume "$PWD/scratch:/scratch" \
    ubuntu:24.04 /bb/tierstore-blackbox --tierstore /pkg/usr/libexec/tierstore/tierstore \
    --fake-s3 /bb/fake-s3 --scratch /scratch --small-disk /small
```

Each case's directory keeps the node file and the logs of both programs (`logs/tierstore.out`,
`.err`, `logs/fake-s3.out`, `.err`).

## In Buck2

The two programs are the package's `:bin` (`//crates/tierstore-blackbox:bin`, buildrules' target of
every binary of a member), and the tests of the driver and of `fake-s3` themselves (against the
model of a node in `src/cases/model.rs`, and against the crate's own programs) are its `:test`, as
for every member.

The released package is tested by `//crates/tierstore-blackbox:released`, a `blackbox_test` of
buildrules (`rules/blackbox`): `buck2 test` of it, on the farm, unpacks the pinned `.deb` of a
release onto the farm's base image and runs `:driver` with that root as its one argument.
`:driver` (`blackbox.bzl`) is the driver and `fake-s3` copied into one directory; given only a
root, the driver takes the binary at `<root>/usr/libexec/tierstore/tierstore`, the `fake-s3`
beside itself and the scratch directory `<root>/var/tmp/tierstore-blackbox`, and runs the cases
that need no mount. A farm job is an ordinary user with no capabilities, no `/dev/fuse` and no
network but loopback, so that is `start-without-fs`; the other cases need what the table above
says, and run by `blackbox.yml` until a farm job can mount. A release passes the package it is
about to publish in place of the pin:

```sh
buck2 test //crates/tierstore-blackbox:released \
    -c blackbox.tierstore_url=<url> -c blackbox.tierstore_sha256=<sha256> -c blackbox.tierstore_size=<bytes>
```
