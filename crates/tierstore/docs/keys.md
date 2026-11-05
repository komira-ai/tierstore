# Keys and roles

A client signs each request with an access key (SigV4, in the `Authorization` header). The key it signs with decides what the request may do. The role belongs to the key, not to the client or the address.

## The keys, and what each may do

| Key | Key of the file | May do |
|---|---|---|
| full | `server.access_key_file` | everything the server serves: put, get, head, list, delete and multipart uploads; also signs presigned GET URLs |
| read-only | `server.read_only_key_files` | get and head objects, list the bucket and its open uploads, head the bucket, read its region (GetBucketLocation); also signs presigned GET URLs. Any write, delete, CreateBucket or multipart call is 403 `AccessDenied`, before its body is read |
| presigning | `server.presign_key_files` | sign presigned PUT URLs, each good for one object under the key's prefix, and nothing else |
| GC | `server.gc_key_files` | a scoped key with a list, a delete and an abort right on one prefix: list, head, delete and abort uploads under it, and nothing else; it never reads a body |
| scoped | `server.scoped_key_files` | read, create and delete under the prefixes its file lists, and nothing else; also signs presigned GET URLs for objects under its read prefixes |
| hot tier credential | `hot_store.credentials_file` | not a client key: what tierstore signs its own requests to an `s3` hot tier with |
| cold tier credential | `cold_store.credentials_file` | not a client key: the same for an `s3` cold tier |

Only the full key is required. The read-only, presigning, GC, scoped keys of the file are lists, one file for each key. A tier that is a directory needs no credential file.

A role is not a prefix: a read-only key may read every object of the bucket. To limit a client to prefixes, use a scoped key (or a GC key, or a presigned URL). The key prefixes `_journal/`, `_tiering/` and `_entries/` belong to tierstore: a client put or delete there is refused, and a listing leaves them out.

## The key file

Every key file holds its key in one of two forms, told apart by the first byte that is not white space: `{` is JSON, anything else is two lines.

```json
{"access_key_id": "AKIDEXAMPLE", "secret_access_key": "the secret"}
```

or, as two lines, the access key id and then the secret access key:

```text
AKIDEXAMPLE
the secret
```

The JSON form needs exactly the two fields, both non-empty; a file with another field, a misspelled field or an empty value is refused. Values are taken as they are, without trimming. Presigning, GC and scoped key files are JSON with more fields (below).

## File permissions

Each key file must be owned by the user the server runs as, with mode 0600 or 0400. The process refuses a file that its group or others can access, and a symbolic link (for any key file). No message of the process carries a secret. The server reads the environment for nothing, and a secret is never on the command line or in the configuration file: the file holds only the paths of key files.

The access key ids of all the keys of one server must differ, because a request names its key by its id. A start with two keys that share an id is refused with exit status 2.

## Presigning keys

A presigning key file holds the key and its ceiling, all required:

```json
{"access_key_id": "pack-presign", "secret_access_key": "the secret",
 "ceiling": {"prefix": "cas/b/", "max_expires_secs": 600, "max_object_bytes": 134217728}}
```

A presigned URL is good for one PUT of a key under the prefix, for at most the smaller of `max_expires_secs` and `server.presign_max_expires` (15 minutes by default), with a body of at most `max_object_bytes` (up to 5 GiB). The body is also held to `server.max_request_bytes`; the smaller of the two applies. A second PUT of the key is 412. The signature does not cover the body, so whoever reads the object must check its content.

## GC keys

A GC key is one instance of a scoped key (below): the server reads a GC key file as a scoped key with a list, a delete and an abort right on `[prefix]` and no read or write prefix (a scoped key file cannot give a list or an abort right), and keeps the file format of the release before. A GC key file is JSON of exactly `access_key_id`, `secret_access_key` and `prefix`, where the prefix ends in `/`. The key signs in the header only. It may list (ListObjectsV2 and ListMultipartUploads, each with a `prefix` inside its own), head, delete, and abort uploads, for keys longer than its prefix. Delete removes the object from both tiers. A list with no prefix or another is 403, not filtered.

## Scoped keys

A scoped key file is JSON of exactly five fields, all required:

```json
{"access_key_id": "frontend-packs", "secret_access_key": "the secret",
 "read_prefixes": ["cas/"],
 "write_prefixes": ["cas/f/frontend/"],
 "delete_prefixes": []}
```

Every prefix ends in `/` and none may name or lie under `_journal/`, `_tiering/` or `_entries/`. A key whose lists are all empty is refused.

The three rights, each on the prefixes of its list:

| right | list | the key may |
|---|---|---|
| read | `read_prefixes` | GetObject, HeadObject, ListObjectsV2 and ListMultipartUploads (a listing's `prefix` inside the list); sign presigned GET URLs |
| write | `write_prefixes` | create-only PutObject and the calls of a multipart upload, abort included |
| delete | `delete_prefixes` | DeleteObject (not AbortMultipartUpload: a key that deletes an object cannot cancel another client's upload) |

The lists are not interchangeable: a write prefix does not let the key read what it wrote, and a read prefix does not let it write. A put by a scoped key is create-only (no `If-Match`, no copy), and a delete needs a delete prefix. A call outside the prefixes of the right it needs is 403 `AccessDenied`.

## Rotation

The server reads its key files once, at start. To rotate a key, write the new file, then restart the unit: it drains first (see [operations](operations.md)). There is no reload.

A change to the tier credentials takes the same restart.

## Presigned GET URLs

A client that holds no key and must read an object for a short time presents a presigned GET URL: the URL of the object with the SigV4 query-string parameters, signed with the full key, a read-only key or a scoped key. Any SDK presigner makes one (the library has `presign_get`). No key and no key file is added for it: the key that signs is one of those above, and the URL has that key's rights and no more. A scoped key's URL reads an object under one of its read prefixes and nothing outside them; a GC key and a presigning key sign no read URL.

The URL is good for a `GET` or `HEAD` (the method it was signed for) of one object, with or without a `Range` header, and for nothing else: another method, a call on the bucket, a multipart call or a parameter beyond the SigV4 ones and `x-id` is 403 `AccessDenied`. The `Range` header is not part of the signature, so one URL serves every part of a parallel download of a large object. The URL is refused from the second `X-Amz-Date + X-Amz-Expires` on, and its `X-Amz-Expires` may not be over `server.presign_max_expires` (15 minutes by default). The cap is measured on the server's clock, so a service whose clock runs ahead of the server's should sign URLs shorter than the cap by at least that skew. A URL is a bearer credential: whoever holds it can use it until it expires, so use TLS where that matters. The calls and the answers are in `packaging/README.md`, under Presigned GET URLs. The read grant of the release before (the `x-tierstore-grant` header, `server.grant_key_files`, `server.grant_max_lifetime`) is gone: the two keys are still accepted and ignored, with a warning at start, so that an old file starts, and a request that carries the header is refused as an unsigned request is, 403 `AccessDenied`.
