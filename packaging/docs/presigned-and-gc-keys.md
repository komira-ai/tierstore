# Presigned PUT URLs and GC keys

## Presigned PUT URLs

A client that holds a presigned URL writes one object with a plain `PUT` of the URL: the server
checks SigV4 query-string authentication as the AWS documentation has it (`X-Amz-Algorithm`,
`X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders`, `X-Amz-Signature`;
payload `UNSIGNED-PAYLOAD`; `host` signed), so any SDK presigner or `tierstore::server::presign_put`
makes one. Only a presigning key (`--presign-key-file`, once per key, mode 0600) can sign a URL; the
full and read-only keys cannot, and a presigning key is refused for any request signed in the
header. The file holds the key and its ceiling, all required (a file without one is refused at
start):

```json
{"access_key_id": "pack-presign", "secret_access_key": "the secret",
 "ceiling": {"prefix": "cas/b/", "max_expires_secs": 600, "max_object_bytes": 134217728}}
```

A URL is good for one `PUT` of a key that starts with the prefix (after the path is decoded: no
`..`, no `.` segment, no control character, never `_journal/` or `_tiering/`), with an
`X-Amz-Expires` of at most the smaller of `max_expires_secs` and `--presign-max-expires` (default
15m), and a body of at most `max_object_bytes` (5 GiB at most, the S3 single-PUT limit; each key file chooses its own), checked on
`Content-Length` and on the bytes received (and held to `--max-request-bytes`, 256 MiB by default, too: a key with a larger
cap needs that raised, and the smaller of the two applies; the body is streamed to the tier and
never held whole). It is refused from `X-Amz-Date` plus `X-Amz-Expires` on, with no grace. The
object appears only after a complete body, a second `PUT` of the key is `412`, and no multipart
call, `If-Match`, read, list or delete is allowed. The signature does not cover the body, so the
reader of the object must check its content. The query string is never logged.

A secret is never on the command line or in the environment: the process reads no environment
variable, and the unit sets none.

## GC keys

A GC key (`--gc-key-file`, once per key; JSON of `access_key_id`, `secret_access_key` and `prefix`,
mode 0600, not a symbolic link, owned by the user the server runs as) is for the process that
collects the objects that presigned PUTs wrote. It signs in the header only (a presigned URL signed
with it is refused) and may do these, only for a key that starts with its prefix (which ends in `/`)
and is longer than it:

| call | allowed |
|---|---|
| ListObjectsV2 | with a `prefix` inside the key's; a list with no prefix or another is 403, not filtered |
| ListMultipartUploads | with a `prefix` inside the key's, the open uploads under it; no prefix or another is 403, not filtered |
| HeadObject | yes |
| DeleteObject | yes: from both tiers, journaled as for any delete |
| AbortMultipartUpload | of an upload of such a key |
| GetObject | no: a GC key never reads a body |
| put, part, start or completion of an upload, copy, CreateBucket, HeadBucket | no |
| `_journal/`, `_tiering/`, any key outside the prefix, the prefix itself | no |

A call the key may not do is refused with 403 `AccessDenied` without its body being read. The
refusals are made in this order: the signature and key id, the method, the decoding of the path and
query (400, or 404 for an unknown bucket), the checksum headers (400 or 501), and last the key or
list prefix; so a call that is both refused and malformed may get the malformed-request error. A delete by a GC key is not
create-only; it is the only way an object leaves the prefix that presigned PUTs write.
