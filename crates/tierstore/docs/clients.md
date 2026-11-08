# Use it from your code

Point any S3 client at the address of an `s3` interface, with path-style addressing, the region `us-east-1` and the key from the key file. tierstore has no credentials of its own: the key id is the first line of `server.access_key_file` and the secret the second. Each snippet sets the SDK's request and response checksum options to "when required".

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
