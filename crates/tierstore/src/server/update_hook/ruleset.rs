//! The text of the two nftables tables, and the reading of a listing of the redirect table.
//!
//! The hook gives the text to [`Nft::apply`](super::Nft::apply), which feeds it to `nft -f -` on
//! standard input in one transaction: no file is written. `add table` is a no-op when the table
//! exists and `flush table` empties it, so the same text installs a table the first time and
//! replaces it afterwards; a packet is judged by the old rules or the new, never a mix. The guard
//! is a separate table, so a swap of the redirect never flushes it.

/// The redirect table: public port to the serving slot.
pub const FORWARD_TABLE: &str = "kbf_forward_tierstore";
/// The guard table: keeps a direct connection to a private port out.
pub const GUARD_TABLE: &str = "kbf_guard_tierstore";

/// The marker in a listing before the port a rule redirects to.
const REDIRECT_TO: &str = "redirect to :";

/// The redirect of `public` to `target`, for both families, as one text for `nft -f -`.
#[must_use]
pub fn redirect_ruleset(public: u16, target: u16) -> String {
    let t = FORWARD_TABLE;
    format!(
        "add table inet {t}\n\
         flush table inet {t}\n\
         add chain inet {t} pre {{ type nat hook prerouting priority dstnat; policy accept; }}\n\
         add chain inet {t} out {{ type nat hook output priority -100; policy accept; }}\n\
         add rule inet {t} pre fib daddr type local tcp dport {public} redirect to :{target}\n\
         add rule inet {t} out fib daddr type local tcp dport {public} redirect to :{target}\n"
    )
}

/// The guard for the two private ports: loopback and the redirected flows of `public` may
/// reach them, anything else is dropped.
#[must_use]
pub fn guard_ruleset(public: u16, ports: [u16; 2]) -> String {
    let t = GUARD_TABLE;
    let [a, b] = ports;
    format!(
        "add table inet {t}\n\
         flush table inet {t}\n\
         add chain inet {t} in {{ type filter hook input priority filter; policy accept; }}\n\
         add rule inet {t} in iifname \"lo\" tcp dport {{ {a}, {b} }} accept\n\
         add rule inet {t} in tcp dport {{ {a}, {b} }} ct status dnat ct original proto-dst {public} accept\n\
         add rule inet {t} in tcp dport {{ {a}, {b} }} drop\n"
    )
}

/// The port the redirect rules of a listing of [`FORWARD_TABLE`] name: `None` when there is no
/// rule, a port that is not a number, or the rules disagree.
#[must_use]
pub fn redirect_target(listing: &str) -> Option<u16> {
    let mut ports = listing.lines().filter_map(|line| {
        let (_, rest) = line.split_once(REDIRECT_TO)?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        Some(digits.parse::<u16>().ok())
    });
    let first = ports.next()??;
    ports.all(|port| port == Some(first)).then_some(first)
}
