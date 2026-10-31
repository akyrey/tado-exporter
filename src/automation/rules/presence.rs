//! Home/away from phone locations, as Auto-Assist geofencing would.
//!
//! AWAY once every located phone has been away for `away_polls` consecutive ticks; HOME as
//! soon as one is home. The rule acts only when the phone-derived presence *changes*, so a
//! presence set by hand in the app is kept until the phones disagree with it again. The first
//! observation (fresh state, or after switching dry-run/live) is adopted without acting.

use serde_derive::{Deserialize, Serialize};

use crate::automation::config::{parse_bool, parse_or};
use crate::automation::{Action, PhoneLocation, Presence, Snapshot};

pub const NAME: &str = "presence";

#[derive(Debug, Clone, PartialEq)]
pub struct PresenceConfig {
    pub enabled: bool,
    /// Consecutive ticks with every phone away before switching to AWAY.
    pub away_polls: u32,
}

impl PresenceConfig {
    pub fn from_lookup(lookup: &impl Fn(&str) -> Option<String>) -> PresenceConfig {
        let config = PresenceConfig {
            enabled: parse_bool(lookup, "AUTOMATION_PRESENCE_ENABLED", false),
            away_polls: parse_or(lookup, "AUTOMATION_PRESENCE_AWAY_POLLS", 2),
        };

        assert!(
            config.away_polls > 0,
            "AUTOMATION_PRESENCE_AWAY_POLLS must be positive"
        );

        config
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PresenceState {
    /// The last presence derived from the phones (and applied if it differed).
    pub confirmed: Option<Presence>,
    /// Consecutive ticks in which every located phone was away.
    pub away_streak: u32,
}

pub fn evaluate(
    config: &PresenceConfig,
    snapshot: &Snapshot,
    state: &PresenceState,
) -> (Vec<Action>, PresenceState) {
    // Phones not fetched or none sharing a location: nothing to base a decision on, and the
    // away streak must restart, as the polls around the gap are not consecutive.
    let observed = match snapshot.phones.as_deref().and_then(observed_presence) {
        Some(observed) => observed,
        None => {
            let next = PresenceState {
                away_streak: 0,
                ..state.clone()
            };
            return (Vec::new(), next);
        }
    };

    let away_streak = match observed {
        Presence::Away => state.away_streak.saturating_add(1),
        Presence::Home => 0,
    };
    let confirmed = match observed {
        Presence::Home => Some(Presence::Home),
        Presence::Away if away_streak >= config.away_polls => Some(Presence::Away),
        Presence::Away => state.confirmed,
    };

    let changed = state.confirmed.is_some() && confirmed != state.confirmed;
    let actions = match confirmed {
        Some(presence) if changed && snapshot.presence != Some(presence) => {
            vec![Action::SetPresence(presence)]
        }
        _ => Vec::new(),
    };

    (
        actions,
        PresenceState {
            confirmed,
            away_streak,
        },
    )
}

fn observed_presence(phones: &[PhoneLocation]) -> Option<Presence> {
    if phones.is_empty() {
        return None;
    }
    if phones.iter().any(|phone| phone.at_home) {
        Some(Presence::Home)
    } else {
        Some(Presence::Away)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::automation::config::tests::lookup;

    fn config() -> PresenceConfig {
        PresenceConfig::from_lookup(&lookup(&[("AUTOMATION_PRESENCE_ENABLED", "true")]))
    }

    fn snapshot(current: Presence, phones: Option<Vec<bool>>) -> Snapshot {
        Snapshot {
            now: 0,
            zones: vec![],
            outside_celsius: None,
            presence: Some(current),
            phones: phones.map(|p| {
                p.into_iter()
                    .map(|at_home| PhoneLocation { at_home })
                    .collect()
            }),
        }
    }

    fn confirmed(presence: Presence) -> PresenceState {
        PresenceState {
            confirmed: Some(presence),
            away_streak: 0,
        }
    }

    #[test]
    fn switches_away_after_consecutive_polls() {
        // Arrange
        let everyone_left = snapshot(Presence::Home, Some(vec![false, false]));

        // Act
        let (first, state) = evaluate(&config(), &everyone_left, &confirmed(Presence::Home));
        let (second, state) = evaluate(&config(), &everyone_left, &state);
        let (third, _) = evaluate(&config(), &everyone_left, &state);

        // Assert
        assert!(first.is_empty(), "a single poll could be a GPS glitch");
        assert_eq!(second, vec![Action::SetPresence(Presence::Away)]);
        assert!(third.is_empty(), "acts only once per change");
    }

    #[test]
    fn switches_home_as_soon_as_one_phone_returns() {
        // Act
        let (actions, state) = evaluate(
            &config(),
            &snapshot(Presence::Away, Some(vec![false, true])),
            &confirmed(Presence::Away),
        );

        // Assert
        assert_eq!(actions, vec![Action::SetPresence(Presence::Home)]);
        assert_eq!(state.away_streak, 0);
    }

    #[test]
    fn respects_manual_presence_until_phones_change() {
        // Arrange: phones are away and confirmed, but someone set HOME by hand.
        let state = PresenceState {
            confirmed: Some(Presence::Away),
            away_streak: 5,
        };

        // Act
        let (actions, _) = evaluate(
            &config(),
            &snapshot(Presence::Home, Some(vec![false])),
            &state,
        );

        // Assert
        assert!(actions.is_empty());
    }

    #[test]
    fn does_not_act_when_presence_already_matches() {
        // Act
        let (actions, state) = evaluate(
            &config(),
            &snapshot(Presence::Home, Some(vec![true])),
            &PresenceState::default(),
        );

        // Assert
        assert!(actions.is_empty());
        assert_eq!(state.confirmed, Some(Presence::Home));
    }

    #[test]
    fn keeps_confirmed_presence_but_restarts_streak_without_located_phones() {
        // Arrange
        let state = PresenceState {
            confirmed: Some(Presence::Home),
            away_streak: 1,
        };
        let expected = PresenceState {
            confirmed: Some(Presence::Home),
            away_streak: 0,
        };

        // Act
        let (not_fetched, kept) = evaluate(&config(), &snapshot(Presence::Home, None), &state);
        let (none_located, kept_again) =
            evaluate(&config(), &snapshot(Presence::Home, Some(vec![])), &state);

        // Assert
        assert!(not_fetched.is_empty() && none_located.is_empty());
        assert_eq!(kept, expected);
        assert_eq!(kept_again, expected);
    }

    #[test]
    fn adopts_first_observation_without_overriding_manual_presence() {
        // Arrange: fresh state, phones home, but someone set AWAY by hand.
        let phones_home = snapshot(Presence::Away, Some(vec![true]));

        // Act
        let (actions, state) = evaluate(&config(), &phones_home, &PresenceState::default());

        // Assert
        assert!(actions.is_empty());
        assert_eq!(state.confirmed, Some(Presence::Home));
    }
}
