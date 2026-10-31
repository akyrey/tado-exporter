//! Automation settings from `AUTOMATION_*` environment variables.
//!
//! Parsing takes a lookup function instead of reading the environment directly so tests do
//! not race on process-wide environment variables. Invalid values fail fast at startup.

use std::env;
use std::fmt::Display;
use std::str::FromStr;

const DEFAULT_STATE_PATH: &str = "tado-automation.json";

#[derive(Debug, Clone, PartialEq)]
pub struct AutomationConfig {
    /// Only log what the rules would do. On by default.
    pub dry_run: bool,
    pub state_path: String,
}

impl AutomationConfig {
    pub fn from_env() -> AutomationConfig {
        AutomationConfig::from_lookup(|name| env::var(name).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> AutomationConfig {
        AutomationConfig {
            dry_run: parse_bool(&lookup, "AUTOMATION_DRY_RUN", true),
            state_path: lookup("AUTOMATION_STATE_PATH")
                .unwrap_or_else(|| DEFAULT_STATE_PATH.to_string()),
        }
    }

    pub fn any_rule_enabled(&self) -> bool {
        false
    }

    pub fn print(&self) {
        println!("Automation dry-run: {}", self.dry_run);
        println!("Automation state path: {}", self.state_path);
    }
}

pub(crate) fn parse_bool(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: bool,
) -> bool {
    match lookup(name).as_deref().map(str::trim) {
        None | Some("") => default,
        Some("1") | Some("true") | Some("TRUE") | Some("True") => true,
        Some("0") | Some("false") | Some("FALSE") | Some("False") => false,
        Some(other) => panic!("{} must be true or false, got {:?}", name, other),
    }
}

pub(crate) fn parse_or<T>(lookup: &impl Fn(&str) -> Option<String>, name: &str, default: T) -> T
where
    T: FromStr,
    T::Err: Display,
{
    match lookup(name) {
        None => default,
        Some(value) => value
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("{} is invalid ({:?}): {}", name, value, e)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::collections::HashMap;

    pub fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let values: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| values.get(name).cloned()
    }

    #[test]
    fn defaults_to_dry_run() {
        let config = AutomationConfig::from_lookup(lookup(&[]));

        assert!(config.dry_run);
        assert_eq!(config.state_path, "tado-automation.json");
        assert!(!config.any_rule_enabled());
    }

    #[test]
    fn reads_overrides() {
        let config = AutomationConfig::from_lookup(lookup(&[
            ("AUTOMATION_DRY_RUN", "false"),
            ("AUTOMATION_STATE_PATH", "/data/a.json"),
        ]));

        assert!(!config.dry_run);
        assert_eq!(config.state_path, "/data/a.json");
    }

    #[test]
    #[should_panic(expected = "AUTOMATION_DRY_RUN must be true or false")]
    fn rejects_invalid_bool() {
        AutomationConfig::from_lookup(lookup(&[("AUTOMATION_DRY_RUN", "maybe")]));
    }

    #[test]
    #[should_panic(expected = "AUTOMATION_X is invalid")]
    fn rejects_invalid_number() {
        parse_or(&lookup(&[("AUTOMATION_X", "ten")]), "AUTOMATION_X", 1u64);
    }
}
