//! The text of the two tables.

use super::{FORWARD_TABLE, GUARD_TABLE, guard_ruleset, redirect_ruleset, redirect_target};

/// Asserts the exact text of the redirect: the table made if absent and flushed, both chains,
/// and one rule per chain naming the public and the private port. It would catch a rule that
/// loses the `fib daddr type local` match (it would redirect the host's connections to other
/// hosts), or a swap that is not one text.
#[test]
fn the_redirect_is_one_text_that_replaces_the_table() {
    let text = redirect_ruleset(9300, 9302);
    let want = "add table inet kbf_forward_tierstore\n\
flush table inet kbf_forward_tierstore\n\
add chain inet kbf_forward_tierstore pre { type nat hook prerouting priority dstnat; policy accept; }\n\
add chain inet kbf_forward_tierstore out { type nat hook output priority -100; policy accept; }\n\
add rule inet kbf_forward_tierstore pre fib daddr type local tcp dport 9300 redirect to :9302\n\
add rule inet kbf_forward_tierstore out fib daddr type local tcp dport 9300 redirect to :9302\n";
    assert_eq!(text, want);
    assert!(text.starts_with(&format!("add table inet {FORWARD_TABLE}\n")));
}

/// Asserts the exact text of the guard: loopback and the redirected flows may reach the private
/// ports and anything else is dropped. It would catch a guard that lets a remote client reach a
/// private port directly.
#[test]
fn the_guard_drops_what_is_not_loopback_or_redirected() {
    let text = guard_ruleset(9300, [9301, 9302]);
    let want = "add table inet kbf_guard_tierstore\n\
flush table inet kbf_guard_tierstore\n\
add chain inet kbf_guard_tierstore in { type filter hook input priority filter; policy accept; }\n\
add rule inet kbf_guard_tierstore in iifname \"lo\" tcp dport { 9301, 9302 } accept\n\
add rule inet kbf_guard_tierstore in tcp dport { 9301, 9302 } ct status dnat ct original proto-dst 9300 accept\n\
add rule inet kbf_guard_tierstore in tcp dport { 9301, 9302 } drop\n";
    assert_eq!(text, want);
    assert!(text.starts_with(&format!("add table inet {GUARD_TABLE}\n")));
}

/// Asserts what is read from a listing: the port both rules name, and nothing when there is no
/// rule, the rules disagree, or the port is not a number. It would catch a swap half applied
/// being taken for a finished one.
#[test]
fn the_target_is_read_from_a_listing() {
    let listing = redirect_ruleset(9300, 9302);
    assert_eq!(redirect_target(&listing), Some(9302));
    assert_eq!(redirect_target(""), None);
    assert_eq!(
        redirect_target("table inet kbf_forward_tierstore {\n}\n"),
        None
    );
    let split = listing.replacen("redirect to :9302", "redirect to :9301", 1);
    assert_eq!(redirect_target(&split), None);
    assert_eq!(redirect_target("tcp dport 9300 redirect to :abc\n"), None);
    assert_eq!(redirect_target("tcp dport 9300 redirect to :99999\n"), None);
    assert_eq!(
        redirect_target("... redirect to :9301\n... redirect to :9301\n"),
        Some(9301)
    );
}
