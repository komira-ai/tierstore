//! [`Live`]: the credentials of a store as requests are signed, fetched from their source, kept,
//! and renewed before they expire (the module docs of `credentials` say when).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use super::answer::Http;
use super::{AwsEnv, CredentialSource, Credentials, CredentialsError, Kind};
use super::{container, env, imds, profile, sts};
use crate::s3::sigv4::Utc;

/// How long before they expire temporary credentials are renewed (or half their lifetime, if
/// that is shorter).
pub(super) const RENEW_BEFORE: Duration = Duration::from_secs(5 * 60);

/// Credentials with this long or less left are never signed with.
pub(super) const MARGIN: Duration = Duration::from_secs(60);

/// After a renewal fails, how long the credentials kept are signed with before the next try.
pub(super) const RETRY_AFTER: Duration = Duration::from_secs(10);

/// One set of keys to sign with.
#[derive(Clone)]
pub(crate) struct Keys {
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
    /// The session token of temporary credentials, sent as `x-amz-security-token`.
    pub(crate) session_token: Option<String>,
}

/// Keys a source gave, and when they expire (`None`: never).
pub(super) struct Fetched {
    keys: Keys,
    expires: Option<SystemTime>,
}

impl Fetched {
    pub(super) fn temporary(keys: Keys, expires: SystemTime) -> Self {
        Self {
            keys,
            expires: Some(expires),
        }
    }

    fn fixed(keys: Keys) -> Self {
        Self {
            keys,
            expires: None,
        }
    }
}

/// The time source of the renewals: the machine's clock, or a test's.
pub(crate) type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// The machine's clock.
pub(crate) fn system_clock() -> Clock {
    #[expect(
        clippy::disallowed_methods,
        reason = "temporary credentials expire by the wall clock the service checks them by; tests hand a clock of their own"
    )]
    Arc::new(SystemTime::now)
}

/// The credentials of a store.
pub(crate) enum Live {
    /// Keys that never change.
    Fixed(Arc<Keys>),
    /// Keys from a source, renewed as they near their end.
    Renewing(Box<Renewing>),
}

/// What a source of credentials needs to be asked, and what it last gave.
pub(crate) struct Renewing {
    source: CredentialSource,
    env: Arc<AwsEnv>,
    http: Http,
    /// The region whose STS endpoint is asked when the environment names none (the store's).
    region: String,
    clock: Clock,
    state: Mutex<State>,
}

/// The source settled on, and the keys it gave last.
#[derive(Default)]
struct State {
    plan: Option<Plan>,
    kept: Option<Kept>,
}

/// Keys kept, and when to renew them.
struct Kept {
    keys: Arc<Keys>,
    from: CredentialSource,
    expires: Option<SystemTime>,
    renew_at: Option<SystemTime>,
}

impl Kept {
    /// Keys of `fetched`, at `now`, from the source `from`.
    fn new(fetched: Fetched, from: CredentialSource, now: SystemTime) -> Self {
        let renew_at = fetched.expires.map(|expires| {
            let life = expires.duration_since(now).unwrap_or(Duration::ZERO);
            expires
                .checked_sub(RENEW_BEFORE.min(life / 2))
                .unwrap_or(UNIX_EPOCH)
        });
        Self {
            keys: Arc::new(fetched.keys),
            from,
            expires: fetched.expires,
            renew_at,
        }
    }

    /// Whether it is not yet time to renew them.
    fn fresh(&self, now: SystemTime) -> bool {
        self.renew_at.is_none_or(|at| now < at)
    }

    /// Whether they may still sign: more than [`MARGIN`] is left.
    fn usable(&self, now: SystemTime) -> bool {
        self.expires
            .is_none_or(|expires| now.checked_add(MARGIN).is_some_and(|end| end < expires))
    }

    /// When they expire, for a message.
    fn expiry(&self) -> String {
        self.expires
            .map_or_else(|| "never".to_owned(), |at| Utc::of(at, false).iso8601())
    }
}

impl Live {
    /// The live credentials of `credentials`: a source's are asked through the client `http`
    /// makes (called for a source alone) and renewed by `clock`, and `region` is the store's.
    ///
    /// # Errors
    /// As `http`.
    pub(crate) fn new<E>(
        credentials: &Credentials,
        http: impl FnOnce() -> Result<Http, E>,
        region: &str,
        clock: Clock,
    ) -> Result<Self, E> {
        Ok(match &credentials.kind {
            Kind::Fixed(keys) => Self::Fixed(Arc::clone(keys)),
            Kind::Source(source, env) => Self::Renewing(Box::new(Renewing {
                source: *source,
                env: Arc::clone(env),
                http: http()?,
                region: region.to_owned(),
                clock,
                state: Mutex::default(),
            })),
        })
    }

    /// The keys to sign the next request with.
    ///
    /// # Errors
    /// As the source fails, or [`CredentialsError::Expired`] (see the module docs).
    pub(crate) async fn keys(&self) -> Result<Arc<Keys>, CredentialsError> {
        match self {
            Self::Fixed(keys) => Ok(Arc::clone(keys)),
            Self::Renewing(renewing) => renewing.keys().await,
        }
    }
}

impl Renewing {
    async fn keys(&self) -> Result<Arc<Keys>, CredentialsError> {
        // Held while a renewal is fetched: the requests behind it wait for its keys, so one
        // renewal is asked for, not one for each request.
        let mut state = self.state.lock().await;
        let now = (self.clock)();
        if let Some(kept) = &state.kept
            && kept.fresh(now)
        {
            return Ok(Arc::clone(&kept.keys));
        }
        match self.fetch(&mut state.plan, now).await {
            Ok((fetched, from)) => {
                let kept = Kept::new(fetched, from, now);
                if !kept.usable(now) {
                    return Err(CredentialsError::Expired {
                        from,
                        at: kept.expiry(),
                        why: "the source gave credentials with a minute or less left".to_owned(),
                    });
                }
                tracing::info!(source = %from, expires = %kept.expiry(), "fetched AWS credentials");
                let keys = Arc::clone(&kept.keys);
                state.kept = Some(kept);
                Ok(keys)
            }
            Err(error) => match &mut state.kept {
                Some(kept) if kept.usable(now) => {
                    tracing::warn!(
                        source = %kept.from,
                        expires = %kept.expiry(),
                        %error,
                        "renewing AWS credentials failed; signing with the current ones until they near their end"
                    );
                    kept.renew_at = now.checked_add(RETRY_AFTER);
                    Ok(Arc::clone(&kept.keys))
                }
                Some(kept) => Err(CredentialsError::Expired {
                    from: kept.from,
                    at: kept.expiry(),
                    why: error.to_string(),
                }),
                None => Err(error),
            },
        }
    }

    /// Fetches keys from the source settled on in `plan`, or settles on one first: the source
    /// named, or for [`CredentialSource::Auto`] the first of [`CHAIN`] that is set up. A source
    /// settled on is kept once it gives keys. The instance metadata service cannot be known to be
    /// there without asking it, so when `Auto` comes to it and it gives no credentials, the error
    /// says why each source did not.
    async fn fetch(
        &self,
        plan: &mut Option<Plan>,
        now: SystemTime,
    ) -> Result<(Fetched, CredentialSource), CredentialsError> {
        if let Some(settled) = plan.as_ref() {
            return Ok((self.ask(settled, now).await?, settled.from()));
        }
        let auto = self.source == CredentialSource::Auto;
        let mut unset = Vec::new();
        for (source, find) in CHAIN.into_iter().filter(|(s, _)| auto || *s == self.source) {
            let settled = match find(&self.env)? {
                Found::Plan(settled) => settled,
                Found::Unset(why) if auto => {
                    unset.push(format!("{source}: {why}"));
                    continue;
                }
                Found::Unset(why) => return Err(CredentialsError::NotSet { from: source, why }),
            };
            return match self.ask(&settled, now).await {
                Ok(fetched) => {
                    *plan = Some(settled);
                    Ok((fetched, source))
                }
                Err(CredentialsError::Failed { why, .. })
                    if auto && source == CredentialSource::InstanceMetadata =>
                {
                    unset.push(format!("{source}: {why}"));
                    Err(CredentialsError::NoSource(unset.join("; ")))
                }
                Err(error) => Err(error),
            };
        }
        Err(CredentialsError::NoSource(unset.join("; ")))
    }

    /// Fetches keys from `plan`.
    async fn ask(&self, plan: &Plan, now: SystemTime) -> Result<Fetched, CredentialsError> {
        let failed = |from: CredentialSource| move |why| CredentialsError::Failed { from, why };
        match plan {
            Plan::Fixed(_, keys) => Ok(Fetched::fixed(keys.clone())),
            Plan::Role(role) => {
                let region = self.env.region().unwrap_or(&self.region);
                let fetched = sts::fetch(&self.http, role, &self.env, region, now).await;
                fetched.map_err(failed(role.from))
            }
            Plan::Container(endpoint) => container::fetch(&self.http, endpoint)
                .await
                .map_err(failed(CredentialSource::Container)),
            Plan::Imds(endpoint) => imds::fetch(&self.http, endpoint)
                .await
                .map_err(failed(CredentialSource::InstanceMetadata)),
        }
    }
}

/// Whether a source is set up in an environment.
///
/// # Errors
/// The source is set up wrongly (one of two variables that go together, a profile that is named
/// and not there or asks for what is not supported, an endpoint that may not be asked).
type Find = fn(&AwsEnv) -> Result<Found, CredentialsError>;

/// The sources [`CredentialSource::Auto`] tries, in order, each with what finds it.
const CHAIN: [(CredentialSource, Find); 5] = [
    (CredentialSource::Environment, env_keys),
    (CredentialSource::Profile, profile::find),
    (CredentialSource::WebIdentity, sts::find),
    (CredentialSource::Container, container::find),
    (CredentialSource::InstanceMetadata, imds::find),
];

/// The keys of the environment, if it has them.
fn env_keys(env: &AwsEnv) -> Result<Found, CredentialsError> {
    Ok(match env::keys(env)? {
        Some(keys) => Found::Plan(Plan::Fixed(CredentialSource::Environment, keys)),
        None => Found::Unset("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are not set".to_owned()),
    })
}

/// A source that is set up: what to ask it.
pub(super) enum Plan {
    /// Keys of the environment or of a profile.
    Fixed(CredentialSource, Keys),
    /// A role taken with a web identity token.
    Role(sts::Role),
    /// The container credentials endpoint.
    Container(container::Endpoint),
    /// The instance metadata service.
    Imds(imds::Endpoint),
}

impl Plan {
    fn from(&self) -> CredentialSource {
        match self {
            Self::Fixed(from, _) => *from,
            Self::Role(role) => role.from,
            Self::Container(_) => CredentialSource::Container,
            Self::Imds(_) => CredentialSource::InstanceMetadata,
        }
    }
}

/// Whether a source is set up.
pub(super) enum Found {
    /// It is: this is what to ask.
    Plan(Plan),
    /// It is not, for this reason.
    Unset(String),
}
