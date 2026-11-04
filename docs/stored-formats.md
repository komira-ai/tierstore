# Stored formats

tierstore keeps a few small objects and files of its own. A new release reads what an older one wrote, and an older release must still read what a newer one writes while the two share a tier during an update. So these formats change only by adding.

## What is stored

| What | Where | Format |
|---|---|---|
| Journal object | `_journal/<proxy_id>/<seq>` in the hot tier, `seq` in 20 digits | JSON list of entries: `op` (`PUT`, `DELETE`, `PUTRESERVED` or `PUTMIRROR`), `key`, `size`, `time`, and the optional `cold_tag` and `hot_tag` (see below) |
| Proxy marker | `_tiering/proxies/<proxy_id>` in the hot tier | empty object |
| Checkpoint | `_tiering/checkpoint/<proxy_id>` in the hot tier | JSON: `seq`, `evict_seq`, `evict_idx` |
| Usage | `_tiering/usage` in the hot tier | JSON: `hot_bytes`, `copy_lag`, `time` |
| Lease | `_tiering/lease` in the hot tier | three lines: the holder, a nonce, the count of bodies written |
| Cache sidecar | `obj/<aa>/<bb>/<hex>.meta` in the local cache directory | JSON: `key`, `size`, `sha256`, `tag`, `modified` |
| Update hook record, two slots (Linux) | `state.json` in the hook's state directory | pretty JSON: `schema`, `serving`, `previous`, `mode`, `markers`, `boot_id`, `guard_placed` |
| Update hook record, single instance (macOS) | `state.json` in the hook's state directory | pretty JSON: `schema`, `kind`, `updating`, `markers`, `from`, `to`, `version` |

### Journal operations and tags

- `PUT`: written by a release that holds no reservation; the object is copied to the cold tier. No tag.
- `DELETE`: the object was deleted from both tiers; `size` is zero. No tag.
- `PUTRESERVED`: a put that holds a reservation of the key in the cold tier. `cold_tag` names the reservation; the put replaces it. No `hot_tag`.
- `PUTMIRROR`: an object a read copied from the cold tier into the hot tier. `cold_tag` is the tag the cold object had and `hot_tag` is the tag the hot tier gave the copy. It is never copied to the cold tier.

Both tags are absent in the entries of releases before these operations, and `hot_tag` is absent in the mirrors of releases before that field. An entry with an operation this release does not know reads as `Unknown` and is skipped.

## The rule

A format changes only additively: a new optional field, a new operation word that a reader may skip, a new object under a new key. A field is never renamed, removed or given another meaning, and a field a reader needs is never added without a default. A reader ignores what it does not know and never refuses a file for it, with one exception: the two-slot hook record, below.

A journal object with an operation this release does not know is read, and the entry is skipped. A cache sidecar, a checkpoint or a usage object with a field this release does not know is read as if it were not there. The single-instance hook record ignores a key it does not know. The two-slot hook record does not: it refuses a key it does not know, so a key added to it raises its `schema`, which an older hook reads as newer than it understands and leaves alone. A test holds that: a record of schema 2 with a new key reads as newer, not as corrupt.

## The fixtures

The bytes of each format as a release wrote them are in `crates/tierstore/tests/stored_formats/fixtures/`. The tests in `crates/tierstore/tests/stored_formats/` use them in two ways. For a fixture the current release writes (each format's plain fixture), the current code reads it to the value the test expects, and writes that value as exactly the bytes of the fixture. The compatibility fixtures are read only: the `*_newer` fixtures (a key or operation added later) and the first-release hook record are read to the value the test expects and are not written back, and the newer two-slot hook record is refused as newer, not read as a value. The lease body holds a random nonce, so its test puts the fixture's nonce in before it compares.

A change that renames, removes or re-means a field fails one of those tests. Do not edit a fixture to make it pass: add a second fixture for the new field, keep the old one, and let both tests read the old fixture too. A change to an existing fixture in a diff is the thing to look at.
