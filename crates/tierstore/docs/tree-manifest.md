# Tree manifest format

A tree manifest lists the children of one directory of a tree. A whole tree is a root manifest plus one manifest for each directory below it, each named by the SHA-256 of its bytes. This page is the reference for version 1, as the library reads it (`tierstore::parse_manifest` and `tierstore::flatten`). No process of tierstore reads or writes a manifest yet.

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
