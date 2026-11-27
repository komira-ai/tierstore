//! The name of a source: who asks (the agent), for which run, and the source's place in the job;
//! and the path under the mount of `tierstore fs` where the source's tree appears, which is made of
//! the name alone, so that a release, a sweep and a mount after a restart of this process find the
//! tree without anything this process remembered.
//!
//! The path is `mount-provider/<agent>/<run>/<index>`, the agent and the run in lowercase hex of
//! their bytes (so any text is a valid name of the mount, and no two names give one path) and the
//! index in decimal. A source that names its attempt (`Source.attempt`, 1 or more) is at
//! `mount-provider/<agent>/<run>/a<attempt>/<index>`, the attempt in decimal: two attempts of one
//! run are two sources with two paths, so a call of one never reaches the other's tree, and none of
//! them is inside another (an index is digits alone, an attempt's name begins with `a`). Every
//! placement of an agent is below `mount-provider/<agent>/`.

use tierstore_trees_api::provider::v1::Source;

use super::ProviderError;
use crate::server::local_cache::trees::Location;

/// The first name of every path the provider places a tree at.
pub(super) const ROOT: &str = "mount-provider";

/// The longest agent or run, in bytes: its hex, twice as long, is still a name of the mount (at
/// most 255 bytes).
pub(super) const MAX_NAME_BYTES: usize = 120;

/// One source of one run of one agent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct SourceId {
    /// The agent's instance.
    pub agent: String,
    /// The run, `js.<run id>`.
    pub run: String,
    /// The source's place in the job.
    pub index: u32,
    /// The attempt of the run on the agent, 0 if the agent did not name one.
    pub attempt: u64,
}

impl SourceId {
    /// The source a request names.
    ///
    /// # Errors
    /// [`ProviderError::Invalid`] for no source, or an agent or a run that is empty or longer than
    /// [`MAX_NAME_BYTES`].
    pub(super) fn of(source: Option<Source>) -> Result<Self, ProviderError> {
        let source = source.ok_or_else(|| ProviderError::Invalid("no source".to_owned()))?;
        check_name("agent", &source.agent)?;
        check_name("run", &source.run)?;
        Ok(Self {
            agent: source.agent,
            run: source.run,
            index: source.index,
            attempt: source.attempt,
        })
    }

    /// The same source as an agent that names no attempt names it: what a provider before
    /// attempts placed, or an agent before it counted them asked for.
    pub(super) fn unnumbered(&self) -> Self {
        Self {
            attempt: 0,
            ..self.clone()
        }
    }

    /// The path under the mount where the source's tree appears.
    pub(super) fn path(&self) -> String {
        format!("{}/{}", self.run_dir(), self.index)
    }

    /// The directory of the paths of every source of the attempt of the run, with no `/` at the
    /// end: what the cache knows the run by between its request and its start
    /// (`tierstore_request_to_start_seconds`). Each attempt has its own.
    pub(super) fn run_dir(&self) -> String {
        let run = format!("{}{}", agent_dir(&self.agent), hex::encode(&self.run));
        match self.attempt {
            0 => run,
            attempt => format!("{run}/a{attempt}"),
        }
    }

    /// The path as a location of the tree store.
    pub(super) fn location(&self) -> Location {
        Location::Fs { path: self.path() }
    }
}

/// Refuses an agent or a run (`what`) that is empty or too long.
///
/// # Errors
/// [`ProviderError::Invalid`] naming `what`.
pub(super) fn check_name(what: &str, name: &str) -> Result<(), ProviderError> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(ProviderError::Invalid(format!(
            "the {what} is 1 to {MAX_NAME_BYTES} bytes"
        )));
    }
    Ok(())
}

/// The directory of every path of `agent`, with its `/` at the end.
pub(super) fn agent_dir(agent: &str) -> String {
    format!("{ROOT}/{}/", hex::encode(agent))
}

/// The source whose path is `path`, a path of the mount, if it is the path of a source of `agent`
/// exactly as [`SourceId::path`] makes it.
pub(super) fn source_of(path: &str, agent: &str) -> Option<SourceId> {
    let rest = path.strip_prefix(&agent_dir(agent))?;
    let (run, rest) = rest.split_once('/')?;
    let (attempt, index) = match rest.split_once('/') {
        Some((attempt, index)) => (attempt.strip_prefix('a')?.parse().ok()?, index),
        None => (0, rest),
    };
    let id = SourceId {
        agent: agent.to_owned(),
        run: String::from_utf8(hex::decode(run).ok()?).ok()?,
        index: index.parse().ok()?,
        attempt,
    };
    (id.path() == path).then_some(id)
}
