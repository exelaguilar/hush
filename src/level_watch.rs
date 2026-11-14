//! Watches the raw microphone level reported by the plugin and decides
//! whether to warn that it is clipping or too quiet for Hush to work well.
//! Pure logic (time is passed in) so it can be unit tested.

use hush_apo::settings::LiveLevels;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Raw peak at or above this counts as clipping (about -0.2 dBFS).
const CLIP_PEAK: f32 = 0.98;
/// Keep the clipping warning this long after the last clipped peak.
const CLIP_HOLD: Duration = Duration::from_secs(5);
/// Speech whose loudest peak stays below this is too quiet (about -30 dBFS).
const QUIET_PEAK: f32 = 0.0316;
/// Once shown, the quiet warning clears only above this (hysteresis).
const QUIET_CLEAR_PEAK: f32 = 0.045;
/// Judge over this much recent speech.
const SPEECH_WINDOW: f32 = 5.0;
/// Do not judge before this much speech was heard.
const MIN_SPEECH: f32 = 2.0;
/// Speech is accumulated in buckets of this length.
const BUCKET: f32 = 0.5;
/// Gaps between updates longer than this are not counted as speech.
const MAX_STEP: f32 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelWarning {
    Clipping,
    TooQuiet,
}

#[derive(Debug, Default)]
pub struct LevelWatch {
    last_clip: Option<Instant>,
    last_update: Option<Instant>,
    /// Finished buckets of speech: (seconds of speech, loudest peak).
    buckets: VecDeque<(f32, f32)>,
    current: (f32, f32),
    quiet_shown: bool,
}

impl LevelWatch {
    /// Feed the latest live levels (`None` when they are stale or missing)
    /// and get the warning to show, if any.
    pub fn update(&mut self, levels: Option<LiveLevels>, now: Instant) -> Option<LevelWarning> {
        let Some(levels) = levels else {
            *self = Self::default();
            return None;
        };
        let step = self
            .last_update
            .map_or(0.0, |last| now.saturating_duration_since(last).as_secs_f32())
            .min(MAX_STEP);
        self.last_update = Some(now);

        if levels.input >= CLIP_PEAK {
            self.last_clip = Some(now);
        }
        if levels.voice {
            self.current.0 += step;
            self.current.1 = self.current.1.max(levels.input);
            if self.current.0 >= BUCKET {
                self.buckets.push_back(std::mem::take(&mut self.current));
                while self.buckets.len() as f32 * BUCKET > SPEECH_WINDOW {
                    self.buckets.pop_front();
                }
            }
        }

        if self
            .last_clip
            .is_some_and(|at| now.saturating_duration_since(at) < CLIP_HOLD)
        {
            return Some(LevelWarning::Clipping);
        }

        let speech: f32 = self.buckets.iter().map(|b| b.0).sum::<f32>() + self.current.0;
        let loudest = self
            .buckets
            .iter()
            .map(|b| b.1)
            .fold(self.current.1, f32::max);
        if speech >= MIN_SPEECH {
            let limit = if self.quiet_shown {
                QUIET_CLEAR_PEAK
            } else {
                QUIET_PEAK
            };
            self.quiet_shown = loudest < limit;
        }
        self.quiet_shown.then_some(LevelWarning::TooQuiet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(input: f32, voice: bool) -> Option<LiveLevels> {
        Some(LiveLevels {
            input,
            output: input,
            voice,
            updated_ms: 0,
        })
    }

    /// Feeds `seconds` of constant levels in 100 ms steps.
    fn run(
        watch: &mut LevelWatch,
        start: Instant,
        from: f32,
        seconds: f32,
        input: f32,
        voice: bool,
    ) -> Option<LevelWarning> {
        let mut result = None;
        let steps = (seconds * 10.0) as u32;
        for i in 1..=steps {
            let at = start + Duration::from_secs_f32(from + i as f32 * 0.1);
            result = watch.update(levels(input, voice), at);
        }
        result
    }

    #[test]
    fn clipping_triggers_and_clears_after_hold() {
        let mut watch = LevelWatch::default();
        let t0 = Instant::now();
        assert_eq!(watch.update(levels(0.2, true), t0), None);
        let at = t0 + Duration::from_millis(100);
        assert_eq!(watch.update(levels(0.99, true), at), Some(LevelWarning::Clipping));
        // Still shown shortly after, even though the signal is normal again.
        assert_eq!(
            run(&mut watch, t0, 0.1, 3.0, 0.3, true),
            Some(LevelWarning::Clipping)
        );
        // Gone once the hold time passed.
        assert_eq!(run(&mut watch, t0, 3.1, 3.0, 0.3, true), None);
    }

    #[test]
    fn quiet_is_only_judged_while_speaking() {
        let mut watch = LevelWatch::default();
        let t0 = Instant::now();
        // A long time of quiet room noise (gate closed) is not "too quiet".
        assert_eq!(run(&mut watch, t0, 0.0, 10.0, 0.005, false), None);
        // Quiet speech: no verdict before enough speech was heard ...
        assert_eq!(run(&mut watch, t0, 10.0, 1.0, 0.01, true), None);
        // ... then the warning appears.
        assert_eq!(
            run(&mut watch, t0, 11.0, 2.0, 0.01, true),
            Some(LevelWarning::TooQuiet)
        );
        // Louder speech clears it (hysteresis: it needs to clearly exceed the limit).
        assert_eq!(run(&mut watch, t0, 13.0, 6.0, 0.2, true), None);
    }

    #[test]
    fn normal_speech_gives_no_warning() {
        let mut watch = LevelWatch::default();
        let t0 = Instant::now();
        assert_eq!(run(&mut watch, t0, 0.0, 8.0, 0.25, true), None);
        assert_eq!(run(&mut watch, t0, 8.0, 2.0, 0.01, false), None);
    }

    #[test]
    fn stale_levels_reset_the_state() {
        let mut watch = LevelWatch::default();
        let t0 = Instant::now();
        assert!(watch.update(levels(1.0, true), t0).is_some());
        assert_eq!(watch.update(None, t0 + Duration::from_secs(1)), None);
        assert_eq!(watch.update(levels(0.2, true), t0 + Duration::from_secs(2)), None);
    }
}
