//! Who may pull from the OCI front, and who may push: the access keys of the S3 front, asked for
//! as HTTP Basic.
//!
//! # Why Basic, not the token flow
//!
//! The distribution specification lets a registry answer 401 with a `Bearer` challenge that points
//! the client at a token service, or with a `Basic` challenge. The token flow needs a service of
//! its own: a `realm` URL the client can reach (the front only knows the address it was bound to,
//! not the name a client dials), a signing key that every instance and every restart agrees on,
//! tokens that expire, and so a clock and a place to keep the key. Basic needs none of that. A
//! client (podman, docker, skopeo, crane, containerd) that is given a `Basic` challenge sends the
//! credentials it holds for the registry (`podman login`, `docker login`) with each request, and
//! the front checks them against the keys the server already has. The price is that the secret
//! travels with every request, so `params.require_auth` is refused on plain HTTP off loopback
//! (see `oci_setup`): the front is then served over TLS, or only to its own host.
//!
//! # Keys and rights
//!
//! The user name is an access key id and the password is its secret. The keys are those of the
//! server, as the S3 front has them: the full key, the read-only keys, the GC keys and the scoped
//! keys (a presigning key has no right here). What a key may do is worked out once, at start,
//! from its kind and the objects the front serves, which are under `<prefix>blobs/sha256/`:
//!
//! - **pull** (`GET` and `HEAD` of manifests and blobs): the full key, a read-only key, and a
//!   scoped key with a read prefix that is a prefix of the front's (a read prefix inside the
//!   front's would cover only some digests, and is not enough);
//! - **push** (`POST`, `PUT`, `PATCH` and `DELETE`): the full key, and a scoped key with a write
//!   prefix that is a prefix of the front's. The front serves no upload yet, so a key that may push
//!   is answered 405 as before; a key that may not is answered 403 first.
//!
//! `GET /v2/`, the call a client makes to log in, asks only that the key has either right.
//!
//! # Answers
//!
//! No credentials, a scheme other than Basic, credentials that do not decode, an id the server does
//! not have and a wrong secret are all the same answer: 401 `UNAUTHORIZED` with
//! `WWW-Authenticate: Basic realm="tierstore"`, so that a client can tell the front wants a login
//! and nothing else. A key that is known and lacks the right for the call is 403 `DENIED`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hyper::header::AUTHORIZATION;
use hyper::{HeaderMap, Method};
use ring::hmac;
use std::sync::Arc;

use super::OciPrefix;
use crate::server::ServerConfig;
use crate::server::scoped::Scopes;

/// What a key may do at the front.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rights {
    pub(super) pull: bool,
    pub(super) push: bool,
}

/// What a call asks of the key that makes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Need {
    /// Either right: `GET /v2/`, and a method the front does not serve.
    Any,
    /// `GET` and `HEAD` of a manifest or a blob, and of a path that is none of the routes.
    Pull,
    /// `POST`, `PUT`, `PATCH` and `DELETE`.
    Push,
}

impl Need {
    /// What `method` asks, and, for a `GET` or `HEAD`, whether the path is `/v2/`.
    pub(super) fn of(method: &Method, ping: bool) -> Self {
        match *method {
            Method::GET | Method::HEAD if ping => Self::Any,
            Method::GET | Method::HEAD => Self::Pull,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE => Self::Push,
            _ => Self::Any,
        }
    }
}

/// Why a call is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refused {
    /// 401: no key, or not one this server has.
    Unauthorized,
    /// 403: a key this server has, without the right the call needs.
    Denied,
}

/// What a key is, before the front it is attached to says which objects it serves.
enum Grant {
    /// The full key.
    Everything,
    /// A read-only key.
    Pulling,
    /// A GC key or a scoped key: the prefixes decide.
    Scoped(Arc<Scopes>),
}

struct Entry {
    id: String,
    secret: String,
    grant: Grant,
    rights: Rights,
}

/// The keys of the server that may log in at the OCI front. What each may do there is worked out
/// from the prefix of the front they are attached to ([`OciFront::with_auth`](super::OciFront)),
/// never from another: until attached, no key has a right.
pub struct OciAuth {
    keys: Vec<Entry>,
}

impl OciAuth {
    /// The keys of `server`, to be attached to a front with
    /// [`OciFront::with_auth`](super::OciFront::with_auth).
    #[must_use]
    pub fn from_server(server: &ServerConfig) -> Self {
        let entry = |id: &str, secret: &str, grant| Entry {
            id: id.to_owned(),
            secret: secret.to_owned(),
            grant,
            rights: Rights {
                pull: false,
                push: false,
            },
        };
        let mut keys = vec![entry(
            &server.access_key_id,
            &server.secret_access_key,
            Grant::Everything,
        )];
        for key in &server.read_only_keys {
            keys.push(entry(&key.id, &key.secret, Grant::Pulling));
        }
        for key in &server.gc_keys {
            keys.push(entry(key.id(), key.secret(), Grant::Scoped(key.scopes())));
        }
        for key in &server.scoped_keys {
            keys.push(entry(key.id(), key.secret(), Grant::Scoped(key.scopes())));
        }
        Self { keys }
    }

    /// The keys with the rights each has over the objects under `prefix`, which are the front's.
    pub(super) fn bound(mut self, prefix: &OciPrefix) -> Self {
        let base = format!("{}blobs/sha256/", prefix.as_str());
        for entry in &mut self.keys {
            entry.rights = match &entry.grant {
                Grant::Everything => Rights {
                    pull: true,
                    push: true,
                },
                Grant::Pulling => Rights {
                    pull: true,
                    push: false,
                },
                Grant::Scoped(scopes) => Rights {
                    pull: scopes.reads_under(&base),
                    push: scopes.writes_under(&base),
                },
            };
        }
        self
    }

    /// Whether the call whose headers are `headers` may be made, given what it `need`s.
    ///
    /// # Errors
    /// [`Refused::Unauthorized`] when the request names no key of the server with its secret,
    /// [`Refused::Denied`] when the key it names lacks the right.
    pub(super) fn check(&self, headers: &HeaderMap, need: Need) -> Result<(), Refused> {
        let rights = self.login(headers).ok_or(Refused::Unauthorized)?;
        let allowed = match need {
            Need::Any => rights.pull || rights.push,
            Need::Pull => rights.pull,
            Need::Push => rights.push,
        };
        if allowed {
            Ok(())
        } else {
            Err(Refused::Denied)
        }
    }

    /// The rights of the key that `Authorization: Basic` names, if its secret is right.
    fn login(&self, headers: &HeaderMap) -> Option<Rights> {
        let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
        let (scheme, encoded) = value.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("basic") {
            return None;
        }
        let decoded = STANDARD.decode(encoded.trim()).ok()?;
        let text = std::str::from_utf8(&decoded).ok()?;
        let (id, secret) = text.split_once(':')?;
        let entry = self.keys.iter().find(|entry| entry.id == id)?;
        same_secret(&entry.secret, secret).then_some(entry.rights)
    }
}

/// Whether `offered` is `secret`, compared without stopping at the first byte that differs: the
/// tag of each as the message under one fixed HMAC key is taken, and `ring` checks the two tags in
/// the same time whatever they are. The secrets are messages, not keys, so a secret and the same
/// secret followed by NUL bytes give different tags.
fn same_secret(secret: &str, offered: &str) -> bool {
    const KEY: &[u8] = b"tierstore oci login";
    let key = hmac::Key::new(hmac::HMAC_SHA256, KEY);
    let offered = hmac::sign(&key, offered.as_bytes());
    hmac::verify(&key, secret.as_bytes(), offered.as_ref()).is_ok()
}
