//! The dates of the service's answers: a `Last-Modified` header and a listing's `<LastModified>`.

use std::time::{Duration, UNIX_EPOCH};

use crate::s3::sigv4::Utc;
use crate::s3::times::MONTHS;

/// The weekday names of an HTTP date, from Thursday: the day of the epoch.
const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

/// `Tue, 03 Nov 2026 08:49:37 GMT`, the form of a `Last-Modified` header.
pub(super) fn http_date(seconds: u64) -> String {
    let utc = Utc::of(UNIX_EPOCH + Duration::from_secs(seconds), false);
    let weekday = WEEKDAYS[((seconds / 86_400) % 7) as usize];
    let month = MONTHS[utc.month as usize - 1];
    format!(
        "{weekday}, {:02} {month} {:04} {:02}:{:02}:{:02} GMT",
        utc.day, utc.year, utc.hour, utc.minute, utc.second
    )
}

/// `2026-11-03T08:49:37.000Z`, the form of a `<LastModified>` of a listing.
pub(super) fn iso_date(seconds: u64) -> String {
    let utc = Utc::of(UNIX_EPOCH + Duration::from_secs(seconds), false);
    utc.iso8601().replace('Z', ".000Z")
}
