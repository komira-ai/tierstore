//! [`run`]: every case against one store, each under its own prefix of a root, and the
//! [`Report`] of what each found.

use std::fmt;

use futures::TryStreamExt as _;
use objstore_s3::{KeyPrefix, ObjectStore, PageSize, listing};

use crate::cases::child;
use crate::violation::failed;
use crate::{Call, Case, IdenticalPut, Violation};

/// What one case found.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The case.
    pub case: Case,
    /// `Ok`, or the first violation it found (or, if it found none, the first its clean-up found).
    pub result: Result<(), Violation>,
}

/// What a run found, case by case in the order of [`Case::ALL`].
#[derive(Clone, Debug)]
pub struct Report {
    /// One outcome per case.
    pub outcomes: Vec<Outcome>,
}

impl Report {
    /// Whether every case passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.outcomes.iter().all(|o| o.result.is_ok())
    }

    /// The violations, in case order.
    pub fn violations(&self) -> impl Iterator<Item = &Violation> {
        self.outcomes.iter().filter_map(|o| o.result.as_ref().err())
    }
}

/// One line per case: `PASS <case>`, or `FAIL <violation>` (the violation names its case).
impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for o in &self.outcomes {
            match &o.result {
                Ok(()) => writeln!(f, "PASS {}", o.case)?,
                Err(v) => writeln!(f, "FAIL {v}")?,
            }
        }
        Ok(())
    }
}

/// Runs every case of [`Case::ALL`] against `store`, each under `<root><case>/`, which should hold
/// nothing, and then deletes every key under that prefix and checks that it lists nothing.
/// `identical` is what the store answers to a write of the bytes a key holds.
pub async fn run<S: ObjectStore>(store: &S, root: &KeyPrefix, identical: IdenticalPut) -> Report {
    let mut outcomes = Vec::with_capacity(Case::ALL.len());
    for case in Case::ALL {
        let result = match child(case, root, &format!("{}/", case.name())) {
            Ok(prefix) => {
                let found = case.check(store, &prefix, identical).await;
                let cleaned = clean(case, store, &prefix).await;
                found.and(cleaned)
            }
            Err(violation) => Err(violation),
        };
        outcomes.push(Outcome { case, result });
    }
    Report { outcomes }
}

/// Deletes every key under `prefix` and checks that the prefix then lists nothing.
async fn clean<S: ObjectStore>(case: Case, store: &S, prefix: &KeyPrefix) -> Result<(), Violation> {
    let shown = format!("{:?}", prefix.as_str());
    let left: Vec<_> = listing::objects(store, prefix, None, PageSize::MAX)
        .try_collect()
        .await
        .map_err(failed(case, Call::List, &shown))?;
    for object in &left {
        let k = &object.key;
        store
            .delete(k)
            .await
            .map_err(failed(case, Call::Delete, k))?;
    }
    crate::cases_list::listed(case, store, prefix, Vec::new()).await
}
