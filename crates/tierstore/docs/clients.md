# Use it from your code

Point any S3 client at the address of an `s3` interface, with path-style addressing, the region `us-east-1` and the key from the key file. tierstore has no credentials of its own: the key id is the first line of `server.access_key_file` and the secret the second. Each snippet sets the SDK's request and response checksum options to "when required". A build node also takes two experimental `POST` calls, which no SDK has a call for and a client signs itself, to warm its cache with the trees of a job before the job starts: see [the hydrate call](operations.md#the-hydrate-call).

## AWS CLI

```sh
export AWS_ACCESS_KEY_ID=<key id> AWS_SECRET_ACCESS_KEY=<secret> AWS_DEFAULT_REGION=us-east-1
aws configure set default.s3.addressing_style path
aws --endpoint-url http://127.0.0.1:9000 s3api put-object --bucket tierstore --key hello.txt --body hello.txt
aws --endpoint-url http://127.0.0.1:9000 s3api list-objects-v2 --bucket tierstore
```

## Python (boto3)

```python
import boto3
from botocore.config import Config

client = boto3.client(
    "s3",
    endpoint_url="http://127.0.0.1:9000",
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
client.put_object(Bucket="tierstore", Key="hello.txt", Body=b"hello")
```

## JavaScript (AWS SDK v3)

```js
import { S3Client, PutObjectCommand } from "@aws-sdk/client-s3";

const client = new S3Client({
  endpoint: "http://127.0.0.1:9000",
  region: "us-east-1",
  forcePathStyle: true,
  credentials: { accessKeyId: KEY_ID, secretAccessKey: SECRET },
  requestChecksumCalculation: "WHEN_REQUIRED",
  responseChecksumValidation: "WHEN_REQUIRED",
});
await client.send(new PutObjectCommand({ Bucket: "tierstore", Key: "hello.txt", Body: "hello" }));
```

## Go (aws-sdk-go-v2)

```go
cfg, err := config.LoadDefaultConfig(ctx,
    config.WithRegion("us-east-1"),
    config.WithCredentialsProvider(credentials.NewStaticCredentialsProvider(keyID, secret, "")),
)
if err != nil {
    return err
}
client := s3.NewFromConfig(cfg, func(o *s3.Options) {
    o.BaseEndpoint = aws.String("http://127.0.0.1:9000")
    o.UsePathStyle = true
    o.RequestChecksumCalculation = aws.RequestChecksumCalculationWhenRequired
    o.ResponseChecksumValidation = aws.ResponseChecksumValidationWhenRequired
})
_, err = client.PutObject(ctx, &s3.PutObjectInput{
    Bucket: aws.String("tierstore"),
    Key:    aws.String("hello.txt"),
    Body:   strings.NewReader("hello"),
})
```

## Rust (aws-sdk-s3)

```rust
use aws_sdk_s3::config::{Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation};

let config = aws_sdk_s3::Config::builder()
    .behavior_version_latest()
    .endpoint_url("http://127.0.0.1:9000")
    .force_path_style(true)
    .region(Region::new("us-east-1"))
    .credentials_provider(Credentials::new(KEY_ID, SECRET, None, None, "static"))
    .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
    .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
    .build();
let client = aws_sdk_s3::Client::from_conf(config);
```

## Java (AWS SDK v2)

```java
S3Client client = S3Client.builder()
    .endpointOverride(URI.create("http://127.0.0.1:9000"))
    .region(Region.US_EAST_1)
    .forcePathStyle(true)
    .credentialsProvider(StaticCredentialsProvider.create(AwsBasicCredentials.create(KEY_ID, SECRET)))
    .requestChecksumCalculation(RequestChecksumCalculation.WHEN_REQUIRED)
    .responseChecksumValidation(ResponseChecksumValidation.WHEN_REQUIRED)
    .build();
client.putObject(PutObjectRequest.builder().bucket("tierstore").key("hello.txt").build(), RequestBody.fromString("hello"));
```

The snippets are from `packaging/README.md`, "Using a stock SDK". The crate's tests drive the front with the Rust SDK set as in its section; the other sections are written from the SDKs' documentation and are not run against the front by the repository's tests.

## Keys under a tree placement

A build node can show a node-local tree as objects of its bucket: a *placement* of the tree at a bucket and a key prefix, kept by the node's tree store (see [`[trees]`](configuration.md#trees)). The `CreateTree` call of the `trees` gRPC interface (the `trees` entry of [`[[interface]]`](configuration.md#interface)) is the front that makes placements; until that front is connected to the tree store, its calls answer `NotServing`. While a placement lasts (until its lease ends or it is released), a key under its prefix is a file of the tree on this node, not an ordinary object. Another node does not see the placement: it answers the same keys from its own objects.

- **Reading.** `GET` and `HEAD` of a file answer as for an object whose bytes are the file's. The length is the file's size, the `ETag` is its SHA-256 as quoted hex, `Last-Modified` is the time the placement was made, and `x-amz-meta-mode` is its mode in four octal digits (`4755`). A `Range`, the conditions (`If-Match` and the others) and the `response-*` overrides work as for an object. The bytes come from the cache; the first read that does not find them there fills them from the upstream, whole and checked against the file's SHA-256.
- **Symbolic links** are objects of no bytes. Their `ETag` is the SHA-256 of nothing, and `x-amz-meta-symlink-target` holds the target, with each byte that is not printable ASCII, and `%`, written `%XX`.
- **Directories are not objects.** A directory, the root of the placement included, and a path that is not in the tree answer `404 NoSuchKey`.
- **Writes are refused.** A `PUT`, an upload part, the start or the completion of a multipart upload and a `DELETE` of a key under a placement answer `409 TreeLocationReadOnly`. The abort of a multipart upload is not refused, since it changes no object. A DeleteObjects call refuses each such key alone, with the same code and message in its `Error` entry, and deletes the others.
- **Listings.** A listing (`ListObjectsV2`, `ListObjects` and the version listing) merges the files and links of the placements it meets with the ordinary objects, in key order, which is the order of the bytes of the keys, not of the names in the tree: a file `a-b` comes before the keys `a/...`, since `-` sorts before `/`. An ordinary object whose key is under a placement is left out, because the tree answers that key on this node. A directory is not a key, so an empty directory shows nothing.
- **A placement that ends** between the lookup of a request and its answer makes the key an ordinary one again, and the request is answered as for an ordinary key.
- **A fill that fails.** When the bytes of a file are not in the cache and cannot be filled, the answer is `503 SlowDown` with `retry-after: 1` if a retry can mend it (the cache is busy, read-only or full, the entry changed under the fill, or the upstream answered a server error), and `500 InternalError` if the source does not hold the file's bytes. A client retries the first and not the second.
