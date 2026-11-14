//! DeepFilterNet 3 noise suppression (full-band, 48 kHz, 10 ms hops) plus a
//! voice gate ("the mic closes when you stop talking").
//!
//! Two ways to run it:
//! * [`ThreadedDenoiser`]: for the audio engine. The model runs on its own
//!   worker thread (MMCSS "Pro Audio" priority); the real-time thread only
//!   pushes/pops samples through wait-free ring buffers (`rtrb`).
//! * [`InlineDenoiser`]: runs the model on the calling thread. For tests and
//!   the offline before/after preview.
//!
//! The model (DeepFilterNet3 ONNX, bundled in the DLL) is loaded once per
//! stream (~150 ms). Until it is ready the output is silence. A model-load
//! failure, worker panic, spawn failure, or sustained underrun falls back to
//! the input signal instead of leaving the microphone silent.

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::Array2;
use rubato::{
    FftFixedInOut, SincFixedIn, SincFixedOut, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

/// Samples per model step (10 ms at 48 kHz).
pub const HOP: usize = 480;
/// The model's only supported sample rate.
pub const RATE: u32 = 48_000;
/// "Off" still runs the model with this (inaudible) limit so the latency
/// stays identical: libDF's true bypass returns undelayed audio, which would
/// make toggling click.
const OFF_DB: f32 = 0.5;
const WATCHDOG_MS: usize = 400;

fn samples_for_ms(rate: u32, ms: usize) -> usize {
    ((rate as usize * ms) + 999) / 1000
}

fn queue_sizes(native_rate: u32) -> (usize, usize, usize) {
    let cushion = samples_for_ms(native_rate, 20);
    let max_queued = samples_for_ms(native_rate, 50);
    let ring = samples_for_ms(native_rate, 320).max(max_queued + cushion);
    (cushion, max_queued, ring)
}

/// Glitch counters (read by the status writer): underruns = the worker was
/// late and silence was played; skipped = samples dropped to keep latency
/// bounded.
pub static UNDERRUNS: AtomicU64 = AtomicU64::new(0);
pub static SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Most noise removal allowed (dB) at 100% strength.
///
/// DeepFilterNet's attenuation limit works as a dry blend: at most this
/// much of anything is removed, so soft parts of words it mistakes for
/// noise dip a little instead of vanishing (the "radio" / underwater sound
/// of short dropouts). Silence between words still goes much lower because
/// the voice gate adds its own -40 dB.
const MAX_ATTEN_DB: f32 = 30.0;

/// Offline comparison controls; live audio uses the defaults.
#[derive(Clone, Copy, Debug)]
pub struct DenoiseOptions {
    /// Fixed gain for level sweeps, compensated after the model (0..18 dB).
    pub model_gain_db: f32,
    /// Room-profile cleaning of the protective dry blend (0..1).
    pub protection_cleaning: f32,
    /// Experimental: assumed room RT60 (s) for a late-reverberation
    /// post-filter on the model output (+10 ms latency). 0 = off (default).
    pub late_reverb_rt60: f32,
    /// Most removal (dB) while talking; the dry blend that protects weak
    /// syllables. Offline diagnostics only; live audio uses 6 dB.
    pub speech_protection_db: f32,
    /// Optional broadband fade after a confirmed pause. Offline only.
    pub pause_tail_db: f32,
    /// RMS detector time for offline compressor comparisons.
    pub leveler_detector_ms: f32,
    /// Fraction of pre-EQ power used by the offline level detectors.
    pub pre_eq_detector_mix: f32,
    /// Fraction of de-esser gain reduction applied by the offline renderer.
    pub deesser_reduction_mix: f32,
}

impl Default for DenoiseOptions {
    fn default() -> Self {
        Self {
            model_gain_db: 0.0,
            protection_cleaning: 1.0,
            late_reverb_rt60: 0.0,
            speech_protection_db: SPEECH_ATTEN_DB,
            pause_tail_db: 0.0,
            leveler_detector_ms: 5.0,
            pre_eq_detector_mix: 0.0,
            deesser_reduction_mix: 1.0,
        }
    }
}

/// Attenuation limit in dB for a 0..1 strength (0.0 = effectively off).
pub fn attenuation_db(strength: f32, active: bool) -> f32 {
    if !active || strength <= 0.0 {
        OFF_DB
    } else {
        (6.0 + strength.min(1.0) * (MAX_ATTEN_DB - 6.0)).max(OFF_DB)
    }
}

/// Voice gate settings as seen by the worker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateConfig {
    pub enabled: bool,
    /// DeepFilterNet's local SNR (dB) a frame needs to count as speech.
    pub threshold_db: f32,
    /// Desk (distant) microphone: lower SNR, so open more easily, hold
    /// longer and keep open further below the threshold (no chopped words).
    pub desk: bool,
}

impl GateConfig {
    /// Sensitivity 0..1: higher opens on quieter speech (lower threshold).
    pub fn from_sensitivity(enabled: bool, sensitivity: f32, desk: bool) -> Self {
        Self {
            enabled,
            // Opening stays strict for every mic; desk mode only makes an
            // open gate stay open more easily (see VoiceGate).
            threshold_db: 12.0 - sensitivity.clamp(0.0, 1.0) * 20.0,
            desk,
        }
    }
}

/// Hold after a brief burst that may not be speech (80 ms).
const BLIP_HOLD_FRAMES: u32 = 8;
/// Frames (10 ms) the gate stays open after the last speech frame.
const GATE_HOLD_FRAMES: u32 = 25;
/// Gain between words: -40 dB. Combined with noise removal this is silent
/// for voice-activity detection (Discord etc.), but fades rather than
/// slamming shut, which sounds more natural than a hard gate.
const GATE_FLOOR: f32 = 0.01;
/// Per-frame closing factor: -6 dB per 10 ms, i.e. ~70 ms to the floor.
const GATE_CLOSE: f32 = 0.5;
/// Hysteresis: once open, frames this much below the open threshold still
/// count as speech, so soft word endings do not flutter the gate.
const GATE_KEEP_OPEN_DB: f32 = 5.0;

/// True while the voice gate is open (read by the live-status publisher).
pub static GATE_OPEN: AtomicBool = AtomicBool::new(false);
/// Speech frames must also carry at least this level (about -54 dBFS RMS).
const GATE_MIN_LEVEL: f32 = 0.002;

/// Voice gate with one frame of lookahead: a frame opens the gate only if it
/// and the next frame are both speech (>= 20 ms), so an isolated key click
/// (<= 10 ms) never opens it, while the first syllable is not cut because
/// the decision is made one frame early.
struct VoiceGate {
    /// Previous frame and whether it was (opening-level, keep-open-level) speech.
    pending: Option<([f32; HOP], bool, bool)>,
    /// Speech frames since the gate last opened: a short blip (cough, tap)
    /// gets a short hold so it does not linger for the full hold time.
    speech_frames: u32,
    hold: u32,
    gain: f32,
    started: bool,
}

impl VoiceGate {
    fn new() -> Self {
        Self {
            pending: None,
            speech_frames: 0,
            hold: 0,
            gain: GATE_FLOOR,
            started: false,
        }
    }

    /// (opens the gate, keeps an open gate open)
    fn classify(frame: &[f32; HOP], lsnr: f32, config: GateConfig) -> (bool, bool) {
        let level = (frame.iter().map(|s| s * s).sum::<f32>() / HOP as f32).sqrt();
        let audible = level >= GATE_MIN_LEVEL;
        (
            audible && lsnr >= config.threshold_db,
            audible
                && lsnr
                    >= config.threshold_db
                        - GATE_KEEP_OPEN_DB
                        - if config.desk { 3.0 } else { 0.0 },
        )
    }

    /// Takes the newest processed frame; returns the previous frame, gated.
    fn push(&mut self, frame: [f32; HOP], lsnr: f32, config: GateConfig) -> [f32; HOP] {
        let (speech, keep) = Self::classify(&frame, lsnr, config);
        if !self.started {
            self.started = true;
            if !config.enabled {
                self.gain = 1.0;
            }
        }
        let Some((mut previous, previous_speech, previous_keep)) =
            self.pending.replace((frame, speech, keep))
        else {
            return [0.0; HOP];
        };
        if !config.enabled {
            let start = self.gain;
            self.gain = 1.0;
            self.hold = GATE_HOLD_FRAMES + if config.desk { 10 } else { 0 };
            if start != 1.0 {
                for (index, sample) in previous.iter_mut().take(HOP / 2).enumerate() {
                    let t = index as f32 / (HOP / 2) as f32;
                    *sample *= start + (1.0 - start) * t;
                }
            }
            return previous;
        }
        // Open: sustained speech (this frame and the next). Stay open while
        // frames are speech-like at the lower keep-open threshold.
        let open = self.hold > 0;
        if (previous_speech && speech) || (open && previous_keep) {
            if previous_speech {
                self.speech_frames = self.speech_frames.saturating_add(1);
            }
            let full = GATE_HOLD_FRAMES + if config.desk { 10 } else { 0 };
            // Under 50 ms of speech so far: probably not talking, short hold.
            self.hold = if self.speech_frames < 5 {
                BLIP_HOLD_FRAMES
            } else {
                full
            };
        } else {
            self.hold = self.hold.saturating_sub(1);
            if self.hold == 0 {
                self.speech_frames = 0;
            }
        }
        let start = self.gain;
        let end = if self.hold > 0 {
            1.0
        } else {
            (self.gain * GATE_CLOSE).max(GATE_FLOOR)
        };
        // Opening fades in over 5 ms (soft, no punch on the first word, and
        // the lookahead frame means nothing is cut); closing over the frame.
        let ramp = if end > start { 240 } else { HOP };
        for (index, sample) in previous.iter_mut().enumerate() {
            let t = (index as f32 / ramp as f32).min(1.0);
            *sample *= start + (end - start) * t;
        }
        self.gain = end;
        previous
    }
}

struct Model {
    df: DfTract,
    noisy: Array2<f32>,
    enhanced: Array2<f32>,
    atten_db: f32,
    input_gain: f32,
    cleaner: Option<crate::protection::ProtectionCleaner>,
    dereverb: Option<crate::dereverb::LateReverbFilter>,
}

impl Model {
    fn load(options: DenoiseOptions) -> Option<Model> {
        let Some(model) = catch_unwind(|| {
            DfTract::new(DfParams::default(), &RuntimeParams::default_with_ch(1)).ok()
        })
        .ok()
        .flatten() else {
            crate::settings::trace_error(
                "noise-removal model failed to load; passing audio through",
            );
            return None;
        };
        if model.sr != RATE as usize || model.hop_size != HOP {
            return None;
        }
        Some(Model {
            cleaner: (options.protection_cleaning.is_finite() && options.protection_cleaning > 0.0)
                .then(|| {
                    crate::protection::ProtectionCleaner::new(
                        &model.df_states[0],
                        model.lookahead,
                        options.protection_cleaning.clamp(0.0, 1.0),
                    )
                }),
            dereverb: (options.late_reverb_rt60.is_finite() && options.late_reverb_rt60 > 0.0)
                .then(|| {
                    crate::dereverb::LateReverbFilter::new(
                        &model.df_states[0],
                        options.late_reverb_rt60,
                    )
                }),
            input_gain: if options.model_gain_db.is_finite() {
                10.0_f32.powf(options.model_gain_db.clamp(0.0, 18.0) / 20.0)
            } else {
                1.0
            },
            df: model,
            noisy: Array2::zeros((1, HOP)),
            enhanced: Array2::zeros((1, HOP)),
            atten_db: 100.0,
        })
    }

    /// Returns DeepFilterNet's local SNR estimate (dB) for the frame.
    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        atten_db: f32,
        learn_noise: bool,
    ) -> Option<f32> {
        if (atten_db - self.atten_db).abs() > 0.01 {
            self.df.set_atten_lim(atten_db);
            self.atten_db = atten_db;
        }
        for (slot, &sample) in self.noisy.iter_mut().zip(input) {
            *slot = sample * self.input_gain;
        }
        let lsnr = self
            .df
            .process(self.noisy.view(), self.enhanced.view_mut())
            .ok()?;
        output.copy_from_slice(self.enhanced.as_slice().unwrap());
        if let Some(cleaner) = &mut self.cleaner {
            let noisy = self.noisy.as_slice().unwrap();
            // libDF returns zeros without advancing the STFT below this
            // power threshold; keep the correction history in step.
            if noisy.iter().map(|x| x * x).sum::<f32>() / HOP as f32 >= 1e-7 {
                cleaner.process(noisy, output, self.atten_db, learn_noise && lsnr < 0.0);
            }
        }
        if let Some(dereverb) = &mut self.dereverb {
            dereverb.process(output, self.atten_db > OFF_DB);
        }
        for sample in output.iter_mut() {
            *sample = if sample.is_finite() {
                *sample / self.input_gain
            } else {
                0.0
            };
        }
        Some(lsnr)
    }
}

/// One model step plus the gate; shared by both denoiser flavours.
struct Pipeline {
    model: Option<Model>,
    gate: VoiceGate,
    vad: Box<nnnoiseless::DenoiseState<'static>>,
    protection: SpeechProtection,
    pause_tail: Option<PauseTail>,
}

struct PauseTail {
    floor: f32,
    gain: f32,
    quiet: u32,
}

impl PauseTail {
    fn apply(&mut self, output: &mut [f32; HOP], quiet: bool) {
        self.quiet = if quiet {
            self.quiet.saturating_add(1)
        } else {
            0
        };
        let end = if self.quiet >= 8 {
            (self.gain - (1.0 - self.floor) / 3.0).max(self.floor)
        } else {
            1.0
        };
        if self.gain != end {
            let ramp = if end > self.gain { HOP / 2 } else { HOP };
            for (i, sample) in output.iter_mut().enumerate() {
                let t = ((i + 1) as f32 / ramp as f32).min(1.0);
                *sample *= self.gain + (end - self.gain) * t;
            }
        } else if end != 1.0 {
            for sample in output {
                *sample *= end;
            }
        }
        self.gain = end;
    }
}

/// Most noise removal (dB) while you are talking.
///
/// With a mic that is far from the mouth, the voice is only a little louder
/// than the room, and DeepFilterNet keeps mistaking weak stretches of the
/// voice for noise, cutting them by 10 dB or more for a few hundred ms:
/// the voice "flutters". While talking, removal is capped here (background
/// under the voice is still clearly reduced, and masked by the voice);
/// between words the full strength applies, plus the gate.
const SPEECH_ATTEN_DB: f32 = 6.0;
/// Frames (10 ms) the speech cap stays on after the last speech frame, so
/// it does not toggle inside words.
const TALKING_HOLD_FRAMES: u32 = 30;

/// RNNoise contributes only a speech probability. DeepFilterNet remains
/// the sole audio suppressor; the RNNoise audio output is discarded.
struct SpeechProtection {
    hold: u32,
    quiet: u32,
    blend: f32,
    /// Removal cap while talking (dB).
    cap_db: f32,
    talking: Arc<AtomicBool>,
}

impl Default for SpeechProtection {
    fn default() -> Self {
        Self::with_cap(SPEECH_ATTEN_DB, Arc::new(AtomicBool::new(false)))
    }
}

impl SpeechProtection {
    fn with_cap(cap_db: f32, talking: Arc<AtomicBool>) -> Self {
        Self {
            hold: 0,
            quiet: 0,
            blend: 0.0,
            cap_db: if cap_db.is_finite() {
                cap_db.clamp(OFF_DB, 100.0)
            } else {
                SPEECH_ATTEN_DB
            },
            talking,
        }
    }

    fn attenuation(&mut self, limit: f32, vad: f32) -> f32 {
        // Current-frame VAD can protect an onset before libDF's delayed
        // output is loud enough for VoiceGate::classify.
        if vad >= 0.6 {
            self.hold = TALKING_HOLD_FRAMES;
            self.quiet = 0;
        }
        let target = if self.hold > 0 { 1.0 } else { 0.0 };
        let step = if target > self.blend { 0.2 } else { 0.1 };
        self.blend += (target - self.blend).clamp(-step, step);
        if limit <= self.cap_db {
            return limit;
        }
        // libDF blends in the linear domain. Fade protection in over 50 ms,
        // out over 100 ms rather than jumping between the two limits.
        let full_dry = 10.0_f32.powf(-limit / 20.0);
        let speech_dry = 10.0_f32.powf(-self.cap_db / 20.0);
        -20.0 * (full_dry + (speech_dry - full_dry) * self.blend).log10()
    }

    fn observe(&mut self, vad: f32, speech_like: bool) {
        if vad >= 0.6 || speech_like {
            self.hold = TALKING_HOLD_FRAMES;
            self.quiet = 0;
        } else {
            self.quiet = if vad < 0.3 {
                self.quiet.saturating_add(1)
            } else {
                0
            };
            // Both estimates must agree on a pause for 60 ms. Ambiguous
            // frames retain the old 300 ms hold for weak word endings.
            self.hold = if self.quiet >= 6 {
                0
            } else {
                self.hold.saturating_sub(1)
            };
        }
        self.talking.store(self.hold > 0, Ordering::Relaxed);
    }
}

impl Pipeline {
    fn new(options: DenoiseOptions, talking: Arc<AtomicBool>) -> Self {
        Self {
            model: Model::load(options),
            gate: VoiceGate::new(),
            vad: nnnoiseless::DenoiseState::new(),
            protection: SpeechProtection::with_cap(options.speech_protection_db, talking),
            pause_tail: (options.pause_tail_db.is_finite() && options.pause_tail_db > 0.0).then(
                || PauseTail {
                    floor: 10.0_f32.powf(-options.pause_tail_db.min(12.0) / 20.0),
                    gain: 1.0,
                    quiet: 0,
                },
            ),
        }
    }

    fn run(&mut self, input: &[f32; HOP], atten_db: f32, gate: GateConfig) -> Option<[f32; HOP]> {
        let mut output = [0.0; HOP];
        let scaled = input.map(|x| (x * 32768.0).clamp(-32768.0, 32767.0));
        let mut discarded_audio = [0.0; HOP];
        let probability = self.vad.process_frame(&mut discarded_audio, &scaled);
        let probability = if probability.is_finite() {
            probability
        } else {
            0.5
        };
        let frame_atten = self.protection.attenuation(atten_db, probability);
        let lsnr = match self.model.as_mut() {
            Some(model) => model.process(
                input,
                &mut output,
                frame_atten,
                probability < 0.1 && self.protection.quiet >= 15,
            )?,
            None => {
                output = *input;
                35.0
            }
        };
        // Talking: speech-like at the gate's (lenient) keep-open level,
        // whether or not the gate itself is switched on.
        let speech_like = self.model.is_some() && VoiceGate::classify(&output, lsnr, gate).1;
        self.protection.observe(probability, speech_like);
        // Without a working model there is no speech estimate: never gate.
        let gate = GateConfig {
            enabled: gate.enabled && self.model.is_some(),
            ..gate
        };
        let mut gated = self.gate.push(output, lsnr, gate);
        if let Some(tail) = &mut self.pause_tail {
            if gate.enabled {
                // The gate returns the previous frame. Eight consecutive
                // quiet decisions cover it as well as the current frame.
                tail.apply(&mut gated, probability < 0.15 && lsnr < 0.0 && !speech_like);
            } else {
                tail.gain = 1.0;
                tail.quiet = 0;
            }
        }
        Some(gated)
    }
}

rubato::implement_resampler!(MonoResampler, &[&[T]], &mut [&mut [T]]);

struct RateConversion {
    to_model: Box<dyn MonoResampler<f32>>,
    from_model: Box<dyn MonoResampler<f32>>,
}

impl RateConversion {
    fn new(native_rate: u32) -> Result<Self, rubato::ResamplerConstructionError> {
        let native_rate = native_rate as usize;
        let model_rate = RATE as usize;
        if native_rate % 100 == 0 {
            let native_hop = native_rate / 100;
            // Rubato 0.14's FftFixedInOut rounds the requested size in
            // output frames. Ask for one 10 ms hop in each direction.
            return Ok(Self {
                to_model: Box::new(FftFixedInOut::new(native_rate, model_rate, HOP, 1)?),
                from_model: Box::new(FftFixedInOut::new(model_rate, native_rate, native_hop, 1)?),
            });
        }

        // An exact-ratio FFT block can be a second long at co-prime rates.
        // Use fixed 480-sample model hops with bounded sinc blocks instead.
        let parameters = || SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 64,
            window: WindowFunction::BlackmanHarris2,
        };
        let ratio = model_rate as f64 / native_rate as f64;
        Ok(Self {
            to_model: Box::new(SincFixedOut::new(ratio, 1.0, parameters(), HOP, 1)?),
            from_model: Box::new(SincFixedIn::new(1.0 / ratio, 1.0, parameters(), HOP, 1)?),
        })
    }
}

/// Runs the 48 kHz pipeline for a microphone at any sample rate: converts
/// to 48 kHz, processes 10 ms frames, and converts back.
/// At 48 kHz no conversion happens. Allocates; never used on the real-time
/// thread (worker thread or offline only).
struct Engine {
    pipeline: Pipeline,
    conversion: Option<RateConversion>,
    /// Rate conversion is impossible for this rate: pass audio through.
    passthrough: bool,
    native_in: Vec<f32>,
    model_in: Vec<f32>,
    model_out: Vec<f32>,
    scratch: Vec<f32>,
}

impl Engine {
    fn new(native_rate: u32, talking: Arc<AtomicBool>) -> Self {
        Self::with_options(native_rate, DenoiseOptions::default(), talking)
    }

    fn with_options(native_rate: u32, options: DenoiseOptions, talking: Arc<AtomicBool>) -> Self {
        let (conversion, passthrough) = if native_rate == RATE {
            (None, false)
        } else {
            match RateConversion::new(native_rate) {
                Ok(conversion) => (Some(conversion), false),
                Err(_) => {
                    crate::settings::trace_error(&format!(
                        "no resampler for {native_rate} Hz; noise removal disabled"
                    ));
                    (None, true)
                }
            }
        };
        Self {
            pipeline: Pipeline::new(options, talking),
            conversion,
            passthrough,
            native_in: Vec::new(),
            model_in: Vec::new(),
            model_out: Vec::new(),
            scratch: Vec::new(),
        }
    }

    fn convert(
        resampler: &mut dyn MonoResampler<f32>,
        pending: &mut Vec<f32>,
        scratch: &mut Vec<f32>,
        out: &mut Vec<f32>,
    ) -> bool {
        loop {
            let need = resampler.input_frames_next();
            if pending.len() < need {
                return true;
            }
            scratch.resize(resampler.output_frames_next(), 0.0);
            let Ok((consumed, produced)) =
                resampler.process_into_buffer(&[&pending[..need]], &mut [&mut scratch[..]], None)
            else {
                return false;
            };
            out.extend_from_slice(&scratch[..produced]);
            pending.drain(..consumed);
        }
    }

    /// Feeds native-rate samples; appends processed native-rate samples.
    fn process(
        &mut self,
        input: &[f32],
        atten_db: f32,
        gate: GateConfig,
        out: &mut Vec<f32>,
    ) -> bool {
        if self.passthrough {
            out.extend_from_slice(input);
            return true;
        }
        match self.conversion.as_mut() {
            Some(conversion) => {
                self.native_in.extend_from_slice(input);
                if !Self::convert(
                    conversion.to_model.as_mut(),
                    &mut self.native_in,
                    &mut self.scratch,
                    &mut self.model_in,
                ) {
                    return false;
                }
            }
            None => self.model_in.extend_from_slice(input),
        }
        while self.model_in.len() >= HOP {
            let mut frame = [0.0_f32; HOP];
            frame.copy_from_slice(&self.model_in[..HOP]);
            self.model_in.drain(..HOP);
            let Some(processed) = self.pipeline.run(&frame, atten_db, gate) else {
                return false;
            };
            match self.conversion.as_ref() {
                Some(_) => self.model_out.extend_from_slice(&processed),
                None => out.extend_from_slice(&processed),
            }
        }
        if let Some(conversion) = self.conversion.as_mut() {
            return Self::convert(
                conversion.from_model.as_mut(),
                &mut self.model_out,
                &mut self.scratch,
                out,
            );
        }
        true
    }
}

/// Runs the model on the calling thread (tests, offline preview).
pub struct InlineDenoiser {
    engine: Engine,
    produced: std::collections::VecDeque<f32>,
    buffer: Vec<f32>,
}

impl InlineDenoiser {
    pub fn new(native_rate: u32) -> Self {
        Self::with_options(native_rate, DenoiseOptions::default())
    }

    pub fn with_options(native_rate: u32, options: DenoiseOptions) -> Self {
        Self {
            engine: Engine::with_options(native_rate, options, Arc::new(AtomicBool::new(false))),
            produced: std::collections::VecDeque::new(),
            buffer: Vec::new(),
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.engine.pipeline.model.is_some()
    }

    pub fn is_talking(&self) -> bool {
        self.engine
            .pipeline
            .protection
            .talking
            .load(Ordering::Relaxed)
    }

    pub(crate) fn gate_open(&self) -> bool {
        self.engine.pipeline.model.is_some() && self.engine.pipeline.gate.hold > 0
    }

    pub fn process(&mut self, sample: f32, atten_db: f32, gate: GateConfig) -> f32 {
        self.buffer.clear();
        let processed = catch_unwind(AssertUnwindSafe(|| {
            self.engine
                .process(&[sample], atten_db, gate, &mut self.buffer)
        }));
        if !matches!(processed, Ok(true)) {
            self.engine.pipeline.model = None;
            self.engine.passthrough = true;
            self.produced.clear();
            self.engine
                .pipeline
                .protection
                .talking
                .store(false, Ordering::Relaxed);
            crate::settings::trace_error("noise-removal inference failed; passing audio through");
            return sample;
        }
        self.produced.extend(self.buffer.iter().copied());
        self.produced.pop_front().unwrap_or(0.0)
    }
}

const LOADING: u8 = 0;
const READY: u8 = 1;
const FAILED: u8 = 2;

struct Shared {
    stop: AtomicBool,
    atten_bits: AtomicU32,
    gate_enabled: AtomicBool,
    gate_threshold_bits: AtomicU32,
    gate_desk: AtomicBool,
    gate_open: AtomicBool,
    state: AtomicU8,
    talking: Arc<AtomicBool>,
}

/// Raises the worker to MMCSS "Pro Audio" scheduling so busy systems (games)
/// do not starve it; reverted when the thread ends.
#[cfg(windows)]
struct ProAudioPriority(Option<windows::Win32::Foundation::HANDLE>);

#[cfg(windows)]
impl ProAudioPriority {
    fn enable() -> Self {
        use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
        let mut index = 0_u32;
        let handle =
            unsafe { AvSetMmThreadCharacteristicsW(windows_core::w!("Pro Audio"), &mut index) }
                .ok();
        Self(handle)
    }
}

#[cfg(windows)]
impl Drop for ProAudioPriority {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = unsafe {
                windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(handle)
            };
        }
    }
}

/// Runs the model on a worker thread; `push`/`pop` are wait-free and safe
/// to call from the real-time audio thread.
pub struct ThreadedDenoiser {
    to_worker: rtrb::Producer<f32>,
    from_worker: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    primed: bool,
    /// Samples this instance skipped (also counted in [`SKIPPED`]).
    skipped: u64,
    cushion: usize,
    max_queued: usize,
    watchdog_limit: usize,
    underrun_samples: usize,
    fallback: bool,
}

impl ThreadedDenoiser {
    /// Allocates and spawns the worker; call outside the real-time thread.
    pub fn new(native_rate: u32) -> Self {
        Self::spawn_at_rate(native_rate, move |talking| {
            Engine::new(native_rate, talking)
        })
    }

    fn spawn_at_rate(
        native_rate: u32,
        load: impl FnOnce(Arc<AtomicBool>) -> Engine + Send + 'static,
    ) -> Self {
        let (cushion, max_queued, ring_size) = queue_sizes(native_rate);
        let (to_worker, mut worker_in) = rtrb::RingBuffer::<f32>::new(ring_size);
        let (mut worker_out, from_worker) = rtrb::RingBuffer::<f32>::new(ring_size);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            atten_bits: AtomicU32::new(100.0_f32.to_bits()),
            gate_enabled: AtomicBool::new(true),
            gate_threshold_bits: AtomicU32::new(2.0_f32.to_bits()),
            gate_desk: AtomicBool::new(true),
            gate_open: AtomicBool::new(false),
            state: AtomicU8::new(LOADING),
            talking: Arc::new(AtomicBool::new(false)),
        });
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("hush-denoise".into())
            .spawn(move || {
                #[cfg(target_arch = "x86_64")]
                #[allow(deprecated)]
                unsafe {
                    let csr = std::arch::x86_64::_mm_getcsr();
                    std::arch::x86_64::_mm_setcsr(csr | 0x8040);
                }
                #[cfg(windows)]
                let _priority = ProAudioPriority::enable();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let mut engine = load(worker_shared.talking.clone());
                    let mut input: Vec<f32> = Vec::with_capacity(ring_size);
                    let mut output: Vec<f32> = Vec::with_capacity(ring_size);
                    if engine.pipeline.model.is_none() || engine.passthrough {
                        return Err("model or resampler unavailable");
                    }
                    worker_shared.state.store(READY, Ordering::Release);
                    while !worker_shared.stop.load(Ordering::Acquire) {
                        let available = worker_in.slots();
                        if available == 0 {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        input.clear();
                        for _ in 0..available {
                            input.push(worker_in.pop().unwrap_or(0.0));
                        }
                        let atten =
                            f32::from_bits(worker_shared.atten_bits.load(Ordering::Relaxed));
                        let gate = GateConfig {
                            enabled: worker_shared.gate_enabled.load(Ordering::Relaxed),
                            threshold_db: f32::from_bits(
                                worker_shared.gate_threshold_bits.load(Ordering::Relaxed),
                            ),
                            desk: worker_shared.gate_desk.load(Ordering::Relaxed),
                        };
                        output.clear();
                        if !engine.process(&input, atten, gate, &mut output) {
                            return Err("noise-removal inference or resampling failed");
                        }
                        worker_shared
                            .gate_open
                            .store(engine.pipeline.gate.hold > 0, Ordering::Relaxed);
                        for &sample in output.iter() {
                            if worker_out.push(sample).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(())
                }));
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(message)) => {
                        worker_shared.state.store(FAILED, Ordering::Release);
                        crate::settings::trace_error(message);
                    }
                    Err(payload) => {
                        worker_shared.state.store(FAILED, Ordering::Release);
                        let message = payload
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("non-string panic");
                        crate::settings::trace_error(&format!(
                            "denoiser worker panicked: {message}"
                        ));
                    }
                }
            })
            .ok();
        let worker = worker.or_else(|| {
            shared.state.store(FAILED, Ordering::Release);
            crate::settings::trace_error("denoiser worker thread could not be spawned");
            None
        });
        Self {
            to_worker,
            from_worker,
            shared,
            worker,
            primed: false,
            skipped: 0,
            cushion,
            max_queued,
            watchdog_limit: samples_for_ms(native_rate, WATCHDOG_MS),
            underrun_samples: 0,
            fallback: false,
        }
    }

    /// True once the model loaded (false while loading or after a failure).
    pub fn is_ready(&self) -> bool {
        !self.fallback && self.shared.state.load(Ordering::Acquire) == READY
    }

    pub(crate) fn speech_activity(&self) -> Option<bool> {
        self.is_ready()
            .then(|| self.shared.talking.load(Ordering::Relaxed))
    }

    pub(crate) fn gate_open(&self) -> bool {
        self.is_ready() && self.shared.gate_open.load(Ordering::Relaxed)
    }

    pub fn configure(&self, atten_db: f32, gate: GateConfig) {
        self.shared
            .atten_bits
            .store(atten_db.to_bits(), Ordering::Relaxed);
        self.shared
            .gate_enabled
            .store(gate.enabled, Ordering::Relaxed);
        self.shared
            .gate_threshold_bits
            .store(gate.threshold_db.to_bits(), Ordering::Relaxed);
        self.shared.gate_desk.store(gate.desk, Ordering::Relaxed);
    }

    fn latch_fallback(&mut self) {
        if !self.fallback {
            self.fallback = true;
            self.shared.stop.store(true, Ordering::Release);
            crate::settings::report_worker_fallback();
        }
    }

    /// Real-time safe.
    pub fn push(&mut self, sample: f32) {
        // The worker cannot keep this audio at real-time pace while loading.
        // Start the resamplers and speech detectors on the first fresh input.
        if self.is_ready() {
            let _ = self.to_worker.push(sample);
        }
    }

    /// Real-time safe. Primes a processed-audio cushion and latches to direct
    /// input after a failed worker or 400 ms of continuous underrun.
    pub fn pop(&mut self, input: f32) -> f32 {
        if self.fallback {
            return input;
        }
        let state = self.shared.state.load(Ordering::Acquire);
        if state == LOADING {
            return 0.0;
        }
        let queued = self.from_worker.slots();
        if state == FAILED && (queued == 0 || !self.primed) {
            self.latch_fallback();
            return input;
        }
        if !self.primed {
            if queued < self.cushion {
                self.underrun_samples = self.underrun_samples.saturating_add(1);
                if self.underrun_samples >= self.watchdog_limit {
                    self.latch_fallback();
                    return input;
                }
                return 0.0;
            }
            self.primed = true;
        }
        if queued > self.max_queued {
            // Gentle latency correction: skip one sample.
            let _ = self.from_worker.pop();
            SKIPPED.fetch_add(1, Ordering::Relaxed);
            self.skipped += 1;
        }
        match self.from_worker.pop() {
            Ok(sample) => {
                self.underrun_samples = 0;
                sample
            }
            Err(_) => {
                self.primed = false;
                UNDERRUNS.fetch_add(1, Ordering::Relaxed);
                self.underrun_samples = self.underrun_samples.saturating_add(1);
                if self.underrun_samples >= self.watchdog_limit {
                    self.latch_fallback();
                    input
                } else {
                    0.0
                }
            }
        }
    }
}

impl Drop for ThreadedDenoiser {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(level: f32) -> [f32; HOP] {
        let mut f = [0.0; HOP];
        for (i, s) in f.iter_mut().enumerate() {
            *s = if i % 2 == 0 { level } else { -level };
        }
        f
    }

    fn rms(f: &[f32]) -> f32 {
        (f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32).sqrt()
    }

    const ON: GateConfig = GateConfig {
        enabled: true,
        threshold_db: 2.0,
        desk: false,
    };

    #[test]
    fn independent_vad_protects_speech_when_the_model_misses_it() {
        let mut protection = SpeechProtection::default();
        let mut attenuation = 30.0;
        for _ in 0..100 {
            attenuation = protection.attenuation(30.0, 0.9);
            protection.observe(0.9, false);
        }
        assert!((attenuation - SPEECH_ATTEN_DB).abs() < 1e-5);
    }

    #[test]
    fn both_detectors_must_agree_before_shortening_the_hold() {
        let mut protection = SpeechProtection::default();
        protection.observe(0.9, false);
        for _ in 0..10 {
            protection.observe(0.1, true);
        }
        assert_eq!(protection.hold, TALKING_HOLD_FRAMES);
        for _ in 0..5 {
            protection.observe(0.1, false);
            assert!(protection.hold > 0);
        }
        protection.observe(0.1, false);
        assert_eq!(protection.hold, 0);
    }

    #[test]
    fn uncertain_vad_keeps_the_full_hold_for_soft_word_endings() {
        let mut protection = SpeechProtection::default();
        protection.observe(0.9, false);
        for _ in 0..TALKING_HOLD_FRAMES - 1 {
            protection.observe(0.4, false);
            assert!(protection.hold > 0);
        }
        protection.observe(0.4, false);
        assert_eq!(protection.hold, 0);
    }

    #[test]
    fn protection_fades_without_exceeding_the_requested_strength() {
        let mut protection = SpeechProtection::default();
        let mut previous = 30.0;
        for _ in 0..5 {
            let attenuation = protection.attenuation(30.0, 0.9);
            assert!(attenuation <= previous + 1e-5);
            assert!(attenuation >= SPEECH_ATTEN_DB - 1e-5);
            previous = attenuation;
        }
        protection.hold = 0;
        for _ in 0..11 {
            let attenuation = protection.attenuation(30.0, 0.0);
            assert!(attenuation >= previous - 1e-5);
            assert!(attenuation <= 30.0 + 1e-5);
            previous = attenuation;
        }
        assert!((previous - 30.0).abs() < 1e-4);
        // Turning suppression off must not be overridden by speech protection.
        assert!((protection.attenuation(OFF_DB, 0.9) - OFF_DB).abs() < 1e-5);
        assert_eq!(HOP, nnnoiseless::DenoiseState::FRAME_SIZE);
    }

    #[test]
    fn single_click_frame_never_opens_the_gate() {
        let mut gate = VoiceGate::new();
        let mut out = Vec::new();
        // silence, one loud "speech-looking" frame (a click), silence...
        let frames = [
            (0.0, -15.0),
            (0.3, 20.0),
            (0.0, -15.0),
            (0.0, -15.0),
            (0.0, -15.0),
        ];
        for (level, lsnr) in frames {
            out.push(gate.push(frame(level), lsnr, ON));
        }
        // out[i] is input frame i-1; the click is out[2].
        // At most the gate floor (-40 dB) of the click gets through.
        assert!(rms(&out[2]) < 0.3 * 0.011, "click passed: {}", rms(&out[2]));
    }

    #[test]
    fn sustained_speech_opens_from_its_first_frame_and_holds() {
        let mut gate = VoiceGate::new();
        let mut out = Vec::new();
        for i in 0..80 {
            let speaking = (5..25).contains(&i);
            let (level, lsnr) = if speaking {
                (0.1, 15.0)
            } else {
                (0.001, -10.0)
            };
            out.push(gate.push(frame(level), lsnr, ON));
        }
        // First speech frame (input 5 -> out[6]) is already mostly open.
        assert!(rms(&out[6]) > 0.08, "onset clipped: {}", rms(&out[6]));
        // Still open shortly after speech ended (hold).
        assert!(rms(&out[30]) > 0.0008, "closed too early");
        // Fully closed well after the hold + fade.
        assert!(
            rms(&out[79]) < 0.001 * 0.011,
            "did not close: {}",
            rms(&out[79])
        );
    }

    #[test]
    fn disabled_gate_passes_everything() {
        let mut gate = VoiceGate::new();
        let off = GateConfig {
            enabled: false,
            ..ON
        };
        gate.push(frame(0.01), -15.0, off);
        let out = gate.push(frame(0.01), -15.0, off);
        assert!((rms(&out) - 0.01).abs() < 1e-6);
    }
}

#[cfg(test)]
mod hysteresis_tests {
    use super::*;

    #[test]
    fn soft_word_endings_keep_an_open_gate_open() {
        let config = GateConfig {
            enabled: true,
            threshold_db: 5.0,
            desk: false,
        };
        let mut gate = VoiceGate::new();
        let mut push = |lsnr: f32| {
            let mut f = [0.0_f32; HOP];
            for (i, s) in f.iter_mut().enumerate() {
                *s = if i % 2 == 0 { 0.05 } else { -0.05 };
            }
            gate.push(f, lsnr, config)
        };
        push(15.0);
        push(15.0);
        // Long soft tail just under the open threshold but above keep-open.
        let mut last = [0.0_f32; HOP];
        for _ in 0..60 {
            last = push(2.0);
        }
        let level = (last.iter().map(|s| s * s).sum::<f32>() / HOP as f32).sqrt();
        assert!(level > 0.045, "gate fluttered closed: {level}");
    }

    #[test]
    fn brief_blip_closes_quickly_even_in_desk_mode() {
        let config = GateConfig::from_sensitivity(true, 0.5, true);
        let mut gate = VoiceGate::new();
        let mut push = |level: f32, lsnr: f32| {
            let mut f = [0.0_f32; HOP];
            for (i, s) in f.iter_mut().enumerate() {
                *s = if i % 2 == 0 { level } else { -level };
            }
            gate.push(f, lsnr, config)
        };
        for _ in 0..5 {
            push(0.001, -10.0);
        }
        // A 30 ms cough-like burst, then quiet.
        for _ in 0..3 {
            push(0.1, 15.0);
        }
        let mut outputs = Vec::new();
        for _ in 0..40 {
            outputs.push(push(0.001, -10.0));
        }
        let level = |f: &[f32; HOP]| (f.iter().map(|s| s * s).sum::<f32>() / HOP as f32).sqrt();
        // 150 ms after the burst the gate is back near its floor.
        assert!(
            level(&outputs[15]) < 0.001 * 0.05,
            "blip lingered: {}",
            level(&outputs[15])
        );
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    /// Audio queued during the load must not be played back fast.
    #[test]
    fn stream_start_does_not_fast_forward_the_load_backlog() {
        paced_start(&[RATE, 44_100, 16_000]);
    }

    /// The same real-time pacing at high microphone rates. Timing-sensitive
    /// in an unoptimized build on a busy machine, so it runs on request:
    /// `cargo test -p hush-apo --release -- --ignored high_rate`.
    #[test]
    #[ignore = "real-time paced; run locally in release"]
    fn high_rate_stream_start_keeps_pace() {
        paced_start(&[48_001, 96_000, 192_000, 384_000]);
    }

    #[test]
    fn resamplers_keep_model_hops_and_native_blocks_bounded() {
        for rate in [
            8_000, 16_000, 22_050, 44_100, 48_001, 96_000, 192_000, 384_000,
        ] {
            let mut conversion = RateConversion::new(rate).unwrap();
            assert_eq!(conversion.to_model.output_frames_next(), HOP);
            assert_eq!(conversion.from_model.input_frames_next(), HOP);
            let native_hop = samples_for_ms(rate, 10);
            let mut output_samples = 0;
            let mut input_samples = 0;
            let mut startup_offset = None;
            let mut model_hop = [0.0; HOP];
            let mut output = vec![0.0; conversion.from_model.output_frames_max()];
            for block in 0..1_000 {
                let need = conversion.to_model.input_frames_next();
                assert!(need <= native_hop + 130, "{rate} Hz: input block {need}");
                let input = vec![0.125; need];
                let (consumed, produced) = conversion
                    .to_model
                    .process_into_buffer(&[&input], &mut [&mut model_hop], None)
                    .unwrap();
                assert_eq!((consumed, produced), (need, HOP));
                input_samples += consumed;
                let (consumed, produced) = conversion
                    .from_model
                    .process_into_buffer(&[&model_hop], &mut [&mut output], None)
                    .unwrap();
                assert_eq!(consumed, HOP);
                assert!(
                    produced.abs_diff(native_hop) <= 130,
                    "{rate} Hz: output block {produced}"
                );
                output_samples += produced;
                if block == 99 {
                    startup_offset = Some(input_samples as isize - output_samples as isize);
                }
            }
            // Sinc needs a fixed amount of filter history. Measure drift
            // after that history fills, independently of its startup delay.
            let offset = input_samples as isize - output_samples as isize;
            assert!(
                offset.abs_diff(startup_offset.unwrap()) <= 2,
                "{rate} Hz: sample count drift from {} to {offset}",
                startup_offset.unwrap()
            );
            assert!(
                (output[0] - 0.125).abs() < 0.001,
                "{rate} Hz: DC gain changed"
            );
        }
    }

    fn paced_start(rates: &[u32]) {
        for &rate in rates {
            let hop = (rate / 100) as usize;
            let (resume, wait) = std::sync::mpsc::channel();
            let mut denoiser = ThreadedDenoiser::spawn_at_rate(rate, move |talking| {
                let _ = wait.recv_timeout(Duration::from_secs(3));
                Engine::new(rate, talking)
            });
            denoiser.configure(OFF_DB, GateConfig::from_sensitivity(false, 0.5, false));
            for _ in 0..hop * 50 {
                denoiser.push(0.25);
                assert_eq!(denoiser.pop(0.25), 0.0);
            }
            resume.send(()).unwrap();
            let start = std::time::Instant::now();
            while !denoiser.is_ready() {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "{rate} Hz model load timed out"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let start = std::time::Instant::now();
            let mut phase = 0.0_f32;
            let mut peak = 0.0_f32;
            for block in 0..45 {
                for _ in 0..hop {
                    phase += 0.05 * RATE as f32 / rate as f32;
                    denoiser.push(0.05 * phase.sin());
                    peak = peak.max(denoiser.pop(0.0).abs());
                }
                if let Some(wait) =
                    Duration::from_millis(10 * (block + 1)).checked_sub(start.elapsed())
                {
                    std::thread::sleep(wait);
                }
            }
            assert!(
                (0.02..0.12).contains(&peak),
                "{rate} Hz: missing fresh audio or stale marker, peak {peak}"
            );
            assert!(
                denoiser.skipped < hop as u64,
                "{rate} Hz: skipped {} samples",
                denoiser.skipped
            );
            assert!(denoiser.is_ready(), "{rate} Hz: watchdog latched");
        }
    }

    #[test]
    fn panicking_worker_falls_back_to_input() {
        let mut denoiser = ThreadedDenoiser::spawn_at_rate(48_000, |_| -> Engine {
            panic!("injected worker startup panic");
        });
        let start = std::time::Instant::now();
        while denoiser.shared.state.load(Ordering::Acquire) != FAILED {
            assert!(start.elapsed() < Duration::from_secs(2));
            std::thread::yield_now();
        }
        assert_eq!(denoiser.pop(0.375), 0.375);
    }

    #[test]
    fn watchdog_latches_after_400_ms_and_ignores_stale_activity() {
        let mut denoiser = ThreadedDenoiser::new(RATE);
        let start = std::time::Instant::now();
        while !denoiser.is_ready() {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
        denoiser.shared.talking.store(true, Ordering::Relaxed);
        denoiser.shared.gate_open.store(true, Ordering::Relaxed);
        assert_eq!(denoiser.speech_activity(), Some(true));
        assert!(denoiser.gate_open());
        for _ in 1..19_200 {
            assert_eq!(denoiser.pop(0.375), 0.0);
        }
        assert_eq!(denoiser.pop(0.375), 0.375);
        assert_eq!(denoiser.pop(0.25), 0.25);
        assert_eq!(denoiser.speech_activity(), None);
        assert!(!denoiser.gate_open());
    }

    #[test]
    fn buffering_limits_scale_with_native_rate() {
        for (rate, expected) in [
            (8_000, (160, 400, 2_560)),
            (44_100, (882, 2_205, 14_112)),
            (96_000, (1_920, 4_800, 30_720)),
            (192_000, (3_840, 9_600, 61_440)),
            (384_000, (7_680, 19_200, 122_880)),
        ] {
            assert_eq!(queue_sizes(rate), expected);
        }
    }
}
