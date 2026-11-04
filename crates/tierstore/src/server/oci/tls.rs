//! The transport of the OCI front: the certificate and key of TLS, read once at start, and the
//! check that the certificate names the address clients dial; or, with no certificate, the
//! warning that a plain HTTP listener is off loopback. The S3 front takes its acceptor from the same
//! code ([`s3_acceptor`]), and differs only in what the certificate must name.

use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;
use x509_parser::extensions::GeneralName;

/// Why the OCI front cannot start, or why the certificate and key of the S3 front are refused.
#[derive(Debug, thiserror::Error)]
pub enum OciError {
    /// The `prefix` param of an `oci` interface is not usable.
    #[error("--oci-prefix {prefix:?}: {why}")]
    Prefix {
        /// The prefix given.
        prefix: String,
        /// What is wrong with it.
        why: String,
    },
    /// Only one of the `tls_cert_file` and `tls_key_file` params is given.
    #[error("{0}")]
    Missing(String),
    /// A certificate or key file cannot be read.
    #[error("{flag} {path}: {source}")]
    Read {
        /// The flag that names the file.
        flag: &'static str,
        /// The file.
        path: PathBuf,
        /// Why it cannot be read.
        #[source]
        source: std::io::Error,
    },
    /// A file is not what it must be, or the certificate does not name the address.
    #[error("{flag} {path}: {why}")]
    Pem {
        /// The flag that names the file.
        flag: &'static str,
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// The certificate and key do not make a TLS configuration.
    #[error("{flags}: {why}")]
    Config {
        /// The two flags that name the files.
        flags: &'static str,
        /// What is wrong with them.
        why: String,
    },
    /// The address cannot be bound.
    #[error("binding {addr}: {source}")]
    Bind {
        /// The address.
        addr: SocketAddr,
        /// Why it cannot be bound.
        #[source]
        source: std::io::Error,
    },
}

impl OciError {
    /// Whether flags and files that a restart cannot mend are at fault, rather than a file that
    /// could not be read or an address that could not be bound.
    #[must_use]
    pub fn is_invalid_flags(&self) -> bool {
        !matches!(self, Self::Read { .. } | Self::Bind { .. })
    }
}

/// How the front's connections are carried.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OciMode {
    /// Plain HTTP/1.1.
    Http,
    /// HTTP/1.1 over TLS.
    #[default]
    Https,
}

impl OciMode {
    /// `http` or `https`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// The line to print at start when the front serves plain HTTP at `listen`, or `None` when
/// `listen` is loopback (127/8, `::1`, or an IPv4-mapped loopback address). Plain HTTP is served
/// on any address; off loopback a connection can be read and changed by whoever is on its path,
/// so the line advises a certificate. The unspecified addresses `0.0.0.0` and `::` are not
/// loopback.
#[must_use]
pub fn plain_warning(listen: SocketAddr) -> Option<String> {
    if listen.ip().to_canonical().is_loopback() {
        None
    } else {
        Some(format!(
            "tierstore: warning: the oci interface at {listen} serves plain HTTP on a non-loopback \
             address; give params tls_cert_file and tls_key_file to serve HTTPS"
        ))
    }
}

/// The flags that name the certificate and the key of a front, for the messages of a refusal.
#[derive(Clone, Copy)]
struct Flags {
    cert: &'static str,
    key: &'static str,
    both: &'static str,
}

/// The flags of the OCI front.
const OCI_FLAGS: Flags = Flags {
    cert: "--oci-tls-cert",
    key: "--oci-tls-key",
    both: "--oci-tls-cert and --oci-tls-key",
};

/// The flags of the S3 front.
const S3_FLAGS: Flags = Flags {
    cert: "--tls-cert",
    key: "--tls-key",
    both: "--tls-cert and --tls-key",
};

/// The acceptor for the certificate chain in `cert` and the key in `key` (both PEM), which serves
/// HTTP/1.1 only and asks no client for a certificate. The leaf must carry an IP subjectAltName
/// for `listen` (any IP subjectAltName, when `listen` is the unspecified address), since the
/// clients of the front dial an IP address and accept only a certificate that names it. A leaf
/// that is expired or not yet valid is refused, and so is a key file that others can read (a
/// group may, as the key of a service often is).
///
/// The files are read once: a new certificate takes a restart, so renew it before it expires.
///
/// # Errors
/// A file that cannot be read, PEM that is not a certificate chain and a key, a certificate that
/// does not name the address or is not valid now, a key file any user can read, or a key that is
/// not the leaf's.
pub fn acceptor(cert: &Path, key: &Path, listen: SocketAddr) -> Result<TlsAcceptor, OciError> {
    acceptor_for(cert, key, &[listen])
}

/// [`acceptor`] for a front that listens at several addresses: the leaf must name each of them.
///
/// # Errors
/// As [`acceptor`], for any of the addresses.
pub fn acceptor_for(
    cert: &Path,
    key: &Path,
    listen: &[SocketAddr],
) -> Result<TlsAcceptor, OciError> {
    acceptor_with(cert, key, OCI_FLAGS, &|facts| {
        listen.iter().find_map(|addr| unnamed(&facts.ips, *addr))
    })
}

/// The acceptor of the S3 front, which `server.tls_cert_file` and `server.tls_key_file` name, for a front that listens
/// at `listen`: as [`acceptor_for`], except that the leaf may name the front by a DNS name. A leaf
/// with at least one DNS subjectAltName is accepted whatever IP addresses it names; one with
/// none must carry an IP subjectAltName for each address, as [`acceptor_for`] asks.
///
/// # Errors
/// As [`acceptor_for`], with the flags of the S3 front in the messages.
pub fn s3_acceptor(
    cert: &Path,
    key: &Path,
    listen: &[SocketAddr],
) -> Result<TlsAcceptor, OciError> {
    acceptor_with(cert, key, S3_FLAGS, &|facts| {
        if facts.dns.is_empty() {
            listen.iter().find_map(|addr| unnamed(&facts.ips, *addr))
        } else {
            None
        }
    })
}

/// The acceptor for `cert` and `key` once `names` has said the leaf names the front: `names` is
/// given the facts of the leaf and returns why it does not, or `None`.
fn acceptor_with(
    cert: &Path,
    key: &Path,
    flags: Flags,
    names: &dyn Fn(&Facts) -> Option<String>,
) -> Result<TlsAcceptor, OciError> {
    let read = |flag, path: &Path| {
        std::fs::read(path).map_err(|source| OciError::Read {
            flag,
            path: path.to_owned(),
            source,
        })
    };
    let pem_error = |flag, path: &Path, why: String| OciError::Pem {
        flag,
        path: path.to_owned(),
        why,
    };
    let cert_pem = read(flags.cert, cert)?;
    let key_pem = read(flags.key, key)?;
    let chain = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|why| pem_error(flags.cert, cert, format!("not PEM certificates: {why}")))?;
    let Some(leaf) = chain.first() else {
        return Err(pem_error(flags.cert, cert, "no certificate".to_owned()));
    };
    let facts = Facts::of(leaf).map_err(|why| pem_error(flags.cert, cert, why))?;
    if let Some(why) = names(&facts) {
        return Err(pem_error(flags.cert, cert, why));
    }
    if let Some(why) = facts.not_valid_now() {
        return Err(pem_error(flags.cert, cert, why));
    }
    // A mode that cannot be read counts as open to all.
    let mode = std::fs::metadata(key).map_or(0o777, |meta| meta.permissions().mode());
    if mode & 0o007 != 0 {
        let why = "any user can read the key; make it mode 0640 or 0600".to_owned();
        return Err(pem_error(flags.key, key, why));
    }
    let private = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|why| pem_error(flags.key, key, format!("not a PEM private key: {why}")))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let built = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|builder| {
            builder
                .with_no_client_auth()
                .with_single_cert(chain, private)
        });
    let mut config = built.map_err(|why| OciError::Config {
        flags: flags.both,
        why: why.to_string(),
    })?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// What the front needs to know of a leaf certificate.
struct Facts {
    /// The IP addresses it names in its subjectAltName.
    ips: Vec<IpAddr>,
    /// The DNS names it names in its subjectAltName.
    dns: Vec<String>,
    /// Whether the time now is within its validity.
    valid: bool,
    /// Its validity, as text, for the message of a refusal.
    period: String,
}

impl Facts {
    fn of(leaf: &CertificateDer<'_>) -> Result<Self, String> {
        let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref())
            .map_err(|why| format!("the first certificate is not X.509: {why}"))?;
        let validity = parsed.validity();
        let (ips, dns) = alt_names(&parsed);
        Ok(Self {
            ips,
            dns,
            valid: validity.is_valid(),
            period: format!(
                "notBefore {}, notAfter {}",
                validity.not_before, validity.not_after
            ),
        })
    }

    /// Why the certificate cannot be served now, or `None` if it can.
    fn not_valid_now(&self) -> Option<String> {
        let period = &self.period;
        (!self.valid).then(|| format!("the certificate is not valid now ({period})"))
    }
}

/// The IP addresses and the DNS names the certificate `parsed` names in its subjectAltName.
fn alt_names(parsed: &x509_parser::certificate::X509Certificate<'_>) -> (Vec<IpAddr>, Vec<String>) {
    // A subjectAltName that cannot be read names nothing.
    let alt = parsed.subject_alternative_name().ok().flatten();
    let names = alt
        .map(|ext| &ext.value.general_names[..])
        .unwrap_or_default();
    let (mut ips, mut dns) = (Vec::new(), Vec::new());
    for name in names {
        match name {
            GeneralName::IPAddress(bytes) => ips.extend(ip_of(bytes)),
            GeneralName::DNSName(text) => dns.push((*text).to_owned()),
            _ => {}
        }
    }
    (ips, dns)
}

/// The address of 4 bytes (IPv4) or 16 (IPv6).
pub(super) fn ip_of(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => <[u8; 4]>::try_from(bytes).ok().map(IpAddr::from),
        16 => <[u8; 16]>::try_from(bytes).ok().map(IpAddr::from),
        _ => None,
    }
}

/// Why `named` does not name `listen`, or `None` if it does.
pub(super) fn unnamed(named: &[IpAddr], listen: SocketAddr) -> Option<String> {
    let ip = listen.ip();
    let covered = if ip.is_unspecified() {
        !named.is_empty()
    } else {
        named.contains(&ip)
    };
    let list: Vec<String> = named.iter().map(ToString::to_string).collect();
    let why = format!(
        "the certificate has no IP subjectAltName for {ip} (it names [{}]); clients dial the \
         front by IP address and accept only a certificate that names it",
        list.join(", ")
    );
    (!covered).then_some(why)
}
