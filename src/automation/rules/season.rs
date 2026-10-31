//! Seasonal on/off: resume the schedules when it has been cold outside, switch heating off
//! when it has been warm, based on the mean outside temperature over a rolling window.
//!
//! Switching happens only when the mean crosses a threshold (with hysteresis between them), so
//! a manual change in the app is kept until the next season change. Only zones following
//! their schedule are switched off, and only overlays this exporter applied are removed.

use log::info;
use serde_derive::{Deserialize, Serialize};

use crate::automation::config::{parse_bool, parse_or};
use crate::automation::state::OwnedOverlay;
use crate::automation::{Action, Snapshot, Timestamp, ZoneControl};

pub const NAME: &str = "season";

const SECONDS_PER_HOUR: u64 = 3600;
/// The window counts as covered once samples span this share of it (tolerates tick jitter).
const COVERAGE_PERCENT: u64 = 95;
/// Upper bound for the window, keeping the time arithmetic far from overflow.
const MAX_WINDOW_HOURS: u64 = 7 * 24;

#[derive(Debug, Clone, PartialEq)]
pub struct SeasonConfig {
    pub enabled: bool,
    /// Heating season starts when the mean outside temperature drops below this.
    pub on_below: f64,
    /// Heating season ends when the mean outside temperature rises above this.
    pub off_above: f64,
    pub window_hours: u64,
}

impl SeasonConfig {
    pub fn from_lookup(lookup: &impl Fn(&str) -> Option<String>) -> SeasonConfig {
        let config = SeasonConfig {
            enabled: parse_bool(lookup, "AUTOMATION_SEASON_ENABLED", false),
            on_below: parse_or(lookup, "AUTOMATION_SEASON_ON_BELOW", 14.0),
            off_above: parse_or(lookup, "AUTOMATION_SEASON_OFF_ABOVE", 17.0),
            window_hours: parse_or(lookup, "AUTOMATION_SEASON_WINDOW_HOURS", 24),
        };

        assert!(
            config.off_above > config.on_below,
            "AUTOMATION_SEASON_OFF_ABOVE ({}) must be greater than AUTOMATION_SEASON_ON_BELOW ({})",
            config.off_above,
            config.on_below
        );
        assert!(
            (1..=MAX_WINDOW_HOURS).contains(&config.window_hours),
            "AUTOMATION_SEASON_WINDOW_HOURS must be between 1 and {}",
            MAX_WINDOW_HOURS
        );

        config
    }

    fn window_seconds(&self) -> u64 {
        self.window_hours * SECONDS_PER_HOUR
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Season {
    Heating,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub at: Timestamp,
    pub celsius: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SeasonState {
    /// Outside temperatures within the window, oldest first.
    pub samples: Vec<Sample>,
    /// The season decided so far; `None` until the first threshold crossing.
    pub season: Option<Season>,
}

pub fn evaluate(
    config: &SeasonConfig,
    snapshot: &Snapshot,
    state: &SeasonState,
) -> (Vec<Action>, SeasonState) {
    let samples = updated_samples(config, snapshot, &state.samples);
    let decided = decide(config, snapshot.now, &samples).or(state.season);

    if decided != state.season {
        if let Some(season) = decided {
            info!(
                "{}: mean outside temperature {:.1}°C over {}h, switching to {:?} season",
                NAME,
                mean(&samples).unwrap_or_default(),
                config.window_hours,
                season
            );
        }
    }

    let is_transition = decided != state.season;
    let actions = match decided {
        Some(season) => actions_for(season, is_transition, snapshot),
        None => Vec::new(),
    };

    (
        actions,
        SeasonState {
            samples,
            season: decided,
        },
    )
}

fn updated_samples(config: &SeasonConfig, snapshot: &Snapshot, samples: &[Sample]) -> Vec<Sample> {
    let oldest_kept = snapshot.now.saturating_sub(config.window_seconds());
    let latest = snapshot.outside_celsius.map(|celsius| Sample {
        at: snapshot.now,
        celsius,
    });

    samples
        .iter()
        .copied()
        .filter(|sample| sample.at >= oldest_kept)
        .chain(latest)
        .collect()
}

/// The season the samples call for, or `None` if the window is not covered yet or the mean
/// lies between the thresholds.
fn decide(config: &SeasonConfig, now: Timestamp, samples: &[Sample]) -> Option<Season> {
    let required_span = config.window_seconds() * COVERAGE_PERCENT / 100;
    let covered = samples
        .first()
        .is_some_and(|oldest| now.saturating_sub(oldest.at) >= required_span);
    if !covered {
        return None;
    }

    let mean = mean(samples)?;
    if mean < config.on_below {
        Some(Season::Heating)
    } else if mean > config.off_above {
        Some(Season::Off)
    } else {
        None
    }
}

fn mean(samples: &[Sample]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    Some(samples.iter().map(|s| s.celsius).sum::<f64>() / samples.len() as f64)
}

fn actions_for(season: Season, is_transition: bool, snapshot: &Snapshot) -> Vec<Action> {
    snapshot
        .zones
        .iter()
        .filter_map(|zone| match (season, zone.control) {
            // Retried every tick: only removes overlays this exporter applied.
            (Season::Heating, ZoneControl::Owned(OwnedOverlay::Off)) => {
                Some(Action::ResumeSchedule { zone: zone.id })
            }
            // Only on the change of season, so resuming a zone by hand is respected.
            (Season::Off, ZoneControl::Schedule) if is_transition => {
                Some(Action::HeatingOff { zone: zone.id })
            }
            // A boost interrupted the off season: switch heating off again afterwards.
            (Season::Off, ZoneControl::BoostEnded { .. }) => {
                Some(Action::HeatingOff { zone: zone.id })
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::automation::config::tests::lookup;
    use crate::automation::ZoneSnapshot;

    const HOUR: u64 = SECONDS_PER_HOUR;

    fn config() -> SeasonConfig {
        SeasonConfig::from_lookup(&lookup(&[("AUTOMATION_SEASON_ENABLED", "true")]))
    }

    fn zone(id: i32, control: ZoneControl) -> ZoneSnapshot {
        ZoneSnapshot {
            id,
            name: format!("zone {}", id),
            control,
            setting_celsius: None,
            humidity: None,
            open_window_detected: false,
            open_window_active: false,
        }
    }

    fn snapshot(now: Timestamp, outside: f64, zones: Vec<ZoneSnapshot>) -> Snapshot {
        Snapshot {
            now,
            zones,
            outside_celsius: Some(outside),
            presence: None,
            phones: None,
        }
    }

    /// Hourly samples at a constant temperature covering the whole window before `now`.
    fn history(now: Timestamp, celsius: f64, season: Option<Season>) -> SeasonState {
        SeasonState {
            samples: (1..=24)
                .rev()
                .map(|h| Sample {
                    at: now - h * HOUR,
                    celsius,
                })
                .collect(),
            season,
        }
    }

    fn all_zones() -> Vec<ZoneSnapshot> {
        vec![
            zone(1, ZoneControl::Schedule),
            zone(2, ZoneControl::Owned(OwnedOverlay::Off)),
            zone(3, ZoneControl::Manual),
            zone(4, ZoneControl::BoostEnded { then_off: false }),
        ]
    }

    #[test]
    fn waits_for_a_full_window_before_deciding() {
        // Arrange
        let now = 100 * HOUR;
        let state = SeasonState {
            samples: vec![Sample {
                at: now - 2 * HOUR,
                celsius: 5.0,
            }],
            season: None,
        };

        // Act
        let (actions, state) = evaluate(&config(), &snapshot(now, 5.0, all_zones()), &state);

        // Assert
        assert!(actions.is_empty());
        assert_eq!(state.season, None);
        assert_eq!(state.samples.len(), 2);
    }

    #[test]
    fn warm_window_switches_scheduled_zones_off_once() {
        // Arrange
        let now = 100 * HOUR;

        // Act
        let (actions, state) = evaluate(
            &config(),
            &snapshot(now, 20.0, all_zones()),
            &history(now, 20.0, Some(Season::Heating)),
        );
        let (repeat, _) = evaluate(&config(), &snapshot(now + HOUR, 20.0, all_zones()), &state);

        // Assert
        assert_eq!(state.season, Some(Season::Off));
        assert_eq!(
            actions,
            vec![
                Action::HeatingOff { zone: 1 },
                Action::HeatingOff { zone: 4 }
            ]
        );
        assert_eq!(
            repeat,
            vec![Action::HeatingOff { zone: 4 }],
            "scheduled zones are not switched off again, ended boosts are"
        );
    }

    #[test]
    fn cold_window_resumes_only_owned_off_zones() {
        // Arrange
        let now = 100 * HOUR;

        // Act
        let (actions, state) = evaluate(
            &config(),
            &snapshot(now, 8.0, all_zones()),
            &history(now, 8.0, Some(Season::Off)),
        );

        // Assert
        assert_eq!(state.season, Some(Season::Heating));
        assert_eq!(actions, vec![Action::ResumeSchedule { zone: 2 }]);
    }

    #[test]
    fn keeps_season_between_thresholds() {
        // Arrange
        let now = 100 * HOUR;

        // Act
        let (actions, state) = evaluate(
            &config(),
            &snapshot(now, 15.5, vec![zone(1, ZoneControl::Schedule)]),
            &history(now, 15.5, Some(Season::Heating)),
        );

        // Assert
        assert!(actions.is_empty());
        assert_eq!(state.season, Some(Season::Heating));
    }

    #[test]
    fn drops_samples_older_than_the_window() {
        // Arrange
        let now = 100 * HOUR;
        let mut state = history(now, 10.0, None);
        state.samples.insert(
            0,
            Sample {
                at: now - 30 * HOUR,
                celsius: 40.0,
            },
        );

        // Act
        let (_, state) = evaluate(&config(), &snapshot(now, 10.0, vec![]), &state);

        // Assert
        assert!(state.samples.iter().all(|s| s.at >= now - 24 * HOUR));
        assert_eq!(state.season, Some(Season::Heating));
    }

    #[test]
    #[should_panic(expected = "AUTOMATION_SEASON_OFF_ABOVE (14) must be greater")]
    fn rejects_inverted_thresholds() {
        SeasonConfig::from_lookup(&lookup(&[
            ("AUTOMATION_SEASON_ON_BELOW", "15"),
            ("AUTOMATION_SEASON_OFF_ABOVE", "14"),
        ]));
    }
}
