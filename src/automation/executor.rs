//! Applies (or, in dry-run mode, only logs) the decisions of one tick.

use std::time::{SystemTime, UNIX_EPOCH};

use log::info;

use super::engine::Engine;
use super::state;
use super::{Action, Decision, Snapshot, Timestamp};
use crate::tado::client::Client;
use crate::tado::metrics;
use crate::tado::model::{MobileDeviceApiResponse, WeatherApiResponse, ZoneStateResponse};

const MODE_DRY_RUN: &str = "dry_run";
const MODE_APPLIED: &str = "applied";
const MODE_FAILED: &str = "failed";

pub fn now() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Evaluates the rules on this tick's data and applies their decisions.
pub async fn run_tick(
    engine: &mut Engine,
    client: &mut Client,
    zones: &[ZoneStateResponse],
    weather: Option<&WeatherApiResponse>,
    phones: Option<&[MobileDeviceApiResponse]>,
) {
    // Without zone states (e.g. a failed request) rules would misread the home: skip the tick.
    if !engine.is_enabled() || zones.is_empty() {
        return;
    }

    let now = now();
    let snapshot = engine.snapshot(now, zones, weather, phones);
    let evaluation = engine.evaluate(&snapshot);
    let mut state = evaluation.state;

    for decision in &evaluation.decisions {
        let description = describe(decision, &snapshot);

        if engine.is_dry_run() {
            info!("[dry-run] {}", description);
            metrics::record_automation_action(decision.rule, decision.action.kind(), MODE_DRY_RUN);
            continue;
        }

        match apply(client, &decision.action).await {
            Ok(()) => {
                info!("{}", description);
                state = state::record_applied(&state, &decision.action, now);
                metrics::record_automation_action(
                    decision.rule,
                    decision.action.kind(),
                    MODE_APPLIED,
                );
            }
            Err(e) => {
                client.report_write_error(&format!("failed: {}", description), &e);
                metrics::record_automation_action(
                    decision.rule,
                    decision.action.kind(),
                    MODE_FAILED,
                );
            }
        }
    }

    engine.commit(state);
}

async fn apply(client: &mut Client, action: &Action) -> Result<(), reqwest::Error> {
    match *action {
        Action::ActivateOpenWindow { zone } => client.activate_open_window(zone).await,
        Action::HeatingOff { zone } => client.set_heating_off(zone).await,
        Action::Boost {
            zone,
            celsius,
            minutes,
            ..
        } => client.set_temperature_for(zone, celsius, minutes).await,
        Action::ResumeSchedule { zone } => client.resume_schedule(zone).await,
        Action::SetPresence(presence) => client.set_presence(presence.as_api_str()).await,
    }
}

/// Human-readable description for logs, e.g. `humidity: boost Bagno to 21°C for 60 min`.
pub fn describe(decision: &Decision, snapshot: &Snapshot) -> String {
    let zone_name = |id: i32| {
        snapshot
            .zone(id)
            .map_or_else(|| format!("zone {}", id), |zone| zone.name.clone())
    };

    let what = match decision.action {
        Action::ActivateOpenWindow { zone } => {
            format!("activate open-window mode in {}", zone_name(zone))
        }
        Action::HeatingOff { zone } => format!("turn heating off in {}", zone_name(zone)),
        Action::Boost {
            zone,
            celsius,
            minutes,
            ..
        } => format!(
            "boost {} to {}°C for {} min",
            zone_name(zone),
            celsius,
            minutes
        ),
        Action::ResumeSchedule { zone } => format!("resume the schedule in {}", zone_name(zone)),
        Action::SetPresence(presence) => format!("set home presence to {}", presence.as_api_str()),
    };

    format!("{}: {}", decision.rule, what)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::automation::{Presence, ZoneControl, ZoneSnapshot};

    fn snapshot() -> Snapshot {
        Snapshot {
            now: 0,
            zones: vec![ZoneSnapshot {
                id: 4,
                name: "Bagno".to_string(),
                control: ZoneControl::Schedule,
                setting_celsius: None,
                humidity: None,
                open_window_detected: false,
                open_window_active: false,
            }],
            outside_celsius: None,
            presence: None,
            phones: None,
        }
    }

    use crate::automation::config::tests::lookup;
    use crate::automation::config::AutomationConfig;
    use crate::tado::auth::Authenticator;
    use crate::tado::token_store::TokenStore;
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn prepared_client(server: &MockServer, dir: &TempDir) -> Client {
        let store = TokenStore::new(dir.path().join("token.json"));
        store.save("refresh-1").unwrap();
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "access-1", "expires_in": 599, "refresh_token": "refresh-2"
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/me"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "homes": [{ "id": 42 }] })),
            )
            .mount(server)
            .await;

        let http_client = reqwest::Client::new();
        let auth_url = format!("{}/oauth2/", server.uri()).parse().unwrap();
        let authenticator = Authenticator::with_base_url(http_client.clone(), auth_url, store);
        let mut client =
            Client::with_base_url(http_client, server.uri().parse().unwrap(), authenticator);
        assert!(client.prepare().await);
        client
    }

    fn zone_with_open_window() -> Vec<ZoneStateResponse> {
        vec![ZoneStateResponse {
            id: 1,
            name: "Salotto".to_string(),
            state_response: serde_json::from_value(json!({
                "tadoMode": "HOME",
                "setting": { "type": "HEATING", "power": "ON", "temperature": { "celsius": 20.0, "fahrenheit": 68.0 } },
                "activityDataPoints": {},
                "sensorDataPoints": {},
                "openWindowDetected": true
            }))
            .unwrap(),
        }]
    }

    async fn run_open_window_tick(dry_run: &str, expected_calls: u64) -> String {
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("POST"))
            .and(path("/api/v2/homes/42/zones/1/state/openWindow/activate"))
            .respond_with(ResponseTemplate::new(204))
            .expect(expected_calls)
            .mount(&server)
            .await;
        let mut client = prepared_client(&server, &dir).await;
        let state_path = dir.path().join("automation.json");
        let state_path_str = state_path.to_str().unwrap().to_string();
        let config = AutomationConfig::from_lookup(lookup(&[
            ("AUTOMATION_DRY_RUN", dry_run),
            ("AUTOMATION_OPEN_WINDOW_ENABLED", "true"),
            ("AUTOMATION_STATE_PATH", state_path_str.as_str()),
        ]));
        let mut engine = Engine::new(config);

        run_tick(
            &mut engine,
            &mut client,
            &zone_with_open_window(),
            None,
            None,
        )
        .await;

        std::fs::read_to_string(state_path).unwrap()
    }

    #[tokio::test]
    async fn live_mode_applies_decisions_and_persists_state() {
        // Act
        let state = run_open_window_tick("false", 1).await;

        // Assert: the mock verifies the call on drop
        let state: serde_json::Value = serde_json::from_str(&state).unwrap();
        assert_eq!(state["open_window"]["handled"], json!([1]));
    }

    #[tokio::test]
    async fn dry_run_never_calls_the_api() {
        // Act
        let state = run_open_window_tick("true", 0).await;

        // Assert: the mock verifies no call was made on drop
        let state: serde_json::Value = serde_json::from_str(&state).unwrap();
        assert_eq!(state["open_window"]["handled"], json!([1]));
    }

    #[test]
    fn describes_decisions_with_zone_names() {
        let boost = Decision {
            rule: "humidity",
            action: Action::Boost {
                zone: 4,
                celsius: 21.0,
                minutes: 60,
                then_off: false,
            },
        };
        let presence = Decision {
            rule: "presence",
            action: Action::SetPresence(Presence::Away),
        };
        let unknown_zone = Decision {
            rule: "season",
            action: Action::HeatingOff { zone: 9 },
        };

        assert_eq!(
            describe(&boost, &snapshot()),
            "humidity: boost Bagno to 21°C for 60 min"
        );
        assert_eq!(
            describe(&presence, &snapshot()),
            "presence: set home presence to AWAY"
        );
        assert_eq!(
            describe(&unknown_zone, &snapshot()),
            "season: turn heating off in zone 9"
        );
    }
}
