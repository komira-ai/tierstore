# tierstore: an example unit

`tierstore` (`crates/tierstore`) is an S3 server over a hot and a cold tier of S3-compatible
stores: it writes new objects to the hot tier, copies them to the cold tier in the background, and
frees the hot tier when it fills. It is a release asset for Linux (x86_64, arm64) and macOS (Apple
silicon), as a tarball with its SBOM and its provenance bundle (`docs/ci-and-release.md`); there is
no package and no container image. These files, for Linux hosts, go beside the binary.

| file | what |
|---|---|
| `tierstore.service` | the unit: the binary as `tierstore` run with the file, an account that is not root, a stop that waits for the drain, and the hardening of the other example units |
| `tierstore.conf` | the sysusers.d entry of the `tierstore` account (no login, no home) |
| `tierstore.toml` | the configuration file of `tierstore.service`: one `s3` interface, the keys, and an S3 bucket for each tier |
| `tierstore-9301.toml`, `tierstore-9302.toml` | the configuration file of each slot of the hand-over: the same settings, with the slot's own port, proxy id and `slot` |
| `tierstore@.service` | the slot unit template of the update hook's hand-over: `tierstore@9301.service` and `tierstore@9302.service`, each running the binary its slot link points at, on its own port and proxy id ([Moving to the update hook](#moving-to-the-update-hook)) |
| `updater.d/tierstore.toml` | the declaration the host updater reads: the hook, its account and rights, the two slot units, the state directory and the boot entry |
| `tierstore-update.conf` | the sysusers.d entry of the hook's own account, `tierstore-update`, with `tierstore` listed in its group |
| `tmpfiles.d/tierstore-update.conf` | the tmpfiles.d entry of the hook's state directory, `/var/lib/updater/hooks/tierstore`, and its `slots` directory: mode 0750, owner and group `tierstore-update` |
| `polkit/50-tierstore-update.rules` | the polkit rule that lets `tierstore-update` start and stop the two slot units, and nothing else |

The values in the unit are examples (`192.0.2.10`, `s3-hot.example`, the bucket names, the
sizes): edit them to the setup's. `crates/tierstore/tests/packaging.rs` reads these files and
checks that the command line parses as the binary parses it and names a file that the binary's loader takes, that no secret is on it or in the file (every key
and credential names a file), that the stop timeout covers the drain, and that nothing
starts the unit by itself. It also checks the files of the update hook: that the hook's own
parser takes the declaration and that it names the ports, units, state directory and account the
slot unit is built on, that the hook holds `CAP_NET_ADMIN` hook-wide, that the slot unit parses
for both slots and covers its drain, that the hook's account is its own, that the state
directory is 0750 with no group write bit, and that the polkit rule names exactly the two slot
units and grants no right over unit files.

## Placing the binary

The release carries `tierstore-<commit>-linux-x86_64.tar.gz` and
`tierstore-<commit>-linux-arm64.tar.gz`, each with `<tarball>.sigstore.json` and listed in
`SHA256SUMS`. Take the tarball of the host's architecture by its digest, and verify it as every
tarball is verified (`docs/ci-and-release.md`). The release also carries
`tierstore-<commit>-macos-arm64.tar.gz`; the files here are for Linux hosts (a Mac's: `macos/README.md`),
and the install guide (`crates/tierstore/docs/install.md`) covers taking and verifying the macOS
binary.
The tarball holds the binary under its own name and `THIRD-PARTY-LICENSES.txt`, the licences of
the crates linked into it (the declaration is not in it yet: place it from this directory, below;
extracting `tierstore` by name leaves the notice in the tarball):

```sh
tar -xzf tierstore-<commit>-linux-x86_64.tar.gz tierstore
install -d -m 0755 /usr/libexec/tierstore
install -m 0755 -o root -g root tierstore /usr/libexec/tierstore/tierstore
```

With `tierstore.service`, nothing updates it afterwards: whoever runs the setup places the next
binary the same way and restarts the unit. A host that runs the two slot units instead is
updated by the host updater through `tierstore update-hook`
([Moving to the update hook](#moving-to-the-update-hook)).

## Placing the unit

```sh
install -m 0644 tierstore.conf /etc/sysusers.d/tierstore.conf
systemd-sysusers /etc/sysusers.d/tierstore.conf
install -d -m 0750 -o root -g tierstore /etc/tierstore
install -m 0640 -o root -g tierstore tierstore.toml /etc/tierstore/tierstore.toml
install -m 0644 tierstore.service /etc/systemd/system/tierstore.service
systemctl daemon-reload
```

Then write the four key files, each owned by `tierstore` with mode 0600 or 0400 (the process
refuses a key file that its group or others can read):

- `/etc/tierstore/access.key`: the full key, the one clients that write sign their requests to
  `tierstore` with (`server.access_key_file`). It may do everything the server serves: put, get, head,
  list, delete, and the calls of a multipart upload;
- `/etc/tierstore/daemons.key`: a read-only key (`server.read_only_key_files`), for the daemons that
  read blobs and must not write or delete them. A request signed with it may get and head an
  object, list the bucket (objects, and the open multipart uploads) and head the bucket; a put, a
  delete, CreateBucket or any call that starts, feeds, completes or aborts a multipart upload is
  answered 403 `AccessDenied`, before its body is read, and changes nothing.
  List one file for each read-only key, to give each reader a key of its own. The access key
  ids of all the keys of one server must differ: a request names its key by its id, so a start
  with two keys that have one id is refused with exit status 2;
- `/etc/tierstore/hot.credentials` and `/etc/tierstore/cold.credentials`: the keys `tierstore`
  signs its requests to the two stores with (`hot_store.credentials_file`,
  `cold_store.credentials_file`). The two tiers may be two stores, and have a key each.

Every one of these files holds its key in one of two forms, and the process tells them apart by
the first byte of the file that is not white space: `{` is JSON, anything else is two lines.

```json
{"access_key_id": "AKIDEXAMPLE", "secret_access_key": "the secret"}
```

or, as two lines, the access key id and then the secret access key:

```text
AKIDEXAMPLE
the secret
```

The JSON form is the one that the pack flags of the frontend and of the daemon read
(`--pack-store-credentials-file`), but `tierstore` is stricter than they are: it needs exactly
the two fields, both non-empty strings, and refuses a file with another field, with a field
misspelled or with an empty value. So one file holds both the key that a daemon signs with and
the key that `tierstore` checks only if it has exactly those two non-empty fields; a pack-flags
file that has another field must be trimmed to be shared. `tierstore` takes the values of the
JSON form as they are, without trimming. No message of the process carries a secret.

A role belongs to a key, not to a prefix: a read-only key may read every object of the bucket. A key limited to prefixes is a GC key or a scoped key (below).
A write limited to one object under a prefix is a presigned PUT URL (below).

## Moving to the update hook

The host updater puts a new release in place through `tierstore update-hook`
(`crates/tierstore/docs/update-hook.md`): two slot units, `tierstore@9301` and `tierstore@9302`,
run side by side during a hand-over, and an nftables redirect that the hook owns sends the public
port 9300 to the one that serves. These files go on the host, each owned by root:

| file | placed as | mode |
|---|---|---|
| `updater.d/tierstore.toml` | `/etc/updater.d/tierstore.toml` (the directory owned by root, no group or other write bit) | 0644 |
| `tierstore@.service` | `/etc/systemd/system/tierstore@.service` | 0644 |
| `tierstore-9301.toml`, `tierstore-9302.toml` | `/etc/tierstore/tierstore-9301.toml`, `/etc/tierstore/tierstore-9302.toml` (owner root, group `tierstore`) | 0640 |
| `tierstore-update.conf` | `/etc/sysusers.d/tierstore-update.conf` | 0644 |
| `tmpfiles.d/tierstore-update.conf` | `/etc/tmpfiles.d/tierstore-update.conf` | 0644 |
| `polkit/50-tierstore-update.rules` | `/etc/polkit-1/rules.d/50-tierstore-update.rules` | 0644 |

The hook runs as `tierstore-update`, an account of its own: it owns the state directory (the
record `state.json`, the lock and the slot links), and the service, which runs as `tierstore`,
can traverse that directory through the group and write nothing in it. The hook is never root:
the updater gives it `CAP_NET_ADMIN` for the redirect, and the polkit rule lets it start and stop
the two slot units. A slot link, `/var/lib/updater/hooks/tierstore/slots/<port>`, points at a
directory that holds a release's `tierstore`, owned by root and not writable by anyone else; the
slot unit runs that binary, and does not start while its slot has no link. Nothing enables a slot
unit: at boot the updater runs `tierstore update-hook restore` (the declaration's
`boot = "restore"`), which applies the redirect and starts the one slot the record names.

The slot unit takes the file of its slot, `/etc/tierstore/tierstore-%i.toml`, and a 70 s
`--drain-lb-wait`, with a `TimeoutStopSec=` of 1100. The two files, `tierstore-9301.toml` and
`tierstore-9302.toml`, differ in the port of the `s3` interface (the wildcard of both families),
`server.proxy_id` and `server.slot`, and in nothing else; `hot_store.lock_file` is the same
file in both. Copy the edits of the host's `tierstore.toml` (the bucket, the tiers, the endpoints,
`hot_store.budget`) into both, and those of `ReadWritePaths=` into `tierstore@.service`, or into a
drop-in of the template (`/etc/systemd/system/tierstore@.service.d/`), which applies to both slots.

The declaration has no `min_version` yet: the first release that carries the hook sets it.

### First install

A host that runs the single `tierstore.service` (with its drop-ins) moves to the slot units once,
in this order. It is the first-install path of the host updater: no update is in flight, and the
hook starts from a record that names one slot. The steps place files, start the new slot and run
the updater's boot entry; none of them stops the running unit until the last, and the public port
moves only once the new slot answers.

1. **The account and the state directory.** Place `tierstore-update.conf` and
   `tmpfiles.d/tierstore-update.conf`, then run `systemd-sysusers` and `systemd-tmpfiles --create`
   for them. Check that `/etc/group` lists `tierstore` in the group `tierstore-update`, and that
   `/var/lib/updater/hooks/tierstore` and its `slots` are mode 0750, owner and group
   `tierstore-update`: the hook refuses any other.
2. **The declaration, the slot unit, the slot files and the polkit rule.** Place the files as the
   table above says, copy the host's edits into the slot unit and the two slot files, and run
   `systemctl daemon-reload`.
   polkit reads its rules directory by itself.
3. **The binary as slot 9301, and the record.** Place the verified release's `tierstore` in a
   directory of its own that root owns and nobody else can write (the updater keeps releases under
   `/var/lib/updater/releases/tierstore/<digest>/`), and make `slots/9301` a symbolic link to that
   directory, owned by `tierstore-update`. Then write the record that names 9301 as the slot that
   serves, `state.json` in the state directory, owned by `tierstore-update`, mode 0600:

   ```json
   {"schema":1,"serving":9301,"mode":"handover","markers":[]}
   ```

   With no record the boot entry has nothing to restore and does nothing; with this one it points
   the redirect at 9301.
4. **The guard, before any slot runs.** The slot unit listens on the wildcard of both families,
   so load the hook's guard table first; it drops every connection to 9301 and 9302, IPv4 and
   IPv6, that neither comes over loopback nor was redirected from 9300, and changes nothing
   for port 9300. As root, give this text to `nft -f -`; it is exactly the table `restore`
   loads, so `restore` later finds it in place and leaves it as it is:

   ```nft
   add table inet tierstore_guard
   flush table inet tierstore_guard
   add chain inet tierstore_guard in { type filter hook input priority filter; policy accept; }
   add rule inet tierstore_guard in iifname "lo" tcp dport { 9301, 9302 } accept
   add rule inet tierstore_guard in tcp dport { 9301, 9302 } ct status dnat ct original proto-dst 9300 accept
   add rule inet tierstore_guard in tcp dport { 9301, 9302 } drop
   ```

   Check that `nft list table inet tierstore_guard` shows it.
5. **Slot 9301, started by hand and ready.** Run `systemctl start tierstore@9301` and wait until
   a `GET` of `/readyz` on `127.0.0.1:9301` answers 200 with a body that starts with `ready`.
   Nothing has moved yet: port 9300 is still the old unit's. If the slot never answers, run
   `systemctl stop tierstore@9301`, read its journal, mend what it says and start it again; the
   old unit served throughout.
6. **`restore`, through the updater.** Have the updater run the declaration's boot entry, as it
   does at its own start: `tierstore update-hook restore` as `tierstore-update`, with
   `CAP_NET_ADMIN`. It loads the guard table (already there: no change), points the redirect of port 9300 at 9301 and starts
   `tierstore@9301.service`, which already runs, so the start changes nothing. `restore` does not
   wait for the slot to answer: it moves the public port first and starts the unit after, and a
   start that fails leaves port 9300 pointing at a slot that does not answer. That is why step 5
   comes first.
7. **Check through the public port.** A request to port 9300 must be answered by slot 9301: the
   `slot` label of `tierstore_build_info` on `/metricz` says 9301. If it is not, or `restore`
   failed, hand port 9300 back to the old unit, which never stopped, by deleting the redirect:
   `nft delete table inet tierstore_forward`. The guard table can stay: it drops only
   connections to 9301 and 9302 that neither come over loopback nor were redirected from 9300.
   Then stop slot 9301, mend it, and go back to step 5. A host that gives up the move deletes
   the guard too, `nft delete table inet tierstore_guard`, once no slot runs.
8. **The old unit, last.** Stop `tierstore.service`, which drains, and disable it if it was
   enabled (`systemctl disable tierstore`, and remove its drop-ins and the `[Install]` drop-in
   that enabled it). From now on the slot units are the only ones, and the updater updates them.

Until step 8 the old unit and slot 9301 run side by side over the same tiers, each with a proxy
id of its own and one reconciler at a time under the lease. That holds for S3 tiers. A host whose
hot tier is a directory (`dir:`), or a build node whose cache directory the old unit writes, must
not run two instances: stop the old unit before step 5, and accept the gap.

### What these files do not do yet

These files put the slot units and the hook's rights in place; they do not by themselves give
updates without a gap. Two things outside them must hold first:

- **The releases.** `prepare` hands over only when both the running release and the new one list
  `handover-v1` in `tierstore --capabilities`; otherwise the update is a plain restart: the
  redirect moves to the new slot, the old slot is stopped, and the new one is started, with a gap.
  Builds from the commit that added the capability on list it; a release built before it does
  not, so the first update of a host from such a release is a restart, and only the updates after
  it hand over.
- **The updater.** The host updater must run the steps and the boot entry from this declaration.
  It does not yet fetch the declaration from a release: it reads the copy placed under
  `/etc/updater.d`, so the file is placed by hand as above, and replaced by hand when it changes.

**Stop budgets.** A slot stops in at most 975 s: the 70 s `--drain-lb-wait`, the 900 s drain and 5 s for the
lease, under the unit's `TimeoutStopSec=` of 1100 and the declaration's `cleanup` timeout of
1300 s. The hook itself waits at most 300 s for `systemctl stop`
([update-hook.md](../crates/tierstore/docs/update-hook.md#on-the-machine)), and `rollback` has
600 s. So a drain that runs past 300 s makes `cleanup` fail with `unit_failed` while systemd goes on
stopping the unit, and `cleanup` run again (the updater retries it) waits for that same stop and
finishes; a `rollback` that stops a slot still draining can fail the same way. Until the hook
waits for a stop as long as the unit allows, a host that needs every step to succeed the first
time keeps `--drain-deadline` at most 220s, which brings the stop within 295 s.

## Presigned PUT URLs

A client that holds a presigned URL writes one object with a plain `PUT` of the URL: the server
checks SigV4 query-string authentication as the AWS documentation has it (`X-Amz-Algorithm`,
`X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders`, `X-Amz-Signature`;
payload `UNSIGNED-PAYLOAD`; `host` signed), so any SDK presigner or `tierstore::server::presign_put`
makes one. Only a presigning key (`server.presign_key_files`, once per key, mode 0600) can sign a URL; the
full and read-only keys cannot, and a presigning key is refused for any request signed in the
header. The file holds the key and its ceiling, all required (a file without one is refused at
start):

```json
{"access_key_id": "pack-presign", "secret_access_key": "the secret",
 "ceiling": {"prefix": "cas/b/", "max_expires_secs": 600, "max_object_bytes": 134217728}}
```

A URL is good for one `PUT` of a key that starts with the prefix (after the path is decoded: no
`..`, no `.` segment, no control character, never `_journal/`, `_tiering/` or `_entries/`), with an
`X-Amz-Expires` of at most the smaller of `max_expires_secs` and `server.presign_max_expires` (default
15m), and a body of at most `max_object_bytes` (5 GiB at most, the S3 single-PUT limit; each key file chooses its own), checked on
`Content-Length` and on the bytes received (and held to `server.max_request_bytes`, 256 MiB by default, too: a key with a larger
cap needs that raised, and the smaller of the two applies; the body is streamed to the tier and
never held whole). It is refused from `X-Amz-Date` plus `X-Amz-Expires` on, with no grace. The
object appears only after a complete body, a second `PUT` of the key is `412`, and no multipart
call, `If-Match`, read, list or delete is allowed. The signature does not cover the body, so the
reader of the object must check its content. The query string is never logged.

A secret is never on the command line or in the environment: the process reads no environment
variable, and the unit sets none.

## GC keys

A GC key (`server.gc_key_files`, once per key; JSON of `access_key_id`, `secret_access_key` and `prefix`,
mode 0600, not a symbolic link, owned by the user the server runs as) is for the process that
collects the objects that presigned PUTs wrote. It signs in the header only (a presigned URL signed
with it is refused) and may do these, only for a key that starts with its prefix (which ends in `/`)
and is longer than it:

| call | allowed |
|---|---|
| ListObjectsV2 | with a `prefix` inside the key's; a list with no prefix or another is 403, not filtered |
| ListMultipartUploads | with a `prefix` inside the key's, the open uploads under it; no prefix or another is 403, not filtered |
| HeadObject | yes |
| DeleteObject | yes: from both tiers, on the entry of the key as for any delete |
| AbortMultipartUpload | of an upload of such a key |
| GetObject | no: a GC key never reads a body |
| put, part, start or completion of an upload, copy, CreateBucket, HeadBucket | no |
| `_journal/`, `_tiering/`, `_entries/`, any key outside the prefix, the prefix itself | no |

A call the key may not do is refused with 403 `AccessDenied` without its body being read. The
refusals are made in this order: the signature and key id, the method, the decoding of the path and
query (400, or 404 for an unknown bucket), the checksum headers (400 or 501), and last the key or
list prefix; so a call that is both refused and malformed may get the malformed-request error. A delete by a GC key is not
create-only; it is the only way an object leaves the prefix that presigned PUTs write.

## Scoped keys

A scoped key (`server.scoped_key_files`, once per key; mode 0600, not a symbolic link, owned by the user
the server runs as) reads, creates and deletes under prefixes that its file lists, and does nothing
else. It is for a process that must touch some prefixes of the bucket and not the rest, and that
neither replaces nor deletes what it did not write. It signs in the header, and
signs presigned GET URLs for objects under its read prefixes (see Presigned GET URLs; a URL of it for
any other method is refused). The file is JSON of exactly these five fields, all required:

```json
{"access_key_id": "frontend-packs", "secret_access_key": "the secret",
 "read_prefixes": ["cas/"],
 "write_prefixes": ["cas/f/frontend/"],
 "delete_prefixes": []}
```

A read prefix lets the key list as well.

Every prefix ends in `/` and is not empty. None may be `_journal/`, `_tiering/` or `_entries/`, be under one, be
a prefix of one, or have one as a segment (`cas/_journal/`); a file that has one is refused at
start. A list may be empty, but a key whose lists are all empty is refused, since it could do
nothing. The lists may overlap: a prefix in `read_prefixes` and in `write_prefixes` is one the key
reads and creates under. A key file that is refused (bad JSON, a missing or unknown field, a bad
prefix, a mode other than 0600) exits 1 at start with a message that names the key and the file and
never the secret, as a bad GC key file does. The access key id differs from those of all other
keys: two keys with one id are refused with exit status 2 (the command line does this; a
`ServerConfig` built in code, as a library user does, is not checked for distinct ids by the server).

A call is allowed only for a key (or, for a listing, a `prefix` parameter) that starts with a prefix
of the list the call needs, after the path is percent-decoded and with no `..`, no `.` segment and
no control character in it. A key must be longer than the prefix: the prefix itself is not an object.
A key or a listing `prefix` that has `..` anywhere in it, even inside a prefix, is refused.

| call | list | allowed |
|---|---|---|
| GetObject | `read_prefixes` | yes, whole or by range |
| HeadObject | `read_prefixes` (a GC key: its list prefix) | yes |
| ListObjectsV2 | `read_prefixes` (a GC key: its list prefix) | with a `prefix` parameter inside such a prefix; no prefix or another is 403, not filtered |
| ListMultipartUploads | `read_prefixes` (a GC key: its list prefix) | the same rule: a `prefix` inside such a prefix |
| PutObject | `write_prefixes` | create-only: no `If-Match`, and `If-None-Match` absent or `*`; a key that exists is `412 PreconditionFailed` |
| CreateMultipartUpload, UploadPart, CompleteMultipartUpload | `write_prefixes` | of a key inside a write prefix, each call checked |
| AbortMultipartUpload | `write_prefixes` (a GC key: its abort prefix) | of an upload of such a key (a delete prefix does not allow an abort) |
| DeleteObject | `delete_prefixes` | from both tiers, on the entry of the key as for any delete |
| a put with `If-Match` or any other `If-None-Match`, a copy (`x-amz-copy-source`, also on a part) | none | no: a scoped key never replaces or copies |
| the calls on the bucket (CreateBucket, HeadBucket, delete) and the root, any other method | none | no |
| `_journal/`, `_tiering/`, `_entries/`, any key outside the list the call needs | none | no |

The lists are not interchangeable: a write prefix does not let the key read what it wrote (add the
prefix to `read_prefixes` too), and a read prefix does not let it write. A key with a write
prefix and no read prefix can still learn that a key exists, by the `412` of a put over it. A
call the key may not do is refused with 403 `AccessDenied` without its body being read. The refusals are made in the order
of the GC key's (see above): the signature and key id, the method, the decoding of the path and
query, the checksum headers, and last the key, the listing prefix and the conditions of the put; so
a call that is both refused and malformed may get the malformed-request error.

Two uses, for clients that keep their objects in a tierstore bucket:

- **A pack store** (a client set up with an endpoint and a credentials file in the JSON form of
  the key above). The client reads packs that other processes wrote under `cas/` and writes its
  own packs under `cas/f/<writer>/`, where `<writer>` names it, one key
  `cas/f/<writer>/<boot>/<sequence>` for each pack. It writes with a plain `PUT` with
  `If-None-Match: *`, one request for a pack, and reads with ranged `GET`s. So the key is
  `read_prefixes: ["cas/"]`, `write_prefixes: ["cas/f/<writer>/"]`, `delete_prefixes: []`. With no
  delete prefix, two best-effort deletes the client makes are refused with a logged warning and
  change nothing: the removal of its create-only self-test object
  (`cas/f/<writer>/<boot>/selftest`, one small object left for each start of the client) and of
  the object of a pack whose write failed. To have both removed, give
  `delete_prefixes: ["cas/f/<writer>/"]`: the key then deletes only what the client itself wrote.
- **An append-only history** (a writer set up with `--history-objects
  's3://<bucket>/history?endpoint=...&conditional-put=true'`, the key
  pair in `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`). The history objects are under
  `history/`. The writer makes only create-only single `PUT`s with `If-None-Match: *`
  (without `conditional-put=true` it is a plain `PUT`, and tierstore answers a retry of a chunk
  whose first reply was lost with `412`, which the writer does not take for "already there", so it
  retries the chunk for good: keep `conditional-put=true`) and the reader makes only ranged
  `GET`s; neither lists, deletes, overwrites or starts a multipart upload (the code makes no list or
  delete call at all, so the expiry of old objects is for a lifecycle rule of the bucket, not the
  server, and a sweeper that deletes would need `delete_prefixes: ["history/"]`). So the key is
  `read_prefixes: ["history/"]`, `write_prefixes: ["history/"]`, `delete_prefixes: []`.

## Presigned GET URLs

A presigned GET URL lets a client that holds no S3 key read one object for a short time with a
plain `GET` of the URL. The process that starts a job signs it with a key that may read (SigV4
query-string authentication, the algorithm of every AWS SDK presigner); tierstore checks it. No
new flag and no new key file: the key is the full key, a read-only key or a scoped key the server
already has. It works on a storage node and on a build node.

The URL carries `X-Amz-Algorithm`, `X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`,
`X-Amz-SignedHeaders` (`host` at least) and `X-Amz-Signature`; the payload is the literal
`UNSIGNED-PAYLOAD`. It has the rights of its key and no more: a URL of the full or a read-only key
reads any object a header-signed request of that key reads, and a URL of a scoped key reads an
object under one of its `read_prefixes` and is `403` for any other. A GC key and a presigning key
sign no read URL (a URL naming either is refused). A URL is good from its `X-Amz-Date` for at most
`server.presign_max_expires` (default 15 minutes), counted on the server's clock, so a URL made by a
service whose clock runs ahead of the server's must be made shorter than the cap by at least that
skew. It is a bearer credential: whoever holds the URL can replay it until it expires, and on
plain HTTP anyone on the path can read it, so use TLS where that matters.

A valid URL allows `GET` or `HEAD` of one object, whichever method it was signed for, with or
without a `Range` header (the header is not signed, so one URL serves any number of ranges: that
is how a large object is fetched in parallel parts and put together). Everything else is `403
AccessDenied` before any body is read:

| request | answer |
|---|---|
| GET or HEAD of an object the key may read, whole or ranged, with the method the URL was signed for | served |
| a scoped key's URL for an object outside its read prefixes | 403 |
| PUT, POST or DELETE of anything (parts too) with the URL of a key that reads | 403 |
| a listing, a call on the bucket or the root | 403 |
| GET with `partNumber`, `uploadId`, `uploads` or any other parameter than the seven above and `x-id` | 403 |
| a URL that is expired, has `X-Amz-Expires` of zero or over the cap, is malformed, is signed with another secret, names a key id the server does not have, or was edited after it was signed | 403 |

A program in Rust that links the `tierstore` library with its `server` feature makes the URL with
`tierstore::server::presign_get(&key, &request)`, as `presign_put` makes a PUT URL (see Presigned
PUT URLs); any SDK presigner makes the same URL.

## The configuration file

`tierstore --config <path>` reads one TOML file, the only place the data path is described: the
interfaces it serves (`[[interface]]`), the tiers, the key files, the limits and the reconciler's
settings. The process takes no flag for any of it: the flags of earlier builds that did are
refused at the start, with exit status 2 and the key that replaces each. The unit above gives the
file and the two flags of the drain, and nothing else. `crates/tierstore/docs/configuration.md`
lists every key with its default and what the loader refuses; this section has what only the
packaging needs. The example for the unit is `tierstore.toml`, placed as
`/etc/tierstore/tierstore.toml`; it lists one `s3` interface, the full and one read-only key, and
an S3 bucket for each tier. `crates/tierstore/tests/packaging.rs` loads it with the binary's own
loader.

A file holds no secret, and a message about it repeats no value except a `[[policy]]` prefix (a
prefix is a key name, not a secret) and the address of an interface that is listed twice: a syntax
refusal names the line, the column and the key; any other refusal names the key. A number above
9223372036854775807 (the largest TOML integer) cannot go in a file.

A file has one of three shapes, told apart by its tables: a storage node (`[server]`,
`[hot_store]` and `[cold_store]`, `[[interface]]` entries (an `s3` one is needed, an `oci` one,
the registry front, is optional) and `[[policy]]`), a build node (`[server]`,
`[local_cache]` and `[cold_store]` with no `[hot_store]`, the same interfaces, and `[[policy]]`),
which is a read cache (see "A build node" below), and a storage node with a cache in front of its
own proxy (all three tables; see the end of "A build node").

### A build node

A file with `[local_cache]` and `[cold_store]` and no `[hot_store]` runs a read cache in front of
one upstream: typically an S3 endpoint that is the tierstore of a storage node, read with a
read-only key. It has no entries, no lease and no reconciler; the only things that run beside the
front are the fills that a miss starts and the eviction pass. It is not a tier that is written to:
every call but GET and HEAD (a put, a part, the start or completion of an upload, a delete, a copy,
CreateBucket) is refused 403 `AccessDenied` with a message that says this is a read cache, before
the body is read. The key files of `[server]` are those of the front, as for a storage node.

With `write_through = true` in `[local_cache]` (default `false`) the node takes the writes its keys
may do and sends each to the upstream; the answer is given only after the upstream has the object,
and a create-only put under a `cache = true` policy is also kept on the node, so the next read of
it is a hit (a multipart upload is not kept: the first read fills it; see "What a put keeps"
below). Create-only
puts (with or without `If-None-Match: *`), presigned PUTs, multipart uploads (start, part,
completion, abort) and deletes go to the upstream; the roles of the full, read-only, scoped,
presigning and GC keys apply as on a storage node, and a put that replaces (`If-Match`) and a copy
are still refused. A create-only put makes one HEAD of the key at the upstream before the body is
read, and answers 412 at once if the object is there; the upstream's conditional put is the
arbiter of two writers. A full upstream (507) is 503 `SlowDown` with `Retry-After`; an upstream that
does not answer is 503. A delete drops the cache's copy and then deletes at the upstream, and a fill
that was reading the object does not keep it; the completion of an upload drops the cache's copy of
the key. Listings leave out the keys under `_journal/`, `_tiering/` and `_entries/`, always. The upstream key
must now be one that writes: with `write_through` the start does not test the upstream (tierstore
verifies nothing of the stores it is given; see "What tierstore requires of a store" below), so a
key that may not write shows as the failure of the first write, and an upstream that cannot be
reached as the error of the first request. The upstream's key is also the cache's: only the front's roles keep one client from another's objects.
The start line then says `mode=write-through-cache`.

What a put keeps. The body of a create-only put under a `cache = true` policy is copied into
`.stage/` as it streams to the upstream. When the upstream has acknowledged the object, the node
asks it for the object's tag and time (one HEAD, so the tag in the cache is the one the upstream
gave, which for a large body is not an MD5), puts the copy in the cache, and then answers 200. The
copy is best effort and never fails the put: if every keep slot is busy (`max_concurrent_keeps`),
the object does not fit the budget, the stage directory cannot be written, the HEAD fails, the disk
falls behind the body, or the object was deleted meanwhile, the copy is dropped, the put is still
200 with the object at the upstream, and the skip is counted. The body is written to the stage file
by a task of its own through a queue of at most 16 chunks and 4 MiB (the bytes bound the memory,
whatever the size of the chunks; at most `max_concurrent_keeps` times 4 MiB in all), so a slow or
hung disk never makes the upload wait: the copy is dropped instead. Once the upstream has
acknowledged the object, the rest of the keep runs on its own task, so a client that goes away at
that point does not leave a half-made copy. The put waits for that task for `keep_wait` and is then
answered; the task ends within `keep_deadline` with the object kept or abandoned. A read of a key
whose copy is still being made never waits for it: it is answered from the upstream. The eviction
pass that a kept object may call for runs on its own task, and the put does not wait for it. A link
that is under way when a copy is abandoned cannot be called back and is undone when it ends; if its
thread never returns (a disk that hangs for good) the staged file and a reference to the cache stay
until the process ends, and the undoing may remove the entry of a fill of the same key made after
the abandonment, which costs one more trip to the upstream and never serves stale data.
A key named by its content (`cas/l/<sha256>`) is hashed as the body streams, copied or not;
a body that does not hash to the name is answered 400 `BadDigest`, the upstream stores nothing, and
nothing is kept. A body that is cut leaves nothing at the upstream or in the cache.
`/metricz` has `tierstore_cache_writes_through_total` (puts the upstream took),
`tierstore_cache_write_exists_total` (puts answered 412) and
`tierstore_cache_write_keep_skipped_total{reason}` with the reasons `budget` (also a body of no
declared length), `io`, `busy`, `head`, `deleted`, `slow` (the disk did not keep up), `deadline` (the copy was abandoned) and `read_only` (the cache does not hold the lock of its directory, so it copies nothing);
`tierstore_cache_keeps_expired_total` counts the puts answered before their copy was done.

```toml
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9400"] }

[server]
proxy_id        = "tierstore-cache-1"
bucket          = "tierstore"
access_key_file = "/etc/tierstore-cache/access.key"

[local_cache]
path       = "/var/lib/tierstore-cache"
layout     = "digest-v1"
budget     = "40GiB"
high_water = 90
low_water  = 75
critical   = 95
free_floor = 10
scrub_rate = "50MiB/s"

[cold_store]
kind             = "s3"
bucket           = "tierstore"
endpoint         = "http://s3-upstream.example:9300"
credentials_file = "/etc/tierstore-cache/upstream-read.key"

[[policy]]
prefix = "cas/b/"
cache  = true

[[policy]]
prefix = "cas/d/"
cache  = true

[[policy]]
prefix = ""
cache  = false
```

What it keeps: a key is kept if the longest `[[policy]]` prefix that holds it says `cache = true`
(and it is not under `_journal/`, `_tiering/` or `_entries/`: a GET, ranged GET or HEAD of such a key is refused with 403 `AccessDenied` without a look at the disk or the upstream, and a listing leaves them out). Every other key that is not kept, and every other listing, goes to the upstream as it is and is never kept. A HEAD, GET or ranged GET of a kept object is answered from the
disk and makes no call to the upstream. A GET of an object that is not there reads it from the
upstream in chunks of 8 MiB into `<path>/.stage/`, hashing the bytes as they pass, and links it into
`<path>/obj/<aa>/<bb>/<hex>` (raw bytes, mode 0444; `<hex>` is the SHA-256 of the key, or, for a
key `cas/l/<sha256>`, the SHA-256 in the key, which the bytes must hash to or the fill is
discarded and counted, and the read is answered with an error, since the upstream's bytes are known
to be wrong) only when the object is whole; a sidecar `<hex>.meta` holds the key, the
size, the hash of the bytes and the upstream's tag. The misses of one key share one fill. A ranged
GET of an object that is not there fills the whole object if it is at most `whole_fill_limit` (64
MiB by default), and otherwise passes the range through and keeps nothing; each such ranged read
costs the upstream a HEAD and a GET. A node that serves container layers through the registry front
(see The registry front) should raise the limit to its largest layer: a layer over it is never kept. A fill that fails (a short body, an object written over
while it was read, an upstream error, a directory that cannot be written) never fails the reader for
the cache's reasons: the reader is answered from the upstream. A kept object is never checked
against the upstream again, and a hit is not hashed again (the scrub below does that, later): put
`cache = true` only on prefixes whose objects are never rewritten (the content-addressed ones), and
note that the start reads the sizes of the files, not their bytes. Anyone who may read through the front may read everything the upstream's
read key can read, so give build clients scoped keys. The directories the cache makes are mode 0755
whatever the umask.

Memory. A fill holds one chunk (8 MiB) at a time, so the memory of the fills is at most
`max_concurrent_fills` chunks (4 by default: 32 MiB). A miss of a key with no fill running, when
that many fills are running, does not wait for one: it is served from the upstream as a
pass-through, keeps nothing, and is counted in `fills_skipped_total` and `fills_skipped_busy_total`.
The misses of a key whose fill is running share it and need no slot. A pass-through is not retried:
the object is filled by the next miss of its key that finds a free slot, so with every slot busy
for long, as under a sustained cold load, new keys are warmed more slowly (watch
`fills_skipped_busy_total`; raise `max_concurrent_fills` if it climbs). That bound is only for the
fills. A read of the cache is streamed: a hit is read from its file, a miss is given to the client
as the fill writes the bytes (every reader of a key reads the staged file through the descriptor
the fill writes it with, never a copy in memory), and a pass-through is read from the upstream a
chunk at a time, so a GET holds at most two chunks of 8 MiB in memory (the front keeps the last one
back) whatever the size of the object. The last chunk of the bytes a client asked for is held back
until the fill has checked the object, so a ranged read of an object of at most 64 MiB waits for
the whole fill before its last chunk. A `cas/l` object whose bytes do not hash to its name, or an
object written over during its fill, ends the response with an error (the connection is closed,
and a client never sees a complete answer), and nothing is kept. A write or delete through the
cache that comes during a fill, or a fill that fails for a reason of the cache, is finished from
the upstream: a reader given no byte is answered as if there had been no fill, and one given
bytes goes on only if the upstream still has the version they came from (otherwise its response
ends with an error). A read of more than one chunk from the upstream is held to the version it
began with (a HEAD before the first chunk and another before the last); the bytes of a `cas/l`
object read from the upstream this way are not hashed, as a pass-through never was. A client that
stops reading keeps the unlinked file of an object that is evicted meanwhile, with its blocks,
until the connection's idle time ends it; those blocks are outside the budget. The cache needs the upstream to give a content tag (an entity tag, a checksum) that changes with the bytes: a fill of an object whose tag is empty, and a read held to one version, are refused with `Unsupported`, since a same-size overwrite could not be seen. A storage node
still holds the bytes of a GET whole. A fill that panics or is cancelled frees its slot and its
key, and the readers that waited for it get a 503 (`fill_failures_total{reason="aborted"}`).

Scrub. A hit is not hashed again, so a background scrub reads the objects at `scrub_rate` (16 MiB a
second by default) and hashes each against the digest it must have: the name for a key
`cas/l/<sha256>`, and for any other key the `sha256` that the sidecar recorded at the fill (every
sidecar has it, so every object is checked). An object whose bytes do not match, or whose sidecar
can no longer be read, is removed from the index and the disk, counted in
`scrub_mismatches_total` and named on stderr; the next read of its key fills it again from the
upstream. The scrub walks the objects in name order, takes the names at the start of a round, and
starts over when it is done; it reads an object to its end, so one object larger than the rate is read
in one go and the next reads wait until the rate has paid for it. It never drops an object whose
fill is still ending, and never one whose file was replaced after it was read. A file or a sidecar that
cannot be read (a failing disk, out of descriptors) is left alone and counted in
`scrub_errors_total`; a sidecar that is missing or does not parse drops the object. The bytes are
hashed in chunks of 1 MiB and a shutdown ends the pass at the next chunk; inside one object the
read is not paced. A round (every object once) starts at most once in `scrub_min_round`. An
object whose check panics is counted in `scrub_errors_total` and skipped for the round. `scrub_rate = "off"`
turns it off; a restart still checks sizes only. A hard link that a job holds to a dropped object
keeps the old bytes for that job: the scrub removes the cache's name only.

Eviction keeps the cache under `budget` bytes: above `high_water` percent of it, and when the free
space of the file system (`statvfs`) is under `free_floor` percent, a pass removes the least
recently used objects until the cache is at `low_water` percent and the free space is back at the
floor. The order of use is in memory, seeded at start from the times of the files and written back
to them (once for each object each pass); an object with a link count over one is in use by another
tree, so it is skipped, counted and stays charged to the budget. A fill is skipped, and counted,
when the object is larger than the budget or would take the cache, with the fills that are running,
above `critical`. The free-space floor is a trigger of the eviction and nothing else: it does not
stop a fill, so on a disk that the jobs fill, the cache may fill the file system below the floor
until the next pass (every 10 seconds) frees it. The start refuses (exit status 2) a `budget` larger
than the size of the file system less `free_floor` percent of it; a file system whose size cannot be
read, or that reports 0 bytes, is not judged: the start warns on stderr and goes on. The files of
`.stage/` are never evicted and are removed at start. `put_permit_wait`, like `max_concurrent_puts` and `put_min_bytes_per_sec`, is accepted in
`[server]` of a build node and does nothing there without write-through, since the node answers every
put 403.

| key | what |
|---|---|
| `local_cache.path` | the directory of the cache, made if it is not there; put it on the file system that jobs' scratch is on, outside every job's mount |
| `local_cache.layout` | `"digest-v1"`, the only layout: raw bytes, hard-linkable |
| `local_cache.budget` | the bytes the cache is meant to hold: a number, or a string such as `"40GiB"`; needed |
| `local_cache.high_water` | percent of the budget above which a pass evicts (default 90) |
| `local_cache.low_water` | percent of the budget a pass evicts down to (default 75, below `high_water`) |
| `local_cache.critical` | percent of the budget above which a fill is skipped (default 95, not below `high_water`) |
| `local_cache.free_floor` | percent of the file system that is kept free (0 to 99, default 10): below it a pass evicts, even under the budget; it does not stop a fill, and `budget` may not be above the file system's size less the floor (exit status 2), unless its size cannot be read or is 0, which the start warns of and does not judge |
| `local_cache.max_concurrent_fills` | the most fills that run at once (1 to 64, default 4); a miss that finds them all running is served from the upstream and kept nowhere |
| `local_cache.max_concurrent_keeps` | the most create-only puts that are copied into the cache at once (1 to 64, default 8), apart from the fills; a put that finds them all running succeeds and is not kept |
| `local_cache.keep_wait` | the longest a put waits, once the upstream has the object, for its copy to be put in the cache (default `5s`, above 0). Past it the put is answered 200 and the copy is finished in the background; counted in `tierstore_cache_keeps_expired_total`. Safe because the upstream already holds the object: a read of it that misses costs only the trip, so read-your-write on the node is best effort past this wait |
| `local_cache.keep_deadline` | the longest a copy takes in all, from the upstream's acknowledgement (default `60s`, above 0 and not below `keep_wait`); a copy that is not done by then (a hung disk or upstream) is abandoned: its stage file is removed, its slot and room are given back, and it is counted as `deadline` |
| `local_cache.write_through` | `false` by default: a read cache that refuses every write with 403 before the role of the key is looked at. `true`: the node takes the writes its keys may do and sends each to the upstream, which must be written with a key that may write; the upstream is not tested at start |
| `local_cache.whole_fill_limit` | the most an object may be for a ranged read of it to fill the whole of it: a number or a size such as `"512MiB"`, from 1 byte to the `budget` (default 64 MiB); a larger object is passed through and kept nowhere. A fill reads 8 MiB at a time whatever the limit, so raising it does not raise the memory of the fills |
| `local_cache.cache` | `true`, the default; `false` is refused (leave `[local_cache]` out to run without a cache) |
| `local_cache.scrub_min_round` | the least time between the starts of two rounds of the scrub (default `"1h"`; at least `"1s"`, or `"0s"`: a round starts as soon as the last is done): a small cache is not read again and again at the rate |
| `local_cache.scrub_rate` | the bytes a second the scrub reads, a size and `/s` such as `"50MiB/s"` (default `"16MiB/s"`); `"off"` turns the scrub off |
| `cold_store.endpoints` | a list of exactly one endpoint, in place of `endpoint`: several are refused as not supported yet, since the cache reads from one upstream |

`[cold_store]` is the upstream: any `kind` (an `s3` bucket, with `endpoint` and a `credentials_file`
of a read-only key, in practice, or of a key that writes with `write_through`), and without `skip_checks` and the keys of the reconciler. `[server]`
is as for a storage node, without `backend_check_wait`. A `[[policy]]` with `cache = true` is needed.
The three tables `[hot_store]`, `[local_cache]` and `[cold_store]` together are a storage node with
a cache in front of its own proxy: the S3 and OCI fronts read through the
cache, the cache reads from and writes to the proxy, `write_through` is true (left out or given;
`false` is refused), and the upstream is the proxy, so the table's `write_through` row below does
not apply to it.
The cache path may not be a tier's directory, inside it or around it. The start line says
`mode=storage-cache`. The node's tables are a storage node's, and the cache's those of this section.

At start the process makes the directory, removes what a fill that was running left in `.stage/`,
reads `obj/` back (an object whose sidecar does not say what the file holds is deleted), and asks
the file system under the cache four questions with a probe object: a second link to a name that
exists is refused (`create-only`), a name made by a link shows the whole file (`atomic-visibility`),
the bytes read back are the bytes written (`raw-bytes`), and the link count reports the second
name (`st_nlink`). A file system that answers one wrongly is not served over: the process says which
check and exits with status 3. The upstream is not asked anything at start. On SIGTERM the process drains as a storage node does.

`GET /metricz` is not signed, and a bucket cannot be called `metricz`. It answers in the Prometheus
text format, `tierstore_cache_<name>`, with: `hits_total`, `misses_total`, `bytes_served_local_total`,
`bytes_served_upstream_total` (the bytes of every read that was not a hit, the read that follows a
fill included, so the byte hit ratio is the first over the sum of the two), `fills_total`,
`fills_skipped_total`, `fills_skipped_busy_total` (those of the skipped fills that found every fill
slot taken), `fill_failures_total{reason}` (`hash_mismatch`, `upstream_error`,
`short_body`, `local_io`, `changed`, `aborted`), `evictions_total{reason}` (`budget`, `floor`), `pinned_skips_total`, `scrubbed_objects_total`, `scrubbed_bytes_total`, `scrub_mismatches_total`, `scrub_errors_total`,
`discarded_at_start_total`, and the gauges `bytes`, `objects`, `fs_free_bytes` and `staging_files`.
A storage node has the proxy's counters instead, the same path on a node with the OCI front (a
storage node, or a build node with an `oci` interface) also has the OCI counters (see Counters), and
a build node with the front shows the OCI block and then the cache block; a process with none of
the three has one line that says so; the proxy's series are in "Metrics" in
`crates/tierstore/README.md`.

### The registry front

The registry front (see The OCI read front) is an `[[interface]]` of type `oci` in a storage
node's file or in a build node's, left out for a server with no front. The two shapes have the
same params and the same rules. On a storage node the front reads through the proxy; on a build
node it reads through the local cache, so a blob or a manifest under a `[[policy]]` prefix with
`cache = true` is served from the disk when it is there and filled from the upstream when it is
not, as an S3 GET is. The blobs the front reads are the keys under `<prefix>blobs/sha256/`
(`oci/` by default), so the file is refused at load, naming `params.prefix` of the `oci`
interface, unless the longest `[[policy]]` prefix that covers them has `cache = true`: a front whose
blobs are kept nowhere would send every pull to the upstream, and there is no use of that to keep
(a registry that passes through is a storage node's front). A blob over
`local_cache.whole_fill_limit` is passed through, still checked against its digest, and not kept.

A kept object is not checked again on a hit, so a blob that rotted on the cache's disk is detected
by the front when it is read whole (the response ends in an error or a cut connection, and
`tierstore_oci_digest_mismatches_total` counts it) but it is not evicted: it stays until the budget
evicts it, and every pull of it fails until then. To refill it, remove its data file and its
`.meta` sidecar under `local_cache.path`: both are named by the SHA-256 of the object's key (for
`oci/blobs/sha256/<digest>` that is the hash of that text, not the digest) and sit in
`obj/<first two hex digits>/<next two>/`. The next
read finds the file gone, drops the entry and fills the object again from the upstream.

The front is another listener of the same process: it starts with the node, drains with it,
and its counters are on the `/metricz` of the S3 listener beside the cache's. The entry, with
the params at their defaults spelled out:

```toml
[[interface]]
type   = "oci"
params = { addresses = ["192.0.2.10:5000"], tls_cert_file = "/etc/tierstore/oci.crt", tls_key_file = "/etc/tierstore/oci.key", prefix = "oci/", max_connections = 64, connection_lifetime = "1h", stage_read = 1048576 }
```

`tls_cert_file` and `tls_key_file` go together; `max_connections` is at least one;
`connection_lifetime` is a duration as elsewhere; `stage_read` is a count of bytes, at least one
(on main, not in v0.0.41, which refuses the param). The certificate and the key are named by path,
as every key file is, so the names end in `_file`. Several `oci` entries make one front on every
address of theirs, and must agree on these params.

### Moving from flags

A unit that gave the flags of earlier builds is moved by writing the file and changing
`ExecStart=` to `--config <path>` and the two flags of the drain (`crates/tierstore/docs/configuration.md`,
"Moving from flags", says where each setting went). A start that still gives one of the removed
flags exits with status 2 and a message that names the key that replaces it, so the unit stops
instead of running with settings it did not read.

## Filling in the file

`tierstore.toml` is the file to edit, and the unit does not change:

| key | what |
|---|---|
| `[[interface]]` of type `s3` | the address the S3 API, `/healthz`, `/readyz` and `/usagez` are served on: `params.addresses`, or `params.port` for the IPv4 wildcard. Further entries or addresses are served by the same server |
| `server.proxy_id` | names this process's registration in the hot tier and the entries it holds; give each process over the same tiers one of its own |
| `server.bucket` | the name of the one bucket clients see |
| `server.access_key_file`, `server.read_only_key_files` | the full key, and the read-only keys (a list); see above |
| `server.tls_cert_file`, `server.tls_key_file` | the certificate chain and the key (PEM) that make the S3 front serve HTTPS; both, or neither (plain HTTP, the default) |
| `server.min_secret_bytes` | the fewest bytes of the secret of any access key (default 16); a key file with a shorter secret refuses the start |
| `server.presign_key_files`, `server.presign_max_expires` | presigning keys with their ceilings, and the longest expiry of any URL (a presigned GET needs no key file of its own: see Presigned GET URLs); see Presigned PUT URLs |
| `server.gc_key_files` | GC keys; see GC keys |
| `server.scoped_key_files` | scoped keys; see Scoped keys |
| `server.grant_key_files`, `server.grant_max_lifetime` | deprecated and ignored (the read grant was removed; use a presigned GET URL, see Presigned GET URLs): accepted so that an old file starts; the server logs a warning and does not read the files |
| `hot_store.kind`, `cold_store.kind` | the tiers: `s3` for a bucket of an S3-compatible store (also `dir` with a `path`, which must also be writable under `ProtectSystem=strict`, so add it to `ReadWritePaths=`, and `memory`) |
| `hot_store.endpoint`, `cold_store.endpoint` | where the tier's store is, `http://host[:port]` (TLS is not built yet); `hot_store.region` and `cold_store.region` set the signing region of a tier, `us-east-1` if not given |
| `hot_store.budget` | the bytes the hot tier is meant to hold; the water marks are shares of it (`hot_store.high_water`, `hot_store.low_water`, `hot_store.critical`) |
| `hot_store.usage_stale_after` | how old the usage that the reconciler publishes may be (2m by default, longer than `hot_store.lease_ttl`) before `/usagez` says 503 with the reason |
| an `oci` interface | the registry front (read-only unless it asks for keys), off unless the file has one; see The OCI read front |

The unit's two flags are the drain on SIGTERM: `--drain-lb-wait` is how long `/readyz` says 503
while the server goes on accepting and serving (the settle), and `--drain-deadline` is how long it
then waits for requests in flight and for the connections it closes. `tierstore --help` lists the
flags of a run.

## Using a stock SDK

The front serves one bucket, path-style, over plain HTTP (over HTTPS when the file has `server.tls_cert_file` and `server.tls_key_file`), and checks SigV4 in the `Authorization`
header only. An SDK with its defaults sends some things the front refuses, so a client needs these
settings. `crates/tierstore/tests/s3_front/sdk.rs` drives the front with aws-sdk-rust (the
`aws-sdk-s3` crate, version 1.96.0, in this repository's tests only) set this way, and that test is
what "tested" means below.

| setting | value | why |
|---|---|---|
| endpoint | `http://<host>:<port>` of the `s3` interface (`https://` when the file has `server.tls_cert_file` and `server.tls_key_file`) | without a certificate the front has no TLS, and an `https://` endpoint fails |
| addressing | path-style (`force_path_style`) | the bucket is in the path, `/<bucket>/<key>`; virtual-host addressing (`<bucket>.<host>`) is not served |
| bucket | the `server.bucket` name | there is one; other names are not served |
| region | tierstore's own `server.region` (not `hot_store.region` or `cold_store.region`, which are for the tiers' stores), `us-east-1` by default | the region is in the credential scope of the signature, and a client with another is refused 403 `InvalidAccessKeyId` |
| credentials | a static key id and secret: the full key, or a read-only key | the front has no session tokens, no STS and no presigned URLs |
| request checksums | only when an operation requires one (`request_checksum_calculation = when_required`) | the SDK default sends a checksum with every upload, and for a streamed body sends it as a trailer in an `aws-chunked` body, which the front refuses with 501 |
| response checksums | only when required (`response_checksum_validation = when_required`) | the front sends no checksum on a download, so there is nothing to validate; this keeps the SDK from asking |
| checksum algorithm | none, or CRC-32, CRC-32C, SHA-1 or SHA-256 | read from the front's code, not tested with an SDK: CRC-64/NVMe, `aws-chunked` and trailing checksums are refused with 501; `Content-MD5` is checked |
| `x-id` | nothing to set | the AWS SDKs add `x-id=<Operation>` to the query of the calls of an object, and no setting stops it; S3 ignores it, and so does the front |
| listing | no `encoding-type` | `encoding-type=url` is refused with 501 (aws-sdk-rust sends it only when asked to) |

Payload signing, read from the front's code and not tested as such (not checked: whether the Rust
SDK signs the hex hash or `UNSIGNED-PAYLOAD` for an in-memory body): a body whose signed `x-amz-content-sha256` is its hex SHA-256 is checked against it;
`UNSIGNED-PAYLOAD` with no checksum header is accepted unchecked; a `STREAMING-` payload is
refused with 501.

aws-sdk-rust, as the test sets it (the test also passes the SDK's HTTP client built without TLS,
`aws_smithy_http_client::Builder::new().build_http()`, which the lines below leave out):

```rust
use aws_sdk_s3::config::{Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation};

let config = aws_sdk_s3::Config::builder()
    .behavior_version_latest()
    .endpoint_url("http://tierstore.example:9000")
    .force_path_style(true)
    .region(Region::new("us-east-1"))
    .credentials_provider(Credentials::new(KEY_ID, SECRET, None, None, "static"))
    .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
    .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
    .build();
let client = aws_sdk_s3::Client::from_conf(config);
```

With these, the test puts an object, gets it whole and by byte ranges, heads it, lists with a prefix
and a continuation token, deletes it, uploads in parts (create, two parts, complete) and aborts an
upload. A second put of a key, with `If-None-Match: *` or without, comes back as an error with
status 412 and code `PreconditionFailed`. A read-only key gets, heads and lists, and its put and
delete come back as 403 `AccessDenied`.
The aws CLI and boto3 are **not tested**: the lines below are from their documentation, and
whether they are enough is not shown. They need a version that has the checksum settings (aws CLI
2.23 or later, boto3 1.36 or later).

```ini
# ~/.aws/config
[default]
region = us-east-1
endpoint_url = http://tierstore.example:9000
request_checksum_calculation = when_required
response_checksum_validation = when_required
s3 =
    addressing_style = path
```

```python
# boto3
import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="http://tierstore.example:9000",
    region_name="us-east-1",
    aws_access_key_id=KEY_ID,
    aws_secret_access_key=SECRET,
    config=Config(
        signature_version="s3v4",
        s3={"addressing_style": "path"},
        request_checksum_calculation="when_required",
        response_checksum_validation="when_required",
    ),
)
```

One thing is known from the front and not from those clients: boto3 and the aws CLI are said to
add `encoding-type=url` to a ListObjects or ListObjectsV2 call on their own, with no setting to
stop it, and the front answers that parameter with 501 `NotImplemented`. If that is so, `aws s3
ls` and `list_objects_v2` fail against the front, and the calls that name a key (`get_object`,
`put_object`, `head_object`, `delete_object`, multipart) are not affected. Check it with a real
client before relying on either.

A put or a CreateMultipartUpload of a key with a character XML 1.0 forbids (U+0000-U+0008, U+000B,
U+000C, U+000E-U+001F, U+FFFE or U+FFFF) is answered 400 `InvalidArgument` and stores nothing: the
front's listings are XML 1.0, which cannot carry those characters, and with no `encoding-type=url`
there is no other way to list such a key. Tab, line feed, U+007F and the C1 controls are allowed.

## The OCI read front

With an `oci` interface (`[[interface]]` with `type = "oci"` in the file), the process also
serves the objects of the store as a read-only container registry, over HTTPS or over plain HTTP
(see Two modes), so that podman (and the other clients of the OCI distribution API) pulls an image
by digest from the same hot and cold tiers the S3 front writes to. It reads through the proxy in
the process: no S3 credential and no hop through the S3 listener. It is off unless the file has
such an interface, and then nothing else about the process changes.

The front is the `oci` interfaces of the file (see The configuration file). A build node's file
takes the same interfaces: the front then reads through the node's local cache, not a proxy (see
The registry front). The params of the entry:

| param | what |
|---|---|
| `port`, `addresses` | where the registry front listens (one of the two): `port` is the IPv4 wildcard, `addresses` a list. With `params.tls_cert_file` and `params.tls_key_file` it is HTTPS; with neither it is plain HTTP, at any address; off loopback (127/8 and `::1` are loopback; `0.0.0.0` and `::` are not) the start prints one warning line on stderr for each such address, and a certificate and key are advised |
| `params.tls_cert_file`, `params.tls_key_file` | both or neither (one without the other is refused, exit 2). The certificate chain (leaf first) and the private key, PEM. Clients dial an IP address, so the leaf must carry an IP subjectAltName for each address of the interface; the certificate and key are read once at start, so a new certificate takes a restart |
| `params.prefix` | the key prefix, ending in `/`, `oci/` if not given. A prefix inside `_journal/`, `_tiering/` or `_entries/`, or one that is not plain path segments, is refused at start (exit 2) |
| `params.max_connections` | connections open at once, 64 if not given; the others wait in the kernel's backlog. Memory is bounded by connections times the buffer a connection holds (up to 4 MiB), about 256 MiB at the default |
| `params.connection_lifetime` | the longest a connection lives, 1h if not given; a request being answered is finished first |
| `params.stage_read` | the most bytes of a stored chunk that the end of a push reads in one call, 1048576 (1 MiB) if not given; at least 1. On main, not in v0.0.41, which refuses a file with it |
| `params.require_auth` | `true`: every request must carry `Authorization: Basic` with an access key id of the server and its secret, else 401 with `WWW-Authenticate: Basic realm="tierstore"`. The full key and the read-only keys pull; scoped keys pull with a read prefix and push with a write prefix over `<prefix>blobs/sha256/`; the full key pushes; a key without the right is 403. Without it the front is read-only. Refused at start (exit 2) on plain HTTP off loopback. `false` if not given: anonymous |

A client has 10 seconds to finish the TLS handshake and then the S3 front's header timeout (a
minute) to send the head of a request, which is also how long an idle connection may wait for its
next request. A connection holds one of the `params.max_connections` slots from accept to close and
there is no limit per client address, so a host that opens that many connections and says nothing
can keep the others waiting: the host's firewall rules are the control for that. A client that takes no byte of an answer for `server.body_idle_timeout`
is cut. A connection that reaches its lifetime finishes the answer it is sending for at most
`--drain-deadline`, and is cut then. On the drain the front goes on serving through `--drain-lb-wait`, takes no new connection,
lets the requests being answered finish, and closes what is left at `--drain-deadline`. The start
line on stdout gets ` oci-listen=<address> (https)` or ` (http)` after `listen=<address>`, and `/metricz`
has `tierstore_oci_front_info{mode="https"}` or `{mode="http"}`. In plain mode there is no handshake,
so the handshake time does not apply; the other limits are the same.

### Two modes

- **HTTPS**: with `params.tls_cert_file` and `params.tls_key_file`, at any address.
- **Plain HTTP**: with neither, at any address. A node's own container engine can pull from the
  front of its own tierstore over loopback, so the pull is a connection from the host to itself and
  the traffic never leaves it. A connection to any address beyond loopback can be read and changed on
  its way: the process starts, prints one warning line on stderr, and a certificate and key are
  advised there. Loopback is reachable by every local process, and by containers that
  share the host's network namespace; the front is anonymous in either mode unless `params.require_auth` is true.

A container engine that pulls over plain HTTP has to be told it may:

- podman: mark the registry insecure for that location in `registries.conf`, e.g.

  ```toml
  [[registry]]
  location = "127.0.0.1:9500"
  insecure = true
  ```

  or pull with `--tls-verify=false`. No `certs.d` directory is needed.
- js-oci needs to be allowed plain HTTP to a loopback registry address for such a pull; that is a
  setting of js-oci, not of tierstore.

What it serves, and nothing else (any other path is 404 and any method but GET and HEAD is 405
`UNSUPPORTED` with `Allow: GET, HEAD`, both with the OCI error document):

| request | answer |
|---|---|
| `/v2/` | 200, `{}`, `Docker-Distribution-API-Version: registry/2.0` |
| `/v2/<name>/manifests/sha256:<hex>` | the manifest (at most 4 MiB), with `Content-Type` from its `mediaType`, `Content-Length`, `Docker-Content-Digest` and an `ETag` of the digest. A tag in place of the digest is read first and the manifest it points at is served; a tag nobody pushed is 404 `MANIFEST_UNKNOWN` |
| `/v2/<name>/tags/list` | `{"name","tags"}`, the tags of the name in ascending order, paged with `n` and `last` (a `Link` header names the next page); a name with no tag is an empty list |
| `/v2/<name>/blobs/sha256:<hex>` | the blob, streamed, `application/octet-stream`, with `Accept-Ranges: bytes`; one `Range` is 206 with `Content-Range`, one past the end 416 |

The `Content-Type` of a manifest is its `mediaType` when that is the OCI image manifest or index or
the Docker schema 2 manifest or manifest list. A manifest with none, or another, is served as an OCI
image manifest if it is `schemaVersion` 2 with a `config` object and a `layers` array, as an OCI
image index if it is `schemaVersion` 2 with a `manifests` array (an index may leave `mediaType`
out), and is 500 `MANIFEST_INVALID` with the reason otherwise.

A malformed digest (not `sha256:` and 64 lowercase hex digits) is 400 `DIGEST_INVALID`, a repository
name that is not valid by the distribution grammar is 400 `NAME_INVALID`, an object that is not in
the store is 404 `BLOB_UNKNOWN` or `MANIFEST_UNKNOWN`, a store that cannot be reached is 503, and
what the store said stays in the log.

### Keys, and one namespace

An object is at `<prefix>blobs/sha256/<64 lowercase hex digits>`, the same for a manifest, an index,
a layer and a config: manifests are blobs. The `<name>` in a URL is checked against the grammar and
then ignored, so there is one content-addressed namespace and any name pulls any digest. A key is
built only from a validated digest, so nothing outside the prefix can be named.

### Integrity

A manifest is read whole and its SHA-256 is checked against its digest before a byte is sent; one
that does not match is 500 `DIGEST_INVALID`, is never served and is counted. The size is asked
before the read, so a manifest replaced by an object of another size between the two can be
answered 500 `DIGEST_INVALID` once; a retry succeeds. A blob is hashed as it is
read, in chunks of 4 MiB, and its last chunk is held back until the hash is right: a blob that does
not match its digest ends the connection before the response is complete, so no client receives a
whole wrong body, and the mismatch is counted. That holds for a read of the whole blob, and for a
`Range` that covers all of it (which is still 206). A blob `HEAD` and a `Range` of part of a blob
report what the store holds and verify nothing; the client verifies the digest of what it
assembles. (A manifest `HEAD` reads and verifies the manifest.) A client that sends several ranges
(as a partial pull of a `zstd:chunked` or `estargz` layer does) is sent the whole blob with 200,
which the specification allows, and falls back to splitting it. `If-Range`, `If-None-Match` and
`Accept` are ignored.

### Anonymous reads

Unless `params.require_auth` is true, the front never asks for a token: a request with an `Authorization` header is served as one without.
Anyone who can connect can read any digest under the prefix, and nothing else. That is the intent:
the images in it are public (a public registry's images are the first thing put there), and the
network decides who can connect (the host's firewall rules). Do not put an image in
the prefix that is not meant to be read by whoever can reach the port.

### Putting an image in the store

Take the blobs of an OCI layout (the files under `blobs/sha256/` of a layout made with
`skopeo copy ... oci:<dir>` or `podman save --format oci-dir`) and put each, create-only, at
`<prefix>blobs/sha256/<hex>` through the S3 front with the full key: the layers, the config, the
manifest, and the index of a multi-platform image, all by the file's own name, which is its digest.
`index.json` of the layout is not put; the digest of the manifest (or index) to pull is in it, and
is what a client is given. A layer larger than `server.max_request_bytes` (256 MiB if not given) needs the
key raised, or a multipart upload, or a presigned PUT whose key has a `max_object_bytes` of up to
5 GiB (see Presigned PUT URLs; the ceiling prefix must cover `oci/`). An object that is already
there is refused (412), which is a retry that is safe.

Check a put with `HEAD` on the front: the `Docker-Content-Digest` of a manifest is the digest of the
bytes it read, after they were checked.

### Pulling

Over plain HTTP on loopback there is no CA to trust; see Two modes for the client settings.

podman, from a host that trusts the CA of the certificate:

```sh
# the CA that issued params.tls_cert_file, in the directory named like the registry in the reference
# (the address, with :port when the reference has one)
install -D -m 0644 farm-ca.crt /etc/containers/certs.d/192.0.2.10/ca.crt
podman pull 192.0.2.10/library/app@sha256:<manifest digest>
```

The reference is a digest or a tag: a tag is a mutable pointer, pushed with `PUT` of a manifest under it. A tag is moved by writing a new
small object, `<prefix>tags/<name>/<tag>/<20 digits>` (the `/` of a name written `~`), whose digits count down so that the newest version lists
first; no stored object is overwritten, and the tag is the newest version. Of two overlapping pushes of one tag with different digests one is
201 and the other 409 `TAG_CONFLICT` (the loser pushes again); pushes of the same digest are both 201. A pull by tag lists the versions of that tag and
reads one 71-byte object before the manifest, and `tags/list` reads every version of the name; old versions are kept (nothing deletes them yet),
one 71-byte object per move. The store must list consistently with the writes before the listing (S3 and MinIO do); on a `dir:` tier the whole
key, `oci/tags/<name>/<tag>/` and 20 digits, is one file name of at most 255 bytes. The repository name (`library/app`) is any name that
fits the grammar. Blobs and manifests are shared by digest whatever the name; tags are scoped to the name. A registry on another port than 443 is written
`192.0.2.10:8443/library/app@sha256:...` and its directory is `certs.d/192.0.2.10:8443`.

A client that allows only `https` registries on port 443 (or the port in the reference), and
registries at a private address only when told to, pulls the same references; allow the address in
that client's own settings.

### Counters

`GET /metricz` on the S3 listener (unsigned, like `/healthz`; a bucket cannot be called `metricz`)
shows, in the Prometheus text format, `tierstore_oci_requests_total{route,outcome}` (routes `ping`,
`manifest`, `blob`, `tags`, `upload`, `other`; outcomes `ok`, `partial`, `not_found`, `invalid`, `not_allowed`,
`range_not_satisfiable`, `unauthorized`, `denied`, `failed`, `conflict`), `tierstore_oci_bytes_served_total` (bytes of blobs read for
responses), `tierstore_oci_digest_mismatches_total` and `tierstore_oci_front_info{mode="https"|"http"}` (1; the mode the front runs in). They are not on the registry listener,
which serves nothing else. Without an `oci` interface the path says the front is not enabled.

## Starting it

The unit does not start at install and does not start at boot: it has no `[Install]` section,
so `systemctl enable` has nothing to link, and it is skipped while `/etc/tierstore/access.key`
does not exist. Start it by hand when the files are in place:

```sh
systemctl start tierstore
```

To start it at boot, add the section in a drop-in (`systemctl edit tierstore` takes
`[Install]` and `WantedBy=multi-user.target`) and run `systemctl enable tierstore`; that is
the operator's choice, and the shipped file does not make it.

## Stopping it

systemd sends SIGTERM. The process answers `/readyz` 503 and puts `Connection: close` on every
answer from then on, serves for `--drain-lb-wait` (the settle) so that a load balancer sees it,
stops accepting (the connections the kernel has already set up for it are taken and served, for
3 seconds at most, as is the first request of a new connection), and
closes each idle connection with a lingering close (it shuts its end for
writing, so that the client reads a clean end of stream, and reads until the client closes, for 60
seconds at most). A request already on the wire is served, with `Connection: close`, never
answered 503; one that arrives within a round trip of the end of the stream cannot be answered
and is counted in `tierstore_drain_late_requests_total`. It waits for the requests in flight and
the connections for at most `--drain-deadline`, gives up the reconciler's lease and exits 0.
`TimeoutStopSec=` is set above
the sum of the two drain flags and the lease's wait, so that systemd does not send SIGKILL to a drain
in progress; if `--drain-deadline` is raised, raise it too. A start that fails is tried again
after 30 seconds; exit status 2 (a command line or a file the process refused: an unknown flag, a flag of the data
path that went, a file that does not load; or two keys with one access key id) is not, since a
restart cannot mend it. Other combinations of settings and key files refused after parsing exit 1, and are tried again
every 30 seconds.

## What tierstore requires of a store

tierstore is a proxy over a hot store and a cold store. It does not test either store, at start or
later, and writes no probe key: it does not verify what is said here.

- The hot store must provide an atomic conditional PUT, create-only (`If-None-Match: *`) and
  `If-Match`, and strong read-after-write consistency for GET, HEAD and LIST. The lease, the
  checkpoint and the registration of the proxies are written with those conditional puts.
- The cold store must provide the same create-only PUT, on a single PUT and on the completion of a
  multipart upload, and an atomic `If-Match` PUT (the completion of a multipart upload included),
  which the reconciler uses to replace a reservation with its object.
- Known to meet them: Amazon S3, MinIO (for the conditional PUT) and tierstore's `dir` store,
  among the calls of one process only (two processes over one `dir` hot tier are held apart by
  `hot_store.lock_file`, not by the store).
- A cold store that cannot be reached at start does not fail the start; the first request that
  needs it fails as an ordinary request does. Opening the proxy reads and writes the hot tier's own
  state, so a hot tier that cannot be reached fails the start (exit 1, `opening the proxy: ...`).
  That, an address in use or a key file refused exits 1 and is tried again after 30 seconds, up to
  the start limit (5 starts in an hour, then systemd gives up until
  `systemctl reset-failed tierstore`). Exit 3 (`RestartPreventExitStatus=2 3`) is now only the file
  system of a cache directory answering the cache's start-up check wrongly.
- `conditional_delete` (`hot_store.conditional_delete`, `cold_store.conditional_delete`) says whether a
  conditional delete of an `s3` tier is sent with `If-Match` (`true`, for a store that enforces it
  on a DELETE, as Amazon S3 does) or as a read of the tag before a plain DELETE (`false`, the
  default, right on every store with a window between the two requests).

`cold_store.skip_checks` and `server.backend_check_wait`, which belonged to the self-test earlier
releases ran, are accepted and ignored, each with one warning on stderr. Probe keys under
`_tiering/selftest/` that an earlier release left are not deleted by this one.
