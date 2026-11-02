# Using a stock SDK

The front serves one bucket, path-style, over plain HTTP, and checks SigV4 in the `Authorization`
header only. An SDK with its defaults sends some things the front refuses, so a client needs these
settings. `crates/tierstore/tests/s3_front/sdk.rs` drives the front with aws-sdk-rust (the
`aws-sdk-s3` crate, version 1.96.0, in this repository's tests only) set this way, and that test is
what "tested" means below.

| setting | value | why |
|---|---|---|
| endpoint | `http://<host>:<port>` of `--listen` | the front has no TLS; an `https://` endpoint fails |
| addressing | path-style (`force_path_style`) | the bucket is in the path, `/<bucket>/<key>`; virtual-host addressing (`<bucket>.<host>`) is not served |
| bucket | the `--bucket` name | there is one; other names are not served |
| region | tierstore's own `--region` (not `--hot-region` or `--cold-region`, which are for the tiers' stores), `us-east-1` by default | the region is in the credential scope of the signature, and a client with another is refused 403 `InvalidAccessKeyId` |
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
