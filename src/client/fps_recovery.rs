const MAX_RECOVERY_STEP: usize = 5;
const MIN_ACTIVE_FPS: usize = 30;

pub(crate) fn state_after_manual_fps_update(previous: Option<usize>) -> Option<usize> {
    previous
}

pub(crate) fn prioritize_active_fps(
    measured: usize,
    requested: usize,
    _under_pressure: bool,
) -> usize {
    measured.max(requested.min(MIN_ACTIVE_FPS))
}

pub(crate) fn next_auto_fps(
    proposed: usize,
    previous: Option<usize>,
    under_pressure: bool,
) -> usize {
    match previous {
        Some(previous) if under_pressure => proposed.min(previous),
        Some(previous) => proposed.min(
            previous
                .saturating_add(MAX_RECOVERY_STEP)
                .max(proposed.min(MIN_ACTIVE_FPS)),
        ),
        None => proposed,
    }
}

#[cfg(test)]
mod tests {
    use super::{next_auto_fps, prioritize_active_fps, state_after_manual_fps_update};

    #[test]
    fn manual_fps_update_preserves_last_automatic_limit() {
        assert_eq!(state_after_manual_fps_update(Some(16)), Some(16));
        assert_eq!(state_after_manual_fps_update(None), None);
    }

    #[test]
    fn recovered_decode_capacity_does_not_cause_a_large_fps_jump() {
        assert_eq!(next_auto_fps(48, Some(30), false), 35);
        assert_eq!(next_auto_fps(48, Some(46), false), 48);
    }

    #[test]
    fn active_video_recovers_to_thirty_fps_before_resuming_gradual_growth() {
        assert_eq!(next_auto_fps(48, Some(10), false), 30);
        assert_eq!(next_auto_fps(48, Some(30), false), 35);
    }

    #[test]
    fn active_video_enforces_thirty_fps_even_under_pressure() {
        assert_eq!(prioritize_active_fps(16, 60, false), 30);
        assert_eq!(prioritize_active_fps(16, 24, false), 24);
        assert_eq!(prioritize_active_fps(16, 60, true), 30);
    }

    #[test]
    fn decoder_pressure_can_lower_but_never_raise_the_previous_request() {
        assert_eq!(next_auto_fps(12, Some(48), true), 12);
        assert_eq!(next_auto_fps(30, Some(16), true), 16);
    }

    #[test]
    fn initial_request_can_use_the_measured_capacity() {
        assert_eq!(next_auto_fps(48, None, false), 48);
    }
}
