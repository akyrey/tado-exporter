//! Open-window auto-off: confirm windows tado° detected, as Auto-Assist would.
//!
//! Without Auto-Assist, tado° only *suggests* open-window mode and heating keeps running until
//! someone confirms it in the app. This rule confirms it for zones the automation may control.

use std::collections::BTreeSet;

use serde_derive::{Deserialize, Serialize};

use crate::automation::config::parse_bool;
use crate::automation::state::OwnedOverlay;
use crate::automation::{Action, Snapshot, ZoneControl, ZoneSnapshot};

pub const NAME: &str = "open_window";

#[derive(Debug, Clone, PartialEq)]
pub struct OpenWindowConfig {
    pub enabled: bool,
}

impl OpenWindowConfig {
    pub fn from_lookup(lookup: &impl Fn(&str) -> Option<String>) -> OpenWindowConfig {
        OpenWindowConfig {
            enabled: parse_bool(lookup, "AUTOMATION_OPEN_WINDOW_ENABLED", false),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenWindowState {
    /// Zones whose current detection was already acted upon (avoids repeating the action,
    /// e.g. in dry-run mode, while the detection lasts).
    pub handled: BTreeSet<i32>,
}

pub fn evaluate(snapshot: &Snapshot, state: &OpenWindowState) -> (Vec<Action>, OpenWindowState) {
    let to_activate: Vec<i32> = snapshot
        .zones
        .iter()
        .filter(|zone| should_activate(zone) && !state.handled.contains(&zone.id))
        .map(|zone| zone.id)
        .collect();

    let handled = snapshot
        .zones
        .iter()
        .filter(|zone| zone.open_window_detected)
        .map(|zone| zone.id)
        .filter(|id| state.handled.contains(id) || to_activate.contains(id))
        .collect();

    let actions = to_activate
        .into_iter()
        .map(|zone| Action::ActivateOpenWindow { zone })
        .collect();

    (actions, OpenWindowState { handled })
}

fn should_activate(zone: &ZoneSnapshot) -> bool {
    let heating_controllable = match zone.control {
        ZoneControl::Schedule | ZoneControl::BoostEnded { .. } => true,
        ZoneControl::Owned(OwnedOverlay::Boost { .. }) => true,
        // Heating is already off, or someone set this zone by hand.
        ZoneControl::Owned(OwnedOverlay::Off) | ZoneControl::Manual => false,
    };

    zone.open_window_detected && !zone.open_window_active && heating_controllable
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::automation::config::tests::lookup;
    use crate::automation::ZoneSnapshot;
    use rstest::rstest;

    fn zone(id: i32, control: ZoneControl, detected: bool, active: bool) -> ZoneSnapshot {
        ZoneSnapshot {
            id,
            name: format!("zone {}", id),
            control,
            setting_celsius: Some(20.0),
            humidity: None,
            open_window_detected: detected,
            open_window_active: active,
        }
    }

    fn snapshot(zones: Vec<ZoneSnapshot>) -> Snapshot {
        Snapshot {
            now: 0,
            zones,
            outside_celsius: None,
            presence: None,
            phones: None,
        }
    }

    const BOOST: OwnedOverlay = OwnedOverlay::Boost {
        celsius: 21.0,
        until: 0,
        then_off: false,
    };

    #[rstest]
    #[case::schedule(ZoneControl::Schedule, true, false, true)]
    #[case::owned_boost(ZoneControl::Owned(BOOST), true, false, true)]
    #[case::boost_ended(ZoneControl::BoostEnded { then_off: false }, true, false, true)]
    #[case::not_detected(ZoneControl::Schedule, false, false, false)]
    #[case::already_active(ZoneControl::Schedule, true, true, false)]
    #[case::manual(ZoneControl::Manual, true, false, false)]
    #[case::owned_off(ZoneControl::Owned(OwnedOverlay::Off), true, false, false)]
    fn activates_detected_windows_in_controllable_zones(
        #[case] control: ZoneControl,
        #[case] detected: bool,
        #[case] active: bool,
        #[case] expected: bool,
    ) {
        // Arrange
        let snapshot = snapshot(vec![zone(1, control, detected, active)]);

        // Act
        let (actions, _) = evaluate(&snapshot, &OpenWindowState::default());

        // Assert
        let expected_actions = if expected {
            vec![Action::ActivateOpenWindow { zone: 1 }]
        } else {
            vec![]
        };
        assert_eq!(actions, expected_actions);
    }

    #[test]
    fn acts_once_per_detection() {
        // Arrange
        let detected = snapshot(vec![zone(1, ZoneControl::Schedule, true, false)]);
        let cleared = snapshot(vec![zone(1, ZoneControl::Schedule, false, false)]);

        // Act
        let (first, state) = evaluate(&detected, &OpenWindowState::default());
        let (second, state) = evaluate(&detected, &state);
        let (_, state) = evaluate(&cleared, &state);
        let (third, _) = evaluate(&detected, &state);

        // Assert
        assert_eq!(first.len(), 1);
        assert!(second.is_empty(), "same detection is not repeated");
        assert_eq!(third.len(), 1, "a new detection is acted upon again");
    }

    #[test]
    fn disabled_by_default() {
        assert!(!OpenWindowConfig::from_lookup(&lookup(&[])).enabled);
        assert!(
            OpenWindowConfig::from_lookup(&lookup(&[("AUTOMATION_OPEN_WINDOW_ENABLED", "true")]))
                .enabled
        );
    }
}
