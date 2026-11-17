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

A tree list is every entry of one tree in one object: the form a client (the build frontend) writes before it submits a job whose layer names `{provider: "tierstore", ref: <key>}`, and that the mount provider reads to make the job's node-local tree ([operations](operations.md#the-mount-provider)). The library reads it with `tierstore::parse_tree_list` and writes it with `tierstore::TreeList::encode`.

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
| `file` | `mode`, `size`, `key` and `offset` exactly when `size` is above 0, optional `sha256` | the `size` bytes at `offset` of the object `key` (a pack, or a whole object `cas/l/<sha256>` at offset 0). A file of size 0 has neither `key` nor `offset`; any other mix is refused `a file of size 0 has no key or offset, and a larger one has both`, and an offset plus size past 2^64 `range overflow`. `sha256` is 64 lowercase hex digits (else `malformed digest`) |
| `dir` | `mode` | an empty directory, or one with a mode of its own |
| `symlink` | `target` | what the link says, 1 to 4096 bytes with no NUL (else `bad target`), never followed |

A refusal of a line names it, from 1 for the header: `tree list line 3: not sorted: "a" follows "b"`. An object that does not end with a newline is refused at its last line, `no newline at the end`.

The object is named `cas/l/<SHA-256 of its bytes>` (`tierstore::tree_list_key`) and written create-only, so a client that writes the same tree again finds it there. A reader given the bytes and the key refuses bytes that hash to another digest (`tree list digest mismatch`), and refuses an object over 256 MiB (`tree list too large`) before it hashes it. `TreeList::encode` writes each entry's members in the order of the example, leaves out what is not set, and sorts the entries, so the same entries give the same bytes and the same key.

The mount provider needs a `sha256` on every file of size above 0: a node-local tree's digest covers each file's contents, and each range is checked against its digest as it is filled ([the design](https://github.com/komira-ai/tierstore/blob/main/docs/design/node-local-trees.md), decision D5). A list with a file of bytes and no `sha256` is read, and refused by the provider: `file "<path>" has no sha256`.

The golden test `tests/stored_formats/tree_list.rs` pins the bytes of an example (`fixtures/tree_list_v1.jsonl`) and its digest, and reads the same list as a later writer may write it, with members this reader does not know (`fixtures/tree_list_v1_newer.jsonl`). A tree list and a version 1 manifest are both under `cas/l/`, and neither reader takes the other: a manifest has no header line, and a tree list no `v`.
