//! Persisted automation state: which overlays the exporter owns, plus each rule's memory.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use log::warn;
use serde_derive::{Deserialize, Serialize};

use super::rules::open_window::OpenWindowState;
use super::rules::season::SeasonState;
use super::{Action, Snapshot, Timestamp, ZoneControl};
use crate::persist::write_private_atomic;

/// Tolerance when comparing a zone's target temperature with the one an owned boost set.
const CELSIUS_TOLERANCE: f64 = 0.01;

/// An overlay this exporter applied. Anything else on a zone is considered manual.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnedOverlay {
    Off,
    Boost {
        celsius: f64,
        until: Timestamp,
        then_off: bool,
    },
}

/// The parts of a zone's current overlay needed to recognise owned overlays.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayInfo {
    pub power_off: bool,
    /// tado°'s termination type: `MANUAL`, `TIMER` or `TADO_MODE`.
    pub termination: String,
    pub celsius: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomationState {
    /// Overlays applied by this exporter, by zone id.
    pub owned_overlays: BTreeMap<i32, OwnedOverlay>,
    pub open_window: OpenWindowState,
    pub season: SeasonState,
}

/// Decides who controls a zone from its current overlay and what this exporter owns.
pub fn zone_control(owned: Option<OwnedOverlay>, overlay: Option<&OverlayInfo>) -> ZoneControl {
    match (owned, overlay) {
        (Some(OwnedOverlay::Boost { then_off, .. }), None) => ZoneControl::BoostEnded { then_off },
        (_, None) => ZoneControl::Schedule,
        (Some(owned), Some(overlay)) if is_same_overlay(owned, overlay) => {
            ZoneControl::Owned(owned)
        }
        (_, Some(_)) => ZoneControl::Manual,
    }
}

fn is_same_overlay(owned: OwnedOverlay, overlay: &OverlayInfo) -> bool {
    match owned {
        OwnedOverlay::Off => overlay.power_off && overlay.termination == "MANUAL",
        OwnedOverlay::Boost { celsius, .. } => {
            overlay.termination == "TIMER"
                && overlay
                    .celsius
                    .map_or(false, |c| (c - celsius).abs() < CELSIUS_TOLERANCE)
        }
    }
}

/// Forgets owned overlays that are gone or were replaced by someone else.
pub fn reconcile(state: &AutomationState, snapshot: &Snapshot) -> AutomationState {
    let owned_overlays = state
        .owned_overlays
        .iter()
        .filter(|(id, _)| {
            snapshot
                .zone(**id)
                .map_or(false, |zone| matches!(zone.control, ZoneControl::Owned(_)))
        })
        .map(|(id, overlay)| (*id, *overlay))
        .collect();

    AutomationState {
        owned_overlays,
        ..state.clone()
    }
}

/// Records the ownership change caused by an action that was successfully applied.
pub fn record_applied(state: &AutomationState, action: &Action, now: Timestamp) -> AutomationState {
    let mut owned_overlays = state.owned_overlays.clone();

    match *action {
        Action::HeatingOff { zone } => {
            owned_overlays.insert(zone, OwnedOverlay::Off);
        }
        Action::Boost {
            zone,
            celsius,
            minutes,
            then_off,
        } => {
            let boost = OwnedOverlay::Boost {
                celsius,
                until: now + minutes * 60,
                then_off,
            };
            owned_overlays.insert(zone, boost);
        }
        Action::ResumeSchedule { zone } => {
            owned_overlays.remove(&zone);
        }
        Action::ActivateOpenWindow { .. } | Action::SetPresence(_) => {}
    }

    AutomationState {
        owned_overlays,
        ..state.clone()
    }
}

pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> StateStore {
        StateStore { path: path.into() }
    }

    /// Loads the state, starting fresh (with a warning) if it is missing or unreadable.
    pub fn load(&self) -> AutomationState {
        match self.try_load() {
            Ok(state) => state,
            Err(e) => {
                warn!(
                    "unable to read automation state {}, starting fresh: {}",
                    self.path.display(),
                    e
                );
                AutomationState::default()
            }
        }
    }

    fn try_load(&self) -> io::Result<AutomationState> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => serde_json::from_str(&contents)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(AutomationState::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, state: &AutomationState) -> io::Result<()> {
        let contents = serde_json::to_vec_pretty(state)?;
        write_private_atomic(&self.path, &contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::automation::ZoneSnapshot;
    use rstest::rstest;
    use tempfile::TempDir;

    fn overlay(power_off: bool, termination: &str, celsius: Option<f64>) -> OverlayInfo {
        OverlayInfo {
            power_off,
            termination: termination.to_string(),
            celsius,
        }
    }

    const BOOST: OwnedOverlay = OwnedOverlay::Boost {
        celsius: 21.0,
        until: 1_000,
        then_off: true,
    };

    #[rstest]
    #[case::schedule(None, None, ZoneControl::Schedule)]
    #[case::manual_off(None, Some(overlay(true, "MANUAL", None)), ZoneControl::Manual)]
    #[case::owned_off(
        Some(OwnedOverlay::Off),
        Some(overlay(true, "MANUAL", None)),
        ZoneControl::Owned(OwnedOverlay::Off)
    )]
    #[case::owned_off_replaced_by_user(
        Some(OwnedOverlay::Off),
        Some(overlay(false, "MANUAL", Some(22.0))),
        ZoneControl::Manual
    )]
    #[case::owned_off_resumed_by_user(Some(OwnedOverlay::Off), None, ZoneControl::Schedule)]
    #[case::owned_boost(
        Some(BOOST),
        Some(overlay(false, "TIMER", Some(21.0))),
        ZoneControl::Owned(BOOST)
    )]
    #[case::boost_changed_by_user(
        Some(BOOST),
        Some(overlay(false, "TIMER", Some(23.0))),
        ZoneControl::Manual
    )]
    #[case::boost_ended(Some(BOOST), None, ZoneControl::BoostEnded { then_off: true })]
    fn decides_zone_control(
        #[case] owned: Option<OwnedOverlay>,
        #[case] current: Option<OverlayInfo>,
        #[case] expected: ZoneControl,
    ) {
        assert_eq!(zone_control(owned, current.as_ref()), expected);
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

    #[test]
    fn reconcile_forgets_overlays_no_longer_owned() {
        // Arrange
        let state = AutomationState {
            owned_overlays: [(1, OwnedOverlay::Off), (2, OwnedOverlay::Off), (3, BOOST)]
                .iter()
                .copied()
                .collect(),
            ..AutomationState::default()
        };
        let snapshot = Snapshot {
            now: 0,
            zones: vec![
                zone(1, ZoneControl::Owned(OwnedOverlay::Off)),
                zone(2, ZoneControl::Manual),
                zone(3, ZoneControl::BoostEnded { then_off: true }),
            ],
            outside_celsius: None,
            presence: None,
            phones: None,
        };

        // Act
        let reconciled = reconcile(&state, &snapshot);

        // Assert
        let owned: Vec<i32> = reconciled.owned_overlays.keys().copied().collect();
        assert_eq!(owned, vec![1]);
        assert_eq!(state.owned_overlays.len(), 3, "input state is not mutated");
    }

    #[test]
    fn record_applied_tracks_owned_overlays() {
        // Arrange
        let state = AutomationState::default();
        let boost = Action::Boost {
            zone: 4,
            celsius: 21.0,
            minutes: 60,
            then_off: false,
        };

        // Act
        let state = record_applied(&state, &Action::HeatingOff { zone: 1 }, 100);
        let state = record_applied(&state, &boost, 100);
        let state = record_applied(&state, &Action::ResumeSchedule { zone: 1 }, 200);

        // Assert
        assert_eq!(state.owned_overlays.get(&1), None);
        assert_eq!(
            state.owned_overlays.get(&4),
            Some(&OwnedOverlay::Boost {
                celsius: 21.0,
                until: 3_700,
                then_off: false
            })
        );
    }

    #[test]
    fn store_round_trips_and_tolerates_missing_or_corrupt_files() {
        // Arrange
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("automation.json");
        let store = StateStore::new(&path);
        let state = record_applied(
            &AutomationState::default(),
            &Action::HeatingOff { zone: 2 },
            0,
        );

        // Act
        let missing = store.load();
        store.save(&state).unwrap();
        let loaded = store.load();
        fs::write(&path, "garbage").unwrap();
        let corrupt = store.load();

        // Assert
        assert_eq!(missing, AutomationState::default());
        assert_eq!(loaded, state);
        assert_eq!(corrupt, AutomationState::default());
    }
}
