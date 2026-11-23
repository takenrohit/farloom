//! Per-thread response statistics for the load tester.


use std::collections::BTreeMap;

/// Counts of responses by status code, plus failed requests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub by_status: BTreeMap<u16, u64>,
    pub errors: u64,
}

impl Stats {
    /// Records one response with the given status code.
    pub fn record_status(&mut self, code: u16) {
        *self.by_status.entry(code).or_insert(0) += 1;
    }

    /// Records one request that failed (I/O error or unparsable response).
    pub fn record_error(&mut self) {
        self.errors += 1;
    }

    /// Adds every count from `other` into `self`.
    pub fn merge(&mut self, other: Stats) {
        for (code, count) in other.by_status {
            *self.by_status.entry(code).or_insert(0) += count;
        }
        self.errors += other.errors;
    }

    /// Total requests recorded: all status counts plus errors.
    pub fn total(&self) -> u64 {
        self.by_status.values().sum::<u64>() + self.errors
    }
}

/// Extracts the status code from a response head such as `HTTP/1.1 200 OK\r\n...`.
///
/// Accepts `HTTP/1.0` and `HTTP/1.1`, followed by one space and exactly three
/// ASCII digits in 100..=599. The digits must be followed by a space, CR, LF,
/// or the end of input.
pub fn parse_status_line(head: &[u8]) -> Option<u16> {
    let rest = head
        .strip_prefix(b"HTTP/1.1 ")
        .or_else(|| head.strip_prefix(b"HTTP/1.0 "))?;
    let digits = rest.get(..3)?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    match rest.get(3) {
        None | Some(b' ') | Some(b'\r') | Some(b'\n') => {}
        Some(_) => return None,
    }
    let code = digits
        .iter()
        .fold(0u16, |acc, d| acc * 10 + u16::from(d - b'0'));
    (100..=599).contains(&code).then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_total() {
        let mut s = Stats::default();
        s.record_status(200);
        s.record_status(200);
        s.record_status(503);
        s.record_error();
        assert_eq!(s.by_status.get(&200), Some(&2));
        assert_eq!(s.by_status.get(&503), Some(&1));
        assert_eq!(s.errors, 1);
        assert_eq!(s.total(), 4);
    }

    #[test]
    fn merge_sums_counts_and_errors() {
        let mut a = Stats::default();
        a.record_status(200);
        a.record_status(404);
        a.record_error();
        let mut b = Stats::default();
        b.record_status(200);
        b.record_status(503);
        b.record_error();
        b.record_error();
        let total_before = a.total() + b.total();

        a.merge(b);
        assert_eq!(a.by_status.get(&200), Some(&2));
        assert_eq!(a.by_status.get(&404), Some(&1));
        assert_eq!(a.by_status.get(&503), Some(&1));
        assert_eq!(a.errors, 3);
        assert_eq!(a.total(), total_before);
    }

    #[test]
    fn merge_empty_is_identity() {
        let mut a = Stats::default();
        a.record_status(200);
        let expected = a.clone();
        a.merge(Stats::default());
        assert_eq!(a, expected);
    }

    #[test]
    fn parse_valid_status_lines() {
        assert_eq!(parse_status_line(b"HTTP/1.1 200 OK\r\n\r\n"), Some(200));
        assert_eq!(parse_status_line(b"HTTP/1.0 404 Not Found\r\n"), Some(404));
        assert_eq!(
            parse_status_line(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\n\r\n"),
            Some(503)
        );
        assert_eq!(parse_status_line(b"HTTP/1.1 100"), Some(100));
        assert_eq!(parse_status_line(b"HTTP/1.1 599\r\n"), Some(599));
    }

    #[test]
    fn parse_invalid_status_lines() {
        assert_eq!(parse_status_line(b""), None);
        assert_eq!(parse_status_line(b"garbage"), None);
        assert_eq!(parse_status_line(b"HTTP/2 200 OK\r\n"), None);
        assert_eq!(parse_status_line(b"http/1.1 200 OK\r\n"), None);
        assert_eq!(parse_status_line(b"HTTP/1.1 20 OK\r\n"), None);
        assert_eq!(parse_status_line(b"HTTP/1.1 2000 OK\r\n"), None);
        assert_eq!(parse_status_line(b"HTTP/1.1 abc OK\r\n"), None);
        assert_eq!(parse_status_line(b"HTTP/1.1 099 X\r\n"), None);
        assert_eq!(parse_status_line(b"HTTP/1.1 600 X\r\n"), None);
        assert_eq!(parse_status_line(b"HTTP/1.1  200 OK\r\n"), None);
    }
}
