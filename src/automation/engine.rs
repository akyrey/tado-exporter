//! Builds snapshots from API responses and asks the enabled rules for decisions.

use std::collections::HashSet;

use log::info;

use super::config::AutomationConfig;
use super::rules::{humidity, open_window, presence, season};
use super::state::{self, AutomationState, OverlayInfo, StateStore};
use super::{Action, Decision, PhoneLocation, Presence, Snapshot, Timestamp, ZoneSnapshot};
use crate::tado::model::{MobileDeviceApiResponse, WeatherApiResponse, ZoneStateResponse};

/// The outcome of one evaluation: what to do, and the rule state to keep afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub decisions: Vec<Decision>,
    pub state: AutomationState,
}

pub struct Engine {
    config: AutomationConfig,
    store: StateStore,
    state: AutomationState,
}

impl Engine {
    pub fn new(config: AutomationConfig) -> Engine {
        let store = StateStore::new(&config.state_path);
        let state = if config.any_rule_enabled() {
            store.load()
        } else {
            AutomationState::default()
        };

        if config.any_rule_enabled() && config.dry_run {
            info!("automation runs in dry-run mode: actions are only logged (AUTOMATION_DRY_RUN=false to apply them)");
        }

        Engine {
            config,
            store,
            state,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.any_rule_enabled()
    }

    pub fn is_dry_run(&self) -> bool {
        self.config.dry_run
    }

    /// Whether a rule needs phone locations (one extra API request per tick).
    pub fn needs_phones(&self) -> bool {
        self.config.presence.enabled
    }

    pub fn snapshot(
        &self,
        now: Timestamp,
        zones: &[ZoneStateResponse],
        weather: Option<&WeatherApiResponse>,
        phones: Option<&[MobileDeviceApiResponse]>,
    ) -> Snapshot {
        build_snapshot(&self.state, now, zones, weather, phones)
    }

    pub fn evaluate(&self, snapshot: &Snapshot) -> Evaluation {
        evaluate(&self.config, &self.state, snapshot)
    }

    /// Keeps and persists the state produced by a tick.
    pub fn commit(&mut self, state: AutomationState) {
        if state == self.state {
            return;
        }
        if let Err(e) = self.store.save(&state) {
            log::warn!("unable to persist automation state: {}", e);
        }
        self.state = state;
    }
}

/// Runs every enabled rule. Rules earlier in the list win when two touch the same zone.
pub fn evaluate(
    config: &AutomationConfig,
    state: &AutomationState,
    snapshot: &Snapshot,
) -> Evaluation {
    let state = state::reconcile(state, snapshot);
    let mut proposed: Vec<Decision> = Vec::new();

    let open_window = run_rule(
        config.open_window.enabled,
        open_window::NAME,
        &state.open_window,
        |s| open_window::evaluate(snapshot, s),
        &mut proposed,
    );
    let humidity = run_rule(
        config.humidity.enabled,
        humidity::NAME,
        &state.humidity,
        |s| humidity::evaluate(&config.humidity, snapshot, s),
        &mut proposed,
    );
    let season = run_rule(
        config.season.enabled,
        season::NAME,
        &state.season,
        |s| season::evaluate(&config.season, snapshot, s),
        &mut proposed,
    );

    let presence = run_rule(
        config.presence.enabled,
        presence::NAME,
        &state.presence,
        |s| presence::evaluate(&config.presence, snapshot, s),
        &mut proposed,
    );

    Evaluation {
        decisions: without_conflicts(proposed),
        state: AutomationState {
            open_window,
            humidity,
            season,
            presence,
            ..state
        },
    }
}

/// Runs a rule if enabled, collecting its decisions; a disabled rule keeps its state.
fn run_rule<S: Clone>(
    enabled: bool,
    rule: &'static str,
    state: &S,
    evaluate: impl FnOnce(&S) -> (Vec<Action>, S),
    proposed: &mut Vec<Decision>,
) -> S {
    if !enabled {
        return state.clone();
    }

    let (actions, next_state) = evaluate(state);
    proposed.extend(actions.into_iter().map(|action| Decision { rule, action }));
    next_state
}

/// Keeps only the first action per zone (and the first presence change).
fn without_conflicts(decisions: Vec<Decision>) -> Vec<Decision> {
    let mut touched_zones = HashSet::new();
    let mut presence_set = false;

    decisions
        .into_iter()
        .filter(|decision| match decision.action.zone() {
            Some(zone) => touched_zones.insert(zone),
            None => !std::mem::replace(&mut presence_set, true),
        })
        .collect()
}

pub fn build_snapshot(
    state: &AutomationState,
    now: Timestamp,
    zones: &[ZoneStateResponse],
    weather: Option<&WeatherApiResponse>,
    phones: Option<&[MobileDeviceApiResponse]>,
) -> Snapshot {
    let presence = zones
        .iter()
        .filter_map(|zone| zone.state_response.tadoMode.as_deref())
        .find_map(Presence::from_api_str);

    Snapshot {
        now,
        zones: zones
            .iter()
            .map(|zone| zone_snapshot(state, zone))
            .collect(),
        outside_celsius: weather.map(|w| w.outsideTemperature.celsius),
        presence,
        phones: phones.map(|devices| {
            devices
                .iter()
                .filter(|device| device.settings.geoTrackingEnabled)
                .filter_map(|device| device.location.as_ref())
                .map(|location| PhoneLocation {
                    at_home: location.atHome,
                })
                .collect()
        }),
    }
}

fn zone_snapshot(state: &AutomationState, zone: &ZoneStateResponse) -> ZoneSnapshot {
    let response = &zone.state_response;
    let overlay = response.overlay.as_ref().map(|overlay| OverlayInfo {
        power_off: overlay.setting.power.as_deref() == Some("OFF"),
        termination: overlay.termination.termination_type.clone(),
        celsius: overlay.setting.temperature.as_ref().map(|t| t.celsius),
    });
    let heating_off = response.setting.power.as_deref() == Some("OFF");

    ZoneSnapshot {
        id: zone.id,
        name: zone.name.clone(),
        control: state::zone_control(
            state.owned_overlays.get(&zone.id).copied(),
            overlay.as_ref(),
        ),
        setting_celsius: response
            .setting
            .temperature
            .as_ref()
            .filter(|_| !heating_off)
            .map(|t| t.celsius),
        humidity: response
            .sensorDataPoints
            .humidity
            .as_ref()
            .map(|h| h.percentage),
        open_window_detected: response.openWindowDetected == Some(true),
        open_window_active: response.openWindow.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::automation::state::OwnedOverlay;
    use crate::automation::{Action, ZoneControl};
    use crate::tado::model::{
        WeatherOutsideTemperatureApiResponse, WeatherSolarIntensityApiResponse,
    };
    use serde_json::json;

    fn zone_response(id: i32, name: &str, state: serde_json::Value) -> ZoneStateResponse {
        ZoneStateResponse {
            id,
            name: name.to_string(),
            state_response: serde_json::from_value(state).unwrap(),
        }
    }

    fn base_state(extra: serde_json::Value) -> serde_json::Value {
        let mut state = json!({
            "tadoMode": "HOME",
            "setting": { "type": "HEATING", "power": "ON", "temperature": { "celsius": 20.0, "fahrenheit": 68.0 } },
            "activityDataPoints": { "heatingPower": { "percentage": 0.0 } },
            "sensorDataPoints": { "humidity": { "percentage": 66.5 } },
            "openWindow": null
        });
        state
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        state
    }

    #[test]
    fn builds_snapshot_from_api_responses() {
        // Arrange
        let manual_off = json!({
            "setting": { "type": "HEATING", "power": "OFF", "temperature": null },
            "overlay": {
                "type": "MANUAL",
                "setting": { "type": "HEATING", "power": "OFF", "temperature": null },
                "termination": { "type": "MANUAL" }
            }
        });
        let zones = vec![
            zone_response(
                1,
                "Salotto",
                base_state(json!({ "openWindowDetected": true })),
            ),
            zone_response(2, "Camera", base_state(manual_off.clone())),
            zone_response(3, "Bagno", base_state(manual_off)),
        ];
        let state = AutomationState {
            owned_overlays: [(3, OwnedOverlay::Off)].iter().copied().collect(),
            ..AutomationState::default()
        };
        let weather = WeatherApiResponse {
            solarIntensity: WeatherSolarIntensityApiResponse { percentage: 0.0 },
            outsideTemperature: WeatherOutsideTemperatureApiResponse {
                celsius: 12.5,
                fahrenheit: 54.5,
            },
        };
        let phones: Vec<MobileDeviceApiResponse> = serde_json::from_value(json!([
            { "settings": { "geoTrackingEnabled": true }, "location": { "atHome": false } },
            { "settings": { "geoTrackingEnabled": true } },
            { "settings": { "geoTrackingEnabled": false }, "location": { "atHome": true } }
        ]))
        .unwrap();

        // Act
        let snapshot = build_snapshot(&state, 99, &zones, Some(&weather), Some(&phones));

        // Assert
        assert_eq!(snapshot.now, 99);
        assert_eq!(snapshot.outside_celsius, Some(12.5));
        assert_eq!(snapshot.presence, Some(Presence::Home));
        assert_eq!(
            snapshot.phones,
            Some(vec![PhoneLocation { at_home: false }])
        );

        let salotto = snapshot.zone(1).unwrap();
        assert_eq!(salotto.control, ZoneControl::Schedule);
        assert_eq!(salotto.setting_celsius, Some(20.0));
        assert_eq!(salotto.humidity, Some(66.5));
        assert!(salotto.open_window_detected);
        assert!(!salotto.open_window_active);

        let camera = snapshot.zone(2).unwrap();
        assert_eq!(camera.control, ZoneControl::Manual);
        assert_eq!(camera.setting_celsius, None);

        assert_eq!(
            snapshot.zone(3).unwrap().control,
            ZoneControl::Owned(OwnedOverlay::Off)
        );
    }

    #[test]
    fn first_decision_per_zone_wins() {
        // Arrange
        let decisions = vec![
            Decision {
                rule: "a",
                action: Action::ActivateOpenWindow { zone: 1 },
            },
            Decision {
                rule: "b",
                action: Action::HeatingOff { zone: 1 },
            },
            Decision {
                rule: "b",
                action: Action::HeatingOff { zone: 2 },
            },
            Decision {
                rule: "c",
                action: Action::SetPresence(Presence::Away),
            },
            Decision {
                rule: "d",
                action: Action::SetPresence(Presence::Home),
            },
        ];

        // Act
        let kept: Vec<(&str, Option<i32>)> = without_conflicts(decisions)
            .iter()
            .map(|d| (d.rule, d.action.zone()))
            .collect();

        // Assert
        assert_eq!(kept, vec![("a", Some(1)), ("b", Some(2)), ("c", None)]);
    }
}
