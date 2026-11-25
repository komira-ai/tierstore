use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

use super::*;

/// Where `endpoint` sends a request for `bucket` named as `addressing` says: the base URL, the
/// `Host` and the start of the path.
fn sent(endpoint: &str, bucket: &str, addressing: Addressing) -> (String, String, String) {
    let target = target_of(endpoint, bucket, addressing).unwrap();
    (target.base.to_string(), target.host, target.bucket_path)
}

fn owned(base: &str, host: &str, bucket_path: &str) -> (String, String, String) {
    (base.to_owned(), host.to_owned(), bucket_path.to_owned())
}

/// Asserts that by default an AWS S3 endpoint (`.amazonaws.com`, `.amazonaws.com.cn`, in any case)
/// puts a bucket whose name has no dot in the host, and that a bucket with a dot, an endpoint of
/// another store (a name that only ends like AWS's too), and an IP address keep it in the path.
#[test]
fn auto_is_virtual_hosted_for_aws_and_path_style_for_anything_else() {
    let auto = Addressing::default();
    assert_eq!(auto, Addressing::Auto);
    assert_eq!(
        sent("https://s3.eu-west-1.amazonaws.com", "my-bucket", auto),
        owned(
            "https://my-bucket.s3.eu-west-1.amazonaws.com/",
            "my-bucket.s3.eu-west-1.amazonaws.com",
            ""
        )
    );
    assert_eq!(
        sent("https://S3.CN-NORTH-1.AmazonAWS.com.CN", "logs", auto),
        owned(
            "https://logs.s3.cn-north-1.amazonaws.com.cn/",
            "logs.s3.cn-north-1.amazonaws.com.cn",
            ""
        )
    );
    assert_eq!(
        sent("https://s3.eu-west-1.amazonaws.com", "my.bucket", auto),
        owned(
            "https://s3.eu-west-1.amazonaws.com/",
            "s3.eu-west-1.amazonaws.com",
            "/my.bucket"
        )
    );
    assert_eq!(
        sent("https://notamazonaws.com", "my-bucket", auto),
        owned(
            "https://notamazonaws.com/",
            "notamazonaws.com",
            "/my-bucket"
        )
    );
    assert_eq!(
        sent("http://minio.internal:9000", "my-bucket", auto),
        owned(
            "http://minio.internal:9000/",
            "minio.internal:9000",
            "/my-bucket"
        )
    );
    assert_eq!(
        sent("http://127.0.0.1:9000", "my-bucket", auto),
        owned("http://127.0.0.1:9000/", "127.0.0.1:9000", "/my-bucket")
    );
}

/// Asserts that an explicit style is kept whatever the endpoint: path style on AWS, virtual-hosted
/// style on another store with its port; and that the `Host` leaves out the scheme's default port.
#[test]
fn an_explicit_style_is_kept_whatever_the_endpoint() {
    assert_eq!(
        sent(
            "https://s3.eu-west-1.amazonaws.com",
            "my-bucket",
            Addressing::Path
        ),
        owned(
            "https://s3.eu-west-1.amazonaws.com/",
            "s3.eu-west-1.amazonaws.com",
            "/my-bucket"
        )
    );
    assert_eq!(
        sent(
            "http://minio.internal:9000",
            "my.bucket",
            Addressing::VirtualHosted
        ),
        owned(
            "http://my.bucket.minio.internal:9000/",
            "my.bucket.minio.internal:9000",
            ""
        )
    );
    assert_eq!(
        sent("https://store.internal:443", "b-1", Addressing::Path),
        owned("https://store.internal/", "store.internal", "/b-1")
    );
}

/// The reason `endpoint` is refused for bucket `b-1`.
fn refusal(endpoint: &str, addressing: Addressing) -> &'static str {
    match target_of(endpoint, "b-1", addressing) {
        Err(S3ConfigError::Endpoint {
            endpoint: named,
            why,
        }) => {
            assert_eq!(named, endpoint);
            why
        }
        other => panic!(
            "{endpoint}: expected a refusal, got {:?}",
            other.map(|t| t.host)
        ),
    }
}

/// Asserts each endpoint that is not `http(s)://host[:port]` is refused with its reason, and that
/// virtual-hosted style is refused for an IP address, v4 or v6, which cannot take a bucket label.
#[test]
fn a_malformed_endpoint_or_an_ip_address_for_virtual_hosted_style_is_refused() {
    let path = Addressing::Path;
    assert_eq!(refusal("s3.amazonaws.com", path), "not a URL");
    assert_eq!(
        refusal("ftp://127.0.0.1", path),
        "the scheme must be http or https"
    );
    assert_eq!(
        refusal("https://k:s@127.0.0.1", path),
        "credentials belong in `credentials`, not the URL"
    );
    for extra in ["https://h/base", "https://h/?x=1", "https://h/#f"] {
        assert_eq!(
            refusal(extra, path),
            "an endpoint has no path, query or fragment"
        );
    }
    for ip in ["https://127.0.0.1:9000", "http://[::1]:9000"] {
        assert_eq!(
            refusal(ip, Addressing::VirtualHosted),
            "virtual-hosted-style addressing needs a host name, not an IP address"
        );
    }
}

/// A CA certificate, PEM.
fn ca_pem() -> String {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .self_signed(&KeyPair::generate().unwrap())
        .unwrap()
        .pem()
}

/// Asserts that `ca_pem` is checked when the store is made: text with no certificate, a
/// certificate block that is not base64, and one whose bytes are not a certificate are each refused
/// as such, and a CA certificate, alone or after another, is taken. Nothing reads the system's CA
/// certificates here (`system` is empty).
#[test]
fn ca_pem_must_hold_certificates() {
    assert!(matches!(
        tls_config(Some(b"no certificate here\n"), Vec::new()),
        Err(S3ConfigError::CaPemEmpty)
    ));
    let not_base64 = b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
    assert!(matches!(
        tls_config(Some(not_base64), Vec::new()),
        Err(S3ConfigError::CaPem(_))
    ));
    let not_a_certificate = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    assert!(matches!(
        tls_config(Some(not_a_certificate), Vec::new()),
        Err(S3ConfigError::CaCertificate(_))
    ));
    let two = format!("{}{}", ca_pem(), ca_pem());
    let config = tls_config(Some(two.as_bytes()), Vec::new()).unwrap();
    assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    assert!(tls_config(None, Vec::new()).is_ok());
}
