use log::warn;
use std::env;

use crate::tado::auth::DEFAULT_CLIENT_ID;

/// 2 requests per tick every 5 minutes is ~576 requests/day, within tado°'s free quota
/// observed in 2026 (1,000/day). Accounts on the documented 100/day need a longer ticker.
const DEFAULT_TICKER_SECONDS: u64 = 300;
const DEFAULT_TOKEN_PATH: &str = "tado-token.json";
/// Variables used by the removed password grant; tado° no longer accepts them.
const REMOVED_VARIABLES: [&str; 3] = [
    "EXPORTER_USERNAME",
    "EXPORTER_PASSWORD",
    "EXPORTER_CLIENT_SECRET",
];

pub struct Config {
    pub ticker: u64,
    pub token_path: String,
    pub client_id: String,
}

impl Config {
    pub fn print(&self) {
        println!("--- tado° exporter configuration ---");
        println!("Ticker seconds: {}", self.ticker);
        println!("Token path: {}", self.token_path);
        println!("Client ID: {}", self.client_id);
        println!("------------------------------------");
    }
}

fn parse_ticker(value: Option<&str>) -> u64 {
    match value {
        None => DEFAULT_TICKER_SECONDS,
        Some(v) => match v.parse::<u64>() {
            Ok(seconds) if seconds > 0 => seconds,
            _ => panic!(
                "EXPORTER_TICKER must be a positive number of seconds, got {:?}",
                v
            ),
        },
    }
}

pub fn load() -> Config {
    for name in REMOVED_VARIABLES
        .iter()
        .filter(|n| env::var_os(n).is_some())
    {
        warn!(
            "{} is ignored: tado° removed password login, authorize the device via the URL logged at startup instead",
            name
        );
    }

    let config = Config {
        ticker: parse_ticker(env::var("EXPORTER_TICKER").ok().as_deref()),
        token_path: env::var("EXPORTER_TOKEN_PATH").unwrap_or_else(|_| DEFAULT_TOKEN_PATH.into()),
        client_id: env::var("EXPORTER_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.into()),
    };

    config.print();

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_load() {
        // Given no env variable are set
        env::remove_var("EXPORTER_TICKER");
        env::remove_var("EXPORTER_TOKEN_PATH");
        env::remove_var("EXPORTER_CLIENT_ID");

        // when
        let config = load();

        // then we should load default values
        assert_eq!(config.ticker, 300);
        assert_eq!(config.token_path, "tado-token.json");
        assert_eq!(config.client_id, DEFAULT_CLIENT_ID);

        // given the following environment variable values
        env::set_var("EXPORTER_TICKER", "30");
        env::set_var("EXPORTER_TOKEN_PATH", "/data/token.json");
        env::set_var("EXPORTER_CLIENT_ID", "client-123");

        // when
        let config = load();

        // then we should have these values set
        assert_eq!(config.ticker, 30);
        assert_eq!(config.token_path, "/data/token.json");
        assert_eq!(config.client_id, "client-123");
        env::remove_var("EXPORTER_TICKER");
        env::remove_var("EXPORTER_TOKEN_PATH");
        env::remove_var("EXPORTER_CLIENT_ID");
    }

    #[test]
    fn test_parse_ticker() {
        assert_eq!(parse_ticker(None), 300);
        assert_eq!(parse_ticker(Some("5400")), 5400);
    }

    #[rstest::rstest]
    #[case("0")]
    #[case("-1")]
    #[case("ten")]
    #[should_panic(expected = "EXPORTER_TICKER must be a positive number of seconds")]
    fn test_parse_ticker_rejects_invalid(#[case] value: &str) {
        parse_ticker(Some(value));
    }
}
