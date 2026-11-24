//! The hand-over of the launchd arm: the way of an update, and what it needs of the releases.
//!
//! There is one job, so an update cannot start a second instance beside the first. When both
//! releases list [`HANDOFF_EXEC_CAPABILITY`], the process of the job executes the next image of
//! its program on `SIGUSR2` (`launchctl kill`), keeping its pid, its sockets and its mount. Any
//! other update is a restart, which boots the job out and unmounts. A node whose file has an `fs`
//! interface holds a mount, and a restart would drop it under running jobs: `prepare` refuses
//! such an update unless the params say `mode = "restart"`.
//!
//! The way is decided from the params and the releases' capabilities alone, in `prepare`,
//! `cutover` and `rollback` alike; the record holds no mode.

use std::path::Path;

use super::super::HANDOFF_EXEC_CAPABILITY;
use super::super::params::Params;
use super::super::report::{Done, Reason};
use super::super::shape::fs_interface;
use super::Ctx;

/// The way of an update.
#[derive(Debug, PartialEq, Eq)]
enum Way {
    /// The job's process executes the next image of its program.
    HandOver,
    /// The job is booted out and in. The releases that lack the word, as the detail of a
    /// refusal names them; none when the params decided.
    Restart(Vec<String>),
}

impl Ctx<'_, '_> {
    /// Whether the update the params describe is a hand-over. A first install, an update whose
    /// params name the fs job (a Mac of the old layout, whose fs job runs the same binary) or say
    /// `mode = "restart"`, every update of a package that carries no binary, and an update between releases of which one lacks the word, are
    /// restarts. The releases are asked only when the params do not decide.
    pub(super) fn hands_over(&self, params: &Params) -> Result<bool, Done> {
        Ok(self.way(params)? == Way::HandOver)
    }

    fn way(&self, params: &Params) -> Result<Way, Done> {
        let Some(previous) = &params.previous else {
            return Ok(Way::Restart(Vec::new()));
        };
        if !self.releases || self.fs.is_some() || params.declared_restart {
            return Ok(Way::Restart(Vec::new()));
        }
        let mut lacking = Vec::new();
        for (which, release) in [
            ("staged", &params.staged_release),
            ("previous", &previous.release),
        ] {
            let words = self.ports.host.capabilities(release)?;
            if !words.iter().any(|word| word == HANDOFF_EXEC_CAPABILITY) {
                lacking.push(format!("the {which} release {}", release.display()));
            }
        }
        Ok(if lacking.is_empty() {
            Way::HandOver
        } else {
            Way::Restart(lacking)
        })
    }

    /// Decides the way in `prepare`, over the text of the node's file: a restart of a node that
    /// holds a mount is refused, and the staged release checks the file before a hand-over.
    /// Returns whether the update is a hand-over.
    pub(super) fn plan(&self, params: &Params, config: &Path, text: &str) -> Result<bool, Done> {
        let lacking = match self.way(params)? {
            Way::HandOver => {
                self.check_staged(params, config)?;
                return Ok(true);
            }
            Way::Restart(lacking) => lacking,
        };
        // A restart that the params ask for, or that the old layout makes (the file service is a
        // job of its own there, and its stop is the unmount), is as it was.
        let decided = lacking.is_empty();
        if !decided && fs_interface(text).is_some() {
            return Err(Done::fail(
                Reason::ModeUnsupported,
                format!(
                    "the node holds a mount (its file has an fs interface) and this update would \
                     be a restart: {} lacks {HANDOFF_EXEC_CAPABILITY}. A restart would drop the \
                     mount under running jobs; mode = \"restart\" in the params overrides this \
                     for a node whose jobs are drained",
                    lacking.join(" and ")
                ),
            ));
        }
        Ok(false)
    }

    /// Has the staged release check `config`: a hand-over must not meet a file the new image
    /// refuses, which would make it exit and be started again by launchd.
    fn check_staged(&self, params: &Params, config: &Path) -> Result<(), Done> {
        match self
            .ports
            .host
            .check_config(&params.staged_release, config)?
        {
            None => Ok(()),
            Some(line) => Err(Done::fail(
                Reason::ConfigRefused,
                format!(
                    "the staged release {} refuses {}: {line}",
                    params.staged_release.display(),
                    config.display()
                ),
            )),
        }
    }
}
