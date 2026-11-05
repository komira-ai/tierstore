# Pluggable interfaces and stores

Status: proposal, for review. No code changes come with this document. It extends section 14(a) and 14(b) of [the tierstore design](tierstore-design.md). Everything it calls "built" was read in the code at the time of writing; everything else is the proposal.

The direction (CEO, 2026-11-05): an abstraction for the interface, so that GCS or a file system can be slotted in later; the implementation decides whether tierstore hosts a server and what the API is; hot and cold may run two different APIs.

Second CEO direction (2026-11-05): the API tierstore presents must be the common denominator of everything configured (every tier, the chain upstream, every front). Section 3 builds that abstraction now; GCS itself is not built now.

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
| Capabilities | Declares its own key rules, listing features and needs as data (section 3); enforces and advertises the effective set, never its own wider one |
| Key rules | How a wire name becomes a store key and back (the rules themselves are in the capability set) | S3's charset and length in the request path; OCI's `Digest` and `names.rs` |
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

## 3. Capabilities: the common denominator

What a user sees must be what every part of the configuration can do. If hot is `s3` and cold is `gcs`, a key GCS cannot hold must not be accepted by the S3 front, because the leader could never copy it to cold. So each store kind and each front **declares its capabilities as data**, and tierstore computes the **effective set** once at start-up, from the configuration alone, with no call to any store (decision 9 of the design: no probes).

**The declaration** (a sketch of the shape; `Backend` and `Interface` each get one `fn capabilities(&self) -> Capabilities`, and the declaration of a kind is a constant of the kind):

| Capability | What it says | Intersection rule |
|---|---|---|
| `key` | Encoding (UTF-8), `max_bytes`, forbidden characters, forbidden segments (empty, `.`, `..`), forbidden prefixes, case rules | The union of the restrictions: the lowest `max_bytes`, every forbidden character, segment and prefix |
| `multipart` | `Native { max_parts, min_part_bytes, max_parts_bytes }`, `Composable { max_sources }` (no native parts, parts staged and composed), or `None` | The weakest: `None` if any is `None`; else `Composable` if any is; the lowest limits |
| `conditional` | `create_only`, `if_match`, `delete_if_match` | Each a logical AND. The hot tier must have the first two to be a hot tier (start-up refuses it); `delete_if_match` stays an optional fence |
| `token` | The version token the store has: `Etag`, `Generation`, `Digest`. The core keeps it as one opaque string and never compares tokens across tiers | None: informational; each front renders the opaque token its own way (section 8) |
| `listing` | Delimiters accepted (`Any`, `Slash`, none), URL-encoded keys, `max_page` | A front's own: the walk is shared (section 2), the features are the front's. `max_page` is the lowest |
| `max_object_bytes` | Largest object, or none | The lowest |
| `ranged_read` | Byte ranges are served by the store | AND |

The set is small on purpose: a capability is added when a front or a store needs to say it, not before (R9). Every number in it is the declaration of a kind, a constant of that kind; a tunable (a page size, a part size) stays a config key with a default.

**Who is intersected.** Every tier (`hot`, `cold`), the upstream of a `local_cache` (a `Backend`, so it declares as any kind does; an in-process hot+cold proxy declares its own effective set; a remote tierstore declares what its kind says, `s3`), and every configured front. A front's key rules are part of the intersection too: a key the GCS front cannot name must not be written through the S3 front either, or it would exist in the store and be unreachable by name there. So the set of keys a user can write is the same on every front.

**Checking that each front can be served.** A front also declares what it needs (`needs`): the OCI front needs nothing the others do not; a file-system front needs a key rule with no empty, `.` or `..` segment and no NUL, because a path is made of segments. If the effective set does not meet a front's needs, the start fails and names the front, the capability and the kind that took it away (for example: `interface "fs" needs keys without empty segments, but tier "cold" (kind "gcs") allows them`). An effective `multipart = None` is not a failure: the S3 front is served and answers a multipart call with S3's own `NotImplemented` error.

**Enforcement.** Every front checks a key against the one effective key rule on each write (put, multipart start, and any other call that creates a name) and answers a violation in its own terms, before an entry is taken: the S3 front 400 (`KeyTooLongError` for length, `InvalidArgument` otherwise, with the rule in the message); the GCS front 400 with its own error body; the file system `ENAMETOOLONG` or `EINVAL`. The core checks it again (a front bug cannot write a key the tiers cannot hold), and each `Backend` rejects a key outside its own rules with a new `Error` case for it, so a rule cannot be skipped on any path. Keys outside the rules therefore never come into being, on any front. Reads, heads, deletes and listings do not check the rule, so what is stored stays reachable; only a front that cannot express a name on its wire cannot show it (section 6).

**Advertising.** S3 has no discovery call, so the S3 front advertises the set in the places S3 allows: the message of its 400 names the rule, and its documentation page is generated from the same declaration. Fronts with a discovery surface use it (the GCS bucket resource, later). The process writes the effective set to its log at start and shows it as an info metric (`tierstore_capabilities`), so an operator sees exactly what was computed.

**Keys stored before a stricter tier is added.** tierstore does not scan the stores at start (no probes). A config that adds a stricter tier (a cold `gcs` over an existing `s3` bucket, a lower `max_bytes`) takes effect for writes from then on. A key already in a store that the new set would refuse stays where it is and stays readable and deletable through every front that can express it. What it cannot do is reach the stricter tier: the leader's copy of it to cold is refused by that store's key error, retried with the usual per-item back-off and counted by the existing failed-copy metric, and eviction never removes the only copy, so the object stays in hot. The operator's step is documented, not automated: before adding a stricter tier, list the keys with the tooling of their choice against the rule the log prints, and rename (copy and delete) the few that break it.

**Chain.** The effective set of a process is the set a client of its fronts sees; a process whose proxy is another process's upstream declares exactly that set as its `Backend` capabilities, so the intersection through a chain is the intersection of every link.

## 4. The store abstraction: `Backend`

The `Backend` trait stays the seam. The one addition is `capabilities()` (section 3); a new kind otherwise implements what the trait already asks. GCS is described here to show the seam fits; it is not built now.

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
| Multipart (capability) | `Native` | `Composable` (below) | `None` (a file is written whole) |
| Key rules (capability) | UTF-8, 1024 bytes | UTF-8, 1024 bytes, no CR or LF, not `.` or `..`, no `.well-known/acme-challenge/` prefix | file-name limits of the directory tier |

**GCS notes (later work).**

- The generation changes on every rewrite, as an ETag does, and the contract only needs the tag from HEAD to equal the tag `put_if_match` returned and to be compared by the same store. Tags are never compared across tiers, so GCS generations and S3 ETags in one tierstore need no mapping.
- GCS answers a failed precondition with 412. The kind maps it as `S3Backend` does: `AlreadyExists` on a create-only put, `PreconditionFailed` on `put_if_match`. 429 and 5xx are `Unavailable`; 408 and transport failures too.
- Large objects: GCS has no S3 multipart. `put_stream` uses a resumable upload that starts with `ifGenerationMatch=0`. Its `multipart` capability is `Composable`: the multipart calls of `Backend` are served by staging parts and composing them with `ifGenerationMatch=0` as the completion (section 9, item 1). Until that is built the kind declares `None`, and the S3 front answers `NotImplemented`.
- Credentials: provider-standard (`GOOGLE_*` or a `credentials_file` key), allowed by rule S2. `endpoint` is a config key so the tests run against a fake server (T12).

**Mixing kinds.** Each tier picks its `kind` independently: hot `s3` with cold `gcs`; hot `dir` with cold `s3`; hot `gcs` with cold `gcs` (the last two once those kinds exist). The effective set is the intersection of the two. The hot tier needs both conditional writes (the entry and the lease live there); the cold tier needs the create-only put and completion. A kind that cannot give create-only is not usable as a tier, as the design already says. A filesystem store is the existing `dir` kind (its atomicity from the directory lock); a shared network file system under it is the operator's to qualify, as any store is.

```toml
[hot]
kind   = "s3"
bucket = "tier-hot"

[cold]
kind        = "gcs"
bucket      = "tier-cold"
credentials_file = "/etc/tierstore/gcs-sa.json"
```

**Chaining.** local_cache's upstream is another `Backend`, so it declares capabilities and is intersected (section 3). A hot+cold proxy in the same process, or in another process reached over the S3 front, is that `Backend`. A chain never needs a different front: the next process is a client of the front type it chooses (s3 today; a `gcs` store kind pointed at a tierstore `gcs` front later).

## 5. How the operations look through a front

The core's steps are in section 5 of the design. These diagrams show the split of work between a front and the core, with the stores chosen to differ (hot = s3, cold = gcs, a later kind), to show that neither side knows the other.

### Put

![A put through the S3 front, hot = s3, cold = gcs](diagrams/seq-iface-put.svg)

The front verifies the signature, checks the key against the effective key rule (section 3), and calls `Proxy::put_stream`. The core's steps are the design's: entry, check both tiers, land in hot, acknowledge with the `If-Match` write. Each store call uses that store's own conditional write. Later the leader copies to cold with `ifGenerationMatch=0`.

### Get

![A get through the S3 front, hot = s3, cold = gcs](diagrams/seq-iface-get.svg)

One `open_read`, hot first, cold on a miss. The front renders the headers from the meta; the ETag it shows is the tag of the tier that answered, as today.

### Delete

![A delete through the S3 front, hot = s3, cold = gcs](diagrams/seq-iface-delete.svg)

The core takes or amends the entry, deletes cold then hot, and finishes or waits for the leader. The front renders 204 or 503.

### A filesystem front read

![A read through the filesystem front](diagrams/seq-iface-fs-read.svg)

The kernel's FUSE request reaches the `fs` front on the `/dev/fuse` descriptor. The front maps the path to a key and makes the same `Proxy::open_read` an S3 front makes; the bytes go back as a FUSE reply. READDIR is the listing walk with delimiter `/`. The first version is read-only; writes, later, are create-only files that become an object when the file is closed whole (no overwrite, no rename, as design 14(b) says).

## 6. Edge cases and the mechanism for each

**Keys.** The proxy's key space is a UTF-8 string, as in S3 (up to 1024 bytes). The store key is what the core passes to a `Backend`.

| Case | Mechanism |
|---|---|
| A key valid for the S3 front but not for a store, or for another front (GCS names: no CR or LF, not `.` or `..`; a path: no empty segment). | It is outside the effective key rule, so it is refused at write time on every front with that front's own error (S3: 400), before an entry is taken. It never exists. |
| A key already stored before a stricter tier or front was configured. | Not scanned (no probes). It stays readable and deletable where the front can express it; its copy to a stricter tier is refused by that store and retried with back-off, and it stays in hot (section 3). The operator checks and renames before adding the tier. |
| A key written via S3 appears via GCS or the file system. | Same key, byte for byte, in binary UTF-8 order. Because the effective rule is the intersection of the fronts too, any key written through one front is nameable through all the others (a path has no empty, `.` or `..` segment; no GCS-forbidden character). |
| A file and a directory of one name (`a` and `a/b`), which S3 allows. | The one case a key rule cannot exclude, because it depends on other keys. Declared as a limit of the `fs` front: its view shows the directory, the file stays reachable through the other fronts, and a create through `fs` that would clash is refused (`EEXIST`, `ENOTDIR`). Listed in the `fs` front's documentation. |
| An fs path longer than the OS allows, or a segment over 255 bytes. | Part of the effective key rule when an `fs` front is configured (`max_bytes`, a segment limit), so S3 and GCS refuse such a key too. |
| Reserved prefixes (`_entries/`, `_journal/`, `_tiering/`). | Filtered once in the core (decision 10), so a new front cannot forget it. |

**Listing consistency across fronts.** All fronts list through the same walk over the union of both tiers, in the same order (byte order of the UTF-8 key), so the same keys appear in the same order in S3, GCS and the file system. What differs is the rendering: S3 folds on any delimiter, GCS `delimiter` and `prefixes`, and the file system always folds on `/`. A listing can lag a recent write on a store that allows it (the trait allows it, and the reconciler loops); GCS lists strongly consistently. Since keys outside the effective rule cannot be written, no front hides a key that another shows, apart from keys that predate the rule.

**Conditional-write semantics differing per store.** The core uses only the three operations of the `Backend` contract: create-only, replace-if-tag, delete (optionally if-tag). Each kind maps them to its store (the table in section 4). A kind is only accepted if its conformance run passes, so an ambiguity (a store whose create-only is not atomic) is the operator's choice of store, documented, not probed. The fronts expose only what tierstore guarantees to clients: objects are write-once, so `ifGenerationMatch=0` and `If-None-Match: *` from a client both mean "create" and the refusal of an existing key is the 412 a client already gets from the S3 front; a client precondition on a generation that tierstore cannot honour (`ifGenerationMatch=N`, N > 0, on a write) gets 412. Each front renders the core's one opaque version token its own way (section 8, item 2).

**Auth per front.** Each front authenticates in its own terms, with keys read from files, never from the environment (S2). `s3`: SigV4 keys, as now. `oci`: as now. `gcs`: bearer tokens from `token_file`, with full and read-only tokens like the S3 keys; OAuth2 is a token an operator mints elsewhere, tierstore only compares. `fs`: none of tierstore's; the mount's owner and mode, `allow_other` off by default, and `read_only` a config key. A request is authorized by the front it came in on and never by the store: the stores see tierstore's own credentials. Presigned URLs stay an S3 feature.

**A front with no port in the hand-off.** The hand-off (`handoff/`) passes `<front> <address>` lines with the sockets over `SCM_RIGHTS`. A FUSE mount has no socket, but it has a descriptor: the `/dev/fuse` connection. The `fs` front offers it the same way, with a line `fs <mount path>` and the descriptor; the successor checks that the path is its own, answers `ack`, and serves the same connection, so the kernel sees no unmount and no request fails. The old instance then stops reading and finishes the requests in flight. A running instance that has no `fs` front never offers such a line, and a successor with none never asks for one, so a mixed pair of the old and new release hands off exactly as before (B8). Until the `fs` front is built, nothing offers such a line; once built, an `fs` front on a platform without descriptor hand-off runs only where the update hook restarts (a gap, documented) and is refused with the socket hand-off configured. `ops` and the others hand off their sockets as they do today. Which platforms the `fs` front runs on is that implementation's own concern (section 8, item 3).

**Drain.** `Reach::None` and `Reach::Mount` fronts take part in the drain like the others: on drain they stop taking new requests, finish those in flight and release the mount. The `/readyz` answer is the ops interface's and covers all fronts.

## 7. Migration plan

Each step is one pull request that can land alone, interfaces before the code that uses them (P7). Coverage stays at the floor of each crate (T1); every new front and store gets a fake peer, not the network (T12). **Built now** is (a) to (c). **Later** is in section 8.

| | Change | Tests to add |
|---|---|---|
| (a) | `Interface` trait and `Reach`; `s3` and `oci` moved behind it; `serve` / `serve_all` iterate over the list. Pure refactor: no wire, config or metric change. | The existing front tests pass unchanged. New: a config's interfaces are served by one loop (two types together, port 0); a front that fails to bind fails the start naming the interface; the drain ends every front; the socket hand-off offers the same lines as before. |
| (b) | `ops` interface split out; `ops` param on `s3` (default true); "at least one interface of any type". | `/healthz` and the others answer on the `ops` address, and on the S3 address when `ops = true`; `ops = false` frees the bucket names `usagez` and `metricz`; a file with no `s3` and an `ops` starts; an old file still starts as it did; `/readyz` goes 503 on drain on each. |
| (c) | `Capabilities` (key, multipart, conditional, token, listing, max object, ranged read) declared by `Backend` and `Interface`; the start-up intersection; the checks of each front's `needs`; enforcement of the effective key rule on every write in the s3 and oci fronts and in the core; the new key `Error` case in each backend; the log line and `tierstore_capabilities` metric. Today's kinds declare theirs: `s3`, `dir`, `memory`, the chained upstream (a proxy or `S3Backend`). | See below. |

Tests for (c):

- The intersection is computed with no store call: a fake store that fails the test on any call is built from a config, and the effective set is still computed.
- Intersection rules, one case each: lowest `max_bytes`, union of forbidden characters, weakest multipart, AND of conditional flags, lowest `max_object_bytes`; a chain's upstream is included.
- A config with a store of a narrower key rule makes the S3 front refuse, with 400 and the exact S3 error body, a key it otherwise accepts; with the wider store alone the same key is accepted. The refusal happens before any store call or entry.
- A store without multipart makes the S3 front answer a multipart call with the S3 `NotImplemented` error body, and a put still works.
- A front whose `needs` the set does not meet fails the start naming the front, the capability and the tier that took it away (a test front with a declared `needs`, as no real front fails today).
- A hot tier without create-only or `If-Match` is refused at start.
- Reads, heads, deletes and listings of a key outside the rule still work (a key put through a store directly before the rule), and its copy to the stricter tier fails with the key error, is retried with back-off and leaves the hot copy in place.
- The core refuses an out-of-rule key a test front forgot to check; each of `s3`, `dir`, `memory` rejects an out-of-rule key with the key error (conformance).
- The log line and metric show the computed set.

(c) changes behaviour only where a configured tier already could not hold a key: that write used to fail late (a failed copy, a store error), and now fails at once with 400.

## 8. Future work enabled by the abstraction

Later, each its own pull request, and none reshapes `Backend`, `Interface` or `Capabilities`.

- **`gcs` store kind.** Implements `Backend` with the GCS table of section 4 and declares its capabilities; the conformance tests and a fake server are its acceptance. Seam: `Backend` and its declaration.
- **`gcs` front.** Implements `Interface`: JSON API, bearer-token auth, error bodies, listing over the shared walk. Seam: `Interface`, `Capabilities.listing`.
- **`fs` front.** Implements `Interface`; reach is a mount; the descriptor hand-off of section 6. Seam: `Reach::Mount` and its `needs`.
- **Compose multipart** and the other two items below.
- Writes through the `fs` front (create-only files closed whole), an NFS gateway, an Azure Blob front and store, per-interface connection limits shared by all types, and a store kind for a shared POSIX file system that proves its own create-only.

The three questions the first draft left open are not decided here. Each has a seam that makes it buildable without changing a trait:

1. **Multipart on a store without native parts.** The `Backend` multipart calls stay as they are. A store declares `multipart = Composable { max_sources }` and implements the same calls by staging parts under a reserved temporary prefix and composing them (create-only) at completion. Fronts see no difference except the limits in the effective set (the lowest `max_sources`, hence a lowest part count). The choice between emulation and `None` for a kind is that kind's declaration, which the intersection already reads.
2. **A version token a front renders as an ETag or a GCS generation.** The core keeps one opaque string, never compared across tiers. The declared `token` kind is informational. A front owns two functions, to render the token in its API (S3: a quoted ETag; GCS: a generation, a number it derives deterministically from the token and the write time) and to read a client precondition back into a token comparison. Changing how a front renders touches only that front.
3. **An `fs` front's platform support.** Which operating systems it runs on is behind `Interface`: constructing it on a platform it does not support is a start-up error naming the platform, and nothing in the traits, the capabilities or the hand-off wire mentions Linux or macOS. `Reach::Mount` is the only thing the rest of the code sees.
