# Tree manifest format

A tree manifest lists the children of one directory of a tree. A whole tree is a root manifest plus one manifest for each directory below it, each named by the SHA-256 of its bytes. This page is the reference for version 1, as the library reads it (`tierstore::parse_manifest` and `tierstore::flatten`), and for the tree list, the flat form a client writes for the mount provider (below).

## The manifest

A manifest is a JSON object:

```json
{"v":1,"children":[
  {"name":"bin","kind":"dir","sha256":"<64 hex of the child manifest>","size":812},
  {"name":"cc","kind":"file","exec":true,"sha256":"<64 hex>","size":1048576,
   "source":{"key":"cas/d/n7/b2/0000000000000041","offset":524288}},
  {"name":"empty.h","kind":"file","sha256":"e3b0c442...","size":0},
  {"name":"lib","kind":"symlink","target":"bin"}
]}
```

- `v` is the number 1. Any other value is refused: `unsupported manifest version`.
- `children` is sorted by `name`, bytewise on the UTF-8 bytes, strictly: a name that sorts before the one ahead of it is refused as `entries not sorted by name`, a repeated name as `duplicate entry name`.
- A `name` is not empty, not `.` or `..`, has no `/` and no NUL, and does not begin `.wh.`. Otherwise: `unsupported entry name`.
- Every `sha256` is 64 lowercase hex digits. Otherwise: `malformed digest`.
- A reader ignores fields it does not know, at every level.

| `kind` | Fields | Meaning |
|---|---|---|
| `dir` | `sha256`, `size` | `sha256` is the digest of the child's manifest. `size` is that manifest's length in bytes as the writer recorded it; a reader does not check it. |
| `file` | `sha256`, `size`, optional `exec` (boolean, false when absent), optional `source` | `sha256` and `size` are the digest and length of the contents. With no `source` the contents are the object `cas/l/<sha256>`. A file of size 0 has no `source`; one that has is refused. |
| `symlink` | `target` | What the link says. |

A `source` is `{"key": <object key>, "offset": <number>}`: the contents are the `size` bytes at `offset` in that object. An offset plus the size that does not fit in 64 bits is refused.

A `kind` that is not in the table is refused: `unsupported entry kind`. That, and the refusal of an unknown `v`, are deliberate: a reader that left out an entry it did not know would serve a wrong tree. A new kind is therefore written only after every reader knows it. Every other change to the format is additive.

The digest of a manifest is the SHA-256 of its bytes as stored. Two encodings of one tree are two trees; no canonical form is needed. A reader given bytes and the digest they were asked for refuses a mismatch: `manifest digest mismatch`.

## Limits

Each is a constant of the library and is refused by name.

| Limit | Value | Refusal |
|---|---|---|
| Size of one manifest | 64 MiB (`MAX_MANIFEST_BYTES`), judged before the bytes are hashed | `manifest too large` |
| Entries below the root, across all directories | 2^24 - 1 = 16,777,215 (`MAX_ENTRIES`) | `tree too large` |
| Depth of an entry: the names on its path from the root | 4096 (`MAX_DEPTH`) | `tree too deep` |

The root is not counted, so with it a tree has at most 2^24 entries, the most a 24-bit index names. An empty directory at depth 4096 is within the limit; nothing in a tree may be at 4097.

## Flattening

`flatten(root_digest, fetch)` returns every entry of the tree as a list, the root first. `fetch` is the caller's function from a digest to the bytes of that manifest. Each distinct manifest is fetched once, however many places in the tree it is listed at. A tree over a limit is refused before any entry is made, so a refused tree costs memory for its distinct manifests only.

The **index** of an entry is its position in the list: the root is 0, and the others follow depth first in manifest order (a directory is followed at once by what is in it). Each entry has its parent's index, its name (empty for the root), and its kind with the attributes above. The indices are a function of the manifests alone: the same manifests give the same indices, and a directory listed in two places has two sets of indices, one below each.

## Stored form

The golden test `tests/stored_formats/tree_manifest.rs` pins the bytes of a version 1 example (`fixtures/tree_manifest_v1.json`) and its digest. Changing that fixture changes what version 1 is.

## The tree list

A tree list is every entry of one tree in one object: the form a client (the build frontend) writes before it submits a job whose layer names `{provider: "tierstore", ref: <key>}`, and that the mount provider reads to make the job's node-local tree ([operations](operations.md#the-mount-provider)). The library reads it with `tierstore::parse_tree_list` and writes it with `tierstore::TreeList::encode`. [Version 2](tree-manifest.md#the-tree-list-version-2) holds the same entries in a binary form that is a third of the size and is read in one pass; the mount provider reads both. The version 1 reader stays until the clients write version 2.

The object is JSON Lines: each line one JSON object, ended by `\n`, with no other whitespace. The first line is the header, then one line for each entry:

```text
{"format":"tierstore.tree-list","version":1}
{"path":"bin/cc","kind":"file","mode":493,"size":1048576,"key":"out/a1/0","offset":524288,"sha256":"<64 hex>"}
{"path":"include/empty.h","kind":"file","mode":420,"size":0}
{"path":"lib","kind":"symlink","target":"bin"}
{"path":"tmp","kind":"dir","mode":1023}
```

- The header's `format` is `tierstore.tree-list` and its `version` is 1. Any other format is refused `unknown format`, any other version `unsupported version`.
- The entries are sorted by `path`, bytewise, with no path twice: a path that sorts before the one of the line before is refused `not sorted`, a repeated one `duplicate path`.
- A `path` is `/`-separated names from the root of the tree: at most 4096 bytes, each name at most 255 bytes, not empty, not `.` or `..`, with no NUL, and not beginning `.wh.`. Otherwise: `bad path`. Every directory a path implies need not be listed; one is listed only when it is empty or its mode is not 0755.
- A `mode` is a number, the permission bits with setuid, setgid and sticky: 0 to 4095 (07777). Anything above is refused `bad mode`.
- A reader ignores members it does not know, in the header and in every entry. A `kind` that is not in the table is refused `unknown kind`: a reader that left out an entry would make a wrong tree, so a new kind or version is written only after every reader knows it.

| `kind` | Members | Meaning |
|---|---|---|
| `file` | `mode`, `size`, and `key`, `offset` and `sha256` exactly when `size` is above 0 (`sha256` may also be given for a file of size 0) | the `size` bytes at `offset` of the object `key` (a pack, or a whole object `cas/l/<sha256>` at offset 0). A file of size 0 has neither `key` nor `offset`; any other mix is refused `a file of size 0 has no key or offset, and a larger one has both`, and an offset plus size past 2^64 `range overflow`. `sha256` is 64 lowercase hex digits (else `malformed digest`) |
| `dir` | `mode` | an empty directory, or one with a mode of its own |
| `symlink` | `target` | what the link says, 1 to 4096 bytes with no NUL (else `bad target`), never followed |

A refusal of a line names it, from 1 for the header: `tree list line 3: not sorted: "a" follows "b"`. An object that does not end with a newline is refused at its last line, `no newline at the end`.

The object is named `cas/l/<SHA-256 of its bytes>` (`tierstore::tree_list_key`) and written create-only, so a client that writes the same tree again finds it there. A reader given the bytes and the key refuses bytes that hash to another digest (`tree list digest mismatch`), and refuses an object over 256 MiB (`tree list too large`) before it hashes it. `TreeList::encode` writes each entry's members in the order of the example, leaves out what is not set, and sorts the entries, so the same entries give the same bytes and the same key.

A file of size above 0 without `sha256` is refused at its line (`file without sha256`): a node-local tree's digest covers each file's contents, and each range is checked against its digest as it is filled ([the design](https://github.com/komira-ai/tierstore/blob/main/docs/design/node-local-trees.md), decision D5). A list made in code rather than read is held to the same rule by `TreeList::into_entries`.

The golden test `tests/stored_formats/tree_list.rs` pins the bytes of an example (`fixtures/tree_list_v1.jsonl`) and its digest, and reads the same list as a later writer may write it, with members this reader does not know (`fixtures/tree_list_v1_newer.jsonl`). A tree list and a version 1 manifest are both under `cas/l/`, and neither reader takes the other: a manifest has no header line, and a tree list no `v`.

## The tree list, version 2

Version 2 (the `TSTREE` format) holds the same entries as version 1 in a binary object of fixed records that a reader checks in one walk over the object's own bytes (it copies only the pack keys), and that holds the reads to issue, pack range by pack range. The object is still named `cas/l/<SHA-256 of its bytes>`, written create-only, and is still the ref of `{provider: "tierstore", ref: <key>}`. The mount provider reads both versions and tells them by their first bytes: a version 2 list begins with the magic below, a version 1 list with `{`. The library reads it with `tierstore::TreeListV2::open`, `TreeListV2::check` and `Checked::build` (or `TreeListV2::build` for both), or keeps it as the tree with `tierstore::TreeImage::open`, and writes it with `tierstore::TreeList::encode_v2`.

Every number is little-endian. The object is a 64-byte header, then six sections in this order, each at an offset that is a multiple of 8, none overlapping the one before:

| Offset | Size | Header field |
|---:|---:|---|
| 0 | 8 | magic: `TSTREE` and two zero bytes (`54 53 54 52 45 45 00 00`) |
| 8 | 2 | major version: 1. A reader refuses any other |
| 10 | 2 | minor version: 0. A reader reads any minor version of its major one |
| 12 | 4 | required flags: 0. A reader refuses a flag it does not know |
| 16 | 8 × 6 | for each section, its offset (4 bytes) and its count (4 bytes) |

| Section | Count | Size | What it holds |
|---|---|---|---|
| entries | `n`, at least 1 (the size of the object bounds it) | `n` × 32 | one record per entry, the root first |
| sha256 | `n` | `n` × 32 | the SHA-256 of entry `i`'s bytes; zero for a directory or a link |
| key offsets | `k` | (`k` + 1) × 4 | where key `i` begins in the keys, then where the last ends |
| keys | bytes | the count | the keys of the objects that hold the files' bytes, back to back: each 1 to 1024 bytes of UTF-8, beginning at 0, the last ending at the end |
| names | bytes | the count | the names of the entries, and the targets of the links, back to back |
| ranges | `r` | `r` × 24 | the ranges of the packs the tree reads |

An **entry record** is 32 bytes:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | parent: the number of the directory the entry is in (0 for the root itself) |
| 4 | 4 | name offset: where the name begins in the names |
| 8 | 4 | a file's key (the number of the object its bytes are in); a directory's first child |
| 12 | 2 | kind and mode: the kind in the top four bits (0 file, 1 directory, 2 symbolic link), the permission bits with setuid, setgid and sticky in the low twelve |
| 14 | 1 | name length, 0 to 255 |
| 15 | 1 | flags: 0, ignored |
| 16 | 8 | a file's offset in its object; a directory's number of children; a link target's offset in the names |
| 24 | 8 | a file's size; a link target's length; 0 for a directory |

A **range record** is 24 bytes: the key number (4 bytes), 4 zero bytes, the offset (8 bytes) and the length (8 bytes) of a range of one pack.

The entries are in **breadth-first order**: entry 0 is the root, a directory with parent 0 and no name; then the root's children; then the children of the root's first child that is a directory, and so on, so that every directory's children are next to one another, sorted by name, bytewise, and come right after the children of the directories before it. A directory says where its children begin and how many there are, so the tree is the file: an entry's number is its inode, a directory's listing is a slice of the records, and a lookup is a binary search of that slice. Every directory the paths imply is an entry of its own, mode 0755 unless the tree gives it another. A file of size 0 has no key (the field is ignored) and has the SHA-256 of no bytes. The records `Checked::build` makes of a version 2 list are those of the version 1 list of the same entries, digest for digest; a node serves a version 2 list as the tree itself, named by the list's own digest (below).

The **ranges** are what the writer says the tree reads: the files' bytes, sorted by key and offset, with the bytes of two files of one pack that are at most 64 KiB apart merged into one range. They are checked only to name a key and to fit in 64 bits. The mount provider reads by them: the spans the walk hands out that lie in one range are one read of the pack, from the first span's start to the last one's end, cut at 8 MiB, and each span is checked against its own file's digest as its bytes pass and kept if it holds; the bytes between the spans are read and not kept. A span in no range is read alone, so the ranges are a hint, never needed for a correct tree.

**Reading.** `TreeListV2::open` checks the size of the object (at most 256 MiB, else `tree list too large`), the magic (`not a version 2 tree list: bad magic`), the major version (`unsupported tree list version`), the required flags, that each section is aligned, after the one before and within the object, that the entry count is at least 1 and the digest count the same (`tree list header: ...`), every key (`tree list key 2: empty`) and every range (`tree list range 0: ...`). It borrows the sections and copies only the keys. `TreeListV2::check` then walks the directories in order, each one's children in order, and for each child checks that it is where the breadth-first order puts it, that its parent is the directory, its kind, its name (in the names, a name a tree may have, after its sibling's), the length of its path (at most 4096 bytes), a file's key and that its offset plus size fits in 64 bits, that a file of size 0 has the digest of no bytes, and a link's target (1 to 4096 bytes of UTF-8 in the names, no NUL); a fault is refused with the number of the entry, from 0: `tree list entry 7: parent 3 is not the directory that holds it`. Entries no directory holds are refused (`tree list entries from 90 on are in no directory`). A tree over a node's limits is refused as the tree store refuses it (`the tree is refused: ...`): more entries than `trees.max_entries`, a path deeper than `trees.max_depth`, a directory with more children than `trees.max_dir_entries`. As soon as a file of size above 0 is checked, the walk hands its caller the file's span, if no file before it had its digest; the span handed out for a digest is that of its first file in the order of the entries, which is the span a tree of records keeps for the digest (`TreeImage::open` hands out every file's span instead: below). After the walk the list is a tree as it is (an entry's number names it, a directory's children are a slice of the entries, a name is found among them by a binary search); `Checked::build` makes the tree store's records of it, hashing the directories, children before parents.

The reader never reads outside the object: every offset and count is checked before it is used. What it makes is bounded too: since names are shared, a small object could name paths of many more bytes than it holds, so the walk refuses a list whose paths together are over 256 MiB (`tree list paths over 268435456 bytes together`), more than its version 1 list could hold. Spans handed out before a fault in a later entry are not taken back; each read checks its bytes against its own digest, so a refused list costs reads, never a wrong byte.

**The digest of the object.** The mount provider hashes the object on a thread of its own while it walks it. As soon as the walk has checked the list, it groups the spans by the ranges and queues the reads, before the tree is placed and before the hash is done; the tree is placed only once the bytes hash to the digest the key names (else `tree list digest mismatch`, and the reads not yet begun are not made). The reads are safe to begin early because each span is checked against its own file's digest.

**Served as it is.** A node makes no records of a version 2 list: the list is the tree (`tierstore::TreeImage`). The mount provider opens it with `TreeImage::open`, the walk of `TreeListV2::check` with every file's span handed out (a file is read at its own span; a span that several files have is one read), and places the object itself, written as it is to the tree's directory (`trees/<digest>/image`, beside the walk, on a third thread), named by the SHA-256 of the object: the digest of its key, not a digest of records. No directory record is made or hashed, and nothing is made per entry: no name, no path. A lookup reads the entries in place: an entry's number is its index (the root is 0), a directory's children are the entries its record says, a name is found among them by a binary search, and what an entry is (kind, mode, size, digest, the span of its bytes, a link's target) is its record and its digest. The S3 front's answers under a placement are read the same way, and the mount (`tierstore fs`) reads the image file from the cache directory and numbers each entry by its index, Linux and macOS alike. A version 1 and a version 2 list of the same entries are therefore two trees of two names, alike at every path.

**Changing the format.** A later writer may set a minor version, use the flags of an entry, or put bytes after the sections, all of which this reader ignores. A change a reader of this version must not ignore sets a required flag or a new major version, and is written only after every reader reads it.

**Writing it the same way.** For the same entries to make the same bytes, and so the same key, `TreeList::encode_v2` writes: the header; the sections in the order above, each at the next multiple of 8, zero between them; the entries breadth first, siblings sorted by name; each name in the names once, at its first entry in that order, and each link's target after its link's name; the keys sorted; the ranges sorted by key and offset, merged within 64 KiB; zero in every ignored field. A writer in another language that follows this writes the same bytes; the golden test below is the check.

**What it costs.** On a tree of 49,352 entries (47,468 files in 65 packs: a komira checkout and the 112 packages of its toolchain; 56,093 entries once every directory is one), measured with the crate's `tree_list_bench` example (`examples/tree_list_bench.rs` says how to run it), built for release, the medians of 25 runs from the bytes in memory, on two machines: a build node without SHA extensions (Xeon E5-4669 v4, idle) and a workstation with them (under load):

| | v1, node | v2, node | v1, workstation | v2, workstation |
|---|---:|---:|---:|---:|
| bytes | 12,373,234 | 4,199,616 | 12,373,234 | 4,199,616 |
| SHA-256 of the list | 67 ms | 23 ms | 4.8 ms | 1.5 ms |
| to the first range of the ranges table | | 0.008 ms | | 0.003 ms |
| to the first span the walk hands out | 225 ms (parse, then the tree built) | 0.07 ms | 70 ms | 0.06 ms |
| the walk: every entry checked, every span handed out | | 6.7 ms | | 2.2 ms |
| the walk and the tree store's records | 225 ms | 38 to 47 ms | 70 ms | 12.5 ms |

The mount provider queues its reads once the walk is done, so they begin after the walk; the hash runs beside it. The records took most of the time to the whole tree: each directory's record is hashed for the tree's digests (the same bytes as the list, about 23 ms on the node without SHA extensions), and each name and each file's path becomes a string of its own (about 100,000 of them). The node no longer makes them (served as it is, above).

**What making the tree costs.** The same tree, measured with the crate's `tree_ready_bench` example (`examples/tree_ready_bench.rs` says how to run it) in the tree store of a real cache on the node's local disk, built for release, the medians of 25 runs from the bytes in memory, on the same two machines. Before is the tree of records (`TreeListV2::build`, then `TreeStore::create`); after is the list kept as the tree (`TreeImage::open`, then `TreeStore::create_image`, the image written beside the walk). The tree is ready when the create returns; the provider waits for the hash first.

| | before, node | after, node | before, workstation | after, workstation |
|---|---:|---:|---:|---:|
| the walk (and the records, before) | 35.9 ms | 3.3 ms | 8.4 ms | 1.4 ms |
| the create | 28.0 ms | 1.9 ms | 12.4 ms | 0.7 ms |
| the tree made: walk and write beside, then the create | 62.6 ms | 3.6 ms | 20.6 ms | 1.4 ms |
| ready as the provider makes it: the same with the SHA-256 of the list beside | 62.8 ms | 32.4 ms | 20.1 ms | 1.7 ms |

On the node the SHA-256 of the list (25 ms alone, slower beside the walk and the write) is what the tree waits for; the tree itself is made in 3.6 ms. The mount reads the image again when a job first looks the tree up, and walks it with no span (3.2 ms on the node, 1.4 ms on the workstation).

The golden test `tests/stored_formats/tree_list_v2.rs` pins the bytes of an example (`fixtures/tree_list_v2.bin`, the entries of `fixtures/tree_list_v1.jsonl`) and its digest, checks that it makes the same tree as the version 1 list, and reads the same list as a later writer may write it, with a minor version, entry flags and bytes after the sections (`fixtures/tree_list_v2_newer.bin`).
