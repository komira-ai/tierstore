# Pluggable interfaces and stores

Status: proposal, for review. No code changes come with this document. It extends section 14(a) and 14(b) of [the tierstore design](tierstore-design.md). Everything it calls "built" was read in the code at the time of writing; everything else is the proposal.

The direction (CEO, 2026-11-05): an abstraction for the interface, so that GCS or a file system can be slotted in later; the implementation decides whether tierstore hosts a server and what the API is; hot and cold may run two different APIs.

Earlier decisions this builds on: the S3 front matches S3 behaviour exactly; configuration is `--config` with an `[[interface]]` list; a chained local cache's upstream is another store behind the same store interface; tierstore documents what a store must do and never probes it (decision 9 of the design); every tunable is a config key with a default.

![Two independent seams: the Interface on the client side and the Backend on the store side, with the proxy core between them](diagrams/interfaces-and-stores.svg)

## 1. Problem

tierstore is not a pass-through proxy. Every request is answered by tierstore itself and turned into its own calls on the tiers (entry, write-once check, landing, copy, delete). That is why the client's API and the store's API are independent, and the code already shows it on one side only.

**Store side, already pluggable.** The `Backend` trait (`backend.rs`) is the seam. `S3Backend`, `DirBackend` and `MemoryBackend` implement it, and `CachedStore` and the wrappers (bounded, faulty, tiered) are built on it. Each tier picks its own `kind` (`config_tier.rs`), so a hot `dir` tier over a cold `s3` tier already works (`config_combined.rs`).

**Front side, hard-wired.** There is no trait. The S3 front and the OCI front are separate paths in `server/` (`mod.rs` `serve` and `serve_all`, `listen.rs`, `oci/`). The S3 front also carries `/healthz`, `/readyz`, `/usagez` and `/metricz` (`mod.rs`), so an `s3` interface is required (`config_interface.rs`), and the names `usagez` and `metricz` are reserved bucket names. `type = "fs"` is reserved in the config and refused. Each new front today means a new copy of the listen, drain, hand-off and metrics wiring.

**What we want.**

- A GCS JSON API front, for clients that speak GCS.
- A filesystem front: a FUSE mount, no port, for tools that want paths.
- OCI stays as it is.
- GCS (native JSON API) and filesystem as store kinds per tier, with GCS's generation preconditions as the conditional-write mechanism.

## 2. The front abstraction: `Interface`

Two implementations exist today (s3, oci), and three more are planned (gcs, fs, ops), so the trait is not a speculative single-implementation trait (R9). The sketch below is the shape, not final signatures.

```rust
trait Interface: Send + Sync {
    /// What the config said it is: "s3", "oci", "gcs", "fs", "ops".
    fn kind(&self) -> &'static str;
    /// How it is reached: sockets to bind, a mount point, or nothing.
    fn reach(&self) -> Reach;                  // Sockets(Vec<SocketAddr>) | Mount(PathBuf) | None
    /// Serve until the drain; the sockets may be inherited from a predecessor.
    async fn serve(self: Arc<Self>, core: Arc<Proxy>, channel: Channel, drain: Drain) -> Drained;
}
```

The core (`Proxy`) knows no wire format, no auth and no store API. What an implementation owns:

| Concern | Owned by the implementation | Today |
|---|---|---|
| Reach | Listening sockets (`s3`, `gcs`, `oci`, `ops`), a mount (`fs`), or nothing | `listen.rs`, per front |
| Wire API | Request parsing, headers, status codes, body framing | `server/request.rs`, `reply.rs`, `oci/` |
| Key rules | Which names the API accepts and how a wire name becomes a store key and back | S3's charset and length in the request path; OCI's `Digest` and `names.rs` |
| Listing format | Rendering the shared walk: S3 `ListObjectsV2` XML, GCS `objects.list` JSON, a directory read | `xml.rs`; the walk is the proxy's `list_page` use |
| Auth | Who may do what, and how that is checked | `keys.rs`, `scoped.rs`, `presign.rs`, `oci/auth.rs` |
| Error mapping | The proxy's `Error` into the API's error body and status | per front |
| Drain and hand-off | Taking part in the drain, and in the socket hand-off of the update hook | `drain.rs`, `handoff/` |

What stays shared and is not repeated per front: reserved-prefix filtering (decision 10 of the design), write-once, the entry, delete waiting, streaming limits, the listening-socket and drain wiring, and the listing walk.

**The listing walk.** Delimiter folding, paging and continuation are the same arithmetic in every API, so they move to one store-neutral walk (`server/listing.rs`, proposed in PR #161). It yields an ordered stream of entries and folded prefixes over the union of both tiers, from a `start_after` key. A front passes its delimiter rule and page size, and gets a page and a *cursor*. The token a client sees is the front's own encoding of the cursor (S3 `NextContinuationToken`, GCS `nextPageToken`, an fs directory offset). The cursor holds the last key returned and nothing else, so no server state is kept and a token works across processes, restarts and the hand-off.

**Ops split out.** `/healthz`, `/readyz`, `/usagez` and `/metricz` move to their own small `ops` interface, so no front type is mandatory: a fs-only node can still be health-checked. To be additive (B8), nothing is removed in the same release:

- `s3` gets a param `ops` (default `true`), which keeps the endpoints on the S3 sockets exactly as today, including the reserved bucket names.
- A file may add `type = "ops"` and set `ops = false` on its `s3` interface; the bucket names are then free.
- The rule "at least one `s3` interface" becomes "at least one interface of any type".

**Config.** Each entry is `type` plus `params`, as now. `type` is the only thing that selects an implementation; a type that is not built is refused with its name, as `fs` is today.

```toml
[[interface]]
type   = "s3"
params = { port = 9300 }                       # ops = true keeps /healthz etc. here

[[interface]]
type   = "gcs"                                  # planned
params = { port = 9301, token_file = "/etc/tierstore/gcs-tokens.json" }

[[interface]]
type   = "oci"
params = { addresses = ["127.0.0.1:5000"], prefix = "oci/" }

[[interface]]
type   = "fs"                                   # planned; Linux first
params = { mount = "/mnt/tierstore", read_only = true, key_prefix = "builds/" }

[[interface]]
type   = "ops"                                  # planned
params = { addresses = ["127.0.0.1:9390"] }
```

`fs` is the case with no listening socket: it takes `mount` and no `port` or `addresses`, and the existing rule "give port or addresses, not both, not neither" becomes a rule of the implementation (each type states which reach params it takes). `key_prefix` is a tunable with the default empty (all keys).

## 3. The store abstraction: `Backend`

The `Backend` trait stays the seam. Nothing about it changes for the new kinds except where this section says so.

**What a new store kind implements.** `put` (create-only), `put_if_match`, `head` and `open_read`, `get_stream` with ranges, `delete`, `list_page` (ordered, honours `start_after`), `put_stream` and `put_stream_if_match`; optionally multipart and `delete_if_match`. The conformance tests in `crates/tierstore/tests/tierstore/conformance.rs` are the acceptance test of a kind. The requirements are those of section 9 of the design, stated for the new kinds and documented, not probed:

| Requirement | S3 | GCS (planned) | dir (built) |
|---|---|---|---|
| Create-only put | `If-None-Match: *` | `ifGenerationMatch=0` | create-only under the directory lock |
| Replace only if the tag matches | `If-Match: <etag>` | `ifGenerationMatch=<generation>` | tag compare under the lock |
| Tag | ETag | generation (decimal string) | SHA-256 of the body |
| Conditional delete (optional fence) | `If-Match` | `ifGenerationMatch` on delete | under the lock |
| Missing key on delete | 204 | 404, mapped to success | success |
| Ranged read | `Range` | `Range` on `alt=media` | file read |
| Listing | strongly ordered, paged | strongly consistent, ordered, paged | ordered, paged |
| Large object | multipart, create-only completion | see below | file |

**GCS notes.**

- The generation changes on every rewrite, as an ETag does, and the contract only needs the tag from HEAD to equal the tag `put_if_match` returned and to be compared by the same store. Tags are never compared across tiers, so GCS generations and S3 ETags in one tierstore need no mapping.
- GCS answers a failed precondition with 412. The kind maps it as `S3Backend` does: `AlreadyExists` on a create-only put, `PreconditionFailed` on `put_if_match`. 429 and 5xx are `Unavailable`; 408 and transport failures too.
- Large objects: GCS has no S3 multipart. `put_stream` uses a resumable upload that starts with `ifGenerationMatch=0` (create-only, committed when the last byte arrives). The multipart calls of `Backend` are optional (`not_supported` today); the cold tier needs a create-only completion for S3-front multipart uploads, so the GCS kind implements them as parts stored under a reserved temporary prefix and a `compose` with `ifGenerationMatch=0` as the completion, then deletes the parts. Compose takes at most 32 sources per call, so a large upload composes in a chain; this is the one part of the GCS kind that needs its own design check in PR (d), before it is built. Until then a GCS tier says `Unsupported` for multipart, which the S3 front already renders.
- Credentials: provider-standard (`GOOGLE_*` or a `credentials_file` key), allowed by rule S2. `endpoint` is a config key so the tests run against a fake server (T12).

**Mixing kinds.** Each tier picks its `kind` independently: hot `s3` with cold `gcs`; hot `dir` with cold `s3`; hot `gcs` with cold `gcs`. The hot tier needs both conditional writes (the entry and the lease live there); the cold tier needs the create-only put and completion. A kind that cannot give create-only is not usable as a tier, as the design already says. A filesystem store is the existing `dir` kind (its atomicity from the directory lock); a shared network file system under it is the operator's to qualify, as any store is.

```toml
[hot]
kind   = "s3"
bucket = "tier-hot"

[cold]
kind        = "gcs"
bucket      = "tier-cold"
credentials_file = "/etc/tierstore/gcs-sa.json"
```

**Chaining.** local_cache's upstream is another `Backend`. A hot+cold proxy in the same process, or in another process reached over the S3 front, is that `Backend`. A chain never needs a different front: the next process is a client of the front type it chooses (s3 today, gcs later through the `gcs` store kind pointed at a tierstore `gcs` front, which also tests both together).

## 4. How the operations look through a front

The core's steps are in section 5 of the design. These diagrams show the split of work between a front and the core, with the stores chosen to differ (hot = s3, cold = gcs), to show that neither side knows the other.

### Put

![A put through the S3 front, hot = s3, cold = gcs](diagrams/seq-iface-put.svg)

The front verifies the signature, applies its key rules, and calls `Proxy::put_stream`. The core's steps are the design's: entry, check both tiers, land in hot, acknowledge with the `If-Match` write. Each store call uses that store's own conditional write. Later the leader copies to cold with `ifGenerationMatch=0`.

### Get

![A get through the S3 front, hot = s3, cold = gcs](diagrams/seq-iface-get.svg)

One `open_read`, hot first, cold on a miss. The front renders the headers from the meta; the ETag it shows is the tag of the tier that answered, as today.

### Delete

![A delete through the S3 front, hot = s3, cold = gcs](diagrams/seq-iface-delete.svg)

The core takes or amends the entry, deletes cold then hot, and finishes or waits for the leader. The front renders 204 or 503.

### A filesystem front read

![A read through the filesystem front](diagrams/seq-iface-fs-read.svg)

The kernel's FUSE request reaches the `fs` front on the `/dev/fuse` descriptor. The front maps the path to a key and makes the same `Proxy::open_read` an S3 front makes; the bytes go back as a FUSE reply. READDIR is the listing walk with delimiter `/`. The first version is read-only; writes, later, are create-only files that become an object when the file is closed whole (no overwrite, no rename, as design 14(b) says).

## 5. Edge cases and the mechanism for each

**Keys.** The proxy's key space is a UTF-8 string, as in S3 (up to 1024 bytes). The store key is what the core passes to a `Backend`.

| Case | Mechanism |
|---|---|
| A key valid for the S3 front but not representable in a store. GCS names are UTF-8 up to 1024 bytes but may not contain CR or LF, may not be `.` or `..`, and may not start with `.well-known/acme-challenge/`. A `dir` store has path limits. | Each store kind declares its key rules as data (`KeyRules`). At start-up, from the configured kinds and without touching a store, the core computes the rules the tiers share, and a front's put or multipart start is refused with the front's own "invalid name" error (S3 `InvalidArgument` / `KeyTooLongError`, GCS 400) before any entry is taken. Nothing is half-written. |
| A key present in a store that a front cannot name (written through another front, or before a rule existed). | Reads of it through that front answer the front's invalid-name or not-found error; listings through that front skip it and count it in a metric (`tierstore_interface_hidden_keys{interface=...}`). It stays reachable through any front whose rules allow it. It is never renamed or deleted by the front that hides it. |
| A key written via S3 appears via GCS. | Same key, byte for byte: the S3 key and the GCS object name are the same store key. Both fronts are UTF-8 names in binary order. A name that GCS cannot take (CR or LF) is refused on put by the shared rules when a `gcs` interface is configured and hidden in the GCS listing when it was written earlier. |
| A key written via S3 appears via the file system. A key is a path only if it has no empty segment (`a//b`), no `.` or `..` segment, no trailing `/` and no NUL; and `a` and `a/b` cannot both exist as file and directory. | The `fs` front shows such keys by the path rule, hides the rest (counted), and when `a` and `a/b` both exist, the directory wins and `a` is hidden (counted). `key_prefix` can scope a mount to a part of the key space where this cannot happen. Case-insensitive or normalizing hosts (macOS) can alias two keys; the macOS mount is not supported until that is designed. |
| An fs path longer than the OS allows, or a segment over 255 bytes. | Hidden and counted, like any unnameable key. A put through `fs` of such a path is `ENAMETOOLONG`. |
| Reserved prefixes (`_entries/`, `_journal/`, `_tiering/`). | Filtered once in the core (decision 10), so a new front cannot forget it. |

**Listing consistency across fronts.** All fronts list through the same walk over the union of both tiers, in the same order (byte order of the UTF-8 key), so the same keys appear in the same order in S3, GCS and the file system. What differs is the rendering: S3 folds on any delimiter, GCS `delimiter` and `prefixes`, and the file system always folds on `/`. A listing can lag a recent write on a store that allows it (the trait allows it, and the reconciler loops); GCS lists strongly consistently. A key hidden from a front by the rules above is the only difference between two fronts' listings, and it is counted.

**Conditional-write semantics differing per store.** The core uses only the three operations of the `Backend` contract: create-only, replace-if-tag, delete (optionally if-tag). Each kind maps them to its store (the table in section 3). A kind is only accepted if its conformance run passes, so an ambiguity (a store whose create-only is not atomic) is the operator's choice of store, documented, not probed. The fronts expose only what tierstore guarantees to clients: objects are write-once, so `ifGenerationMatch=0` and `If-None-Match: *` from a client both mean "create" and the refusal of an existing key is the 412 a client already gets from the S3 front; a client precondition on a generation that tierstore cannot honour (`ifGenerationMatch=N`, N > 0, on a write) gets 412. The GCS front reports `generation` as a value tierstore derives from the object's tag and write time and says so in its documentation; `metageneration` is always 1.

**Auth per front.** Each front authenticates in its own terms, with keys read from files, never from the environment (S2). `s3`: SigV4 keys, as now. `oci`: as now. `gcs`: bearer tokens from `token_file`, with full and read-only tokens like the S3 keys; OAuth2 is a token an operator mints elsewhere, tierstore only compares. `fs`: none of tierstore's; the mount's owner and mode, `allow_other` off by default, and `read_only` a config key. A request is authorized by the front it came in on and never by the store: the stores see tierstore's own credentials. Presigned URLs stay an S3 feature.

**A front with no port in the hand-off.** The hand-off (`handoff/`) passes `<front> <address>` lines with the sockets over `SCM_RIGHTS`. A FUSE mount has no socket, but it has a descriptor: the `/dev/fuse` connection. The `fs` front offers it the same way, with a line `fs <mount path>` and the descriptor; the successor checks that the path is its own, answers `ack`, and serves the same connection, so the kernel sees no unmount and no request fails. The old instance then stops reading and finishes the requests in flight. A running instance that has no `fs` front never offers such a line, and a successor with none never asks for one, so a mixed pair of the old and new release hands off exactly as before (B8). Until this is built (PR f), an `fs` front runs only where the update hook restarts (a gap, documented) and is refused with the socket hand-off configured. `ops` and the others hand off their sockets as they do today. The macOS service model has no FUSE in the base system, so `fs` is Linux only at first.

**Drain.** `Reach::None` and `Reach::Mount` fronts take part in the drain like the others: on drain they stop taking new requests, finish those in flight and release the mount. The `/readyz` answer is the ops interface's and covers all fronts.

## 6. Migration plan

Each step is one pull request that can land alone, interfaces before the code that uses them (P7). Coverage stays at the floor of each crate (T1); every new store and front gets a fake peer, not the network (T12).

| | Change | Tests to add |
|---|---|---|
| (a) | `Interface` trait and `Reach`; `s3` and `oci` moved behind it; `serve` / `serve_all` iterate over the list. Pure refactor: no wire, config or metric change. | The existing front tests pass unchanged. New: the list of interfaces from a config is served by one loop (two types together, with a port of 0); a front that fails to bind fails the start naming the interface; the drain ends every front; the socket hand-off offers the same lines as before. |
| (b) | `ops` interface split out; `ops` param on `s3` (default true); "at least one interface of any type". | `/healthz` and the others answer on the `ops` address, and on the S3 address when `ops = true`; `ops = false` frees the bucket names `usagez` and `metricz`; a file with no `s3` and an `ops` starts; an old file still starts as it did; `/readyz` goes 503 on drain on each. |
| (c) | Key-rules seam per front: `KeyRules` for s3 and oci, `KeyRules` on each store kind (s3, dir, memory), the shared rules computed at start, `hidden_keys` metric. | A put of a key a configured store cannot hold is refused before an entry is taken, with the exact error body of the front; a key hidden from a front is skipped in its listing and counted; same store key reached through the s3 and oci rules is the same key; property test on random UTF-8 keys: valid for both rule sets implies round trip. |
| (d) | `gcs` store kind (JSON API, generations, resumable upload, compose for multipart) with the conformance tests and a fake GCS server in the test kit. Tier config keys `bucket`, `credentials_file`, `endpoint`. | The full conformance suite on the fake; create-only twice gives `AlreadyExists`; `put_if_match` with a stale generation gives `PreconditionFailed`; 404 on delete is success; 429 and 5xx are `Unavailable`; a body error mid-upload leaves no object; hot = s3 and cold = gcs end to end through the S3 front (put, get, delete, eviction, copy). The compose design check is part of this PR's description. |
| (e) | `gcs` front: JSON API object get, insert (media and resumable), delete, list (`prefix`, `delimiter`, `pageToken`, `maxResults`), `token_file` auth, error bodies, generation preconditions. Uses the shared listing walk. | Each call against the exact JSON a GCS client library sends and expects (golden bodies); the same object through the s3 and gcs fronts; token with read-only right is refused a write with the GCS error body; listing paging and prefixes equal the S3 front's on the same data; hidden keys. |
| (f) | `fs` front (Linux FUSE, read-only first): path rules, LOOKUP / READDIR / READ, `/dev/fuse` descriptor in the hand-off, `mount`, `read_only`, `key_prefix`; `fs` accepted in config. | Path and key mapping round trip; unnameable keys hidden and counted; READDIR paging over a cursor equal to the S3 listing; ENOENT / EIO mapping; hand-off of the descriptor between two instances with a read in flight; a mixed pair (one without `fs`) hands off as before; refusal on macOS and with write mode until built. A test that needs a real mount runs where FUSE is available and says why it skips (T12). |

## 7. Future work

- Writes through the `fs` front (create-only files closed whole), and an NFS gateway.
- An Azure Blob front and store, as further `Interface` and `Backend` implementations.
- Per-interface rate and connection limits shared by all types (today only `oci` has them).
- A store kind for a shared POSIX file system that proves its own create-only (as the `dir` lock does on one host).

## 8. Open questions

1. Is the GCS `generation` shown by the front derived from the tag (proposed) or should the GCS front be allowed only over GCS stores, so it is the store's own generation? The first is simpler to run; the second is truer to the API.
2. Compose-based multipart on GCS (section 3): accept the chain and temporary parts, or leave multipart `Unsupported` on a GCS cold tier and say so in the S3 front's documentation?
3. Is a macOS `fs` front wanted at all, given FUSE there is a third-party install?
