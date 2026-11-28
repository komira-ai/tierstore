//! The commands of `fake-s3`'s control socket, as the program reads them.

use super::*;

fn stalled(prefix: &str) -> Command {
    Command::Serve(Serving::Stalled {
        prefix: prefix.to_owned(),
    })
}

fn failing(status: u16, prefix: &str) -> Command {
    Command::Serve(Serving::Failing {
        prefix: prefix.to_owned(),
        status,
    })
}

/// Asserts that each command is read with its words, a prefix left out meaning every key, and
/// whitespace around the words ignored.
#[test]
fn every_command_is_read_with_its_words() {
    assert_eq!(parse("serve"), Ok(Command::Serve(Serving::All)));
    assert_eq!(parse("stall"), Ok(stalled("")));
    assert_eq!(parse("  stall cas/p/  "), Ok(stalled("cas/p/")));
    assert_eq!(parse("fail 503"), Ok(failing(503, "")));
    assert_eq!(parse("fail 500 cas/"), Ok(failing(500, "cas/")));
    assert_eq!(parse("fail 400"), Ok(failing(400, "")));
    assert_eq!(parse("fail 599"), Ok(failing(599, "")));
    assert_eq!(parse("stats"), Ok(Command::Stats));
    assert_eq!(parse("stop"), Ok(Command::Stop));
}

/// Asserts that a line that is no command is refused with its reason: an empty line, an unknown
/// word, a `fail` with no status or with one that is not an error status, and a word after a
/// whole command.
#[test]
fn a_line_that_is_no_command_is_refused_with_its_reason() {
    assert_eq!(parse(""), Err("an empty line".to_owned()));
    assert_eq!(parse("   "), Err("an empty line".to_owned()));
    assert_eq!(parse("halt"), Err("unknown command \"halt\"".to_owned()));
    assert_eq!(parse("fail"), Err("fail needs a status".to_owned()));
    for word in ["399", "600", "slow", "-1"] {
        assert_eq!(
            parse(&format!("fail {word}")),
            Err(format!("{word:?} is not a status of 400 to 599"))
        );
    }
    assert_eq!(
        parse("serve now"),
        Err("\"now\" after the command".to_owned())
    );
    assert_eq!(
        parse("stall a b"),
        Err("\"b\" after the command".to_owned())
    );
    assert_eq!(
        parse("fail 503 a b"),
        Err("\"b\" after the command".to_owned())
    );
    assert_eq!(parse("stats x"), Err("\"x\" after the command".to_owned()));
}
