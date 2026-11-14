// Offline click and ambience lab: mixes a recorded background take (room
// tone with real keyboard and mouse clicks, no speech) over a speech take
// and measures what the chain lets through, stage by stage.
//
// cargo run --release --example clicklab -- <mode> <speech.wav> <background.wav> <settings.conf> <out-dir> [variant]
//
// Modes:
//   events   list the transients found in the background take
//   trace    per-event level before the model, after it, after click ducking
//   score    held-out click residue, noise leakage and speech guardrails
//   clips    level-matched listening files
//
// The residue of a known added signal is measured two ways: the difference
// between the processed mix and the processed speech alone (everything the
// addition changed), and its projection on the added signal.

use hush_apo::denoise::DenoiseOptions;
use hush_apo::dsp::{Biquad, CleanMic, EqShape, MicSettings};
use std::path::Path;

const BANDS: [(&str, f32, f32); 5] = [
    ("0.1-0.5k", 100.0, 500.0),
    ("0.5-1.5k", 500.0, 1500.0),
    ("1.5-4k", 1500.0, 4000.0),
    ("4-8k", 4000.0, 8000.0),
    ("8-16k", 8000.0, 16000.0),
];

fn read(path: &str) -> (u32, Vec<f32>) {
    let mut reader = hound::WavReader::open(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(Result::unwrap).collect(),
        hound::SampleFormat::Int => reader
            .samples::<i32>()
            .map(|s| s.unwrap() as f32 / (1_u64 << (spec.bits_per_sample - 1)) as f32)
            .collect(),
    };
    let mono = samples
        .chunks(spec.channels as usize)
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect();
    (spec.sample_rate, mono)
}

fn write(path: &Path, rate: u32, samples: &[f32]) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
    for &s in samples {
        writer.write_sample(s).unwrap();
    }
    writer.finalize().unwrap();
}

fn db(power: f64) -> f64 {
    10.0 * power.max(1e-20).log10()
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|&v| (v as f64) * (v as f64)).sum()
}

fn power(x: &[f32]) -> f64 {
    energy(x) / x.len().max(1) as f64
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[((values.len() - 1) as f64 * p).round() as usize]
}

/// Fourth-order band-pass (two cascaded second-order sections each side).
fn band(x: &[f32], rate: u32, low: f32, high: f32) -> Vec<f32> {
    let rate = rate as f32;
    let mut lp = [
        Biquad::new(EqShape::LowPass, rate, high.min(rate * 0.45), 0.54),
        Biquad::new(EqShape::LowPass, rate, high.min(rate * 0.45), 1.31),
    ];
    let mut lp2 = [
        Biquad::new(EqShape::LowPass, rate, low, 0.54),
        Biquad::new(EqShape::LowPass, rate, low, 1.31),
    ];
    x.iter()
        .map(|&v| {
            let first = lp[0].process(v);
            let top = lp[1].process(first);
            let low = lp2[0].process(top);
            top - lp2[1].process(low)
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
struct Event {
    at: usize,
    /// Samples until the high band has fallen 20 dB below its peak.
    length: usize,
    peak_db: f64,
    /// Share of the event energy above 4 kHz.
    bright: f64,
}

/// Transients in a speech-free take: 1 ms high-band power that jumps 15 dB
/// above the slow floor.
fn find_events(bg: &[f32], rate: u32) -> Vec<Event> {
    let high = band(bg, rate, 1500.0, 16000.0);
    let top = band(bg, rate, 4000.0, 16000.0);
    let step = (rate / 1000) as usize;
    let frames: Vec<f64> = high.chunks(step).map(power).collect();
    let mut sorted = frames.clone();
    let floor = median(&mut sorted);
    let mut events = Vec::new();
    let mut i = 0;
    while i < frames.len() {
        if frames[i] > floor * 31.6 {
            let end = (i + 80).min(frames.len());
            let (peak_at, peak) =
                frames[i..end]
                    .iter()
                    .enumerate()
                    .fold(
                        (0, 0.0),
                        |best, (k, &v)| if v > best.1 { (k, v) } else { best },
                    );
            let mut length = peak_at;
            while i + length < frames.len() && length < 200 && frames[i + length] > peak * 0.01 {
                length += 1;
            }
            let from = i * step;
            let to = ((i + length.max(5)) * step).min(bg.len());
            events.push(Event {
                at: from,
                length: length * step,
                peak_db: db(peak),
                bright: energy(&top[from..to]) / energy(&high[from..to]).max(1e-20),
            });
            i += length.max(60);
        } else {
            i += 1;
        }
    }
    events
}

struct Stages {
    /// 0 model output, 1 after click ducking, 7 final.
    audio: [Vec<f32>; 3],
}

const TAPS: [usize; 3] = [0, 1, 7];

/// Renders `warm` first (discarded) so adaptive state has settled, then the
/// take, with stage taps.
fn render(
    rate: u32,
    warm: &[f32],
    input: &[f32],
    settings: &MicSettings,
    options: DenoiseOptions,
) -> Stages {
    let mut chain = CleanMic::new_inline_with_options(rate, 1, options);
    let block = (rate / 100) as usize;
    let mut scratch = warm.to_vec();
    for chunk in scratch.chunks_mut(block) {
        chain.process(chunk, settings);
    }
    let mut audio = [
        Vec::with_capacity(input.len()),
        Vec::with_capacity(input.len()),
        Vec::with_capacity(input.len()),
    ];
    let mut data = input.to_vec();
    for chunk in data.chunks_mut(block) {
        chain.process_traced(chunk, settings, |trace| {
            for (slot, &tap) in audio.iter_mut().zip(&TAPS) {
                slot.push(trace.audio[tap]);
            }
        });
    }
    Stages { audio }
}

/// Lag (samples) of `processed` behind `raw`, by cross-correlation of the
/// 200 Hz - 4 kHz band.
fn lag(raw: &[f32], processed: &[f32], rate: u32) -> usize {
    let a = band(raw, rate, 200.0, 4000.0);
    let b = band(processed, rate, 200.0, 4000.0);
    let n = a.len().min(b.len());
    let (mut best, mut best_lag) = (f64::MIN, 0);
    for lag in 1800..3400 {
        let mut sum = 0.0;
        let mut i = 0;
        while i + lag < n {
            sum += a[i] as f64 * b[i + lag] as f64;
            i += 3;
        }
        if sum > best {
            best = sum;
            best_lag = lag;
        }
    }
    best_lag
}

fn options_for(variant: &str) -> DenoiseOptions {
    let mut options = DenoiseOptions::default();
    for part in variant.split('+') {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        let number = value.parse::<f32>().unwrap_or(f32::NAN);
        match name {
            "base" | "mix" | "" => {}
            "protect" => options.speech_protection_db = number,
            "supp" => {}
            "dereverb" => options.late_reverb_rt60 = if number.is_finite() { number } else { 0.5 },
            other => panic!("unknown variant part {other}"),
        }
    }
    options
}

/// Synthetic late reverberation (four-line feedback delay network after a
/// 20 ms gap), the same probe as examples/render.rs.
fn late_reverb(input: &[f32], rate: u32, rt60: f32) -> Vec<f32> {
    let pre = (rate as f32 * 0.02) as usize;
    let lengths = [1499_usize, 1889, 2381, 2999].map(|d| d * rate as usize / 48_000);
    let gains = lengths.map(|d| 10.0_f32.powf(-3.0 * d as f32 / (rt60 * rate as f32)));
    let mut lines: Vec<Vec<f32>> = lengths.iter().map(|&d| vec![0.0; d]).collect();
    let mut position = [0_usize; 4];
    let mut out = vec![0.0; input.len()];
    for n in 0..input.len() {
        let x = if n >= pre { input[n - pre] } else { 0.0 };
        let taps: [f32; 4] = std::array::from_fn(|i| lines[i][position[i]] * gains[i]);
        out[n] = taps.iter().sum::<f32>() * 0.5;
        let h = [
            taps[0] + taps[1] + taps[2] + taps[3],
            taps[0] - taps[1] + taps[2] - taps[3],
            taps[0] + taps[1] - taps[2] - taps[3],
            taps[0] - taps[1] - taps[2] + taps[3],
        ];
        for i in 0..4 {
            lines[i][position[i]] = x + 0.5 * h[i];
            position[i] = (position[i] + 1) % lengths[i];
        }
    }
    out
}

fn shifted(bg: &[f32], offset: usize, gain: f32, len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| bg[(i + bg.len() - offset % bg.len()) % bg.len()] * gain)
        .collect()
}

fn add(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).map(|(x, y)| x + y).collect()
}

fn sub(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).map(|(x, y)| x - y).collect()
}

/// 10 ms frames where the raw speech take is clearly speech.
fn speech_frames(raw: &[f32], rate: u32) -> Vec<bool> {
    let hop = (rate / 100) as usize;
    raw.chunks(hop).map(|c| db(power(c)) > -45.0).collect()
}

struct Guard {
    level_db: f64,
    dip_db: f64,
    flutter_p90: f64,
    drops: usize,
    tail_db: f64,
    quiet_db: f64,
    consonant_db: f64,
}

/// Speech guardrails on the final output against the raw take, aligned.
fn guard(raw: &[f32], processed: &[f32], rate: u32, delay: usize) -> Guard {
    let hop = (rate / 100) as usize;
    let n = raw.len().min(processed.len().saturating_sub(delay));
    let frames = n / hop;
    let r: Vec<f64> = (0..frames)
        .map(|f| db(power(&raw[f * hop..(f + 1) * hop])))
        .collect();
    let p: Vec<f64> = (0..frames)
        .map(|f| db(power(&processed[delay + f * hop..delay + (f + 1) * hop])))
        .collect();
    let speech: Vec<bool> = r.iter().map(|&v| v > -45.0).collect();
    // 100 ms windows of raw speech: gain relative to the median.
    let mut gains = Vec::new();
    for w in 0..frames / 10 {
        let range = w * 10..(w + 1) * 10;
        let raw_power = power(&raw[range.start * hop..range.end * hop]);
        if db(raw_power) > -45.0 {
            gains.push(
                db(power(
                    &processed[delay + range.start * hop..delay + range.end * hop],
                )) - db(raw_power),
            );
        }
    }
    let mid = median(&mut gains.clone());
    let worst = gains.len().div_ceil(10);
    gains.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let dip = mid - gains[..worst].iter().sum::<f64>() / worst as f64;
    // Frame-to-frame wobble of the gain inside speech.
    let mut wobble = Vec::new();
    for f in 1..frames {
        if speech[f] && speech[f - 1] {
            wobble.push(((p[f] - r[f]) - (p[f - 1] - r[f - 1])).abs());
        }
    }
    // Short drops: 1-8 processed frames 25 dB under the local gain while
    // raw speech continues on both sides.
    let mut drops = 0;
    let mut f = 1;
    while f < frames {
        if speech[f] && p[f] - r[f] < mid - 25.0 {
            let start = f;
            while f < frames && speech[f] && p[f] - r[f] < mid - 25.0 {
                f += 1;
            }
            if f - start <= 8 && start > 0 && speech[start - 1] && f < frames && speech[f] {
                drops += 1;
            }
        } else {
            f += 1;
        }
    }
    // Word endings: last speech frame before >= 150 ms of raw pause.
    let mut tails = Vec::new();
    for f in 5..frames.saturating_sub(20) {
        if speech[f] && (1..=15).all(|k| !speech[f + k]) && (1..=5).all(|k| speech[f - k]) {
            let word = power(&processed[delay + (f - 5) * hop..delay + (f + 1) * hop]);
            let tail = power(&processed[delay + (f + 3) * hop..delay + (f + 20) * hop]);
            tails.push(db(tail) - db(word));
        }
    }
    // Quiet syllables: the quietest fifth of raw speech frames.
    let mut levels: Vec<f64> = (0..frames).filter(|&f| speech[f]).map(|f| r[f]).collect();
    let cut = percentile(&mut levels, 0.2);
    let mut quiet = Vec::new();
    for f in 0..frames {
        if speech[f] && r[f] <= cut {
            quiet.push(p[f] - r[f]);
        }
    }
    // Consonants: speech frames whose raw energy sits mostly above 4 kHz.
    let raw_high = band(raw, rate, 4000.0, 16000.0);
    let out_high = band(processed, rate, 4000.0, 16000.0);
    let mut consonants = Vec::new();
    for f in 0..frames {
        let range = f * hop..(f + 1) * hop;
        let high = power(&raw_high[range.clone()]);
        if speech[f] && high > 0.25 * power(&raw[range.clone()]) {
            consonants
                .push(db(power(&out_high[delay + range.start..delay + range.end])) - db(high));
        }
    }
    let active: Vec<f32> = (0..frames)
        .filter(|&f| speech[f])
        .flat_map(|f| {
            processed[delay + f * hop..delay + (f + 1) * hop]
                .iter()
                .copied()
        })
        .collect();
    Guard {
        level_db: db(power(&active)),
        dip_db: dip,
        flutter_p90: percentile(&mut wobble, 0.9),
        drops,
        tail_db: median(&mut tails),
        quiet_db: median(&mut quiet) - mid,
        consonant_db: median(&mut consonants) - mid,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args[0].as_str();
    let (rate, speech) = read(&args[1]);
    let (bg_rate, bg) = if args[2] == "reflect" {
        let mut tail = late_reverb(&speech, rate, 0.8);
        let scale = (power(&speech) / power(&tail).max(1e-20)).sqrt() as f32 * 0.316;
        tail.iter_mut().for_each(|v| *v *= scale);
        (rate, tail)
    } else {
        read(&args[2])
    };
    assert_eq!(rate, bg_rate);
    let settings = MicSettings {
        enabled: true,
        ..hush_apo::settings::parse(&std::fs::read_to_string(&args[3]).expect("settings file"))
    };
    let mut settings = settings;
    if let Some(value) = args
        .get(5)
        .and_then(|v| v.split('+').find_map(|part| part.strip_prefix("supp=")))
    {
        settings.suppression = value.parse().expect("supp=0..1");
    }
    let out_dir = Path::new(&args[4]);
    std::fs::create_dir_all(out_dir).unwrap();
    let variant = args.get(5).map(String::as_str).unwrap_or("base");
    let options = options_for(variant);
    // "reflect" as the background path: a synthetic late-reverberation tail
    // of the speech itself, 10 dB below it, kept in step with the speech.
    let reflect = args[2] == "reflect";
    let events = if reflect {
        Vec::new()
    } else {
        find_events(&bg, rate)
    };
    let ms = |samples: usize| samples as f64 * 1000.0 / rate as f64;

    if mode == "events" {
        println!("# {} events in the background take", events.len());
        println!("index\ttime_s\tlength_ms\tpeak_dB\tbright");
        for (i, e) in events.iter().enumerate() {
            println!(
                "{i}\t{:.3}\t{:.0}\t{:.1}\t{:.2}",
                e.at as f64 / rate as f64,
                ms(e.length),
                e.peak_db,
                e.bright
            );
        }
        let mut quiet = bg.clone();
        for e in &events {
            let from = e.at.saturating_sub(480);
            for v in &mut quiet[from..(e.at + e.length + 4800).min(bg.len())] {
                *v = 0.0;
            }
        }
        let kept: Vec<f32> = quiet.iter().copied().filter(|&v| v != 0.0).collect();
        println!(
            "# ambience outside events: {:.1} dBFS, whole take {:.1} dBFS",
            db(power(&kept)),
            db(power(&bg))
        );
        return;
    }

    let clean = render(rate, &speech, &speech, &settings, options);
    let delays: [usize; 3] = {
        let chain = CleanMic::new_inline_with_options(rate, 1, options);
        let d = chain.trace_delays(&settings);
        let base = lag(&speech, &clean.audio[0], rate);
        [base, base + d[1], base + d[7]]
    };
    let speaking = speech_frames(&speech, rate);
    let hop = (rate / 100) as usize;
    let is_speech = |sample: usize| speaking.get(sample / hop).copied().unwrap_or(false);

    if mode == "alone" {
        let mut chain = CleanMic::new_inline_with_options(rate, 1, options);
        let mut data = if variant.contains("mix") {
            add(&speech, &shifted(&bg, 1200, 1.0, speech.len()))
        } else {
            bg.clone()
        };
        for chunk in data.chunks_mut(480) {
            chain.process(chunk, &settings);
        }
        return;
    }
    if mode == "peaks" {
        // How far each click's 1 ms peak stands above the voice's own 1 ms
        // peaks within +-50 ms, per band, for several placements.
        let step = (rate / 1000) as usize;
        println!("band	events	median_dB	p10	p90	share>6dB");
        for (name, low, high) in [
            ("0.7-2k", 700.0, 2000.0),
            ("2-4k", 2000.0, 4000.0),
            ("4-8k", 4000.0, 8000.0),
            ("6-12k", 6000.0, 12000.0),
            ("8-16k", 8000.0, 16000.0),
            ("2-16k", 2000.0, 16000.0),
        ] {
            let s_band = band(&speech, rate, low, high);
            let b_band = band(&bg, rate, low, high);
            let s_frames: Vec<f64> = s_band.chunks(step).map(power).collect();
            let b_frames: Vec<f64> = b_band.chunks(step).map(power).collect();
            let mut margins = Vec::new();
            for k in 0..12 {
                let offset = k * 37123 + 1200;
                for e in &events {
                    let at = (e.at + offset) % bg.len();
                    if at + 4800 >= speech.len()
                        || at < 4800
                        || !(is_speech(at) && is_speech(at + 960))
                    {
                        continue;
                    }
                    let b0 = e.at / step;
                    let click = b_frames[b0..b0 + 15].iter().cloned().fold(0.0, f64::max);
                    let s0 = at / step;
                    let mut around: Vec<f64> = s_frames[s0 - 50..s0 + 50].to_vec();
                    margins.push(db(click) - db(percentile(&mut around, 0.95)));
                }
            }
            let share = margins.iter().filter(|&&m| m > 6.0).count() as f64 / margins.len() as f64;
            println!(
                "{name}	{}	{:.1}	{:.1}	{:.1}	{:.2}",
                margins.len(),
                median(&mut margins.clone()),
                percentile(&mut margins.clone(), 0.1),
                percentile(&mut margins.clone(), 0.9),
                share
            );
        }
        return;
    }

    if mode == "trace" {
        // One placement at the recorded level; time profile of each event.
        let offset: usize = args.get(6).and_then(|v| v.parse().ok()).unwrap_or(0);
        let added = shifted(&bg, offset, 1.0, speech.len());
        let mixed = render(rate, &speech, &add(&speech, &added), &settings, options);
        println!("# variant {variant}, model delay {:.2} ms", ms(delays[0]));
        println!("event\ttime_s\tspeech\tband\traw_dB\tmodel_dB\tclick_dB\tprofile(model-raw dB per 5 ms from -5 ms)");
        for (index, e) in events.iter().enumerate() {
            let at = (e.at + offset) % bg.len();
            if at + 4800 >= speech.len() || at < 480 {
                continue;
            }
            let window = at - 240..at + 2880;
            for (name, low, high) in BANDS {
                let raw_band = band(&added, rate, low, high);
                let stage_band: Vec<Vec<f32>> = (0..2)
                    .map(|s| band(&sub(&mixed.audio[s], &clean.audio[s]), rate, low, high))
                    .collect();
                let raw_e = energy(&raw_band[window.clone()]);
                let at_stage = |s: usize| {
                    energy(&stage_band[s][window.start + delays[s]..window.end + delays[s]])
                };
                let profile: Vec<String> = (0..10)
                    .map(|k| {
                        let w = at - 240 + k * 240..at + k * 240;
                        format!(
                            "{:.0}",
                            db(energy(
                                &stage_band[0][w.start + delays[0]..w.end + delays[0]]
                            )) - db(energy(&raw_band[w]))
                        )
                    })
                    .collect();
                println!(
                    "{index}\t{:.3}\t{}\t{name}\t{:.1}\t{:.1}\t{:.1}\t{}",
                    at as f64 / rate as f64,
                    is_speech(at) as u8,
                    db(raw_e / window.len() as f64),
                    db(at_stage(0)) - db(raw_e),
                    db(at_stage(1)) - db(raw_e),
                    profile.join(" ")
                );
            }
        }
        return;
    }

    if mode == "score" {
        // Even events tune, odd events validate.
        let offsets: Vec<usize> = if reflect {
            vec![0]
        } else {
            (0..6).map(|k| k * 7919 * 9 + 1200).collect()
        };
        let mut report = String::from(
            "gain_dB\tset\tevents\tfinal_residue_med_dB\tp10\tp90\tworst\tprojection_med_dB\tmodel_med_dB\tclick_stage_med_dB\n",
        );
        let final_gain = db(power(&clean.audio[2])) - db(power(&clean.audio[0]));
        let (mut leak_dot, mut leak_energy, mut leak_residue) =
            ([0.0_f64; 6], [0.0_f64; 6], [0.0_f64; 6]);
        let mut leak_windows = 0;
        let (mut gap_dot, mut gap_energy, mut gap_mix, mut gap_clean, mut gap_samples) = (
            [0.0_f64; 4],
            [0.0_f64; 4],
            [0.0_f64; 4],
            [0.0_f64; 4],
            [0.0_f64; 4],
        );
        for gain_db in [0.0_f32, 6.0] {
            let gain = 10.0_f32.powf(gain_db / 20.0);
            let mut rows: [Vec<[f64; 6]>; 4] = Default::default();
            for &offset in &offsets {
                let added = shifted(&bg, offset, gain, speech.len());
                let mixed = render(rate, &speech, &add(&speech, &added), &settings, options);
                let residue: Vec<Vec<f32>> = (0..3)
                    .map(|s| sub(&mixed.audio[s], &clean.audio[s]))
                    .collect();
                let high_added = band(&added, rate, 1500.0, 8000.0);
                let high_model = band(&residue[0], rate, 1500.0, 8000.0);
                let high_final = band(&residue[2], rate, 1500.0, 8000.0);
                let high_gain = db(power(&band(&clean.audio[2], rate, 1500.0, 8000.0)))
                    - db(power(&band(&clean.audio[0], rate, 1500.0, 8000.0)));
                let mut near_event = vec![false; speech.len()];
                for (index, e) in events.iter().enumerate() {
                    let at = (e.at + offset) % bg.len();
                    if at + 4800 >= speech.len() || at < 4800 {
                        continue;
                    }
                    for flag in &mut near_event[at - 2400..(at + e.length + 9600).min(speech.len())]
                    {
                        *flag = true;
                    }
                    // On speech, or in a short gap between words (the gate
                    // is still open and speech protection still on).
                    let on_speech = is_speech(at) && is_speech(at + 960);
                    let in_gap = !is_speech(at)
                        && !is_speech(at + 960)
                        && (1..=30).any(|k| at >= k * hop && is_speech(at - k * hop));
                    if !(on_speech || in_gap) {
                        continue;
                    }
                    let window = at - 240..at + e.length.clamp(960, 2880);
                    let raw_e = energy(&added[window.clone()]);
                    let stage = |s: usize| {
                        let w = window.start + delays[s]..window.end + delays[s];
                        db(energy(&residue[s][w])) - db(raw_e)
                    };
                    // Projection of the final residue on the added signal.
                    let w = window.start + delays[2]..window.end + delays[2];
                    let dot: f64 = residue[2][w]
                        .iter()
                        .zip(&added[window.clone()])
                        .map(|(a, b)| *a as f64 * *b as f64)
                        .sum();
                    let projection = 20.0 * (dot.abs() / raw_e.max(1e-20)).max(1e-10).log10();
                    let w0 = window.start + delays[0]..window.end + delays[0];
                    let dot0: f64 = residue[0][w0]
                        .iter()
                        .zip(&added[window.clone()])
                        .map(|(a, b)| *a as f64 * *b as f64)
                        .sum();
                    let model_projection =
                        20.0 * (dot0.abs() / raw_e.max(1e-20)).max(1e-10).log10();
                    if std::env::var_os("CLICKLAB_DETAIL").is_some() {
                        let hb = |x: &[f32]| db(power(&band(x, rate, 1500.0, 8000.0)[4800..]));
                        let pad = window.start - 4800..window.end;
                        println!(
                            "E	{gain_db}	{offset}	{index}	{:.3}	speech_hi {:.1}	click_hi {:.1}	model_res {:.1}	model_proj {:.1}",
                            at as f64 / rate as f64,
                            hb(&speech[pad.clone()]),
                            hb(&added[pad]),
                            stage(0),
                            model_projection
                        );
                    }
                    rows[index % 2 + if in_gap { 2 } else { 0 }].push([
                        stage(2) - final_gain,
                        projection - final_gain,
                        stage(0),
                        model_projection,
                        db(energy(
                            &high_model[window.start + delays[0]..window.end + delays[0]],
                        )) - db(energy(&high_added[window.clone()])),
                        db(energy(
                            &high_final[window.start + delays[2]..window.end + delays[2]],
                        )) - db(energy(&high_added[window.clone()]))
                            - high_gain,
                    ]);
                }
                if gain_db == 0.0 {
                    // Steady noise under speech: 50 ms windows away from
                    // events, projected on the added noise at the model
                    // output (speech changes are uncorrelated with it).
                    let mut names = vec![("all", 60.0, 16000.0)];
                    names.extend(BANDS);
                    for (slot, (_, low, high)) in names.iter().enumerate() {
                        let a = band(&added, rate, *low, *high);
                        let r = band(&residue[0], rate, *low, *high);
                        let mut w = 0;
                        while w + 2400 < speech.len().saturating_sub(delays[0]) {
                            let range = w..w + 2400;
                            if range
                                .clone()
                                .step_by(240)
                                .all(|i| is_speech(i) && !near_event[i])
                            {
                                let out = range.start + delays[0]..range.end + delays[0];
                                leak_dot[slot] += r[out.clone()]
                                    .iter()
                                    .zip(&a[range.clone()])
                                    .map(|(x, y)| *x as f64 * *y as f64)
                                    .sum::<f64>();
                                leak_energy[slot] += energy(&a[range.clone()]);
                                leak_residue[slot] += energy(&r[out]);
                                leak_windows += 1;
                            }
                            w += 2400;
                        }
                    }
                    // The same projection by time since the last speech
                    // frame, with the level of the finished mix there.
                    let mut since = 0_usize;
                    let mut w = 0;
                    while w + 960 < speech.len().saturating_sub(delays[2]) {
                        since = if is_speech(w) { 0 } else { since + 2 };
                        let range = w..w + 960;
                        if !range.clone().step_by(240).any(|i| near_event[i]) {
                            let slot = match since {
                                0 => 0,
                                1..=15 => 1,
                                16..=40 => 2,
                                _ => 3,
                            };
                            let out = range.start + delays[0]..range.end + delays[0];
                            let last = range.start + delays[2]..range.end + delays[2];
                            gap_dot[slot] += residue[0][out]
                                .iter()
                                .zip(&added[range.clone()])
                                .map(|(x, y)| *x as f64 * *y as f64)
                                .sum::<f64>();
                            gap_energy[slot] += energy(&added[range.clone()]);
                            gap_mix[slot] += energy(&mixed.audio[2][last.clone()]);
                            gap_clean[slot] += energy(&clean.audio[2][last]);
                            gap_samples[slot] += 960.0;
                        }
                        w += 960;
                    }
                }
            }
            for (set, rows) in ["speech tune", "speech held-out", "gap tune", "gap held-out"]
                .iter()
                .zip(&rows)
            {
                let column = |c: usize| rows.iter().map(|r| r[c]).collect::<Vec<f64>>();
                let mut residue = column(0);
                report += &format!(
                    "{gain_db}\t{set}\t{}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\n",
                    rows.len(),
                    median(&mut residue),
                    percentile(&mut residue, 0.1),
                    percentile(&mut residue, 0.9),
                    percentile(&mut residue, 1.0),
                    median(&mut column(1)),
                    median(&mut column(2)),
                    median(&mut column(3)),
                    median(&mut column(4)),
                    median(&mut column(5)),
                    percentile(&mut column(5), 0.9),
                );
            }
        }
        report += &format!("# steady noise under speech at the model output, {} windows: band, projected gain dB, residue energy dB
", leak_windows / 6);
        let mut names = vec![("all", 0.0, 0.0)];
        names.extend(BANDS);
        for (slot, (name, _, _)) in names.iter().enumerate() {
            report += &format!(
                "#   {name}	{:.2}	{:.2}
",
                20.0 * (leak_dot[slot].abs() / leak_energy[slot].max(1e-20))
                    .max(1e-10)
                    .log10(),
                db(leak_residue[slot]) - db(leak_energy[slot])
            );
        }
        report += "# added noise by time since speech: where, seconds, model-output projected gain dB, finished mix dBFS, finished speech-only dBFS\n";
        for (slot, name) in [
            "during speech",
            "0-150 ms after",
            "150-400 ms after",
            "later",
        ]
        .iter()
        .enumerate()
        {
            report += &format!(
                "#   {name}\t{:.1}\t{:.2}\t{:.1}\t{:.1}\n",
                gap_samples[slot] / rate as f64,
                20.0 * (gap_dot[slot].abs() / gap_energy[slot].max(1e-20))
                    .max(1e-10)
                    .log10(),
                db(gap_mix[slot] / gap_samples[slot].max(1.0)),
                db(gap_clean[slot] / gap_samples[slot].max(1.0)),
            );
        }
        // Background alone (pauses) and speech alone (false triggers).
        let alone = render(rate, &speech, &bg, &settings, options);
        report += &format!(
            "# background alone: in {:.1} dBFS, out {:.1} dBFS\n",
            db(power(&bg)),
            db(power(&alone.audio[2][delays[2]..]))
        );
        let g = guard(&speech, &clean.audio[2], rate, delays[2]);
        report += &format!(
            "# speech alone: level {:.2} dBFS, dip {:.2} dB, p90 flutter {:.2} dB, short drops {}, tail {:.2} dB, quiet syllables {:+.2} dB, consonants >4k {:+.2} dB, delay {:.2} ms\n",
            g.level_db, g.dip_db, g.flutter_p90, g.drops, g.tail_db, g.quiet_db, g.consonant_db, ms(delays[2])
        );
        print!("# variant {variant}\n{report}");
        std::fs::write(
            out_dir.join(format!("score-{}.tsv", variant.replace('=', "-"))),
            report,
        )
        .unwrap();
        write(
            &out_dir.join(format!("speech-{}.wav", variant.replace('=', "-"))),
            rate,
            &clean.audio[2],
        );
        return;
    }

    if mode == "clips" {
        let offset: usize = args.get(6).and_then(|v| v.parse().ok()).unwrap_or(1200);
        let gain_db: f32 = args.get(7).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let added = shifted(&bg, offset, 10.0_f32.powf(gain_db / 20.0), speech.len());
        let mixed = render(rate, &speech, &add(&speech, &added), &settings, options);
        // Match the active-speech loudness of the baseline, so a level
        // difference is not heard as a quality difference. The added
        // sounds keep their recorded level.
        let reference = render(rate, &speech, &speech, &settings, DenoiseOptions::default());
        let level = guard(&speech, &clean.audio[2], rate, delays[2]).level_db;
        let target = guard(&speech, &reference.audio[2], rate, delays[2]).level_db;
        let scale = 10.0_f64.powf((target - level) / 20.0) as f32;
        let matched = |x: &[f32]| x.iter().map(|v| v * scale).collect::<Vec<f32>>();
        let name = variant.replace('=', "-");
        write(
            &out_dir.join(format!("speech-{name}.wav")),
            rate,
            &matched(&clean.audio[2]),
        );
        write(
            &out_dir.join(format!("with-added-{name}.wav")),
            rate,
            &matched(&mixed.audio[2]),
        );
        println!("{name}: active speech {level:.2} dBFS, matched to {target:.2} dBFS");
        return;
    }
    panic!("unknown mode {mode}");
}
