use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceMode {
    Full,
    #[default]
    Balanced,
    Saver,
}
impl PerformanceMode {
    pub fn background_fps(self) -> u32 {
        match self {
            Self::Full => 0,
            Self::Balanced => 12,
            Self::Saver => 6,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderPolicy {
    pub visible: bool,
    pub fps: u32,
}
pub fn policy(
    mode: PerformanceMode,
    selected: bool,
    running: bool,
    recording: bool,
    recent_input: bool,
) -> RenderPolicy {
    RenderPolicy {
        visible: mode == PerformanceMode::Full || selected || running || recording || recent_input,
        // Game logic may advance on each animation frame. Do not change its clock
        // when a running profile moves behind another one.
        fps: if selected || running || recording {
            0
        } else {
            mode.background_fps()
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn macro_tabs_stay_visible_for_input_acknowledgements() {
        assert_eq!(
            policy(PerformanceMode::Balanced, false, true, false, false),
            RenderPolicy {
                visible: true,
                fps: 0
            }
        );
    }
    #[test]
    fn idle_background_tabs_are_hidden() {
        assert!(!policy(PerformanceMode::Balanced, false, false, false, false).visible);
    }
    #[test]
    fn running_game_timing_is_independent_of_selection_and_mode() {
        for mode in [
            PerformanceMode::Full,
            PerformanceMode::Balanced,
            PerformanceMode::Saver,
        ] {
            assert_eq!(
                policy(mode, false, true, false, false),
                policy(mode, true, true, false, false)
            );
        }
    }
    #[test]
    fn foreground_recording_and_release_grace_are_preserved() {
        assert_eq!(
            policy(PerformanceMode::Saver, true, false, false, false).fps,
            0
        );
        assert_eq!(
            policy(PerformanceMode::Saver, false, false, true, false).fps,
            0
        );
        assert!(policy(PerformanceMode::Saver, false, false, false, true).visible);
    }
    #[test]
    fn full_mode_restores_previous_rendering_behavior() {
        assert_eq!(
            policy(PerformanceMode::Full, false, false, false, false),
            RenderPolicy {
                visible: true,
                fps: 0
            }
        );
    }
}
