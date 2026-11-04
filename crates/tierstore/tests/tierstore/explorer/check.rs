//! What the explorer checks of a finished run: the history of what the clients were answered, and
//! the state the tiers were left in once the reconciler had nothing more to do. The check is a
//! function of that record alone, so its own tests feed it histories made by hand, including ones
//! that break each rule.

/// What one actor did or was answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// The actor: the index of the client, or [`SEED`] for the setup before the run.
    pub who: usize,
    pub kind: Kind,
}

/// The actor that made the setup of a run, before any client began.
pub const SEED: usize = usize::MAX;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    PutStart {
        text: String,
    },
    /// A put's end; `acked` is whether it was answered `Ok`.
    PutDone {
        text: String,
        acked: bool,
    },
    GetStart,
    /// A get's answer: the bytes, or `None` for not found.
    GetDone {
        got: Option<String>,
    },
    GetFailed,
    DeleteStart,
    DeleteDone {
        acked: bool,
    },
    /// A step of the reconciler, and how many keys it found whose hot and cold bytes differ.
    Step {
        mismatched: usize,
    },
    /// The end of the quiet phase: whether a step changed nothing within the allowed steps.
    Quiet {
        reached: bool,
    },
}

/// The state left behind, read after the quiet phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct End {
    pub hot: Option<String>,
    pub cold: Option<String>,
    /// What a get of the key by a proxy with nothing in flight answered: the bytes, `None` for not
    /// found, or the text of its failure.
    pub read: Result<Option<String>, String>,
    /// The entries left in the hot tier.
    pub entries: Vec<String>,
}

fn is_put_done(kind: &Kind) -> Option<(&str, bool)> {
    match kind {
        Kind::PutDone { text, acked } => Some((text, *acked)),
        _ => None,
    }
}

/// Whether the put that ended at `ended` began before the event at `limit`: only such a put could
/// have supplied a get that was answered at `limit`. A put with no start in the history is taken
/// to have begun at the first event.
fn put_started_before(history: &[Event], ended: usize, limit: usize) -> bool {
    let who = history[ended].who;
    history[..ended]
        .iter()
        .rposition(|earlier| earlier.who == who && matches!(earlier.kind, Kind::PutStart { .. }))
        .unwrap_or(0)
        < limit
}

/// The rules a run breaks, each as a sentence; empty for a run that breaks none.
///
/// - **Write-once.** Two acknowledged puts of the key have a delete that ended between them.
/// - **No acknowledged put lost.** The last acknowledged put, if no delete ended after it, is what
///   a get answers and what the cold tier holds at the end.
/// - **No acknowledged delete undone.** After an acknowledged delete, neither tier holds the key
///   and a get answers not found, unless a put overlapped the delete: an acknowledged put that
///   ended after the delete was acknowledged, or a refused put that ended after the delete began
///   (a refusal may still have landed its bytes). A refused put that ended before the delete began
///   excuses nothing.
/// - **No get of a deleted key.** A get that began after a delete was acknowledged never answers
///   the bytes of a put that the delete covered, unless a put of the same bytes that ended after
///   the delete had begun before the get was answered.
/// - **Liveness.** From the quiet state the reconciler reaches a step that changes nothing, no
///   entry is left, and the two tiers never hold different bytes.
#[must_use]
pub fn judge(history: &[Event], end: &End) -> Vec<String> {
    let mut broken = Vec::new();
    let acked_puts: Vec<(usize, &str)> = history
        .iter()
        .enumerate()
        .filter_map(|(at, event)| match is_put_done(&event.kind) {
            Some((text, true)) => Some((at, text)),
            _ => None,
        })
        .collect();
    let put_ends: Vec<(usize, &str, bool)> = history
        .iter()
        .enumerate()
        .filter_map(|(at, event)| is_put_done(&event.kind).map(|(text, acked)| (at, text, acked)))
        .collect();
    let delete_ends: Vec<(usize, bool)> = history
        .iter()
        .enumerate()
        .filter_map(|(at, event)| match event.kind {
            Kind::DeleteDone { acked } => Some((at, acked)),
            _ => None,
        })
        .collect();
    let known: Vec<&str> = put_ends.iter().map(|(_, text, _)| *text).collect();

    for pair in acked_puts.windows(2) {
        let between = delete_ends
            .iter()
            .any(|(at, _)| *at > pair[0].0 && *at < pair[1].0);
        if !between {
            broken.push(format!(
                "write-once: puts of {:?} and {:?} were both acknowledged with no delete between",
                pair[0].1, pair[1].1
            ));
        }
    }

    if let Some(&(at, text)) = acked_puts.last()
        && !delete_ends.iter().any(|(ended, _)| *ended > at)
    {
        if end.read != Ok(Some(text.to_owned())) {
            broken.push(format!(
                "acknowledged put lost: {text:?} was acknowledged and no delete followed, a get answers {:?}",
                end.read
            ));
        }
        if end.cold.as_deref() != Some(text) {
            broken.push(format!(
                "acknowledged put not replicated: the cold tier holds {:?}, not {text:?}",
                end.cold
            ));
        }
    }

    let last_acked_delete = delete_ends.iter().rev().find(|(_, acked)| *acked);
    if let Some(&(at, _)) = last_acked_delete {
        let began = history[..at]
            .iter()
            .rposition(|earlier| {
                earlier.who == history[at].who && earlier.kind == Kind::DeleteStart
            })
            .unwrap_or(0);
        // A put overlaps the delete when it ended after the delete began. An acknowledged put
        // that ended before the delete was acknowledged is ordered by it; a refused put may have
        // landed its bytes late, so only one that overlaps the delete is excused. A refused put
        // that ended before the delete began leaves the delete's duty whole.
        let settled = put_ends.iter().all(
            |&(ended, _, acked)| {
                if acked { ended < at } else { ended < began }
            },
        );
        let present = end.hot.is_some() || end.cold.is_some() || end.read != Ok(None);
        if settled && present {
            broken.push(format!(
                "acknowledged delete undone: hot {:?}, cold {:?}, a get answers {:?}",
                end.hot, end.cold, end.read
            ));
        }
    }

    for (at, event) in history.iter().enumerate() {
        let Kind::GetDone { got: Some(text) } = &event.kind else {
            continue;
        };
        if !known.contains(&text.as_str()) {
            broken.push(format!("a get answered {text:?}, which no put wrote"));
            continue;
        }
        let began = history[..at]
            .iter()
            .rposition(|earlier| earlier.who == event.who && earlier.kind == Kind::GetStart)
            .unwrap_or(0);
        let covered = delete_ends.iter().any(|&(deleted, acked)| {
            let put_before = acked_puts
                .iter()
                .any(|&(put, bytes)| bytes == text && put < deleted);
            let rewritten = put_ends.iter().any(|&(ended, bytes, _)| {
                bytes == text && ended > deleted && put_started_before(history, ended, at)
            });
            acked && deleted < began && put_before && !rewritten
        });
        if covered {
            broken.push(format!(
                "a get began after a delete was acknowledged and answered {text:?}, which the delete covered"
            ));
        }
    }

    for event in history {
        match &event.kind {
            Kind::Step { mismatched } if *mismatched > 0 => {
                broken.push("a step found hot and cold bytes of the key that differ".to_owned());
            }
            Kind::GetFailed => broken.push("a get failed with no fault injected".to_owned()),
            Kind::Quiet { reached: false } => {
                broken.push("the reconciler never reached a step that changes nothing".to_owned());
            }
            _ => {}
        }
    }
    if !end.entries.is_empty() {
        broken.push(format!(
            "entries left after the quiet phase: {:?}",
            end.entries
        ));
    }
    for (tier, bytes) in [("hot", &end.hot), ("cold", &end.cold)] {
        if let Some(text) = bytes
            && !known.contains(&text.as_str())
        {
            broken.push(format!(
                "the {tier} tier holds {text:?}, which no put wrote"
            ));
        }
    }
    if end.hot.is_some() && end.cold.is_some() && end.hot != end.cold {
        broken.push(format!(
            "the tiers hold different bytes: hot {:?}, cold {:?}",
            end.hot, end.cold
        ));
    }
    broken
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(who: usize, kind: Kind) -> Event {
        Event { who, kind }
    }

    fn put(who: usize, text: &str, acked: bool) -> Vec<Event> {
        vec![
            ev(
                who,
                Kind::PutStart {
                    text: text.to_owned(),
                },
            ),
            ev(
                who,
                Kind::PutDone {
                    text: text.to_owned(),
                    acked,
                },
            ),
        ]
    }

    fn delete(who: usize, acked: bool) -> Vec<Event> {
        vec![
            ev(who, Kind::DeleteStart),
            ev(who, Kind::DeleteDone { acked }),
        ]
    }

    fn get(who: usize, got: Option<&str>) -> Vec<Event> {
        let got = got.map(str::to_owned);
        vec![ev(who, Kind::GetStart), ev(who, Kind::GetDone { got })]
    }

    fn holding(text: &str) -> End {
        End {
            hot: Some(text.to_owned()),
            cold: Some(text.to_owned()),
            read: Ok(Some(text.to_owned())),
            entries: Vec::new(),
        }
    }

    fn empty() -> End {
        End {
            hot: None,
            cold: None,
            read: Ok(None),
            entries: Vec::new(),
        }
    }

    /// Asserts that a run that breaks no rule is reported clean: a put, then a delete, then
    /// a put of other bytes, each acknowledged, with the last put held in both tiers.
    #[test]
    fn a_run_that_breaks_no_rule_is_clean() {
        let mut history = put(SEED, "bytes0", true);
        history.extend(delete(0, true));
        history.extend(put(0, "bytes1", true));
        history.extend(get(0, Some("bytes1")));
        history.push(ev(1, Kind::Quiet { reached: true }));
        assert_eq!(judge(&history, &holding("bytes1")), Vec::<String>::new());
    }

    /// Asserts that an acknowledged put that no delete covers and that a get does not find, and
    /// the cold tier does not hold, is reported twice: lost and not replicated.
    #[test]
    fn an_acknowledged_put_that_is_gone_is_reported_lost() {
        let history = put(0, "bytes0", true);
        let broken = judge(&history, &empty());
        assert_eq!(broken.len(), 2, "{broken:?}");
        assert!(broken[0].starts_with("acknowledged put lost"), "{broken:?}");
        assert!(
            broken[1].starts_with("acknowledged put not replicated"),
            "{broken:?}"
        );
    }

    /// Asserts that a put a delete ended after is not owed: the key may be gone.
    #[test]
    fn a_put_covered_by_a_later_delete_may_be_gone() {
        let mut history = put(0, "bytes0", true);
        history.extend(delete(1, false));
        assert_eq!(judge(&history, &empty()), Vec::<String>::new());
    }

    /// Asserts that a put that was not acknowledged is not owed.
    #[test]
    fn a_put_that_was_refused_is_not_owed() {
        let history = put(0, "bytes0", false);
        assert_eq!(judge(&history, &empty()), Vec::<String>::new());
    }

    /// Asserts that bytes left in a tier after an acknowledged delete, with every put ended
    /// before it, are reported.
    #[test]
    fn bytes_left_after_an_acknowledged_delete_are_reported() {
        let mut history = put(SEED, "bytes0", true);
        history.extend(delete(0, true));
        let mut end = empty();
        end.cold = Some("bytes0".to_owned());
        let broken = judge(&history, &end);
        assert_eq!(broken.len(), 1, "{broken:?}");
        assert!(
            broken[0].starts_with("acknowledged delete undone"),
            "{broken:?}"
        );
    }

    /// Asserts that a refused put that ended before an acknowledged delete began does not excuse
    /// bytes left in both tiers: an acknowledged put of "old", a refused put of "new", an
    /// acknowledged delete, and an end that holds "old" is reported as an undone delete.
    #[test]
    fn a_refused_put_before_an_acknowledged_delete_does_not_excuse_resurrection() {
        let mut history = put(0, "old", true);
        history.extend(put(1, "new", false));
        history.extend(delete(0, true));
        history.extend(get(0, Some("old")));
        let broken = judge(&history, &holding("old"));
        assert!(
            broken
                .iter()
                .any(|line| line.starts_with("acknowledged delete undone")),
            "{broken:?}"
        );
    }

    /// Asserts that a refused put that overlaps an acknowledged delete is excused: its bytes may
    /// have landed after the delete.
    #[test]
    fn a_refused_put_overlapping_an_acknowledged_delete_is_excused() {
        let mut history = put(SEED, "old", true);
        history.push(ev(
            1,
            Kind::PutStart {
                text: "new".to_owned(),
            },
        ));
        history.extend(delete(0, true));
        history.push(ev(
            1,
            Kind::PutDone {
                text: "new".to_owned(),
                acked: false,
            },
        ));
        assert_eq!(judge(&history, &holding("new")), Vec::<String>::new());
    }

    /// Asserts that a delete that was not acknowledged does not oblige the key to be gone.
    #[test]
    fn an_unacknowledged_delete_leaves_the_key_free_to_stay() {
        let mut history = put(SEED, "bytes0", true);
        history.extend(delete(0, false));
        history.extend(get(0, Some("bytes0")));
        assert_eq!(judge(&history, &holding("bytes0")), Vec::<String>::new());
    }

    /// Asserts that a get that began after an acknowledged delete and answered the covered bytes
    /// is reported, and that a get that began before the acknowledgement is not.
    #[test]
    fn a_get_after_an_acknowledged_delete_may_not_answer_the_deleted_bytes() {
        let mut after = put(SEED, "bytes0", true);
        after.extend(delete(0, true));
        after.extend(get(0, Some("bytes0")));
        let broken = judge(&after, &empty());
        assert_eq!(broken.len(), 1, "{broken:?}");
        assert!(
            broken[0].starts_with("a get began after a delete"),
            "{broken:?}"
        );

        let mut before = put(SEED, "bytes0", true);
        before.push(ev(1, Kind::GetStart));
        before.extend(delete(0, true));
        before.push(ev(
            1,
            Kind::GetDone {
                got: Some("bytes0".to_owned()),
            },
        ));
        assert_eq!(judge(&before, &empty()), Vec::<String>::new());
    }

    /// Asserts that a get of bytes that were put again after the delete is not reported: the new
    /// put may have landed them.
    #[test]
    fn a_get_of_bytes_put_again_after_the_delete_is_not_reported() {
        let mut history = put(SEED, "bytes0", true);
        history.extend(delete(0, true));
        history.extend(put(0, "bytes0", true));
        history.extend(get(0, Some("bytes0")));
        assert_eq!(judge(&history, &holding("bytes0")), Vec::<String>::new());
    }

    /// Asserts that a put of the same bytes that begins only after the get was answered does not
    /// excuse the get: the get returned deleted bytes before any replacement put began.
    #[test]
    fn a_later_put_of_the_same_bytes_does_not_excuse_an_earlier_get() {
        let mut history = put(SEED, "bytes0", true);
        history.extend(delete(0, true));
        history.extend(get(1, Some("bytes0")));
        history.extend(put(0, "bytes0", true));
        let broken = judge(&history, &holding("bytes0"));
        assert_eq!(broken.len(), 1, "{broken:?}");
        assert!(
            broken[0].starts_with("a get began after a delete"),
            "{broken:?}"
        );
    }

    /// Asserts that a put of the same bytes that began before the get was answered and ended
    /// after the delete still excuses the get, though it ended after the get.
    #[test]
    fn a_put_in_flight_when_the_get_is_answered_excuses_it() {
        let mut history = put(SEED, "bytes0", true);
        history.extend(delete(0, true));
        history.push(ev(
            2,
            Kind::PutStart {
                text: "bytes0".to_owned(),
            },
        ));
        history.extend(get(1, Some("bytes0")));
        history.push(ev(
            2,
            Kind::PutDone {
                text: "bytes0".to_owned(),
                acked: true,
            },
        ));
        assert_eq!(judge(&history, &holding("bytes0")), Vec::<String>::new());
    }

    /// Asserts that a get of bytes no put wrote is reported.
    #[test]
    fn a_get_of_bytes_no_put_wrote_is_reported() {
        let history = get(0, Some("strange"));
        let broken = judge(&history, &empty());
        assert_eq!(
            broken,
            vec!["a get answered \"strange\", which no put wrote".to_owned()]
        );
    }

    /// Asserts that two acknowledged puts with no delete between are reported, and that a delete
    /// between them makes it legal.
    #[test]
    fn two_acknowledged_puts_need_a_delete_between() {
        let mut twice = put(0, "bytes0", true);
        twice.extend(put(1, "bytes1", true));
        let broken = judge(&twice, &holding("bytes1"));
        assert_eq!(broken.len(), 1, "{broken:?}");
        assert!(broken[0].starts_with("write-once"), "{broken:?}");

        let mut apart = put(0, "bytes0", true);
        apart.extend(delete(2, true));
        apart.extend(put(1, "bytes1", true));
        assert_eq!(judge(&apart, &holding("bytes1")), Vec::<String>::new());
    }

    /// Asserts the liveness findings: an entry left, a reconciler that never went quiet, a failed
    /// get, a step that found differing bytes, bytes no put wrote and tiers that differ.
    #[test]
    fn a_run_that_does_not_settle_is_reported() {
        let mut history = vec![
            ev(0, Kind::Quiet { reached: false }),
            ev(0, Kind::Step { mismatched: 1 }),
            ev(1, Kind::GetFailed),
        ];
        history.extend(put(SEED, "bytes0", true));
        let mut end = holding("bytes0");
        end.entries = vec!["_entries/x".to_owned()];
        end.hot = Some("other".to_owned());
        let broken = judge(&history, &end);
        let wanted = [
            "never reached a step",
            "differ",
            "get failed",
            "entries left",
            "which no put wrote",
            "different bytes",
        ];
        for part in wanted {
            assert!(
                broken.iter().any(|line| line.contains(part)),
                "{part}: {broken:?}"
            );
        }
    }
}
