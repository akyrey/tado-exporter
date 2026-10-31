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
