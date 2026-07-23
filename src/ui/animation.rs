use std::time::Duration;

/// Durations for the app's small motion budget. UI animations stay under 300ms;
/// the spinner breathes slower so it does not feel frantic.
pub const ENTER_DURATION: Duration = Duration::from_millis(200);
pub const PANEL_DURATION: Duration = Duration::from_millis(180);
pub const DIALOG_DURATION: Duration = Duration::from_millis(220);
pub const SPINNER_PERIOD: Duration = Duration::from_millis(1600);
