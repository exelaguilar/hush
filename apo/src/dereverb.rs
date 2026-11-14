//! Experimental late-reverberation post-filter for the model output.
//!
//! DeepFilterNet's training target keeps the direct sound, the first 5 ms
//! of reflections and a reverberant tail shortened to an RT60 of about
//! 0.5 s (libDF `RandReverbSim`), so a distant desk microphone keeps much
//! of the room. This filter estimates late reverberation with the
//! statistical exponential-decay model (Lebart, Boucher and Denbigh 2001;
//! Habets 2007): late power now = decay x power 50 ms ago, in
//! DeepFilterNet's own ERB bands, and applies a bounded spectral gain.
//!
//! It uses libDF's STFT, so it adds one hop (10 ms) of latency. It is not
//! part of the live chain; the offline renderer enables it for listening
//! tests.

use df::{Complex32, DFState};
use std::collections::VecDeque;

/// Late reverberation starts this many frames (10 ms) after the sound that
/// caused it; the direct sound and early reflections in between are kept.
const LATE_DELAY_FRAMES: usize = 5;
/// Most the filter may remove (-10 dB): it shortens tails rather than
/// gating them, which keeps weak syllables after loud ones audible.
const FLOOR: f32 = 0.316_227_77;

pub(crate) struct LateReverbFilter {
    state: DFState,
    spectrum: Vec<Complex32>,
    history: VecDeque<Vec<f32>>,
    gains: Vec<f32>,
    unity: Vec<f32>,
    decay: f32,
    scratch: Vec<f32>,
}

impl LateReverbFilter {
    /// `rt60`: assumed reverberation time of the room in seconds.
    pub(crate) fn new(state: &DFState, rt60: f32) -> Self {
        let bands = state.erb.len();
        let delay_s = (LATE_DELAY_FRAMES * state.frame_size) as f32 / state.sr.max(1) as f32;
        let mut state = state.clone();
        state.reset();
        Self {
            spectrum: vec![Complex32::default(); state.freq_size],
            history: (0..LATE_DELAY_FRAMES).map(|_| vec![0.0; bands]).collect(),
            gains: vec![1.0; bands],
            unity: vec![1.0; bands],
            // Energy falls 60 dB per RT60: 10^(-6 t / RT60) after t seconds.
            decay: 10.0_f32.powf(-6.0 * delay_s / rt60.clamp(0.1, 2.0)),
            scratch: vec![0.0; state.frame_size],
            state,
        }
    }

    /// Filters one hop in place; the output is delayed by one hop. When not
    /// `active` the signal still passes through the STFT (same latency).
    pub(crate) fn process(&mut self, frame: &mut [f32], active: bool) {
        self.state.analysis(frame, &mut self.spectrum);
        let mut past = self.history.pop_front().unwrap();
        let mut offset = 0;
        for (band, &width) in self.state.erb.iter().enumerate() {
            let now = self.spectrum[offset..offset + width]
                .iter()
                .map(|x| x.norm_sqr())
                .sum::<f32>()
                / width as f32;
            let late = self.decay * past[band];
            let target = (1.0 - late / now.max(1e-20)).max(0.0).sqrt().max(FLOOR);
            // Follow tails at about room speed; recover at once on onsets.
            let coefficient = if target > self.gains[band] { 0.8 } else { 0.5 };
            self.gains[band] += coefficient * (target - self.gains[band]);
            past[band] = now;
            offset += width;
        }
        self.history.push_back(past);
        if active {
            self.state.apply_mask(&mut self.spectrum, &self.gains);
        } else {
            self.state.apply_mask(&mut self.spectrum, &self.unity);
        }
        self.state.synthesis(&mut self.spectrum, &mut self.scratch);
        frame.copy_from_slice(&self.scratch);
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

    fn removed_db(input: &[f32], output: &[f32], frames: std::ops::Range<usize>) -> f32 {
        let (mut x, mut y) = (0.0_f32, 0.0_f32);
        for i in frames.start * HOP..frames.end * HOP {
            x += input[i].powi(2);
            y += output[i + HOP].powi(2);
        }
        10.0 * (x / y).log10()
    }

    #[test]
    fn shortens_decaying_tails_but_keeps_steady_voice() {
        let rt60 = 0.5;
        let mut filter = LateReverbFilter::new(&DFState::default(), rt60);
        let per_frame = 10.0_f32.powf(-3.0 * 0.01 / rt60);
        let (mut input, mut output) = (Vec::new(), Vec::new());
        for frame in 0..90 {
            let x = tone(frame, 0.05 * per_frame.powi(frame as i32 - 50).min(1.0));
            let mut y = x.clone();
            filter.process(&mut y, true);
            input.extend(x);
            output.extend(y);
        }
        output.extend(vec![0.0; HOP]);
        let steady = removed_db(&input, &output, 30..48);
        let tail = removed_db(&input, &output, 60..80);
        assert!(steady < 1.5, "steady voice was cut by {steady} dB");
        assert!(tail > 6.0, "tail only reduced by {tail} dB");
        assert!(tail < 10.5, "exceeded the 10 dB bound: {tail} dB");
    }

    #[test]
    fn inactive_filter_is_a_one_hop_delay() {
        let mut filter = LateReverbFilter::new(&DFState::default(), 0.5);
        let (mut input, mut output) = (Vec::new(), Vec::new());
        for frame in 0..20 {
            let x = tone(frame, 0.05);
            let mut y = x.clone();
            filter.process(&mut y, false);
            input.extend(x);
            output.extend(y);
        }
        for (x, y) in input.iter().zip(&output[HOP..]) {
            assert!((x - y).abs() < 1e-5, "not transparent");
        }
    }
}
