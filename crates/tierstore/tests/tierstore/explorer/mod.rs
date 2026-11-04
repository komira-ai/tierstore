//! The conformance explorer: the real [`Proxy`](tierstore::Proxy) and
//! [`EntryReconciler`](tierstore::EntryReconciler) over in-memory tiers, run under every
//! interleaving of their store calls that a bound allows, and checked against the rules of the
//! design (section 8, "The model check"): no acknowledged put lost, no get of an acknowledged
//! delete's bytes, write-once, and a reconciler that reaches a quiet state with no entry left.
//!
//! - `world` is the harness: the actors, the gates that hold each of their calls, the driver.
//! - `check` judges a finished run, and has the tests of that judgement on histories made by hand.
//! - `search` chooses the schedules: all of them within a bound of preemptions, or seeded walks.
//! - `scenarios` are the runs, and the two counterexamples of the model as fixed schedules.

mod check;
mod scenarios;
mod search;
mod world;
