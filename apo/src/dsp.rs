//! Clean-mic processing chain (mono voice):
//! high-pass (rumble) -> DeepFilterNet 3 noise suppression -> voice EQ
//! (bass / clarity / air) -> leveler -> gain -> limiter.
//!
//! Real-time rules: in the threaded configuration (the audio engine), the
//! real-time thread only does filter math and wait-free ring-buffer
//! push/pop; model inference runs on a worker thread (see `denoise`).
//! Nothing on the real-time path allocates, locks, or does I/O.

use crate::denoise::{attenuation_db, GateConfig, InlineDenoiser, ThreadedDenoiser, HOP};

/// DeepFilterNet's frame (10 ms at 48 kHz).
pub const FRAME: usize = HOP;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MicSettings {
    /// Master switch. When off, audio passes through untouched (still delayed
    /// by the fixed latency so switching does not glitch).
    pub enabled: bool,
    pub noise_suppression: bool,
    /// 0.0 = keep original, 1.0 = fully denoised.
    pub suppression: f32,
    /// Remove low-frequency rumble (desk bumps, fans, plosives) below ~80 Hz.
    pub high_pass: bool,
    /// Even out loud and quiet speech.
    pub leveler: bool,
    /// Output gain applied before the limiter, in dB.
    pub gain_db: f32,
    /// Voice EQ (dB): warmth/body around 150 Hz.
    pub bass_db: f32,
    /// Voice EQ (dB): presence/intelligibility around 4 kHz.
    pub clarity_db: f32,
    /// Voice EQ (dB): "air"/crispness above ~9 kHz.
    pub air_db: f32,
    /// Voice gate: silence between words (the mic "closes").
    pub gate: bool,
    /// Gate sensitivity 0..1: higher opens on quieter speech.
    pub gate_sensitivity: f32,
    /// Tame harsh "s" and "t" sounds.
    pub deesser: bool,
    /// Duck the highs on sharp keyboard/mouse clicks while you talk.
    pub click_suppression: bool,
    /// Tape-style saturation for warmth, 0..1 (0 = off).
    pub warmth: f32,
    /// Mic position: on the desk (farther from the mouth) rather than close.
    /// Only changes how much auto level may boost and how easily the gate
    /// stays open; tone stays with the EQ sliders.
    pub desk_mic: bool,
    /// Which preset the EQ values came from (UI only; the DSP ignores it).
    pub preset: crate::settings::Preset,
}

impl Default for MicSettings {
    fn default() -> Self {
        let mut settings = Self {
            enabled: true,
            noise_suppression: true,
            // ~36 dB: natural-sounding voice; the gate handles silence.
            suppression: 0.9,
            high_pass: true,
            leveler: true,
            gain_db: 0.0,
            bass_db: 0.0,
            clarity_db: 0.0,
            air_db: 0.0,
            gate: true,
            gate_sensitivity: 0.5,
            deesser: true,
            click_suppression: true,
            warmth: 0.25,
            desk_mic: true,
            preset: crate::settings::Preset::Custom,
        };
        crate::settings::Preset::default().apply(&mut settings);
        settings
    }
}

/// RBJ "audio EQ cookbook" biquad: low shelf, peaking or high shelf.
#[derive(Debug, Clone, Copy)]
pub enum EqShape {
    LowShelf,
    Peak,
    HighShelf,
    LowPass,
    BandPass,
}

#[derive(Debug, Clone)]
pub struct Biquad {
    shape: EqShape,
    rate: f32,
    freq: f32,
    q: f32,
    gain_db: f32,
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    pub fn new(shape: EqShape, rate: f32, freq: f32, q: f32) -> Self {
        let mut filter = Self {
            shape,
            rate: rate.max(8000.0),
            freq,
            q,
            gain_db: f32::NAN,
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        };
        filter.set_gain(0.0);
        filter
    }

    /// Recomputes coefficients only when the gain actually changes (cheap
    /// enough to call once per buffer from the real-time thread).
    pub fn set_gain(&mut self, gain_db: f32) {
        if gain_db == self.gain_db {
            return;
        }
        self.gain_db = gain_db;
        let a = 10.0_f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * (self.freq / self.rate).min(0.49);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * self.q);
        let (b0, b1, b2, a0, a1, a2) = match self.shape {
            EqShape::LowPass => (
                (1.0 - cos) / 2.0,
                1.0 - cos,
                (1.0 - cos) / 2.0,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            EqShape::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            EqShape::Peak => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            EqShape::LowShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + s),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - s),
                    (a + 1.0) + (a - 1.0) * cos + s,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - s,
                )
            }
            EqShape::HighShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + s),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - s),
                    (a + 1.0) - (a - 1.0) * cos + s,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - s,
                )
            }
        };
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// Voice EQ: bass (low shelf 150 Hz), a fixed gentle "mud" cut at 350 Hz
/// while bass is boosted, clarity (peak 4 kHz) and air (high shelf 9 kHz).
#[derive(Debug, Clone)]
pub struct VoiceEq {
    bass: Biquad,
    mud: Biquad,
    clarity: Biquad,
    air: Biquad,
    air_enabled: bool,
}

impl VoiceEq {
    pub fn new(rate: f32) -> Self {
        Self {
            bass: Biquad::new(EqShape::LowShelf, rate, 150.0, 0.7),
            mud: Biquad::new(EqShape::Peak, rate, 350.0, 1.2),
            // Presence region: ~4 kHz, broad, avoids the nasal 2.5-3 kHz.
            clarity: Biquad::new(EqShape::Peak, rate, 4000.0, 0.7),
            air: Biquad::new(EqShape::HighShelf, rate, 9000.0, 0.7),
            air_enabled: rate >= 24_000.0,
        }
    }

    pub fn configure(&mut self, settings: &MicSettings) {
        // Tone is only ever set by the sliders (mic position does not
        // touch it). Boosting low end also boosts boxy 300-400 Hz; trim it.
        self.bass.set_gain(settings.bass_db);
        self.mud.set_gain(-(settings.bass_db.max(0.0) * 0.4));
        self.clarity.set_gain(settings.clarity_db);
        self.air.set_gain(if self.air_enabled {
            settings.air_db
        } else {
            0.0
        });
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.clarity.process(self.mud.process(self.bass.process(x)));
        if self.air_enabled {
            self.air.process(y)
        } else {
            y
        }
    }

    pub fn reset(&mut self) {
        self.bass.reset();
        self.mud.reset();
        self.clarity.reset();
        self.air.reset();
    }
}

pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn time_coeff(ms: f32, rate: f32) -> f32 {
    (-1.0 / ((ms.max(0.1) / 1000.0) * rate.max(8000.0))).exp()
}

#[inline]
fn crossfade(dry: f32, wet: f32, mix: f32) -> f32 {
    if mix == 0.0 {
        dry
    } else if mix == 1.0 {
        wet
    } else {
        dry + mix * (wet - dry)
    }
}

/// RBJ biquad high-pass.
#[derive(Debug, Clone)]
struct HighPass {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl HighPass {
    fn new(rate: f32, cutoff: f32) -> Self {
        let rate = rate.max(8000.0);
        let w0 = 2.0 * std::f32::consts::PI * cutoff / rate;
        let alpha = w0.sin() / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let cos = w0.cos();
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 + cos) / 2.0) / a0,
            b1: (-(1.0 + cos)) / a0,
            b2: ((1.0 + cos) / 2.0) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// Envelope follower helper: fast attack, slower release.
fn follow(envelope: &mut f32, level: f32, attack: f32, release: f32) -> f32 {
    let coeff = if level > *envelope { attack } else { release };
    *envelope = coeff * *envelope + (1.0 - coeff) * level;
    *envelope
}

fn detector_power(previous: f32, coeff: f32, signal: f32, reference: f32, mix: f32) -> f32 {
    let weight = 1.0 - coeff;
    let after_eq = weight * signal * signal;
    let input = if mix == 0.0 {
        after_eq
    } else {
        (1.0 - mix) * after_eq + mix * (weight * reference * reference)
    };
    coeff * previous + input
}

/// Automatic level, like Broadcast's auto gain / Sonar's volume stabilizer:
/// slowly steers speech towards a target loudness (only while you talk, so
/// silence is never pumped up), then a light compressor catches peaks.
/// Auto level gain of the most recent stream (dB, f32 bits), shared by all
/// streams in this process so each new one starts at the right level.
static LAST_AUTO_GAIN_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[derive(Debug, Clone)]
struct AutoLevel {
    power: f32,
    power_coeff: f32,
    gain_db: f32,
    up_per_sample: f32,
    down_per_sample: f32,
    peaks: Leveler,
    pre_eq_mix: f32,
    activity_hold_frames: u32,
    activity_remaining: u32,
}

impl AutoLevel {
    /// Target speech level (RMS) and limits of the automatic gain.
    /// Comfortable voice-chat loudness (was -20, which listeners found loud).
    const TARGET_DB: f32 = -23.0;
    const MIN_GAIN_DB: f32 = -10.0;
    /// Below this level nothing is adapted (silence / closed gate).
    const SPEECH_DB: f32 = -50.0;

    /// Starts at the gain the previous stream ended with.
    fn new(rate: f32) -> Self {
        Self::with_gain(
            rate,
            f32::from_bits(LAST_AUTO_GAIN_BITS.load(std::sync::atomic::Ordering::Relaxed)),
        )
    }

    fn with_gain(rate: f32, gain_db: f32) -> Self {
        let rate = rate.max(8000.0);
        Self {
            power: 0.0,
            power_coeff: time_coeff(300.0, rate),
            // Start where the last stream left off, so a new call (or Mic
            // Test) does not begin quiet while the level catches up.
            gain_db,
            // Rise 3 dB/s, fall 12 dB/s: fast enough to catch shouting,
            // slow enough to never pump.
            up_per_sample: 3.0 / rate,
            down_per_sample: 12.0 / rate,
            peaks: Leveler::new(rate),
            pre_eq_mix: 0.0,
            activity_hold_frames: (rate * 0.120).round() as u32,
            activity_remaining: 0,
        }
    }

    /// `max_gain_db`: most boost allowed (more for a distant desk mic).
    fn process_with_activity(
        &mut self,
        x: f32,
        max_gain_db: f32,
        before_eq: f32,
        talking: Option<bool>,
    ) -> f32 {
        // Keep adapting across brief gaps in speech. The denoiser's detector
        // can go quiet while the delayed signal still contains a word ending.
        self.activity_remaining = match talking {
            Some(true) => self.activity_hold_frames,
            Some(false) => self.activity_remaining.saturating_sub(1),
            None => 0, // The denoiser is unavailable or has fallen back.
        };
        self.power = detector_power(self.power, self.power_coeff, x, before_eq, self.pre_eq_mix);
        let level_db = 10.0 * self.power.max(1e-12).log10();
        if self.activity_remaining > 0 && level_db > Self::SPEECH_DB {
            let desired = (Self::TARGET_DB - level_db).clamp(Self::MIN_GAIN_DB, max_gain_db);
            if desired > self.gain_db {
                self.gain_db = (self.gain_db + self.up_per_sample).min(desired);
            } else {
                self.gain_db = (self.gain_db - self.down_per_sample).max(desired);
            }
        }
        LAST_AUTO_GAIN_BITS.store(self.gain_db.to_bits(), std::sync::atomic::Ordering::Relaxed);
        let gain = db_to_linear(self.gain_db);
        self.peaks
            .process(x * gain, before_eq * gain, self.pre_eq_mix)
    }

    fn reset(&mut self) {
        self.power = 0.0;
        self.peaks.envelope = 0.0;
        self.activity_remaining = 0;
    }
}

/// Click suppressor for keyboard and mouse clicks that land *while* you talk
/// (the voice gate is open then, so they would pass).
///
/// A voice is spiky too: every vocal-cord pulse is a small burst in the
/// highs, repeating every 3-12 ms at a similar strength. A click stands out
/// against the voice's *own recent pulses*, so each spike above 1.5 kHz is
/// compared with the strongest spikes of the last ~40 ms; only one 16 dB
/// above them counts. Only armed during ongoing speech (gate open > 50 ms),
/// which is when clicks can get through. With a 2 ms lookahead the highs are
/// ducked by 12 dB for ~8 ms just before the click. Transparent otherwise.
#[derive(Debug, Clone)]
struct ClickSuppressor {
    detect: HighPass,
    split: Biquad,
    /// Spike heights over the look-ahead window (same indexing as `delay`).
    history: [f32; 1024],
    /// Decaying memory of recent (non-click) spike heights.
    recent: f32,
    recent_decay: f32,
    delay: [f32; 1024],
    delay_len: usize,
    position: usize,
    gain: f32,
    hold: u32,
    hold_samples: u32,
    release: f32,
}

impl ClickSuppressor {
    /// A spike must exceed recent voice spikes by this factor (+16 dB).
    const RATIO: f32 = 6.0;
    /// ...and be at least this loud in the upper band (about -34 dBFS).
    const MIN_LEVEL: f32 = 0.02;
    /// Gain applied to the highs during a click (-12 dB).
    const FLOOR: f32 = 0.25;

    fn new(rate: f32) -> Self {
        Self {
            detect: HighPass::new(rate, 1500.0),
            split: Biquad::new(EqShape::LowPass, rate, 1500.0, 0.7),
            recent: 0.0,
            history: [0.0; 1024],
            recent_decay: time_coeff(40.0, rate),
            delay: [0.0; 1024],
            delay_len: ((rate * 0.002) as usize).clamp(1, 1024),
            position: 0,
            gain: 1.0,
            hold: 0,
            hold_samples: (rate * 0.008) as u32,
            release: time_coeff(6.0, rate),
        }
    }

    /// `armed`: you have been talking for a moment (see [`CleanMic`]).
    fn process(&mut self, x: f32, armed: bool, enabled: bool) -> f32 {
        // Detect on the undelayed signal (lookahead).
        let high = self.detect.process(x).abs();
        let is_click =
            enabled && armed && high > Self::MIN_LEVEL && high > self.recent * Self::RATIO;
        if is_click {
            self.hold = self.hold_samples;
        }
        // The memory learns from spikes at least 2 ms old (they leave this
        // history ring as the matching audio leaves the delay line), so a
        // click's own first samples cannot hide it from detection, while the
        // memory still keeps up with the voice's regular pulses.
        let old = self.history[self.position];
        self.history[self.position] = high;
        self.recent = old.max(self.recent * self.recent_decay);
        let target = if self.hold > 0 {
            self.hold -= 1;
            Self::FLOOR
        } else {
            1.0
        };
        self.gain = if target < self.gain {
            target
        } else {
            self.release * self.gain + (1.0 - self.release) * target
        };

        // Apply to the delayed signal: low band untouched, highs ducked.
        let delayed = self.delay[self.position];
        self.delay[self.position] = x;
        self.position = (self.position + 1) % self.delay_len;
        let low = self.split.process(delayed);
        if !enabled && self.gain >= 0.9999 {
            // Switched off: a pure delay, bit for bit.
            self.gain = 1.0;
            return delayed;
        }
        low + (delayed - low) * self.gain
    }
}

/// Pop guard: when a plosive ("p", "b") slams the low end (<160 Hz),
/// briefly high-pass the voice at 150 Hz. Transparent otherwise.
#[derive(Debug, Clone)]
struct PopGuard {
    low: Biquad,
    cut: HighPass,
    envelope: f32,
    attack: f32,
    release: f32,
}

impl PopGuard {
    const THRESHOLD: f32 = 0.063; // -24 dBFS
    const RATIO: f32 = 8.0;

    fn new(rate: f32) -> Self {
        Self {
            low: Biquad::new(EqShape::LowPass, rate, 160.0, 0.7),
            cut: HighPass::new(rate, 150.0),
            envelope: 0.0,
            attack: time_coeff(1.0, rate),
            release: time_coeff(80.0, rate),
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let low = self.low.process(x);
        follow(&mut self.envelope, low.abs(), self.attack, self.release);
        // Blend towards a 150 Hz high-passed copy while a pop is detected;
        // with no pop (gain 1) the output is exactly the input.
        let cut = self.cut.process(x);
        x + (cut - x) * (1.0 - self.gain())
    }

    fn gain(&self) -> f32 {
        if self.envelope > Self::THRESHOLD {
            (self.envelope / Self::THRESHOLD).powf(1.0 / Self::RATIO - 1.0)
        } else {
            1.0
        }
    }
}

/// De-esser (relative): turns down only the highs while the 5-9 kHz
/// sibilance band *dominates* the voice, which is what a harsh "s"/"t" is.
/// Normal speech, whose energy sits lower, is left alone at any loudness.
#[derive(Debug, Clone)]
struct DeEsser {
    detect: Biquad,
    split: Biquad,
    sibilance: f32,
    overall: f32,
    attack: f32,
    release: f32,
    reduction_mix: f32,
}

impl DeEsser {
    /// Sibilance band level relative to the whole voice that starts acting.
    const RELATIVE: f32 = 0.4;
    /// Ignore very quiet hiss (-50 dBFS in the band).
    const MIN_LEVEL: f32 = 0.003;
    const RATIO: f32 = 6.0;
    /// Never turn the highs down by more than 12 dB.
    const MAX_REDUCTION: f32 = 0.25;

    fn new(rate: f32) -> Self {
        Self {
            detect: Biquad::new(EqShape::BandPass, rate, 7000.0, 0.8),
            split: Biquad::new(EqShape::LowPass, rate, 4500.0, 0.7),
            sibilance: 0.0,
            overall: 0.0,
            attack: time_coeff(1.0, rate),
            release: time_coeff(60.0, rate),
            reduction_mix: 1.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let band = self.detect.process(x).abs();
        follow(&mut self.sibilance, band, self.attack, self.release);
        follow(&mut self.overall, x.abs(), self.attack, self.release);
        let low = self.split.process(x);
        low + (x - low) * self.gain()
    }

    fn gain(&self) -> f32 {
        let reference = self.overall * Self::RELATIVE;
        if self.sibilance > Self::MIN_LEVEL && self.sibilance > reference && reference > 0.0 {
            let gain = (self.sibilance / reference)
                .powf(1.0 / Self::RATIO - 1.0)
                .max(Self::MAX_REDUCTION);
            if self.reduction_mix == 1.0 {
                gain
            } else {
                1.0 - (1.0 - gain) * self.reduction_mix
            }
        } else {
            1.0
        }
    }
}

/// Warmth: gentle tape-style saturation blended in parallel (0 = off).
/// Adds the low-order harmonics a good preamp gives a voice; at normal
/// speech levels the effect is subtle, louder syllables get rounder.
fn warmth(x: f32, amount: f32) -> f32 {
    if amount <= 0.0 {
        return x;
    }
    const DRIVE: f32 = 2.5;
    let saturated = (x * DRIVE).tanh() / DRIVE;
    // Blend at most 60% so it never turns into audible distortion.
    let mix = amount.clamp(0.0, 1.0) * 0.6;
    x + (saturated - x) * mix
}

/// Voice compressor used inside [`AutoLevel`]: follows the short-term RMS
/// level (5 ms), 3.5:1 with an 8 dB soft knee starting 8 dB below the
/// auto-level target, 5 ms attack and 120 ms release. Because it works
/// right at speech level, every syllable comes out at about the same
/// loudness (the smooth Broadcast/Sonar sound) instead of only the loudest
/// peaks being touched. Make-up gain keeps speech at the target overall.
#[derive(Debug, Clone)]
struct Leveler {
    power: f32,
    power_coeff: f32,
    gain_db: f32,
    attack: f32,
    release: f32,
    envelope: f32,
}

impl Leveler {
    const THRESHOLD_DB: f32 = AutoLevel::TARGET_DB - 8.0;
    const RATIO: f32 = 3.5;
    const KNEE_DB: f32 = 8.0;

    fn new(rate: f32) -> Self {
        Self {
            power: 0.0,
            power_coeff: time_coeff(5.0, rate),
            gain_db: 0.0,
            attack: time_coeff(5.0, rate),
            release: time_coeff(120.0, rate),
            envelope: 0.0,
        }
    }

    /// Make-up gain: speech sitting at the auto-level target comes out at
    /// the target, louder syllables are pulled down, softer ones lifted.
    fn makeup_db() -> f32 {
        -Self::reduction_db(AutoLevel::TARGET_DB)
    }

    /// Gain reduction (dB, <= 0) for a level, with a quadratic soft knee.
    fn reduction_db(level_db: f32) -> f32 {
        let over = level_db - Self::THRESHOLD_DB;
        let slope = 1.0 / Self::RATIO - 1.0;
        let half = Self::KNEE_DB / 2.0;
        if over <= -half {
            0.0
        } else if over < half {
            slope * (over + half).powi(2) / (2.0 * Self::KNEE_DB)
        } else {
            slope * over
        }
    }

    fn process(&mut self, x: f32, reference: f32, mix: f32) -> f32 {
        self.power = detector_power(self.power, self.power_coeff, x, reference, mix);
        let level_db = 10.0 * self.power.max(1e-12).log10();
        // Make-up applies to speech-level sound only: it fades out between
        // -45 and -55 dBFS so hiss and room tone between words stay put.
        let speech = ((level_db + 55.0) / 10.0).clamp(0.0, 1.0);
        let target = Self::reduction_db(level_db) + Self::makeup_db() * speech;
        // Gain moves towards more reduction at the attack rate, back at release.
        let coeff = if target < self.gain_db {
            self.attack
        } else {
            self.release
        };
        self.gain_db = coeff * self.gain_db + (1.0 - coeff) * target;
        self.envelope = self.gain_db;
        x * db_to_linear(self.gain_db)
    }
}

/// Look-ahead peak limiter: sees peaks 1.5 ms early and turns the level
/// down smoothly before they arrive (no clipping crunch), releases over
/// ~60 ms. A final hard clamp guarantees the ceiling.
#[derive(Debug, Clone)]
struct Limiter {
    ceiling: f32,
    release: f32,
    attack: f32,
    delay: [f32; 1024],
    delay_len: usize,
    position: usize,
    held: f32,
    hold: usize,
    gain: f32,
    envelope: f32,
    /// Samples where the safety clamp (not the smooth gain) did the limiting.
    clamped: u64,
}

impl Limiter {
    fn new(rate: f32) -> Self {
        let delay_len = ((rate * 0.0015) as usize).clamp(1, 1024);
        Self {
            ceiling: db_to_linear(-1.0),
            release: time_coeff(60.0, rate),
            // Reach the required gain within the look-ahead window.
            attack: time_coeff(0.3, rate),
            delay: [0.0; 1024],
            delay_len,
            position: 0,
            held: 1.0,
            hold: 0,
            gain: 1.0,
            envelope: 0.0,
            clamped: 0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        // Gain needed for the incoming (not yet output) sample.
        let level = x.abs();
        let needed = if level > self.ceiling {
            self.ceiling / level
        } else {
            1.0
        };
        if needed <= self.held {
            self.held = needed;
            self.hold = self.delay_len;
        } else if self.hold > 0 {
            self.hold -= 1;
        } else {
            self.held = self.release * self.held + (1.0 - self.release);
        }
        // Aim 0.2 dB below the needed gain so the exponential approach lands
        // under the ceiling within the look-ahead window.
        let target = if self.held < 1.0 {
            self.held * 0.977
        } else {
            1.0
        };
        self.gain = if target < self.gain {
            self.attack * self.gain + (1.0 - self.attack) * target
        } else {
            target
        };
        self.envelope = self.gain;

        let delayed = self.delay[self.position];
        self.delay[self.position] = x;
        self.position = (self.position + 1) % self.delay_len;
        let limited = delayed * self.gain;
        if limited.abs() > self.ceiling {
            self.clamped += 1;
        }
        limited.clamp(-self.ceiling, self.ceiling)
    }
}

enum Denoiser {
    /// Audio engine: inference on a worker thread.
    Threaded(ThreadedDenoiser),
    /// Tests and offline preview: inference on the calling thread.
    Inline(Box<InlineDenoiser>),
}

impl Denoiser {
    fn speech_activity(&self) -> Option<bool> {
        match self {
            Self::Threaded(denoiser) => denoiser.speech_activity(),
            Self::Inline(denoiser) => denoiser.is_loaded().then(|| denoiser.is_talking()),
        }
    }

    fn gate_open(&self) -> bool {
        match self {
            Self::Threaded(denoiser) => denoiser.gate_open(),
            Self::Inline(denoiser) => denoiser.gate_open(),
        }
    }
}

pub struct CleanMic {
    channels: usize,
    high_pass: HighPass,
    denoiser: Denoiser,
    click_suppressor: ClickSuppressor,
    /// Samples since the voice gate opened (arms the click suppressor).
    speaking: u32,
    arm_after: u32,
    pop_guard: PopGuard,
    eq: VoiceEq,
    deesser: DeEsser,
    auto_level: AutoLevel,
    limiter: Limiter,
    rate: u32,
    output_gain: f32,
    gain_coeff: f32,
    bypass_delay: [f32; 2048],
    bypass_delay_len: usize,
    bypass_position: usize,
    wet_mix: f32,
    high_pass_mix: f32,
    deesser_mix: f32,
    /// 0 = auto level off, 1 = on; ramps so the toggle does not step the level.
    leveler_mix: f32,
    /// False until the first buffer: the mixes then start at the settings'
    /// values instead of fading in from "everything on".
    started: bool,
}

/// Per-sample observations for the offline renderer. The gain array contains
/// click, pop blend, auto gain, compressor, de-esser, and limiter gain in dB.
/// The pop blend value describes the low-end mix, not full-band attenuation.
pub struct MicTrace {
    pub audio: [f32; 8],
    pub gain_db: [f32; 6],
}

impl MicTrace {
    pub const STAGES: [&'static str; 8] = [
        "denoised",
        "click",
        "pop",
        "eq",
        "level",
        "saturation",
        "deess",
        "limiter",
    ];
    pub const GAINS: [&'static str; 6] = [
        "click",
        "pop_blend",
        "auto",
        "compressor",
        "deess",
        "limiter",
    ];
}

impl CleanMic {
    /// For the audio engine. Allocates and starts the denoiser worker; call
    /// outside the real-time thread.
    pub fn new(rate: u32, channels: usize) -> Self {
        Self::build(
            rate,
            channels,
            true,
            crate::denoise::DenoiseOptions::default(),
        )
    }

    /// Runs inference on the calling thread (tests, offline preview).
    pub fn new_inline(rate: u32, channels: usize) -> Self {
        Self::build(
            rate,
            channels,
            false,
            crate::denoise::DenoiseOptions::default(),
        )
    }

    /// Fixed denoiser variants for offline, level-matched comparisons.
    pub fn new_inline_with_options(
        rate: u32,
        channels: usize,
        options: crate::denoise::DenoiseOptions,
    ) -> Self {
        let mut chain = Self::build(rate, channels, false, options);
        let ms = options.leveler_detector_ms;
        if ms.is_finite() && ms > 0.0 && ms != 5.0 {
            chain.auto_level.peaks.power_coeff = time_coeff(ms, rate.max(8_000) as f32);
        }
        if options.pre_eq_detector_mix.is_finite() {
            chain.auto_level.pre_eq_mix = options.pre_eq_detector_mix.clamp(0.0, 1.0);
        }
        if options.deesser_reduction_mix.is_finite() {
            chain.deesser.reduction_mix = options.deesser_reduction_mix.clamp(0.0, 1.0);
        }
        chain
    }

    fn build(
        rate: u32,
        channels: usize,
        threaded: bool,
        options: crate::denoise::DenoiseOptions,
    ) -> Self {
        let rate = rate.max(8_000);
        let rate_f = rate as f32;
        // Any microphone rate works: the denoiser converts to the model's
        // 48 kHz and back internally.
        let denoiser = if threaded {
            Denoiser::Threaded(ThreadedDenoiser::new(rate))
        } else {
            Denoiser::Inline(Box::new(InlineDenoiser::with_options(rate, options)))
        };
        let click_suppressor = ClickSuppressor::new(rate_f);
        let limiter = Limiter::new(rate_f);
        let bypass_delay_len = click_suppressor.delay_len + limiter.delay_len;
        Self {
            channels: channels.max(1),
            high_pass: HighPass::new(rate_f, 70.0),
            denoiser,
            click_suppressor,
            speaking: 0,
            arm_after: (rate_f * 0.05) as u32,
            pop_guard: PopGuard::new(rate_f),
            eq: VoiceEq::new(rate_f),
            deesser: DeEsser::new(rate_f),
            auto_level: if threaded {
                AutoLevel::new(rate_f)
            } else {
                AutoLevel::with_gain(rate_f, 0.0)
            },
            limiter,
            rate,
            output_gain: 1.0,
            gain_coeff: time_coeff(20.0, rate_f),
            bypass_delay: [0.0; 2048],
            bypass_delay_len: bypass_delay_len.clamp(1, 2048),
            bypass_position: 0,
            wet_mix: 1.0,
            high_pass_mix: 1.0,
            deesser_mix: 1.0,
            leveler_mix: 1.0,
            started: false,
        }
    }

    /// Approximate end-to-end latency in frames: the model window and
    /// two-frame lookahead, the gate's one-frame lookahead and the worker
    /// cushion.
    pub fn latency_frames(rate: u32) -> usize {
        (rate as usize * 60).div_ceil(1000)
    }

    /// Clears filter history (used when the stream restarts).
    pub fn reset(&mut self) {
        self.high_pass.reset();
        self.eq.reset();
        self.auto_level.reset();
        self.limiter.envelope = 0.0;
        self.started = false;
    }

    /// Processes interleaved samples in place. All channels receive the same
    /// cleaned mono voice signal (microphones are mono sources).
    pub fn process(&mut self, samples: &mut [f32], settings: &MicSettings) {
        self.process_inner::<false>(samples, settings, |_| {});
    }

    /// Render the same inline chain with taps at the end of each stage.
    pub fn process_traced(
        &mut self,
        samples: &mut [f32],
        settings: &MicSettings,
        trace: impl FnMut(MicTrace),
    ) {
        assert!(matches!(self.denoiser, Denoiser::Inline(_)));
        self.process_inner::<true>(samples, settings, trace);
    }

    /// Delay of each tap relative to the post-denoiser tap.
    pub fn trace_delays(&self, settings: &MicSettings) -> [usize; 8] {
        let _ = settings;
        let click = self.click_suppressor.delay_len;
        let limiter = self.limiter.delay_len;
        [0, click, click, click, click, click, click, click + limiter]
    }

    fn process_inner<const TRACE: bool>(
        &mut self,
        samples: &mut [f32],
        settings: &MicSettings,
        mut trace: impl FnMut(MicTrace),
    ) {
        let active = settings.enabled && settings.noise_suppression;
        let atten = attenuation_db(settings.suppression, active);
        let gate = GateConfig::from_sensitivity(
            active && settings.gate,
            settings.gate_sensitivity,
            settings.desk_mic,
        );
        if let Denoiser::Threaded(denoiser) = &self.denoiser {
            denoiser.configure(atten, gate);
        }
        self.eq.configure(settings);
        let target_gain = db_to_linear(settings.gain_db.clamp(-24.0, 24.0));
        let (mut in_peak, mut out_peak) = (0.0_f32, 0.0_f32);
        // Click suppression only during ongoing speech (gate open > 50 ms).
        let frames = (samples.len() / self.channels) as u32;
        self.speaking = if self.denoiser.gate_open() {
            self.speaking.saturating_add(frames)
        } else {
            0
        };
        let armed = self.speaking >= self.arm_after;
        if !self.started && frames != 0 {
            self.started = true;
            let on = |flag: bool| if flag { 1.0 } else { 0.0 };
            self.wet_mix = on(settings.enabled);
            self.high_pass_mix = on(settings.enabled && settings.high_pass);
            self.deesser_mix = on(settings.enabled && settings.deesser && self.rate >= 24_000);
            self.leveler_mix = on(settings.leveler);
            self.output_gain = target_gain;
        }

        for frame in samples.chunks_exact_mut(self.channels) {
            let control_step = 1.0 / (self.rate as f32 * 0.010).max(1.0);
            let high_pass_target = if settings.enabled && settings.high_pass {
                1.0
            } else {
                0.0
            };
            self.high_pass_mix +=
                (high_pass_target - self.high_pass_mix).clamp(-control_step, control_step);
            let deesser_target = if settings.enabled && settings.deesser && self.rate >= 24_000 {
                1.0
            } else {
                0.0
            };
            self.deesser_mix +=
                (deesser_target - self.deesser_mix).clamp(-control_step, control_step);
            let mut mono = frame.iter().sum::<f32>() / self.channels as f32;
            if !mono.is_finite() {
                mono = 0.0;
            }
            in_peak = in_peak.max(mono.abs());
            let filtered = self.high_pass.process(mono);
            mono = crossfade(mono, filtered, self.high_pass_mix);
            let mut out = match &mut self.denoiser {
                Denoiser::Threaded(denoiser) => {
                    denoiser.push(mono);
                    denoiser.pop(mono)
                }
                Denoiser::Inline(denoiser) => denoiser.process(mono, atten, gate),
            };
            let mut stages = [out; 8];
            let bypass = self.bypass_delay[self.bypass_position];
            self.bypass_delay[self.bypass_position] = out;
            self.bypass_position = (self.bypass_position + 1) % self.bypass_delay_len;
            out = self.click_suppressor.process(
                out,
                armed,
                settings.enabled && settings.click_suppression && settings.noise_suppression,
            );
            {
                stages[1] = out;
                let guarded = self.pop_guard.process(out);
                out = crossfade(out, guarded, self.high_pass_mix);
                let before_eq = out;
                stages[2] = out;
                // Broadcast order: EQ -> compression -> colour -> de-ess ->
                // limiter (compression brings sibilance up, so de-ess after).
                out = self.eq.process(out);
                stages[3] = out;
                // The level difference can be many dB, so the toggle fades
                // over 50 ms; fully off, the leveler is not run at all.
                let leveler_target = if settings.leveler { 1.0 } else { 0.0 };
                self.leveler_mix += (leveler_target - self.leveler_mix)
                    .clamp(-control_step * 0.2, control_step * 0.2);
                if self.leveler_mix > 0.0 {
                    let max_gain = if settings.desk_mic { 18.0 } else { 12.0 };
                    let leveled = self.auto_level.process_with_activity(
                        out,
                        max_gain,
                        before_eq,
                        self.denoiser.speech_activity(),
                    );
                    out = crossfade(out, leveled, self.leveler_mix);
                } else {
                    self.auto_level.activity_remaining = 0;
                }
                stages[4] = out;
                out = warmth(out, settings.warmth);
                stages[5] = out;
                if self.rate >= 24_000 {
                    let deessed = self.deesser.process(out);
                    out = crossfade(out, deessed, self.deesser_mix);
                }
                stages[6] = out;
                out = self.limiter.process(out * self.output_gain);
                stages[7] = out;
            }
            let target_mix = if settings.enabled { 1.0 } else { 0.0 };
            self.wet_mix += (target_mix - self.wet_mix).clamp(-control_step, control_step);
            out = crossfade(bypass, out, self.wet_mix);
            self.output_gain += (1.0 - self.gain_coeff) * (target_gain - self.output_gain);
            if TRACE {
                let db = |enabled, value: f32| {
                    if enabled {
                        20.0 * value.max(1e-12).log10()
                    } else {
                        0.0
                    }
                };
                let mut gain_db = [0.0; 6];
                if settings.enabled {
                    gain_db[0] = db(
                        settings.click_suppression && settings.noise_suppression,
                        self.click_suppressor.gain,
                    );
                    gain_db[1] = db(settings.high_pass, self.pop_guard.gain());
                    if settings.leveler {
                        gain_db[2] = self.auto_level.gain_db;
                        gain_db[3] = self.auto_level.peaks.gain_db;
                    }
                    gain_db[4] = db(settings.deesser, self.deesser.gain());
                    gain_db[5] = db(true, self.limiter.gain);
                }
                trace(MicTrace {
                    audio: stages,
                    gain_db,
                });
            }
            out_peak = out_peak.max(out.abs());
            for sample in frame.iter_mut() {
                *sample = out;
            }
        }
        if let Denoiser::Threaded(denoiser) = &self.denoiser {
            crate::denoise::GATE_OPEN
                .store(denoiser.gate_open(), std::sync::atomic::Ordering::Relaxed);
            crate::settings::record_levels(in_peak, out_peak);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn tone(len: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin())
            .collect()
    }

    /// Deterministic white noise in [-amp, amp].
    fn noise(len: usize, amp: f32, seed: u32) -> Vec<f32> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// Low-passed noise: closer to fans, room tone and PC hum.
    fn room_noise(len: usize, amp: f32) -> Vec<f32> {
        let mut state = 0.0;
        noise(len, 1.0, 7)
            .into_iter()
            .map(|w| {
                state = 0.9 * state + w;
                state * amp * 0.3
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    fn db(ratio: f32) -> f32 {
        20.0 * ratio.max(1e-9).log10()
    }

    /// Only the noise canceller: no EQ, leveler or high-pass.
    fn denoise_only() -> MicSettings {
        MicSettings {
            leveler: false,
            high_pass: false,
            bass_db: 0.0,
            clarity_db: 0.0,
            air_db: 0.0,
            deesser: false,
            click_suppression: false,
            warmth: 0.0,
            ..MicSettings::default()
        }
    }

    #[test]
    fn suppresses_room_noise() {
        let input = room_noise(96_000, 0.05);
        let mut buf = input.clone();
        CleanMic::new_inline(48_000, 1).process(&mut buf, &denoise_only());
        let reduction = db(rms(&buf[48_000..]) / rms(&input[48_000..]));
        assert!(
            reduction < -25.0,
            "room noise only reduced by {reduction:.1} dB"
        );
    }

    #[test]
    fn removes_keyboard_like_clicks() {
        // Short broadband bursts (key presses) every 150 ms over quiet room tone.
        let mut input = room_noise(96_000, 0.002);
        let clicks = noise(96_000, 0.5, 99);
        for start in (0..96_000).step_by(7_200) {
            for i in start..(start + 240).min(96_000) {
                let decay = (-((i - start) as f32) / 40.0).exp();
                input[i] += clicks[i] * decay;
            }
        }
        let mut buf = input.clone();
        CleanMic::new_inline(48_000, 1).process(&mut buf, &denoise_only());
        let reduction = db(rms(&buf[24_000..]) / rms(&input[24_000..]));
        assert!(
            reduction < -15.0,
            "clicks only reduced by {reduction:.1} dB"
        );
    }

    #[test]
    fn disabled_passes_level_through() {
        let settings = MicSettings {
            enabled: false,
            ..MicSettings::default()
        };
        let input = room_noise(96_000, 0.05);
        let mut buf = input.clone();
        CleanMic::new_inline(48_000, 1).process(&mut buf, &settings);
        let change = db(rms(&buf[48_000..]) / rms(&input[48_000..]));
        assert!(change.abs() < 1.5, "bypass changed level by {change:.1} dB");
    }

    #[test]
    fn pre_eq_reference_drives_both_level_detectors() {
        let mut baseline = AutoLevel::with_gain(SR, 0.0);
        let mut with_eq = AutoLevel::with_gain(SR, 0.0);
        with_eq.pre_eq_mix = 1.0;
        for sample in tone(48_000, 180.0, 0.025) {
            let reference = baseline.process_with_activity(sample, 12.0, sample, Some(true));
            let boosted = with_eq.process_with_activity(sample * 2.0, 12.0, sample, Some(true));
            assert_eq!(boosted, reference * 2.0);
        }
        assert!(with_eq.gain_db > 2.0);
        assert!(with_eq.peaks.gain_db.abs() > 0.1);
    }

    #[test]
    fn voice_eq_adds_bass_and_air() {
        let mut eq = VoiceEq::new(SR);
        let settings = MicSettings {
            bass_db: 6.0,
            clarity_db: 0.0,
            air_db: 6.0,
            ..MicSettings::default()
        };
        eq.configure(&settings);
        for (freq, expect_min) in [(100.0, 4.0), (12_000.0, 4.0), (1_000.0, -1.5)] {
            let input = tone(48_000, freq, 0.1);
            let output: Vec<f32> = input.iter().map(|&x| eq.process(x)).collect();
            let gain = db(rms(&output[24_000..]) / rms(&input[24_000..]));
            assert!(gain >= expect_min, "{freq} Hz gain {gain:.1} dB");
            if freq == 1_000.0 {
                assert!(
                    gain.abs() < 1.5,
                    "1 kHz should be ~unchanged, got {gain:.1} dB"
                );
            }
            eq.reset();
        }
    }

    #[test]
    fn stays_bounded_and_finite() {
        let settings = MicSettings {
            gain_db: 24.0,
            ..MicSettings::default()
        };
        let mut chain = CleanMic::new_inline(48_000, 2);
        let mut buf = tone(48_000, 200.0, 4.0);
        buf[1000] = f32::NAN;
        buf[2000] = f32::INFINITY;
        chain.process(&mut buf, &settings);
        let ceiling = db_to_linear(-1.0) + 1e-4;
        assert!(buf.iter().all(|s| s.is_finite() && s.abs() <= ceiling));
    }

    #[test]
    fn stereo_channels_get_the_same_signal() {
        let mut chain = CleanMic::new_inline(48_000, 2);
        let mut buf = tone(9600, 300.0, 0.3);
        chain.process(&mut buf, &MicSettings::default());
        for frame in buf.as_chunks::<2>().0 {
            assert_eq!(frame[0], frame[1]);
        }
    }

    #[test]
    fn other_sample_rates_get_noise_removal_too() {
        for rate in [44_100_u32, 16_000] {
            let input = room_noise(rate as usize * 2, 0.05);
            let mut buf = input.clone();
            CleanMic::new_inline(rate, 1).process(&mut buf, &denoise_only());
            assert!(buf.iter().all(|s| s.is_finite()));
            let half = rate as usize;
            let reduction = db(rms(&buf[half..]) / rms(&input[half..]));
            assert!(reduction < -20.0, "{rate} Hz: only {reduction:.1} dB");
        }
    }

    #[test]
    fn deesser_tames_sibilance_but_not_voice() {
        let mut deesser = DeEsser::new(SR);
        let harsh = tone(48_000, 7_000.0, 0.4);
        let out: Vec<f32> = harsh.iter().map(|&x| deesser.process(x)).collect();
        assert!(db(rms(&out[24_000..]) / rms(&harsh[24_000..])) < -4.0);
        let mut deesser = DeEsser::new(SR);
        let voice = tone(48_000, 300.0, 0.4);
        let out: Vec<f32> = voice.iter().map(|&x| deesser.process(x)).collect();
        assert!(db(rms(&out[24_000..]) / rms(&voice[24_000..])).abs() < 0.5);
    }

    #[test]
    fn pop_guard_squashes_plosives_only() {
        let mut guard = PopGuard::new(SR);
        let pop = tone(4_800, 60.0, 0.8);
        let out: Vec<f32> = pop.iter().map(|&x| guard.process(x)).collect();
        assert!(db(rms(&out[2_400..]) / rms(&pop[2_400..])) < -6.0);
        let mut guard = PopGuard::new(SR);
        let voice = tone(48_000, 1_000.0, 0.3);
        let out: Vec<f32> = voice.iter().map(|&x| guard.process(x)).collect();
        assert!(db(rms(&out[24_000..]) / rms(&voice[24_000..])).abs() < 0.5);
    }

    #[test]
    fn auto_level_brings_quiet_speech_up_and_ignores_silence() {
        let mut level = AutoLevel::with_gain(SR, 0.0);
        // -32 dBFS RMS "speech" for 6 s: should be raised towards -20 dBFS.
        let quiet = tone(48_000 * 6, 220.0, 0.035);
        let out: Vec<f32> = quiet
            .iter()
            .map(|&x| level.process_with_activity(x, 12.0, x, Some(true)))
            .collect();
        let gained = db(rms(&out[48_000 * 5..]) / rms(&quiet[48_000 * 5..]));
        assert!(gained > 8.0, "only {gained:.1} dB of gain");
        // An audible non-speech signal must not train the automatic gain.
        let mut level = AutoLevel::with_gain(SR, 0.0);
        for &sample in &quiet {
            level.process_with_activity(sample, 12.0, sample, Some(false));
        }
        assert_eq!(level.gain_db, 0.0);
    }

    #[test]
    fn auto_level_bridges_speech_gaps_but_freezes_on_pause_or_failure() {
        let mut level = AutoLevel::with_gain(SR, 0.0);
        let sample = 0.025;
        level.power = sample * sample;
        level.process_with_activity(sample, 12.0, sample, Some(true));
        let speech_gain = level.gain_db;
        for _ in 1..level.activity_hold_frames {
            level.process_with_activity(sample, 12.0, sample, Some(false));
        }
        assert!(level.gain_db > speech_gain);
        let held_gain = level.gain_db;
        level.process_with_activity(sample, 12.0, sample, Some(false));
        assert_eq!(level.gain_db, held_gain);

        level.process_with_activity(sample, 12.0, sample, Some(true));
        let gain_before_failure = level.gain_db;
        assert!(gain_before_failure > held_gain);
        level.process_with_activity(sample, 12.0, sample, None);
        level.process_with_activity(sample, 12.0, sample, Some(false));
        assert_eq!(level.gain_db, gain_before_failure);
    }

    #[test]
    fn threaded_worker_processes_in_real_time() {
        // Feed 10 ms blocks at roughly real-time pace, like the audio engine.
        let mut chain = CleanMic::new(48_000, 1);
        let input = room_noise(48_000 * 2, 0.05);
        let mut output = input.clone();
        for block in output.chunks_mut(480) {
            chain.process(block, &denoise_only());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(output.iter().all(|s| s.is_finite()));
        let noise_floor = rms(&output[48_000..]);
        assert!(noise_floor > 0.0, "the worker only returned silence");
        let reduction = db(noise_floor / rms(&input[48_000..]));
        assert!(
            reduction < -20.0,
            "threaded: noise only reduced by {reduction:.1} dB"
        );
        let Denoiser::Threaded(denoiser) = &chain.denoiser else {
            panic!("expected threaded denoiser");
        };
        assert!(denoiser.is_ready(), "model should be loaded");
    }

    #[test]
    fn click_suppressor_ducks_clicks_but_not_voice() {
        // Voice-like signal with upper harmonics, plus sharp clicks.
        let voice: Vec<f32> = (0..48_000)
            .map(|i| {
                let t = i as f32 / SR;
                (1..=12)
                    .map(|h| (2.0 * std::f32::consts::PI * 180.0 * h as f32 * t).sin() / h as f32)
                    .sum::<f32>()
                    * 0.12
            })
            .collect();
        let mut with_clicks = voice.clone();
        let burst = noise(48_000, 0.8, 11);
        let mut click_mask = vec![false; 48_000];
        for start in (4_800..48_000).step_by(9_600) {
            for i in start..start + 96 {
                let decay = (-((i - start) as f32) / 20.0).exp();
                with_clicks[i] += burst[i] * decay;
                click_mask[i] = true;
            }
        }
        let mut suppressor = ClickSuppressor::new(SR);
        let out: Vec<f32> = with_clicks
            .iter()
            .map(|&x| suppressor.process(x, true, true))
            .collect();
        // Compare click energy (output delayed by the 2 ms lookahead).
        let delay = (SR * 0.002) as usize;
        let (mut before, mut after) = (0.0_f32, 0.0_f32);
        for i in 0..48_000 - delay {
            if click_mask[i] {
                before += (with_clicks[i] - voice[i]).powi(2);
                after += (out[i + delay] - voice[i]).powi(2);
            }
        }
        assert!(
            db((after / before).sqrt()) < -6.0,
            "clicks only reduced by {:.1} dB",
            db((after / before).sqrt())
        );

        // Voice alone passes through unchanged (apart from the delay).
        let mut suppressor = ClickSuppressor::new(SR);
        let clean: Vec<f32> = voice
            .iter()
            .map(|&x| suppressor.process(x, true, true))
            .collect();
        let change = db(rms(&clean[24_000..]) / rms(&voice[24_000 - delay..48_000 - delay]));
        assert!(change.abs() < 0.5, "voice changed by {change:.1} dB");
    }

    #[test]
    fn lookahead_limiter_never_exceeds_ceiling_and_is_smooth() {
        let mut limiter = Limiter::new(SR);
        let hot = tone(48_000, 200.0, 3.0);
        let out: Vec<f32> = hot.iter().map(|&x| limiter.process(x)).collect();
        let ceiling = db_to_linear(-1.0) + 1e-6;
        assert!(out.iter().all(|s| s.abs() <= ceiling));
        // Smooth gain: the clamp should almost never be what limits.
        let clamped = limiter.clamped as usize;
        assert!(
            clamped < out.len() / 50,
            "{clamped} samples hit the hard clamp"
        );
    }

    #[test]
    fn vocal_compressor_evens_out_loud_speech() {
        // Speech at the auto-level target comes out at the target.
        let at_target = db_to_linear(AutoLevel::TARGET_DB) * std::f32::consts::SQRT_2;
        let mut comp = Leveler::new(SR);
        let input = tone(48_000, 220.0, at_target);
        let out: Vec<f32> = input.iter().map(|&x| comp.process(x, x, 0.0)).collect();
        let change = db(rms(&out[24_000..]) / rms(&input[24_000..]));
        assert!(change.abs() < 0.5, "target level changed by {change:.1} dB");

        // Syllables 6 dB above and below the target: a 12 dB swing comes
        // out as under 5 dB.
        let level = |amp: f32| {
            let mut comp = Leveler::new(SR);
            let input = tone(48_000, 220.0, amp);
            let out: Vec<f32> = input.iter().map(|&x| comp.process(x, x, 0.0)).collect();
            db(rms(&out[24_000..]))
        };
        let swing = level(at_target * 2.0) - level(at_target * 0.5);
        assert!(swing < 5.0, "12 dB swing came out as {swing:.1} dB");
    }

    #[test]
    fn vocal_compressor_catches_a_sudden_loud_syllable() {
        // 300 ms at the target, then a word 12 dB louder: within 10 ms the
        // compressor has taken most of the jump out.
        let base = db_to_linear(AutoLevel::TARGET_DB) * std::f32::consts::SQRT_2;
        let mut input = tone(14_400, 220.0, base);
        input.extend(tone(14_400, 220.0, base * 4.0));
        let mut comp = Leveler::new(SR);
        let out: Vec<f32> = input.iter().map(|&x| comp.process(x, x, 0.0)).collect();
        let before = db(rms(&out[9_600..14_400]));
        let after = db(rms(&out[14_400 + 480..14_400 + 960]));
        assert!(
            after - before < 6.0,
            "jump came out as {:.1} dB",
            after - before
        );
    }

    #[test]
    fn warmth_off_is_transparent_and_on_is_subtle() {
        for x in [-0.9_f32, -0.2, 0.0, 0.1, 0.5] {
            assert_eq!(warmth(x, 0.0), x);
        }
        // Normal speech level changes by well under 1 dB.
        let speech = 0.1_f32;
        assert!(db(warmth(speech, 1.0) / speech).abs() < 1.0);
    }

    /// Realistic voiced speech: a 130 Hz glottal pulse train (sharp pulses,
    /// like real vocal cords) shaped by three formant resonances, at a
    /// normal post-auto-level loudness (~ -20 dBFS RMS).
    fn glottal_voice(len: usize) -> Vec<f32> {
        let mut formants = [
            Biquad::new(EqShape::BandPass, SR, 700.0, 4.0),
            Biquad::new(EqShape::BandPass, SR, 1_800.0, 6.0),
            Biquad::new(EqShape::BandPass, SR, 3_000.0, 8.0),
        ];
        let period = (SR / 130.0) as usize;
        let raw: Vec<f32> = (0..len)
            .map(|i| {
                let pulse = if i % period == 0 { 1.0 } else { 0.0 };
                formants[0].process(pulse) * 1.0
                    + formants[1].process(pulse) * 0.5
                    + formants[2].process(pulse) * 0.3
                    + pulse * 0.05
            })
            .collect();
        let gain = 0.1 / rms(&raw).max(1e-9);
        raw.into_iter().map(|s| s * gain).collect()
    }

    /// Energy above 2 kHz (where clarity lives), in dB relative to input.
    fn highs_change_db(input: &[f32], output: &[f32], delay: usize) -> f32 {
        let mut hp_in = HighPass::new(SR, 2_000.0);
        let mut hp_out = HighPass::new(SR, 2_000.0);
        let a: Vec<f32> = input.iter().map(|&x| hp_in.process(x)).collect();
        let b: Vec<f32> = output.iter().map(|&x| hp_out.process(x)).collect();
        let n = input.len();
        db(rms(&b[n / 2 + delay..]) / rms(&a[n / 2..n - delay]))
    }

    #[test]
    fn click_suppressor_leaves_real_voice_alone() {
        let voice = glottal_voice(48_000);
        let mut suppressor = ClickSuppressor::new(SR);
        let out: Vec<f32> = voice
            .iter()
            .map(|&x| suppressor.process(x, true, true))
            .collect();
        let change = highs_change_db(&voice, &out, (SR * 0.002) as usize);
        assert!(
            change > -1.0,
            "click suppressor muffles voice: highs {change:.1} dB"
        );
    }

    #[test]
    fn deesser_leaves_real_voice_alone() {
        let voice = glottal_voice(48_000);
        let mut deesser = DeEsser::new(SR);
        let out: Vec<f32> = voice.iter().map(|&x| deesser.process(x)).collect();
        let change = highs_change_db(&voice, &out, 0);
        assert!(
            change > -1.0,
            "de-esser muffles voice: highs {change:.1} dB"
        );
    }
}
