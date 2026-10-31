//! Mould guard: when a zone's humidity stays high, heat it briefly to dry it out.
//!
//! A boost is a timed overlay owned by this exporter. If it replaced an owned "heating off"
//! (e.g. during the off season), heating is switched off again once the boost ends.

use std::collections::BTreeMap;

use serde_derive::{Deserialize, Serialize};

use crate::automation::config::{parse_bool, parse_or};
use crate::automation::state::OwnedOverlay;
use crate::automation::{Action, Snapshot, Timestamp, ZoneControl, ZoneSnapshot};

pub const NAME: &str = "humidity";

const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_DAY: u64 = 24 * 3600;
/// Temperature range and step accepted by tado° heating zones.
const MIN_CELSIUS: f64 = 5.0;
const MAX_CELSIUS: f64 = 25.0;
const CELSIUS_STEPS_PER_DEGREE: f64 = 2.0;
/// Upper bounds for durations, keeping the time arithmetic far from overflow.
const MAX_DURATION_MINUTES: u64 = 7 * 24 * 60;
const MAX_BOOST_MINUTES: u64 = 24 * 60;
/// Observations further apart than this do not prove humidity stayed high in between.
const MAX_OBSERVATION_GAP_SECONDS: u64 = 3600;

#[derive(Debug, Clone, PartialEq)]
pub struct HumidityConfig {
    pub enabled: bool,
    /// Relative humidity (%) above which a zone counts as too humid.
    pub threshold: f64,
    /// How long humidity must stay above the threshold before boosting.
    pub duration_minutes: u64,
    pub boost_celsius: f64,
    pub boost_minutes: u64,
    /// Maximum boosts per zone in any 24 hours.
    pub max_boosts_per_day: usize,
}

impl HumidityConfig {
    pub fn from_lookup(lookup: &impl Fn(&str) -> Option<String>) -> HumidityConfig {
        let config = HumidityConfig {
            enabled: parse_bool(lookup, "AUTOMATION_HUMIDITY_ENABLED", false),
            threshold: parse_or(lookup, "AUTOMATION_HUMIDITY_THRESHOLD", 65.0),
            duration_minutes: parse_or(lookup, "AUTOMATION_HUMIDITY_DURATION_MINUTES", 180),
            boost_celsius: parse_or(lookup, "AUTOMATION_HUMIDITY_BOOST_CELSIUS", 21.0),
            boost_minutes: parse_or(lookup, "AUTOMATION_HUMIDITY_BOOST_MINUTES", 60),
            max_boosts_per_day: parse_or(lookup, "AUTOMATION_HUMIDITY_MAX_BOOSTS_PER_DAY", 2),
        };

        assert!(
            (0.0..=100.0).contains(&config.threshold),
            "AUTOMATION_HUMIDITY_THRESHOLD must be a percentage, got {}",
            config.threshold
        );
        assert!(
            (MIN_CELSIUS..=MAX_CELSIUS).contains(&config.boost_celsius)
                && (config.boost_celsius * CELSIUS_STEPS_PER_DEGREE).fract() == 0.0,
            "AUTOMATION_HUMIDITY_BOOST_CELSIUS must be between {} and {} in 0.5 steps, got {}",
            MIN_CELSIUS,
            MAX_CELSIUS,
            config.boost_celsius
        );
        assert!(
            (1..=MAX_BOOST_MINUTES).contains(&config.boost_minutes),
            "AUTOMATION_HUMIDITY_BOOST_MINUTES must be between 1 and {}",
            MAX_BOOST_MINUTES
        );
        assert!(
            config.duration_minutes <= MAX_DURATION_MINUTES,
            "AUTOMATION_HUMIDITY_DURATION_MINUTES must be at most {}",
            MAX_DURATION_MINUTES
        );

        config
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HumidityZoneState {
    /// Since when humidity has been above the threshold without interruption.
    pub high_since: Option<Timestamp>,
    /// When the boosts of the last 24 hours started.
    pub boosts: Vec<Timestamp>,
    /// When this zone's humidity was last observed.
    pub last_seen: Option<Timestamp>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HumidityState {
    pub zones: BTreeMap<i32, HumidityZoneState>,
}

/// `restore_after_boost`: switch heating off again once a boost that replaced an owned "off"
/// ends. Disabled when the season rule is on, as it decides that based on the season.
pub fn evaluate(
    config: &HumidityConfig,
    snapshot: &Snapshot,
    state: &HumidityState,
    restore_after_boost: bool,
) -> (Vec<Action>, HumidityState) {
    let results: Vec<(i32, Option<Action>, HumidityZoneState)> = snapshot
        .zones
        .iter()
        .map(|zone| {
            let previous = state.zones.get(&zone.id).cloned().unwrap_or_default();
            let (action, next) =
                evaluate_zone(config, snapshot.now, zone, &previous, restore_after_boost);
            (zone.id, action, next)
        })
        .collect();

    let actions = results
        .iter()
        .filter_map(|(_, action, _)| action.clone())
        .collect();
    // Zones missing from this snapshot keep their state (a partial response is no reset).
    let zones = state
        .zones
        .clone()
        .into_iter()
        .chain(
            results
                .into_iter()
                .map(|(id, _, zone_state)| (id, zone_state)),
        )
        .collect();

    (actions, HumidityState { zones })
}

fn evaluate_zone(
    config: &HumidityConfig,
    now: Timestamp,
    zone: &ZoneSnapshot,
    state: &HumidityZoneState,
    restore_after_boost: bool,
) -> (Option<Action>, HumidityZoneState) {
    let recent_boosts: Vec<Timestamp> = state
        .boosts
        .iter()
        .copied()
        .filter(|at| now.saturating_sub(*at) < SECONDS_PER_DAY)
        .collect();
    let observed_continuously = state
        .last_seen
        .is_some_and(|seen| now.saturating_sub(seen) <= MAX_OBSERVATION_GAP_SECONDS);
    let high_since = match zone.humidity {
        Some(humidity) if humidity > config.threshold => state
            .high_since
            .filter(|_| observed_continuously)
            .or(Some(now)),
        _ => None,
    };
    let last_seen = zone.humidity.map(|_| now).or(state.last_seen);

    let humid_long_enough = high_since.is_some_and(|since| {
        now.saturating_sub(since) >= config.duration_minutes * SECONDS_PER_MINUTE
    });
    let can_boost = recent_boosts.len() < config.max_boosts_per_day
        && boostable_control(zone.control).is_some()
        && !zone.open_window_detected
        && !zone.open_window_active
        && zone
            .setting_celsius
            .is_none_or(|celsius| celsius < config.boost_celsius);

    if humid_long_enough && can_boost {
        let then_off = boostable_control(zone.control).unwrap_or(false);
        let action = Action::Boost {
            zone: zone.id,
            celsius: config.boost_celsius,
            minutes: config.boost_minutes,
            then_off,
        };
        let boosts = recent_boosts.into_iter().chain(Some(now)).collect();
        // Humidity must stay high for another full duration before the next boost.
        let next = HumidityZoneState {
            high_since: None,
            boosts,
            last_seen,
        };
        return (Some(action), next);
    }

    // A boost that replaced an owned "off" has ended: switch heating off again.
    let restore = match zone.control {
        ZoneControl::BoostEnded { then_off: true } if restore_after_boost => {
            Some(Action::HeatingOff { zone: zone.id })
        }
        _ => None,
    };

    (
        restore,
        HumidityZoneState {
            high_since,
            boosts: recent_boosts,
            last_seen,
        },
    )
}

/// Whether a boost may replace the zone's current control, and if so whether heating must be
/// switched off again afterwards.
fn boostable_control(control: ZoneControl) -> Option<bool> {
    match control {
        ZoneControl::Schedule => Some(false),
        ZoneControl::Owned(OwnedOverlay::Off) => Some(true),
        ZoneControl::BoostEnded { then_off } => Some(then_off),
        ZoneControl::Owned(OwnedOverlay::Boost { .. }) | ZoneControl::Manual => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::automation::config::tests::lookup;
    use rstest::rstest;

    const MINUTE: u64 = SECONDS_PER_MINUTE;
    const NOW: Timestamp = 1_000_000;

    fn config() -> HumidityConfig {
        HumidityConfig::from_lookup(&lookup(&[("AUTOMATION_HUMIDITY_ENABLED", "true")]))
    }

    fn zone(control: ZoneControl, humidity: f64, setting: Option<f64>) -> ZoneSnapshot {
        ZoneSnapshot {
            id: 4,
            name: "Bagno".to_string(),
            control,
            setting_celsius: setting,
            humidity: Some(humidity),
            open_window_detected: false,
            open_window_active: false,
        }
    }

    fn snapshot(zone: ZoneSnapshot) -> Snapshot {
        Snapshot {
            now: NOW,
            zones: vec![zone],
            outside_celsius: None,
            presence: None,
            phones: None,
        }
    }

    fn humid_since(minutes_ago: u64, boosts: Vec<Timestamp>) -> HumidityState {
        HumidityState {
            zones: [(
                4,
                HumidityZoneState {
                    high_since: Some(NOW - minutes_ago * MINUTE),
                    boosts,
                    last_seen: Some(NOW - 5 * MINUTE),
                },
            )]
            .iter()
            .cloned()
            .collect(),
        }
    }

    fn boost(then_off: bool) -> Action {
        Action::Boost {
            zone: 4,
            celsius: 21.0,
            minutes: 60,
            then_off,
        }
    }

    #[test]
    fn starts_tracking_when_humidity_rises() {
        // Act
        let (actions, state) = evaluate(
            &config(),
            &snapshot(zone(ZoneControl::Schedule, 70.0, None)),
            &HumidityState::default(),
            true,
        );

        // Assert
        assert!(actions.is_empty());
        assert_eq!(state.zones[&4].high_since, Some(NOW));
    }

    #[test]
    fn resets_when_humidity_drops() {
        // Act
        let (actions, state) = evaluate(
            &config(),
            &snapshot(zone(ZoneControl::Schedule, 60.0, None)),
            &humid_since(170, vec![]),
            true,
        );

        // Assert
        assert!(actions.is_empty());
        assert_eq!(state.zones[&4].high_since, None);
    }

    #[rstest]
    #[case::schedule_off(ZoneControl::Schedule, None, Some(boost(false)))]
    #[case::schedule_cooler(ZoneControl::Schedule, Some(19.0), Some(boost(false)))]
    #[case::owned_off(ZoneControl::Owned(OwnedOverlay::Off), None, Some(boost(true)))]
    #[case::already_warmer(ZoneControl::Schedule, Some(22.0), None)]
    #[case::manual(ZoneControl::Manual, None, None)]
    fn boosts_after_duration(
        #[case] control: ZoneControl,
        #[case] setting: Option<f64>,
        #[case] expected: Option<Action>,
    ) {
        // Act
        let (actions, state) = evaluate(
            &config(),
            &snapshot(zone(control, 70.0, setting)),
            &humid_since(180, vec![]),
            true,
        );

        // Assert
        assert_eq!(actions, expected.clone().into_iter().collect::<Vec<_>>());
        if expected.is_some() {
            assert_eq!(state.zones[&4].boosts, vec![NOW]);
            assert_eq!(state.zones[&4].high_since, None);
        }
    }

    #[test]
    fn limits_boosts_per_day() {
        // Arrange
        let earlier_today = vec![NOW - 600 * MINUTE, NOW - 300 * MINUTE];
        let yesterday = vec![NOW - 25 * 60 * MINUTE, NOW - 300 * MINUTE];

        // Act
        let (limited, _) = evaluate(
            &config(),
            &snapshot(zone(ZoneControl::Schedule, 70.0, None)),
            &humid_since(180, earlier_today),
            true,
        );
        let (allowed, state) = evaluate(
            &config(),
            &snapshot(zone(ZoneControl::Schedule, 70.0, None)),
            &humid_since(180, yesterday),
            true,
        );

        // Assert
        assert!(limited.is_empty());
        assert_eq!(allowed, vec![boost(false)]);
        assert_eq!(state.zones[&4].boosts, vec![NOW - 300 * MINUTE, NOW]);
    }

    #[test]
    fn skips_zones_with_open_windows() {
        // Arrange
        let mut humid_zone = zone(ZoneControl::Schedule, 70.0, None);
        humid_zone.open_window_detected = true;

        // Act
        let (actions, _) = evaluate(
            &config(),
            &snapshot(humid_zone),
            &humid_since(180, vec![]),
            true,
        );

        // Assert
        assert!(actions.is_empty());
    }

    #[test]
    fn restores_heating_off_after_boost_replaced_owned_off() {
        // Act
        let (actions, _) = evaluate(
            &config(),
            &snapshot(zone(
                ZoneControl::BoostEnded { then_off: true },
                50.0,
                Some(20.0),
            )),
            &HumidityState::default(),
            true,
        );

        // Assert
        assert_eq!(actions, vec![Action::HeatingOff { zone: 4 }]);
    }

    #[test]
    fn leaves_restore_to_the_season_rule_when_enabled() {
        // Act
        let (actions, _) = evaluate(
            &config(),
            &snapshot(zone(
                ZoneControl::BoostEnded { then_off: true },
                50.0,
                Some(20.0),
            )),
            &HumidityState::default(),
            false,
        );

        // Assert
        assert!(actions.is_empty());
    }

    #[test]
    fn restarts_tracking_after_an_observation_gap() {
        // Arrange: humid for long enough, but the last observation is 2 hours old.
        let mut state = humid_since(400, vec![]);
        state.zones.get_mut(&4).unwrap().last_seen = Some(NOW - 120 * MINUTE);

        // Act
        let (actions, state) = evaluate(
            &config(),
            &snapshot(zone(ZoneControl::Schedule, 70.0, None)),
            &state,
            true,
        );

        // Assert
        assert!(actions.is_empty());
        assert_eq!(state.zones[&4].high_since, Some(NOW));
    }

    #[test]
    fn keeps_state_of_zones_missing_from_the_snapshot() {
        // Arrange
        let mut state = humid_since(100, vec![NOW - 60 * MINUTE]);
        state.zones.insert(9, state.zones[&4].clone());

        // Act
        let (_, next) = evaluate(
            &config(),
            &snapshot(zone(ZoneControl::Schedule, 70.0, None)),
            &state,
            true,
        );

        // Assert
        assert_eq!(next.zones[&9], state.zones[&9]);
    }

    #[rstest]
    #[case("30")]
    #[case("21.3")]
    #[should_panic(expected = "AUTOMATION_HUMIDITY_BOOST_CELSIUS must be between")]
    fn rejects_invalid_boost_temperature(#[case] value: &str) {
        HumidityConfig::from_lookup(&lookup(&[("AUTOMATION_HUMIDITY_BOOST_CELSIUS", value)]));
    }
}
