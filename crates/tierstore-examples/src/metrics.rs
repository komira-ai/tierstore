//! Readings of tierstore's `/metricz`, and how much a series rose between two of them.
//!
//! A reading maps each series, written as the page writes it (`name` or `name{label="v",...}`),
//! to the text of its value. The page is read with one HTTP/1.0 GET, which the server answers
//! with the body whole and then closes, so the runner needs no HTTP client.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::error::Error;

/// One reading of `/metricz`: the value of each series, as text.
pub type Reading = BTreeMap<String, String>;

/// How long a read of `/metricz` may wait for the server. Only a stuck server reaches it: it ends
/// the run with an error rather than hanging it.
const SCRAPE_WAIT: Duration = Duration::from_secs(60);

/// The series of a page in the Prometheus text format. Comments and blank lines are skipped; a
/// label value may hold spaces and escaped quotes; a timestamp after the value is dropped.
#[must_use]
pub fn parse(text: &str) -> Reading {
    let mut reading = Reading::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let end = series_end(line);
        let (series, rest) = line.split_at(end);
        if let Some(value) = rest.split_whitespace().next() {
            reading.insert(series.to_owned(), value.to_owned());
        }
    }
    reading
}

/// Where the series of `line` ends: at the first space outside the braces of its labels.
fn series_end(line: &str) -> usize {
    let (mut quoted, mut escaped, mut depth) = (false, false, 0_u32);
    for (at, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' if depth > 0 => quoted = !quoted,
            '{' if !quoted => depth += 1,
            '}' if !quoted => depth = depth.saturating_sub(1),
            c if c.is_whitespace() && depth == 0 => return at,
            _ => {}
        }
    }
    line.len()
}

/// How much `series` rose from `before` to `after`: a whole number when both values are whole,
/// `absent` when it is in neither reading, `absent before` or `absent after` when it is in one,
/// and `not a number` when a value does not parse.
#[must_use]
pub fn rise(before: &Reading, after: &Reading, series: &str) -> String {
    let number = |reading: &Reading| reading.get(series).map(|v| v.parse::<f64>());
    match (number(before), number(after)) {
        (None, None) => "absent".to_owned(),
        (None, Some(_)) => "absent before".to_owned(),
        (Some(_), None) => "absent after".to_owned(),
        (Some(Ok(from)), Some(Ok(to))) => {
            let rise = to - from;
            if rise.fract() == 0.0 && rise.abs() < 9.0e15 {
                // Whole and under 2^53, so the cast is exact.
                (rise as i64).to_string()
            } else {
                rise.to_string()
            }
        }
        _ => "not a number".to_owned(),
    }
}

/// Reads `/metricz` of the S3 front at `addr`.
///
/// # Errors
/// The connection fails, the answer is not a 200, or its body is not UTF-8.
pub fn scrape(addr: SocketAddr) -> Result<Reading, Error> {
    let fail = |why: String| Error::Metrics {
        addr: addr.to_string(),
        why,
    };
    let mut stream = TcpStream::connect(addr).map_err(|e| fail(e.to_string()))?;
    stream
        .set_read_timeout(Some(SCRAPE_WAIT))
        .and_then(|()| write!(stream, "GET /metricz HTTP/1.0\r\nHost: {addr}\r\n\r\n"))
        .map_err(|e| fail(e.to_string()))?;
    let mut answer = Vec::new();
    stream
        .read_to_end(&mut answer)
        .map_err(|e| fail(e.to_string()))?;
    let text = String::from_utf8(answer).map_err(|e| fail(e.to_string()))?;
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status = head.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(fail(format!("the answer is {status:?}")));
    }
    Ok(parse(body))
}
