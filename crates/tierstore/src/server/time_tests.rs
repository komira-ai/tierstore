//! The calendar forms of S3.

use super::time::{amz_date, http_date, iso8601, parse_http_date};

/// Asserts that an HTTP date has the right weekday, month and time at the epoch, at a date of
/// the SigV4 documentation, on a leap day of a century year, and on the day after the February
/// of a century year that is not a leap year.
#[test]
fn an_http_date_is_right_at_known_times() {
    assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    let documented = http_date(1_369_353_600_000);
    assert_eq!(documented, "Fri, 24 May 2013 00:00:00 GMT");
    assert_eq!(http_date(951_868_799_000), "Tue, 29 Feb 2000 23:59:59 GMT");
    let century = http_date(4_107_542_400_000);
    assert_eq!(century, "Mon, 01 Mar 2100 00:00:00 GMT");
}

/// Asserts that the time of a listing has the milliseconds, and the right day at the same
/// known times.
#[test]
fn a_listing_time_has_milliseconds() {
    assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso8601(1_369_353_600_123), "2013-05-24T00:00:00.123Z");
    assert_eq!(iso8601(951_868_799_999), "2000-02-29T23:59:59.999Z");
    assert_eq!(iso8601(4_107_542_400_000), "2100-03-01T00:00:00.000Z");
}

/// Asserts that the date of a signature is right at the known times, and that two of them
/// compare in the order of their times, across a day and a year.
#[test]
fn a_signature_date_is_right_and_sorts_by_time() {
    assert_eq!(amz_date(0), "19700101T000000Z");
    assert_eq!(amz_date(1_369_353_600_000), "20130524T000000Z");
    assert_eq!(amz_date(951_868_799_000), "20000229T235959Z");
    assert_eq!(amz_date(4_107_542_400_000), "21000301T000000Z");
    assert!(amz_date(86_399_000) < amz_date(86_400_000));
    assert!(amz_date(946_684_799_000) < amz_date(946_684_800_000));
}

/// Asserts that the date of a signature is read back to the time it stands for, at the known
/// times, and that text that is not a date, or not the form of one, or a date that does not exist,
/// is not read.
#[test]
fn an_amz_date_is_read_back() {
    use super::time::parse_amz_date;
    for ms in [
        0,
        1_369_353_600_000,
        951_868_799_000,
        4_107_542_400_000,
        1_800_000_000_000,
    ] {
        assert_eq!(parse_amz_date(&amz_date(ms)), Some(ms));
    }
    for text in [
        "",
        "20130524",
        "20130524T000000",
        "20130524T000000Zx",
        "20130524 000000Z",
        "20130230T000000Z",
        "20130524T250000Z",
        "20130524T006000Z",
        "2013052aT000000Z",
        "+0130524T000000Z",
        "20131324T000000Z",
        "20130500T000000Z",
        "é0130524T00000Z",
        "20130524T000000X",
        "19600101T000000Z",
    ] {
        assert_eq!(parse_amz_date(text), None, "{text:?}");
    }
}

/// Asserts that an HTTP date is read in the three forms of RFC 7231 (the example of the RFC in
/// each), that a two-digit year is read on either side of 1970, and that the date `http_date`
/// writes is read back.
#[test]
fn an_http_date_is_read_in_the_three_forms() {
    let time = 784_111_777_000;
    assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(time));
    assert_eq!(
        parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
        Some(time)
    );
    assert_eq!(parse_http_date("Sun Nov  6 08:49:37 1994"), Some(time));
    let later = parse_http_date("Sunday, 06-Nov-26 00:00:00 GMT");
    assert_eq!(later, parse_http_date("Fri, 06 Nov 2026 00:00:00 GMT"));
    assert_eq!(later, Some(1_793_923_200_000));
    let written = http_date(951_868_799_000);
    assert_eq!(parse_http_date(&written), Some(951_868_799_000));
}

/// Asserts that text which is no HTTP date is not read: a wrong number of words, a month, a
/// year, a clock or a day that is not one, a clock with a field too many, and a day before 1970.
#[test]
fn text_that_is_no_http_date_is_not_read() {
    let bad = [
        "",
        "yesterday",
        "Sun, 06 Nov 1994 08:49:37",
        "Sun, 06 Nov 1994 08:49:37 UTC",
        "Sun, 06 Jol 1994 08:49:37 GMT",
        "Sun, 06 J 1994 08:49:37 GMT",
        "Sun, xx Nov 1994 08:49:37 GMT",
        "Sun, 06 Nov 994 08:49:37 GMT",
        "Sun, 06 Nov 19x4 08:49:37 GMT",
        "Sun, 06 Nov xx 08:49:37 GMT",
        "Sun, 06 Nov 1994 08:49 GMT",
        "Sun, 06 Nov 1994 08:49:37:00 GMT",
        "Sun, 06 Nov 1994 08:xx:37 GMT",
        "Sun, 06 Nov 1994 24:00:00 GMT",
        "Sun, 06 Nov 1994 08:60:00 GMT",
        "Sun, 06 Nov 1994 08:00:60 GMT",
        "Sun, 31 Feb 2000 00:00:00 GMT",
        "Sun, 00 Feb 2000 00:00:00 GMT",
        "Sun, 01 Jan 1960 00:00:00 GMT",
    ];
    for text in bad {
        assert_eq!(parse_http_date(text), None, "{text:?}");
    }
}
