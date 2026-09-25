pub(crate) mod about_window;
pub(crate) mod calibrate_window;
pub(crate) mod control_panel;
pub(crate) mod history_window;
pub(crate) mod live_data;
pub(crate) mod log_window;
pub(crate) mod plot;
pub(crate) mod recipe_panel;
pub(crate) mod usb_panel;

pub(crate) fn format_duration(total_seconds: f64) -> String {
    let total_seconds = total_seconds as u64;
    let h = total_seconds / 3600;
    let m = (total_seconds % 3600) / 60;
    let s = total_seconds % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

pub(crate) fn format_timestamp(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value).map_or_else(
        |_| value.to_owned(),
        |timestamp| {
            timestamp
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string()
        },
    )
}

pub(crate) fn format_test_state(state: &crate::core::TestState) -> &'static str {
    use crate::core::TestState;
    match state {
        TestState::Idle => "Idle",
        TestState::Starting => "Starting",
        TestState::Running => "Running",
        TestState::Stopping => "Stopping",
        TestState::Stopped => "Stopped",
        TestState::Completed => "Completed",
        TestState::RecoveredUncertain => "State uncertain",
    }
}

pub(crate) fn format_cycle_state(state: crate::core::CycleState) -> &'static str {
    use crate::core::CycleState;
    match state {
        CycleState::Idle => "Idle",
        CycleState::Preparing => "Preparing",
        CycleState::StartingStep => "Starting step",
        CycleState::RunningStep => "Running step",
        CycleState::Settling => "Settling",
        CycleState::Resting => "Resting",
        CycleState::Stopping => "Stopping",
        CycleState::Completed => "Completed",
        CycleState::Stopped => "Stopped",
        CycleState::Interrupted => "Interrupted",
    }
}

#[cfg(test)]
mod test {
    use super::{format_cycle_state, format_duration, format_test_state, format_timestamp};

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0.0), "00:00");
        assert_eq!(format_duration(59.0), "00:59");
        assert_eq!(format_duration(60.0), "01:00");
        assert_eq!(format_duration(61.0), "01:01");
        assert_eq!(format_duration(3599.0), "59:59");
        assert_eq!(format_duration(3600.0), "1:00:00");
        assert_eq!(format_duration(3661.0), "1:01:01");
    }

    #[test]
    fn history_timestamps_and_states_are_readable_without_changing_source_data() {
        assert_eq!(
            format_timestamp("2026-09-24T22:15:03+02:00"),
            "2026-09-24 20:15:03 UTC"
        );
        assert_eq!(format_timestamp("legacy-value"), "legacy-value");
        assert_eq!(
            format_cycle_state(crate::core::CycleState::RunningStep),
            "Running step"
        );
        assert_eq!(
            format_test_state(&crate::core::TestState::RecoveredUncertain),
            "State uncertain"
        );
    }
}
