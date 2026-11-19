# The tree list L2

An L2 tree list is a tree's [version 2 list](../../tierstore/docs/tree-manifest.md#the-tree-list-version-2) (`TSTREE`) with two tables added: **PACKS**, one record per object the files' bytes are in, and **CELLS**, the SHA-256 of every 1 MiB cell of each of those objects. A reader verifies any cell of a pack the moment its bytes arrive, in any order, and a file is ready once the cells that cover it are present; no file is hashed whole before its first byte is served ([the design](../../../docs/design/fetch-cache-serve.md), section 3). The crate `tierstore-list` reads it (`TreeListL2`, which implements `ListL2` of `tierstore-fetch-api`) and writes it (`sidecar_from_v2`, with the cell digests of `CellHasher`).

## Identity: a sidecar of the version 2 list

A tree is named by its version 2 list: the key `cas/l/<id>`, where `<id>` is the SHA-256 of the version 2 list's bytes. That is the `ref` a job's layer names (`{provider: "tierstore", ref: "cas/l/<id>"}`), and nothing else is keyed by it: the action cache is keyed by the client's action digest, which the tree list is made from, never the other way round.

The L2 list of that tree is stored beside it, at **`cas/l2/<id>`** (`sidecar_key`), and its header holds the same `<id>`. It keeps the version 2 list's entries, digests, keys, names and ranges byte for byte, so the version 2 list is a function of the L2 list (its **projection**, below) and a reader checks that the projection hashes to `<id>`. That pins every path, mode, size, file digest, pack key, offset and range of the L2 list to the id the job names. What the id does not pin are the PACKS fields that the version 2 list does not have (a pack's size, cell count, flags) and the cell digests: a wrong cell digest makes a good cell fail its check (an error or a miss, never a wrong byte); a wrong cell passes only if whoever wrote the sidecar also wrote the pack.

The sidecar is a function of the version 2 list and what its writer is told of each pack: its size and cell digests (functions of the pack's bytes, which never change), its `upload_part_size` (fixed when the pack was written) and whether it is shared. Only the last can differ between two writers of one sidecar (a pack crosses `shared_min` between the first write and a backfill): a create-only PUT that finds the key written keeps the first sidecar, whose `shared` bit may be stale. That bit only orders Prepare's fetches (design 6.1) and is never read for correctness, so a stale bit costs fetch order, never a byte.

## Layout

Every number is little-endian. The object is a header, then eight sections in this order, each at an offset that is a multiple of 8, at or after the end of the header and of the section before it, and within the object. A section is addressed by its offset and its count, both 8 bytes.

### Header (192 bytes)

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | magic: `TSL2TREE` (`54 53 4c 32 54 52 45 45`) |
| 8 | 2 | major version: 1. A reader refuses any other (`ListVersionUnsupported`, `unknown_version`) |
| 10 | 2 | minor version: 0. A reader reads any minor version of its major one |
| 12 | 4 | required flags: 0. A reader refuses a flag it does not know |
| 16 | 4 | header length: 192. At least 192 and a multiple of 8; a later minor version may write a longer header, whose bytes past 192 this reader ignores |
| 20 | 1 | cell digest: 1 = SHA-256, the only value written. A reader refuses one it does not know (`ListVersionUnsupported`, `unknown_cell_digest`) |
| 21 | 1 | default cell shift: 20 (1 MiB cells). A reader refuses any other |
| 22 | 2 | zero, ignored |
| 24 | 32 | tree id: the SHA-256 of the version 2 list the tree is named by |
| 56 | 8 × 16 | for each section, its offset (8 bytes) and its count (8 bytes; below 2^32) |
| 184 | 4 | zero, ignored |
| 188 | 4 | CRC-32C (Castagnoli) of the header's bytes, its length long, with these four bytes as zero |

### Sections

| Section | Count | Size | What it holds |
|---|---|---|---|
| entries | `n`, at least 1 | `n` × 32 | the entry records of the version 2 list, the root first, breadth first; a file's key number is its pack's number |
| sha256 | `n` | `n` × 32 | the SHA-256 of entry `i`'s bytes; zero for a directory or a link |
| packs | `p` | `p` × 48 | one record per key of the version 2 list, in the same (sorted) order |
| cells | `c`, the packs' cell counts together | `c` × 32 | the cell digests, pack by pack |
| keys | bytes | the count | the packs' keys back to back, as in the version 2 list |
| names | bytes | the count | the names and link targets, as in the version 2 list |
| ranges | `r` | `r` × 24 | the range records of the version 2 list: {pack (4 bytes), 4 zero bytes, offset (8), length (8)} |
| extensions | bytes, a multiple of 8 | the count | extension records; none in this version |

The **entry records** are those of version 2, 32 bytes each: parent (4), name offset (4), a file's pack number or a directory's first child (4), kind and mode (2: the kind in the top four bits, 0 file, 1 directory, 2 link; the mode in the low twelve), name length (1), flags (1, ignored), a file's offset or a directory's child count or a target's offset (8), a file's size or a target's length (8). Their order and every rule on them are version 2's: breadth first, each directory's children together and sorted by name bytewise, so an entry's number is its inode, a directory's listing is a slice of the records and a lookup is a binary search of it.

A **PACKS record** is 48 bytes:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | key offset: where the key begins in the keys; the first at 0, each where the one before it ends |
| 4 | 4 | key length: 1 to 1024 bytes of UTF-8; the keys sort ascending, bytewise, each once |
| 8 | 8 | `pack_size`: the bytes of the pack, above 0 |
| 16 | 8 | `cell_first`: the index of its first digest in CELLS: the cell counts of the packs before it together |
| 24 | 4 | `cell_count`: `ceil(pack_size / 2^20)` |
| 28 | 1 | `cell_shift`: 20. A reader refuses any other |
| 29 | 1 | `pack_flags`: bit 0, the list's files cover the pack's bytes with no gap; bit 1, **shared** (at least `shared_min` trees name the pack, design 6.1). A reader ignores the bits it does not know |
| 30 | 2 | zero, ignored |
| 32 | 8 | `upload_part_size`: 0 for a pack written by one PUT; the part size of a pack uploaded in parts. No reader uses it |
| 40 | 8 | zero, ignored |

**CELLS**: `digest[cell_first + i] = SHA-256(pack bytes [i << 20, min((i + 1) << 20, pack_size)))`; the last cell of a pack may be short.

**`pack_root`** of a pack is `SHA-256("TSPACK1" || pack_size as 8 bytes || cell_shift as 1 byte || digest[0] || ... || digest[n - 1])`. Two lists naming the same pack bytes give the same root, so one cache entry; a pack whose bytes changed gives another.

An **extension record** is a tag (4 bytes), a length (4 bytes) and that many bytes, padded to a multiple of 8. Bit 31 of the tag is *required*: a reader that does not know a required tag refuses the list; one it does not know without that bit it skips. This version defines no tag and writes none.

### The projection: the version 2 list an L2 list holds

The version 2 list is: its 64-byte header (`TSTREE\0\0`, major 1, minor 0, no flags, and the offset (4 bytes) and count (4 bytes) of six sections), then the entries, the sha256s, the key offsets (`p + 1` numbers of 4 bytes: 0, then the end of each pack's key), the keys, the names and the ranges, each at the next multiple of 8 after the one before (the first at 64), with zero bytes between them and nothing after the last. Every count is the L2 section's (the key offsets' count is `p`). Its SHA-256 is the tree id.

## Reading

`TreeListL2::open` checks, in this order, and refuses with the first fault:

1. the magic: a version 1 list (`{`) or a version 2 list (`TSTREE\0\0`) is `ListVersionUnsupported` (`v1_list`), anything else `bad magic`;
2. the header: at least 192 bytes, the major version (`ListVersionUnsupported`, `unknown_version`), the header length, the CRC-32C, the required flags, the cell digest (`ListVersionUnsupported`, `unknown_cell_digest`) and the cell shift;
3. each section: aligned, at or after the end of the one before, a count below 2^32, within the object; at least one entry, as many sha256s as entries, an extension section a multiple of 8;
4. each PACKS record: its key, its size, its cell shift, cell count and first cell; that the keys end with the last pack's, and that CELLS holds exactly the packs' cells;
5. each range: a pack it names, not empty, within the pack;
6. each extension record: within the section, no required tag;
7. the entries, walked once, directory by directory, by the rules of version 2: the root, each directory's children where the breadth-first order puts them, each child's parent, kind, name (1 to 255 bytes of UTF-8, not `.` or `..`, no `/` or NUL, after its sibling's), path (at most 4096 bytes), a file of size 0 having the digest of no bytes, a link's target (1 to 4096 bytes of UTF-8 in the names, no NUL), and a file of size above 0 naming a pack and lying within it; entries no directory holds are refused.

It gives the tree's shape (entries, files, their bytes, the deepest depth, the largest directory, the bytes of all paths together), by which a caller holds the tree to its own limits. It does not check the tree id.

`TreeListL2::open_sidecar(fetched, id)` is how a node opens the sidecar of the tree a job names: no object is `ListVersionUnsupported` (`missing_sidecar`); then `open`; then the header's tree id must be `id`, and the projection must hash to it. A caller that hashes on another thread beside other work calls `open` and `check_tree_id` itself.

After open, every read indexes into the bytes: `node`, `children`, `file_sha256`, `pack_key`, `ranges`, and the `ListL2` methods `pack_record`, `cell_digest`, `pack_root` (computed when first asked, kept per list) and `dir_lookup` (a binary search of a directory's records). An index outside the list reads as `None`.

## Writing

`sidecar_from_v2(v2, packs)` makes the L2 list of a version 2 list `v2`, asking `packs` for each key's `PackInfo` (its size and cell digests, its `upload_part_size`, whether it is shared). It takes only a version 2 list whose bytes are exactly what the version 2 writer writes (major 1, minor 0, no flags, sections at the next multiple of 8 with zeros between them, nothing after the last, sorted keys), so its projection is `v2` itself; it refuses a key it has no cells for, cells that are not those of the pack's size, an empty range, and a file or range past the end of its pack. It does not walk the entries: a version 2 list whose entries its own reader refuses makes a sidecar the L2 reader refuses for the same fault, so the writer is given a list that was read back. It sets `pack_flags` bit 0 where the files cover the pack. The same inputs give the same bytes: the header (192 bytes), then the sections in order, each at the next multiple of 8, zero between them, an empty extension section at the end.

`CellHasher` computes a pack's cell digests as its bytes stream (one 1 MiB accumulator; the last cell hashed short), for a writer that has the pack's bytes once, as it uploads them.

## Changing the format

A later writer may set a minor version, write a longer header, use the ignored fields, set `pack_flags` bits above 1, and add extension records without the required bit: this reader reads all of those to the same tree. A change this reader must not ignore sets a required flag, a required extension tag or a new major version, and is written only after every reader reads it.

## What it costs

Measured with the crate's `l2_bench` example (`examples/l2_bench.rs` says how to run it), built for release in the `rust:1.95.0` container on a build node without SHA extensions (Xeon E5-4669 v4, lightly loaded), each figure of the table below the median of 25 runs from the bytes in memory, two runs of it alike to within 3%.

On the tree of the version 2 measurements of `tierstore`'s `tree-manifest.md` (a komira checkout and the 112 packages of its toolchain: 56,093 entries, 47,468 files, 4.28 GB in 65 packs of 64 MiB, 4,110 cells), read from its saved list with stand-in cell digests:

| | |
|---|---:|
| version 2 list | 4,199,616 B |
| L2 sidecar (the same plus PACKS and CELLS) | 4,334,120 B |
| `sidecar_from_v2` | 14.3 ms |
| SHA-256 of the version 2 list (what a node checks today) | 12.4 ms |
| `open`: header, sections, packs, cells, ranges and the walk of every entry | 2.85 ms |
| `check_tree_id`: the projection hashed | 12.4 ms |
| `open_sidecar`: both | 15.3 ms |
| a walk of every entry through `children` and `node` | 1.1 ms |
| every one of the 56,092 paths looked up from the root, name by name | 22.4 ms |
| `open` and every pack's `pack_root` | 3.26 ms |

The id check costs what the SHA-256 of the version 2 list costs a node today: it hashes the same bytes. A caller that needs the tree before the hash (as the mount provider does with a version 2 list) runs `open` and `check_tree_id` on two threads.

**What cell digests cost the writer of packs.** Walking the 614 MB of the Rust 1.95 toolchain (166 files in packs of 64 MiB) on the same node, ring's SHA-256 hashed the whole files at 319 MB/s and the same bytes as 1 MiB cells at 325 MB/s, each on one core (one pass over the files, the time of each hash summed; not a median): cell digests double the hashing of a writer that already hashes each file, about 3.1 s of one core per GB more. A backfill that makes the sidecars of stored trees reads each pack once and hashes it at that rate: about 0.9 core-hours per TiB, beside the read.
