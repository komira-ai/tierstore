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

A tree list is every entry of one tree in one object: the form a client (the build frontend) writes before it submits a job whose layer names `{provider: "tierstore", ref: <key>}`, and that the mount provider reads to make the job's node-local tree ([operations](operations.md#the-mount-provider)). The library reads it with `tierstore::parse_tree_list` and writes it with `tierstore::TreeList::encode`. [Version 2](#the-tree-list-version-2) holds the same entries in a binary form that is a third of the size and is read in one pass; the mount provider reads both.

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

Version 2 holds the same entries as version 1 in a binary object that a reader checks once and walks once, issuing each read of a file's bytes as it meets the file. The object is still named `cas/l/<SHA-256 of its bytes>`, written create-only, and is still the ref of `{provider: "tierstore", ref: <key>}`. The mount provider reads both versions and tells them by their first bytes: a version 2 list begins with the magic below, a version 1 list with `{`. The library reads it with `tierstore::TreeListV2::open` and `TreeListV2::build`, and writes it with `tierstore::TreeList::encode_v2`.

Every number is little-endian. The object is a header, three string tables and the records:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | magic: the bytes `TSLIST\r\n` (`54 53 4c 49 53 54 0d 0a`) |
| 8 | 4 | version: 2 |
| 12 | 4 | header length: 40 (a reader takes 40 or more and reads the first 40) |
| 16 | 4 | record length: 64 (a reader takes 64 or more and reads the first 64 of each) |
| 20 | 4 | the number of records |
| 24 | 4 | the offset of the key table |
| 28 | 4 | the offset of the name table |
| 32 | 4 | the offset of the target table |
| 36 | 4 | the offset of the records |

A **string table** is a count `n` (4 bytes), then `n` ends (4 bytes each), then the bytes of the strings, one after another: string `i` is the bytes from the end of string `i - 1` (0 for the first) to its own end. Every string is UTF-8 and each table is strictly ascending, bytewise, so each string is in it once and the number of a string orders as the string does.

- **keys**: the key of each object that holds the bytes of a file, 1 to 1024 bytes.
- **names**: the name of each entry: 1 to 255 bytes, not `.` or `..`, with no `/` or NUL, and not beginning `.wh.`.
- **targets**: the target of each symbolic link: 1 to 4096 bytes with no NUL.

A **record** is 64 bytes:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 1 | kind: 1 directory, 2 symbolic link, 3 file |
| 1 | 1 | 0, ignored |
| 2 | 2 | mode: the permission bits with setuid, setgid and sticky, at most 07777; ignored for a link |
| 4 | 4 | name: the number of the entry's name in the name table |
| 8 | 4 | parent: the number of the directory record the entry is in, or `0xffffffff` for the root |
| 12 | 4 | a file's key (the number of the object its bytes are in, `0xffffffff` exactly when its size is 0), or a link's target (its number in the target table); ignored for a directory |
| 16 | 8 | size: a file's length; ignored otherwise |
| 24 | 8 | offset: where a file's bytes begin in the object of its key; ignored otherwise |
| 32 | 32 | sha256: the SHA-256 of a file's bytes (for a file of size 0, that of no bytes); ignored otherwise |

The records come in the order the reader needs them:

1. **Every directory**, those the paths imply included, each after the directory it is in. A directory's record says its mode (0755 unless the tree gives it another).
2. **The symbolic links.**
3. **The files**, ordered by key number and then by offset, the files of size 0 (whose key is `0xffffffff`) last.

So a reader meets each parent before what it holds, and meets the files pack by pack, in ascending offset within each pack, and it can ask for each file's bytes as it reads the file's record. A name twice in one directory is refused. The tree a version 2 list makes is the tree the version 1 list of the same entries makes, digest for digest.

The reader (`TreeListV2::open`) checks the size of the object (at most 256 MiB, else `tree list too large`), the magic (`not a version 2 tree list: bad magic`), the version (`unsupported tree list version`), that the header, each table and the records lie within the object (`tree list header: ...`), and every key; it copies nothing. The walk (`TreeListV2::build`) then reads every record once, in order. For each it checks the kind, the order of the kinds, the mode, that the name is in the table, that the parent is a directory record before it, a link's target, a file's key, that an offset plus size fits in 64 bits, that the files are in the order above and that a file of size 0 has the SHA-256 of no bytes; a fault is refused with the number of the record, from 0: `tree list record 7: parent 9 is not a directory before it`. It hands the caller the span of every file whose digest it has not met before, as soon as the file's record is checked, and puts the entry in the tree. After the walk it checks the name and target tables (`tree list name 3: not strictly ascending`) and the tree: a name twice in one directory, more entries than `trees.max_entries`, a path deeper than `trees.max_depth`, a directory with more than `trees.max_dir_entries` children (`the tree is refused: ...`). The span of a digest is that of its first file in the order of the records, which is the span the tree store keeps for it.

The reader never reads outside the object, and a record, a table or a header that would is refused: every offset and count is checked before it is used. Spans handed out before a fault in a later record are not taken back; the reads they start check each span's bytes against its own digest, so a refused list costs reads, never a wrong byte.

**Changing the format.** A later writer may add fields at the end of the header (a larger header length) or of each record (a larger record length), which this reader skips. A new kind, or a change to what a field means, is a new version, written only after every reader reads it.

**Writing it the same way.** For the same entries to make the same bytes, and so the same key, `TreeList::encode_v2` writes: the header of 40 bytes, then the key, name and target tables in that order, then the records, with nothing between them; the directories sorted by path, then the links sorted by path, then the files with bytes sorted by key, offset and path, then the files of size 0 sorted by path; zero in every ignored field. A writer in another language that follows this writes the same bytes; the golden test below is the check.

**What it costs.** On a tree of 49,352 entries (47,468 files in 65 packs: a komira checkout and the 112 packages of its toolchain), measured with `cargo run --release -p tierstore --example tree_list_bench` on one x86-64 core with SHA extensions, the medians of 25 runs from the bytes in memory:

| | version 1 (JSON Lines) | version 2 |
|---|---:|---:|
| bytes | 12,373,234 | 4,324,350 |
| SHA-256 of the list | 4.7 ms | 1.6 ms |
| to the first span to read | 70.5 ms (parse, then the tree built) | 1.1 ms; 3.2 ms with the SHA-256 checked first |
| to the whole tree in memory | 70.5 ms | 17.8 ms |

The mount provider checks the SHA-256 before the walk, so that no span of a list other than the one the ref names is read: half of the 3.2 ms. It could hash beside the walk instead and refuse before placing the tree, at the cost of reads of a list that is then refused.

The golden test `tests/stored_formats/tree_list_v2.rs` pins the bytes of an example (`fixtures/tree_list_v2.bin`, the entries of `fixtures/tree_list_v1.jsonl`) and its digest, checks that it makes the same tree as the version 1 list, and reads the same list as a later writer may write it, with a longer header and longer records (`fixtures/tree_list_v2_newer.bin`).
