//! Persisted automation state: which overlays the exporter owns, plus each rule's memory.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::PathBuf;

use log::warn;
use serde_derive::{Deserialize, Serialize};

use super::rules::humidity::{self, HumidityState};
use super::rules::open_window::{self, OpenWindowState};
use super::rules::presence::{self, PresenceState};
use super::rules::season::{self, SeasonState};
use super::{Action, Snapshot, Timestamp, ZoneControl};
use crate::persist::write_private_atomic;

/// Tolerance when comparing a zone's target temperature with the one an owned boost set.
/// Boost temperatures are validated to tado°'s 0.5°C steps, so this only absorbs rounding.
const CELSIUS_TOLERANCE: f64 = 0.01;
/// A boost overlay that disappears this close to its end counts as expired, not cancelled.
const BOOST_END_SLACK_SECONDS: u64 = 120;

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
    pub humidity: HumidityState,
    pub presence: PresenceState,
    /// The mode the rule state was built in. Edge-triggered rule memory recorded in dry-run
    /// must not suppress the first real action after switching to live mode (and vice versa).
    pub dry_run: Option<bool>,
}

impl AutomationState {
    /// Returns this state ready for `dry_run` mode: if it was recorded in the other mode, the
    /// edge-triggered memory (season, presence, handled windows) is reset.
    pub fn for_mode(&self, dry_run: bool) -> AutomationState {
        if self.dry_run == Some(dry_run) {
            return self.clone();
        }

        AutomationState {
            open_window: OpenWindowState::default(),
            season: SeasonState {
                season: None,
                ..self.season.clone()
            },
            presence: PresenceState::default(),
            dry_run: Some(dry_run),
            ..self.clone()
        }
    }

    /// Takes the given rules' state from `previous`, e.g. because their action was not applied
    /// and must be decided again next tick.
    pub fn with_rules_from(
        &self,
        previous: &AutomationState,
        rules: &BTreeSet<&str>,
    ) -> AutomationState {
        let pick = |rule: &str| rules.contains(rule);

        AutomationState {
            open_window: if pick(open_window::NAME) {
                previous.open_window.clone()
            } else {
                self.open_window.clone()
            },
            humidity: if pick(humidity::NAME) {
                previous.humidity.clone()
            } else {
                self.humidity.clone()
            },
            season: if pick(season::NAME) {
                previous.season.clone()
            } else {
                self.season.clone()
            },
            presence: if pick(presence::NAME) {
                previous.presence.clone()
            } else {
                self.presence.clone()
            },
            ..self.clone()
        }
    }
}

/// Decides who controls a zone from its current overlay and what this exporter owns.
pub fn zone_control(
    owned: Option<OwnedOverlay>,
    overlay: Option<&OverlayInfo>,
    now: Timestamp,
) -> ZoneControl {
    match (owned, overlay) {
        (
            Some(OwnedOverlay::Boost {
                then_off, until, ..
            }),
            None,
        ) if now.saturating_add(BOOST_END_SLACK_SECONDS) >= until => {
            ZoneControl::BoostEnded { then_off }
        }
        // No overlay and nothing owned, or an owned boost cancelled early (by hand).
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
                    .is_some_and(|c| (c - celsius).abs() < CELSIUS_TOLERANCE)
        }
    }
}

/// Forgets owned overlays that are gone or were replaced by someone else.
///
/// Kept are overlays still in place, zones missing from this snapshot (a partial response is
/// no proof the overlay is gone), and ended boosts in `pending_restore` whose follow-up action
/// was not applied yet, so it is retried next tick.
pub fn reconcile(
    state: &AutomationState,
    snapshot: &Snapshot,
    pending_restore: &BTreeSet<i32>,
) -> AutomationState {
    let owned_overlays = state
        .owned_overlays
        .iter()
        .filter(
            |(id, _)| match snapshot.zone(**id).map(|zone| zone.control) {
                None | Some(ZoneControl::Owned(_)) => true,
                Some(ZoneControl::BoostEnded { .. }) => pending_restore.contains(id),
                Some(ZoneControl::Schedule) | Some(ZoneControl::Manual) => false,
            },
        )
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
                until: now.saturating_add(minutes.saturating_mul(60)),
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

    const NOW: Timestamp = 10_000;

    const BOOST: OwnedOverlay = OwnedOverlay::Boost {
        celsius: 21.0,
        until: NOW - 30,
        then_off: true,
    };

    #[rstest]
    #[case::schedule(None, None, ZoneControl::Schedule)]
    #[case::boost_cancelled_early(
        Some(OwnedOverlay::Boost { celsius: 21.0, until: NOW + 1_800, then_off: true }),
        None,
        ZoneControl::Schedule
    )]
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
        assert_eq!(zone_control(owned, current.as_ref(), NOW), expected);
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
        let owned = [
            (1, OwnedOverlay::Off),
            (2, OwnedOverlay::Off),
            (3, BOOST),
            (4, BOOST),
            (5, OwnedOverlay::Off),
        ];
        let state = AutomationState {
            owned_overlays: owned.iter().copied().collect(),
            ..AutomationState::default()
        };
        let snapshot = Snapshot {
            now: NOW,
            zones: vec![
                zone(1, ZoneControl::Owned(OwnedOverlay::Off)),
                zone(2, ZoneControl::Manual),
                zone(3, ZoneControl::BoostEnded { then_off: true }),
                zone(4, ZoneControl::BoostEnded { then_off: true }),
                // zone 5 is missing from this (partial) snapshot
            ],
            outside_celsius: None,
            presence: None,
            phones: None,
        };
        let pending_restore: BTreeSet<i32> = [4].iter().copied().collect();

        // Act
        let reconciled = reconcile(&state, &snapshot, &pending_restore);

        // Assert
        let kept: Vec<i32> = reconciled.owned_overlays.keys().copied().collect();
        assert_eq!(kept, vec![1, 4, 5]);
        assert_eq!(state.owned_overlays.len(), 5, "input state is not mutated");
    }

    #[test]
    fn switching_mode_resets_edge_memory_only() {
        // Arrange
        let mut state = AutomationState {
            owned_overlays: [(1, OwnedOverlay::Off)].iter().copied().collect(),
            dry_run: Some(true),
            ..AutomationState::default()
        };
        state.season.season = Some(season::Season::Off);
        state.season.samples = vec![season::Sample {
            at: 1,
            celsius: 20.0,
        }];
        state.presence.confirmed = Some(crate::automation::Presence::Away);
        state.open_window.handled.insert(2);

        // Act
        let same_mode = state.for_mode(true);
        let live = state.for_mode(false);

        // Assert
        assert_eq!(same_mode, state);
        assert_eq!(live.dry_run, Some(false));
        assert_eq!(live.season.season, None);
        assert_eq!(live.season.samples.len(), 1, "temperature history is kept");
        assert_eq!(live.presence, PresenceState::default());
        assert!(live.open_window.handled.is_empty());
        assert_eq!(live.owned_overlays, state.owned_overlays);
    }

    #[test]
    fn with_rules_from_restores_only_named_rules() {
        // Arrange
        let previous = AutomationState::default();
        let mut next = AutomationState::default();
        next.open_window.handled.insert(1);
        next.presence.away_streak = 3;
        let rules: BTreeSet<&str> = [open_window::NAME].iter().copied().collect();

        // Act
        let restored = next.with_rules_from(&previous, &rules);

        // Assert
        assert!(restored.open_window.handled.is_empty());
        assert_eq!(restored.presence.away_streak, 3);
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
