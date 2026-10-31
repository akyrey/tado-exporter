//! Parses tado°'s API quota headers.
//!
//! tado° reports the daily quota on every REST response:
//! - `ratelimit: "perday";r=<remaining>[;t=<seconds until refill>]`
//! - `ratelimit-policy: "perday";q=<quota>;w=<window seconds>`

use reqwest::header::HeaderMap;

const RATELIMIT_HEADER: &str = "ratelimit";
const RATELIMIT_POLICY_HEADER: &str = "ratelimit-policy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Requests left in the current window.
    pub remaining: Option<u64>,
    /// Seconds until the quota refills; only sent once the quota is exhausted.
    pub reset_seconds: Option<u64>,
    /// Total requests allowed per window.
    pub quota: Option<u64>,
}

impl RateLimit {
    /// Returns `None` when the response carries no quota headers at all.
    pub fn from_headers(headers: &HeaderMap) -> Option<RateLimit> {
        let limit = header_str(headers, RATELIMIT_HEADER);
        let policy = header_str(headers, RATELIMIT_POLICY_HEADER);

        if limit.is_none() && policy.is_none() {
            return None;
        }

        Some(RateLimit {
            remaining: limit.and_then(|v| parameter(v, "r")),
            reset_seconds: limit.and_then(|v| parameter(v, "t")),
            quota: policy.and_then(|v| parameter(v, "q")),
        })
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Extracts a numeric `key=value` parameter from a structured header value such as
/// `"perday";r=123;t=45`.
fn parameter(value: &str, key: &str) -> Option<u64> {
    value
        .split(';')
        .filter_map(|item| item.split_once('='))
        .find(|(name, _)| name.trim() == key)
        .and_then(|(_, number)| number.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    use reqwest::header::HeaderValue;
    use rstest::rstest;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| (name.parse().unwrap(), HeaderValue::from_static(value)))
            .collect()
    }

    #[rstest]
    #[case::remaining_only(
        &[("ratelimit", "\"perday\";r=123")],
        Some(RateLimit { remaining: Some(123), reset_seconds: None, quota: None })
    )]
    #[case::exhausted(
        &[("ratelimit", "\"perday\";r=0;t=3600")],
        Some(RateLimit { remaining: Some(0), reset_seconds: Some(3600), quota: None })
    )]
    #[case::with_policy(
        &[("RateLimit", "\"perday\";r=95"), ("RateLimit-Policy", "\"perday\";q=100;w=86400")],
        Some(RateLimit { remaining: Some(95), reset_seconds: None, quota: Some(100) })
    )]
    #[case::whitespace(
        &[("ratelimit", "\"perday\"; r = 7 ; t = 12")],
        Some(RateLimit { remaining: Some(7), reset_seconds: Some(12), quota: None })
    )]
    #[case::malformed_values(
        &[("ratelimit", "\"perday\";r=lots")],
        Some(RateLimit { remaining: None, reset_seconds: None, quota: None })
    )]
    #[case::absent(&[("content-type", "application/json")], None)]
    fn parses_rate_limit_headers(
        #[case] pairs: &[(&'static str, &'static str)],
        #[case] expected: Option<RateLimit>,
    ) {
        // Arrange
        let headers = headers(pairs);

        // Act
        let actual = RateLimit::from_headers(&headers);

        // Assert
        assert_eq!(actual, expected);
    }
}
