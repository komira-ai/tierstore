//! The dates of the fake service: the second its clock says, and the two forms a stamp is written in.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::super::sigv4::Utc;
use super::super::times::MONTHS;

/// The weekday names of an HTTP date, from Thursday: the day of the epoch.
const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

/// The second the system clock says.
#[expect(
    clippy::disallowed_methods,
    reason = "the fake service stamps objects with the machine's time, as a real service does"
)]
pub(super) fn system_seconds() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |since| since.as_secs())
}

/// `Sun, 06 Nov 1994 08:49:37 GMT`, the form of a `Last-Modified` header.
pub(super) fn http_date(seconds: u64) -> String {
    let utc = Utc::of(UNIX_EPOCH + Duration::from_secs(seconds), false);
    let weekday = WEEKDAYS[((seconds / 86_400) % 7) as usize];
    let month = MONTHS[utc.month as usize - 1];
    format!(
        "{weekday}, {:02} {month} {:04} {:02}:{:02}:{:02} GMT",
        utc.day, utc.year, utc.hour, utc.minute, utc.second
    )
}

/// `2009-10-12T17:50:30.000Z`, the form of a `<LastModified>` of a listing.
pub(super) fn iso_date(seconds: u64) -> String {
    let utc = Utc::of(UNIX_EPOCH + Duration::from_secs(seconds), false);
    utc.iso8601().replace('Z', ".000Z")
}
