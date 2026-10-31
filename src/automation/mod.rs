//! Rule-based automation of tado° thermostats.
//!
//! Every tick the exporter builds a [`Snapshot`] from the data it already fetched, the
//! [`engine`] asks each enabled rule for [`Action`]s, and [`executor`] applies them (or only
//! logs them in dry-run mode). Rules are pure functions of the snapshot and their own
//! persisted state, so they are unit-tested without the API.
//!
//! Manual settings always win: rules only act on zones that follow their schedule or carry an
//! overlay this exporter applied itself (tracked in [`state::AutomationState`]).

pub mod config;
pub mod engine;
pub mod executor;
pub mod rules;
pub mod state;

use serde_derive::{Deserialize, Serialize};

use state::OwnedOverlay;

/// Seconds since the Unix epoch.
pub type Timestamp = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Presence {
    Home,
    Away,
}

impl Presence {
    /// The value tado° uses for this presence.
    pub fn as_api_str(self) -> &'static str {
        match self {
            Presence::Home => "HOME",
            Presence::Away => "AWAY",
        }
    }

    pub fn from_api_str(value: &str) -> Option<Presence> {
        match value {
            "HOME" => Some(Presence::Home),
            "AWAY" => Some(Presence::Away),
            _ => None,
        }
    }
}

/// Who currently controls a zone's heating.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZoneControl {
    /// No overlay: the zone follows its schedule.
    Schedule,
    /// An overlay this exporter applied and still owns.
    Owned(OwnedOverlay),
    /// An overlay set by someone else (app, thermostat buttons). Never touched.
    Manual,
    /// The zone follows its schedule again because a boost this exporter applied has ended.
    BoostEnded { then_off: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZoneSnapshot {
    pub id: i32,
    pub name: String,
    pub control: ZoneControl,
    /// Target temperature currently applied, `None` when heating is off.
    pub setting_celsius: Option<f64>,
    pub humidity: Option<f64>,
    /// tado° detected an open window that is not active yet.
    pub open_window_detected: bool,
    /// Open-window mode is currently active.
    pub open_window_active: bool,
}

/// Location of a phone that has geofencing enabled and shares its location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhoneLocation {
    pub at_home: bool,
}

/// Everything the rules may base a decision on, taken once per tick.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub now: Timestamp,
    pub zones: Vec<ZoneSnapshot>,
    pub outside_celsius: Option<f64>,
    /// Current home presence, `None` if unknown.
    pub presence: Option<Presence>,
    /// Located phones, `None` when they were not fetched this tick.
    pub phones: Option<Vec<PhoneLocation>>,
}

impl Snapshot {
    pub fn zone(&self, id: i32) -> Option<&ZoneSnapshot> {
        self.zones.iter().find(|zone| zone.id == id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Confirm a detected open window, pausing heating for the zone's timeout.
    ActivateOpenWindow {
        zone: i32,
    },
    /// Turn the zone's heating off until changed again (an owned overlay).
    HeatingOff {
        zone: i32,
    },
    /// Heat the zone for a while (an owned timer overlay); `then_off` restores heating off
    /// afterwards because the boost replaced an owned "off".
    Boost {
        zone: i32,
        celsius: f64,
        minutes: u64,
        then_off: bool,
    },
    /// Remove an owned overlay so the zone follows its schedule.
    ResumeSchedule {
        zone: i32,
    },
    SetPresence(Presence),
}

impl Action {
    /// Stable name used in logs and metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Action::ActivateOpenWindow { .. } => "activate_open_window",
            Action::HeatingOff { .. } => "heating_off",
            Action::Boost { .. } => "boost",
            Action::ResumeSchedule { .. } => "resume_schedule",
            Action::SetPresence(_) => "set_presence",
        }
    }

    pub fn zone(&self) -> Option<i32> {
        match self {
            Action::ActivateOpenWindow { zone }
            | Action::HeatingOff { zone }
            | Action::Boost { zone, .. }
            | Action::ResumeSchedule { zone } => Some(*zone),
            Action::SetPresence(_) => None,
        }
    }
}

/// An action together with the rule that asked for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub rule: &'static str,
    pub action: Action,
}
