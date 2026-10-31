//! Automation rules. Each rule is a pure function of its config, the tick's snapshot and its
//! own persisted state, returning the actions to take and its next state.

pub mod humidity;
pub mod open_window;
pub mod presence;
pub mod season;
