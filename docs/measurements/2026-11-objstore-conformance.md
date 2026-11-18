# ObjectStore conformance, November 2026: the suite, the tierstore target, and a SeaweedFS reference run

This report covers task T-S0 of `docs/design/fetch-cache-serve.md` (section 18). Its first part is
the `ObjectStore` conformance suite: the crate `crates/objstore-conformance` and the targets it runs
against in CI. Its second part is the optional reference run of 17.2 item 6: the same suite against
a single-node SeaweedFS `weed server -s3`, with the result read against 10.12.

**In short.** The suite checks what 10.9 asks of the store's S3 subset: create-only writes, `412` on
different bytes, ranges, HEAD, DELETE and LIST. Four targets run it. The in-memory model and the
loopback fake S3 run it in `objstore-conformance`'s tests. tierstore's current S3 front runs it in
`tierstore`'s tests, and its LIST is checked again after a restart. SeaweedFS 4.54 ran it once on a
farm node. Every CI target passes every case. SeaweedFS passes 9 of 10 cases, among them create-only
under 8 concurrent writers. It fails LIST, and the cause is not LIST. A conditional PUT of key `a`
is refused with `412` when no object `a` exists but a key `a/b` does: SeaweedFS's filer reads `a`
as a directory. Its LIST is exact before and after a restart.

## The suite

The suite takes any `objstore_s3::ObjectStore`, the trait the CAS service writes packs through. It
also takes a declaration of what the store answers when a key is written again with the bytes it
already holds. An S3 bucket with conditional writes answers `412` (`refused`). The storage tier's
front will answer `200` (`accepted`, 10.9). Each case runs under its own prefix of a root. Each case
gives `PASS` or the first violation it found, and the violation names the case, the call, the key,
and what was wanted against what came back. When a case ends, the suite deletes every key under its
prefix and checks that the prefix then lists nothing.

| Case | What it checks |
|---|---|
| `claims` | The store claims conditional writes and not Object Lock. A retention date is refused (`Unsupported`), and the refusal leaves no object. |
| `create-only` | A write of different bytes to a used key is refused (`AlreadyExists`, which is `412`), whether the bytes are the same length, one byte longer or one byte shorter, and whether written whole or streamed. The key keeps its first bytes. |
| `identical-put` | A write of the bytes the key holds gets the declared answer, whole or streamed, and the bytes stay. |
| `create-race` | 8 concurrent writers of different bytes to one key: exactly one succeeds, the other 7 get `AlreadyExists`, and a read gives the winner's bytes. 8 concurrent writers of the same bytes: all 8 succeed (`accepted`) or exactly 1 (`refused`). |
| `streamed` | A streamed write stores exactly its bytes. A body that ends short, runs long or fails is refused (`Body`), and the key is then absent from both HEAD and GET. |
| `range` | Reads of 8 windows of a 1000-byte object are byte-exact, and a window that runs past the end is cut there. A range that starts at or past the end is `InvalidRange`. An absent key is `NotFound`. |
| `head` | HEAD gives the lengths 0, 1 and 4096. An absent key is `None`. A read of the empty object is `InvalidRange`. |
| `delete` | A delete of an absent key succeeds. After a delete, the key is gone from GET, HEAD and LIST. A second delete succeeds. The key can then be written again with other bytes. |
| `list` | 10 keys are written, 3 are deleted, and 2 are written again (one with other bytes, one with the same). The listing is then exactly the live keys with their sizes, in byte order: the empty object, `a` beside `a/b`, and `x y/café` included. Each key still reads its bytes. A listing of the narrower prefix `k1` gives `k1` and `k10` and not `k2`. A prefix that holds nothing lists nothing. |
| `pages` | 5 keys are listed with `max_keys` 1, 2, 3, 5 and 1000. No page is longer than its `max_keys`, and the pages joined are the listing. A listing after `p1`, after the absent key `p1x`, and after the last key starts right after it. |

`LiveSet` is the `list` case split in two, so that a target can restart its store between the
writes and the check. That is the check 10.9 asks for "across a node restart".

**The suite is checked by its own negative cases.** `crates/objstore-conformance/tests/model` breaks
an in-memory store in 23 ways, one at a time: a store that overwrites a used key, that checks and
then writes (so all 8 concurrent writers win), that keeps a short streamed body, that cuts a read one
byte short, that lists a deleted key, that ignores `max_keys`, that never ends a listing, and more.
For each one the test asserts the whole text of the violation the matching case reports.

## Targets in CI

| Target | Where | Declared | Result |
|---|---|---|---|
| In-memory model, conforming | `objstore-conformance` tests/model | `accepted` and `refused` | every case passes |
| Fake S3 over loopback (`objstore_s3::s3::fake`) | `objstore-conformance` tests/fake_s3.rs | `refused` | every case passes; `LiveSet` through a second client |
| tierstore's S3 front over loopback, through `S3Store` | `tierstore` tests/s3_front/conformance.rs | `refused` | every case passes; `LiveSet` across a restart of the server over the same tiers |

The tierstore target runs the entry reconciler beside the front, as a node does. A DELETE hands
the removal of its entry to the reconciler, and a second DELETE or a PUT of the same key waits until
the reconciler has finished it. Without the reconciler, the `delete` case's second DELETE ran into
the client's request timeout, and the `list` case's re-PUT got `503`. That was measured once while
writing the test. It is how the front is built, not a defect.

The storage tier's own S3 front (T-S6) is a later target of the same suite, declared `accepted`. It
adds "with one node down" to the `LiveSet` check.

## Reference run: SeaweedFS `weed server -s3`, single node

**Setup.** One farm node, on loopback, in a scratch directory.

- SeaweedFS 4.54, from the release asset `linux_amd64.tar.gz`
  (`https://github.com/seaweedfs/seaweedfs/releases/download/4.54/linux_amd64.tar.gz`, SHA-256
  `e7424a82eee327809bfa449fda4924c9a0f42f6669a01288eebb1b79674c8196`, checked before use).
- One process: `weed server -s3 -ip=127.0.0.1 -ip.bind=127.0.0.1 -dir=<scratch>
  -volume.max=4 -master.volumeSizeLimitMB=256 -s3.config=<identities>`, with one identity
  that has the actions `Admin`, `Read`, `Write`, `List` and `Tagging`.
- The runner, built in release mode at this change:
  `objstore-conformance --endpoint http://127.0.0.1:<port> --bucket conformance
  --access-key-id <id> --secret-access-key-file <file> --prefix runs/<n>/ --identical <answer>
  [--create-bucket]`.

**Results.** There were three runs. Run 1 created the bucket and declared `refused`. Run 2 declared
`accepted`. Run 3 declared `refused`. The run with the matching declaration and the one with the
wrong declaration both behaved as follows.

| Case | Run 1 and run 3 (`refused`) | Run 2 (`accepted`) |
|---|---|---|
| claims | PASS | PASS |
| create-only | PASS | PASS |
| identical-put | PASS | FAIL: the identical PUT got `412` (`AlreadyExists`) |
| create-race | PASS (1 of 8 different-byte writers, 1 of 8 same-byte writers) | FAIL: 1 of 8 same-byte writers succeeded, 8 were declared |
| streamed | PASS | PASS |
| range | PASS | PASS |
| head | PASS | PASS |
| delete | PASS | PASS |
| list | FAIL: `put_new of runs/<n>/list/a failed: object runs/<n>/list/a already exists` | the same |
| pages | PASS | PASS |
| ETag | `"abb51e32e3af80762d4685f5829d79ef"`, the MD5 of the body | the same |

**The LIST failure, isolated.** The probes below used `curl --aws-sigv4` against a fresh data
directory. Each line is one request and its answer, measured.

| Probe | Answer |
|---|---|
| PUT `x/a/b` with `If-None-Match: *` | `200` |
| PUT `x/a` with `If-None-Match: *` (no object `x/a` exists) | **`412 PreconditionFailed`** |
| PUT `x/a` with no condition | `200`; GET `x/a` then gives its bytes |
| PUT `y/a`, then PUT `y/a/b`, both with `If-None-Match: *` | `200`, `200`; LIST `y/` gives `y/a`, `y/a/b` |
| PUT `y/a` again with the same bytes and `If-None-Match: *` | `412` |
| PUT `k1`, `k10`, `k2`, `x y/café` and `e`, then DELETE `e`; LIST `z/` | exactly `k1`, `k10`, `k2`, `x y/café` |
| LIST `z/k1` | `k1`, `k10` |
| LIST `z/` with `max-keys=2` | `k1`, `k10`, truncated |
| PUT `e` with other bytes after its DELETE, with `If-None-Match: *` | `200` |
| restart the process on the same data directory, LIST `z/`, GET `e` | `e`, `k1`, `k10`, `k2`, `x y/café`; `e` gives the new bytes |

The cause is the conditional PUT. SeaweedFS's filer keeps keys as paths, and its `If-None-Match: *`
check counts the implicit directory `x/a/` as an existing `x/a`. LIST itself, a re-PUT after a
DELETE, and LIST after a restart all matched the suite.

**Not run.** These parts of 17.2 item 6 were not run, and the reasons follow.

- "Behaviour with the lock service down": a single `weed server` runs master, volume, filer and S3
  in one process, so there is no separate lock service to stop.
- The two-node `replication 001` whole-pack read with and without vacuum, replaying a day of the
  PUT and GET mix with 1 GiB packs: this needs two idle storage nodes, and the storage nodes come
  with T0-S. It is a benchmark and not a conformance check, and it stays with 17.2 item 6 for when
  those nodes exist.

**What this means for 10.12** (inferred, not measured). As a fallback store, SeaweedFS 4.54 gives the
CAS service the contract it needs (create-only writes under concurrency, ranges, HEAD, DELETE, LIST),
with one condition. No key the CAS service writes may be followed by `/` in another key, or the
create-only write of the shorter key is refused although nothing holds it. This report does not
check the CAS service's key layout against that condition. SeaweedFS answers `412` to an identical
re-PUT as S3 does, where the storage tier answers `200`. A CAS writer that retries a PUT whose
answer it lost therefore has to treat `AlreadyExists` as success on SeaweedFS, as it already must on
S3. The ETag is the MD5 of the body, not a content generation.
