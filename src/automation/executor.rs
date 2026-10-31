//! Applies (or, in dry-run mode, only logs) the decisions of one tick.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use log::info;

use super::engine::Engine;
use super::state::{self, AutomationState};
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
///
/// A rule whose decision was not applied (failed write, or dropped for a higher-priority rule
/// acting on the same zone) keeps its previous state, so it decides again next tick.
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

    let mut applied: Vec<&Action> = Vec::new();
    let mut not_applied: Vec<&Decision> = evaluation.dropped.iter().collect();

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
                applied.push(&decision.action);
                metrics::record_automation_action(
                    decision.rule,
                    decision.action.kind(),
                    MODE_APPLIED,
                );
            }
            Err(e) => {
                client.report_write_error(&format!("failed: {}", description), &e);
                not_applied.push(decision);
                metrics::record_automation_action(
                    decision.rule,
                    decision.action.kind(),
                    MODE_FAILED,
                );
            }
        }
    }

    let state = next_state(
        engine.state(),
        evaluation.state,
        &snapshot,
        &applied,
        &not_applied,
        now,
    );
    engine.commit(state);
}

/// Combines the rules' proposed state with what actually happened this tick.
fn next_state(
    previous: &AutomationState,
    proposed: AutomationState,
    snapshot: &Snapshot,
    applied: &[&Action],
    not_applied: &[&Decision],
    now: Timestamp,
) -> AutomationState {
    let retry_rules: BTreeSet<&str> = not_applied.iter().map(|d| d.rule).collect();
    let pending_restore: BTreeSet<i32> =
        not_applied.iter().filter_map(|d| d.action.zone()).collect();

    let state = proposed.with_rules_from(previous, &retry_rules);
    let state = state::reconcile(&state, snapshot, &pending_restore);

    applied.iter().fold(state, |state, action| {
        state::record_applied(&state, action, now)
    })
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

    fn scheduled_zone() -> Vec<ZoneStateResponse> {
        vec![ZoneStateResponse {
            id: 1,
            name: "Salotto".to_string(),
            state_response: serde_json::from_value(json!({
                "tadoMode": "HOME",
                "setting": { "type": "HEATING", "power": "ON", "temperature": { "celsius": 20.0, "fahrenheit": 68.0 } },
                "activityDataPoints": {},
                "sensorDataPoints": {}
            }))
            .unwrap(),
        }]
    }

    fn warm_weather() -> WeatherApiResponse {
        serde_json::from_value(json!({
            "solarIntensity": { "percentage": 50.0 },
            "outsideTemperature": { "celsius": 20.0, "fahrenheit": 68.0 }
        }))
        .unwrap()
    }

    /// A day of warm samples while the season is still "heating", recorded in `dry_run` mode:
    /// the next tick switches the season off.
    fn warm_day_in_heating_season(dry_run: bool) -> serde_json::Value {
        let now = now();
        let samples: Vec<serde_json::Value> = (1..=24)
            .rev()
            .map(|h| json!({ "at": now - h * 3600, "celsius": 20.0 }))
            .collect();
        json!({
            "season": { "samples": samples, "season": "Heating" },
            "dry_run": dry_run
        })
    }

    struct Tick<'a> {
        env: &'a [(&'a str, &'a str)],
        seed: Option<serde_json::Value>,
        zones: Vec<ZoneStateResponse>,
        weather: Option<WeatherApiResponse>,
        mock: (&'a str, &'a str, u16, u64),
    }

    /// Runs one tick against a mock tado° API and returns the resulting automation state.
    async fn run(tick: Tick<'_>) -> serde_json::Value {
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        let (http_method, http_path, status, expected_calls) = tick.mock;
        Mock::given(method(http_method))
            .and(path(http_path))
            .respond_with(ResponseTemplate::new(status))
            .expect(expected_calls)
            .mount(&server)
            .await;
        let mut client = prepared_client(&server, &dir).await;

        let state_path = dir.path().join("automation.json");
        if let Some(seed) = &tick.seed {
            std::fs::write(&state_path, seed.to_string()).unwrap();
        }
        let state_path_str = state_path.to_str().unwrap().to_string();
        let env: Vec<(&str, &str)> = tick
            .env
            .iter()
            .copied()
            .chain(Some(("AUTOMATION_STATE_PATH", state_path_str.as_str())))
            .collect();
        let mut engine = Engine::new(AutomationConfig::from_lookup(lookup(&env)));

        run_tick(
            &mut engine,
            &mut client,
            &tick.zones,
            tick.weather.as_ref(),
            None,
        )
        .await;

        // The engine's state is what the next tick starts from (and what was persisted, if
        // anything changed).
        serde_json::to_value(engine.state()).unwrap()
    }

    const OPEN_WINDOW_LIVE: &[(&str, &str)] = &[
        ("AUTOMATION_DRY_RUN", "false"),
        ("AUTOMATION_OPEN_WINDOW_ENABLED", "true"),
    ];
    const OPEN_WINDOW_DRY_RUN: &[(&str, &str)] = &[("AUTOMATION_OPEN_WINDOW_ENABLED", "true")];
    const SEASON_LIVE: &[(&str, &str)] = &[
        ("AUTOMATION_DRY_RUN", "false"),
        ("AUTOMATION_SEASON_ENABLED", "true"),
    ];
    const SEASON_DRY_RUN: &[(&str, &str)] = &[("AUTOMATION_SEASON_ENABLED", "true")];
    const ACTIVATE: &str = "/api/v2/homes/42/zones/1/state/openWindow/activate";
    const OVERLAY: &str = "/api/v2/homes/42/zones/1/overlay";

    #[tokio::test]
    async fn live_success_records_rule_state() {
        // Act (the mock verifies exactly one call when dropped)
        let state = run(Tick {
            env: OPEN_WINDOW_LIVE,
            seed: None,
            zones: zone_with_open_window(),
            weather: None,
            mock: ("POST", ACTIVATE, 204, 1),
        })
        .await;

        // Assert
        assert_eq!(state["open_window"]["handled"], json!([1]));
        assert_eq!(state["dry_run"], json!(false));
    }

    #[tokio::test]
    async fn live_failure_rolls_back_rule_state_for_a_retry() {
        // Act
        let state = run(Tick {
            env: OPEN_WINDOW_LIVE,
            seed: None,
            zones: zone_with_open_window(),
            weather: None,
            mock: ("POST", ACTIVATE, 500, 1),
        })
        .await;

        // Assert
        assert_eq!(state["open_window"]["handled"], json!([]));
    }

    #[tokio::test]
    async fn dry_run_never_calls_the_api() {
        // Act (the mock verifies no call was made when dropped)
        let state = run(Tick {
            env: OPEN_WINDOW_DRY_RUN,
            seed: None,
            zones: zone_with_open_window(),
            weather: None,
            mock: ("POST", ACTIVATE, 204, 0),
        })
        .await;

        // Assert
        assert_eq!(state["open_window"]["handled"], json!([1]));
        assert_eq!(state["dry_run"], json!(true));
    }

    #[tokio::test]
    async fn live_heating_off_takes_ownership_of_the_overlay() {
        // Act
        let state = run(Tick {
            env: SEASON_LIVE,
            seed: Some(warm_day_in_heating_season(false)),
            zones: scheduled_zone(),
            weather: Some(warm_weather()),
            mock: ("PUT", OVERLAY, 200, 1),
        })
        .await;

        // Assert
        assert_eq!(state["owned_overlays"], json!({ "1": { "kind": "off" } }));
        assert_eq!(state["season"]["season"], json!("Off"));
    }

    #[tokio::test]
    async fn failed_heating_off_keeps_the_season_change_pending() {
        // Act
        let state = run(Tick {
            env: SEASON_LIVE,
            seed: Some(warm_day_in_heating_season(false)),
            zones: scheduled_zone(),
            weather: Some(warm_weather()),
            mock: ("PUT", OVERLAY, 500, 1),
        })
        .await;

        // Assert: no ownership, and the season transition is retried next tick
        assert_eq!(state["owned_overlays"], json!({}));
        assert_eq!(state["season"]["season"], json!("Heating"));
    }

    #[tokio::test]
    async fn dry_run_never_takes_ownership() {
        // Act
        let state = run(Tick {
            env: SEASON_DRY_RUN,
            seed: Some(warm_day_in_heating_season(true)),
            zones: scheduled_zone(),
            weather: Some(warm_weather()),
            mock: ("PUT", OVERLAY, 200, 0),
        })
        .await;

        // Assert
        assert_eq!(state["owned_overlays"], json!({}));
        assert_eq!(state["season"]["season"], json!("Off"));
    }

    #[tokio::test]
    async fn switching_from_dry_run_to_live_acts_on_the_pending_season() {
        // Arrange: dry-run already recorded the off season.
        let mut seed = warm_day_in_heating_season(true);
        seed["season"]["season"] = json!("Off");

        // Act
        let state = run(Tick {
            env: SEASON_LIVE,
            seed: Some(seed),
            zones: scheduled_zone(),
            weather: Some(warm_weather()),
            mock: ("PUT", OVERLAY, 200, 1),
        })
        .await;

        // Assert
        assert_eq!(state["owned_overlays"], json!({ "1": { "kind": "off" } }));
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
