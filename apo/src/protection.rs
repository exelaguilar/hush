//! Clean only the raw protection blend. The model's complex enhanced signal
//! remains intact. The correction uses the same STFT and lookahead as libDF.

use df::{Complex32, DFState};
use std::collections::VecDeque;

pub(crate) struct ProtectionCleaner {
    analysis: DFState,
    synthesis: DFState,
    spectra: VecDeque<(Vec<Complex32>, bool)>,
    noise: Vec<f32>,
    power: Vec<f32>,
    gains: Vec<f32>,
    scratch: Vec<Complex32>,
    correction: Vec<f32>,
    learned: u32,
    amount: f32,
}

impl ProtectionCleaner {
    pub(crate) fn new(state: &DFState, lookahead: usize, amount: f32) -> Self {
        Self {
            analysis: state.clone(),
            synthesis: state.clone(),
            spectra: (0..=lookahead)
                .map(|_| (vec![Complex32::default(); state.freq_size], false))
                .collect(),
            noise: vec![0.0; state.erb.len()],
            power: vec![0.0; state.erb.len()],
            gains: vec![1.0; state.erb.len()],
            scratch: vec![Complex32::default(); state.freq_size],
            correction: vec![0.0; state.frame_size],
            learned: 0,
            amount,
        }
    }

    pub(crate) fn process(&mut self, input: &[f32], output: &mut [f32], limit: f32, learn: bool) {
        let (mut latest, _) = self.spectra.pop_front().unwrap();
        self.analysis.analysis(input, &mut latest);
        self.spectra.push_back((latest, learn));
        let (noisy, learn) = self.spectra.front().unwrap();
        if limit <= 0.5 {
            // Bypass must not retain a correction from the preceding frame.
            self.gains.fill(1.0);
            self.synthesis.reset();
            return;
        }
        let ready = self.learned >= 20;
        let mut offset = 0;
        for (band, &width) in self.analysis.erb.iter().enumerate() {
            let power = noisy[offset..offset + width]
                .iter()
                .map(|x| x.norm_sqr())
                .sum::<f32>()
                / width as f32;
            self.power[band] += 0.25 * (power - self.power[band]);
            if *learn {
                let coefficient = if self.learned == 0 { 1.0 } else { 0.02 };
                self.noise[band] += coefficient * (power - self.noise[band]);
            }
            let target = if ready {
                // Smooth power subtraction, bounded to -12 dB on the backup.
                (1.0 - self.noise[band] / self.power[band].max(1e-20))
                    .max(0.0)
                    .sqrt()
                    .max(0.251_188_64)
            } else {
                1.0
            };
            // Restore consonants quickly; reduce stationary noise gradually.
            let coefficient = if target > self.gains[band] {
                0.65
            } else {
                0.08
            };
            self.gains[band] += coefficient * (target - self.gains[band]);
            offset += width;
        }
        if *learn {
            self.learned = self.learned.saturating_add(1);
        }
        if !ready {
            return;
        }
        self.scratch.copy_from_slice(noisy);
        self.analysis.apply_mask(&mut self.scratch, &self.gains);
        let dry = 10.0_f32.powf(-limit / 20.0) * self.amount;
        for (correction, &original) in self.scratch.iter_mut().zip(noisy) {
            *correction = (*correction - original) * dry;
        }
        self.synthesis
            .synthesis(&mut self.scratch, &mut self.correction);
        for (sample, &correction) in output.iter_mut().zip(&self.correction) {
            *sample += correction;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::denoise::HOP;

    fn tone(frame: usize, level: f32) -> Vec<f32> {
        (0..HOP)
            .map(|i| level * ((frame * HOP + i) as f32 * 0.073).sin())
            .collect()
    }

    #[test]
    fn unknown_profile_leaves_the_enhanced_output_untouched() {
        let mut cleaner = ProtectionCleaner::new(&DFState::default(), 2, 1.0);
        for frame in 0..100 {
            let mut enhanced = tone(frame, 0.03);
            let original = enhanced.clone();
            cleaner.process(&tone(frame, 0.05), &mut enhanced, 6.0, false);
            assert_eq!(enhanced, original);
        }
    }

    #[test]
    fn correction_matches_libdf_overlap_and_lookahead() {
        for lookahead in [1, 2] {
            let mut cleaner = ProtectionCleaner::new(&DFState::default(), lookahead, 0.65);
            cleaner.learned = 20;
            cleaner.noise.fill(1e10);
            cleaner.gains.fill(0.251_188_64);
            let mut input = Vec::new();
            let mut output = Vec::new();
            for frame in 0..20 {
                let x = tone(frame, 0.05);
                let mut y = vec![0.0; HOP];
                cleaner.process(&x, &mut y, 6.0, false);
                input.extend(x);
                output.extend(y);
            }
            let scale = (0.251_188_64 - 1.0) * 10.0_f32.powf(-6.0 / 20.0) * 0.65;
            let delay = (lookahead + 1) * HOP;
            for (&x, &y) in input.iter().zip(&output[delay..]) {
                assert!((y - scale * x).abs() < 1e-6, "correction is misaligned");
            }
        }
    }

    #[test]
    fn speech_cannot_update_the_noise_profile() {
        let mut cleaner = ProtectionCleaner::new(&DFState::default(), 2, 1.0);
        for frame in 0..80 {
            cleaner.process(&tone(frame, 0.001), &mut vec![0.0; HOP], 6.0, true);
        }
        // Drain the pending learning decisions before the comparison.
        for frame in 80..83 {
            cleaner.process(&tone(frame, 0.01), &mut vec![0.0; HOP], 6.0, false);
        }
        let profile = cleaner.noise.clone();
        for frame in 83..150 {
            cleaner.process(&tone(frame, 0.05), &mut vec![0.0; HOP], 6.0, false);
        }
        assert_eq!(cleaner.noise, profile);
    }

    #[test]
    fn bypass_discards_the_correction_tail_immediately() {
        let mut cleaner = ProtectionCleaner::new(&DFState::default(), 2, 1.0);
        cleaner.learned = 20;
        cleaner.noise.fill(1e10);
        for frame in 0..30 {
            cleaner.process(&tone(frame, 0.05), &mut vec![0.0; HOP], 6.0, false);
        }
        let mut enhanced = tone(30, 0.05);
        let original = enhanced.clone();
        cleaner.process(&tone(30, 0.05), &mut enhanced, 0.5, false);
        assert_eq!(enhanced, original);
    }
}
