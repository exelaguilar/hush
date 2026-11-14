// Analyzes a Mic Test recording: level, clipping, choppiness (gaps inside
// speech), discontinuities, tonal balance and noise floor.
// cargo run --release --example analyze -- <file.wav> [raw-reference.wav]

use hush_apo::dsp::{Biquad, EqShape};

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

fn read_mono(path: &str) -> (u32, Vec<f32>) {
    let mut reader = hound::WavReader::open(path).expect("open reference wav");
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(Result::unwrap).collect(),
        hound::SampleFormat::Int => reader
            .samples::<i32>()
            .map(|s| s.unwrap() as f32 / (1_u64 << (spec.bits_per_sample - 1)) as f32)
            .collect(),
    };
    (
        spec.sample_rate,
        samples
            .chunks(spec.channels as usize)
            .map(|c| c.iter().sum::<f32>() / c.len() as f32)
            .collect(),
    )
}

// Compare the same input-defined windows in every variant. Estimating latency
// first prevents a delayed syllable from being counted as a suppression dip.
fn compare_reference(processed: &[f32], rate: u32, path: &str) {
    let (raw_rate, raw) = read_mono(path);
    assert_eq!(rate, raw_rate, "reference sample rate must match");
    let correlation = |delay: usize| {
        let n = raw.len().min(processed.len().saturating_sub(delay));
        let (mut xy, mut xx, mut yy) = (0.0_f64, 0.0_f64, 0.0_f64);
        for i in (0..n).step_by(16) {
            let (x, y) = (raw[i] as f64, processed[i + delay] as f64);
            xy += x * y;
            xx += x * x;
            yy += y * y;
        }
        xy / (xx * yy).sqrt().max(1e-20)
    };
    let max_delay = (rate as usize / 4).min(processed.len().saturating_sub(1));
    let coarse = (0..=max_delay)
        .step_by(16)
        .max_by(|&a, &b| correlation(a).total_cmp(&correlation(b)))
        .unwrap_or(0);
    let delay = (coarse.saturating_sub(16)..=(coarse + 16).min(max_delay))
        .max_by(|&a, &b| correlation(a).total_cmp(&correlation(b)))
        .unwrap_or(0);
    let n = raw.len().min(processed.len().saturating_sub(delay));
    let window = (rate as usize / 10).max(1);
    let (mut gains, mut pause_gains, mut pause_levels) = (Vec::new(), Vec::new(), Vec::new());
    for (input, output) in raw[..n]
        .chunks_exact(window)
        .zip(processed[delay..delay + n].chunks_exact(window))
    {
        let input_db = db(rms(input));
        let output_db = db(rms(output));
        if input_db > -45.0 {
            gains.push(output_db - input_db);
        } else if input_db < -50.0 {
            pause_gains.push(output_db - input_db);
            pause_levels.push(output_db);
        }
    }
    let percentile = |values: &mut Vec<f32>, p: usize| {
        values.sort_by(f32::total_cmp);
        values[(values.len() - 1) * p / 100]
    };
    println!(
        "raw reference: aligned by {delay} samples ({:.2} ms), correlation {:.3}",
        delay as f32 * 1000.0 / rate as f32,
        correlation(delay)
    );
    if !gains.is_empty() {
        let median = percentile(&mut gains, 50);
        let p10 = percentile(&mut gains, 10);
        println!(
            "100 ms raw speech (> -45 dBFS): {} windows, median gain {median:.2} dB, worst-10% dip {:.2} dB below median",
            gains.len(), median - p10
        );
    }
    if !pause_gains.is_empty() {
        let reduction = -percentile(&mut pause_gains, 50);
        let level = percentile(&mut pause_levels, 50);
        println!(
            "100 ms raw pauses (< -50 dBFS): {} windows, median reduction {reduction:.2} dB, output {level:.2} dBFS",
            pause_gains.len()
        );
    }
    word_tails(&raw[..n], &processed[delay..delay + n], rate);
}

/// Room sound after words. Word endings are found in the raw take (no new
/// onset for 200 ms and a fall of at least 10 dB); the energy 30-200 ms
/// later is compared with the last 50 ms of the word, in raw and processed
/// audio over the same aligned frames. A drier result has a lower tail.
/// The raw decay includes the voice's own fade, so its "apparent RT60" is
/// an upper bound on the room's reverberation time.
fn word_tails(raw: &[f32], processed: &[f32], rate: u32) {
    let frame = (rate as usize / 100).max(1);
    let power = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32;
    let raw_p: Vec<f32> = raw.chunks_exact(frame).map(power).collect();
    let out_p: Vec<f32> = processed.chunks_exact(frame).map(power).collect();
    if raw_p.len().min(out_p.len()) < 3 {
        return;
    }
    let to_db = |p: f32| 10.0 * p.max(1e-12).log10();
    let raw_db: Vec<f32> = raw_p.iter().map(|&p| to_db(p)).collect();
    // Flutter on the raw take's speech frames (the same frames for every
    // variant): 10 ms level minus the mean of its neighbours, processed
    // wobble beyond what the raw voice itself does.
    let out_db: Vec<f32> = out_p.iter().map(|&p| to_db(p)).collect();
    let mut wobble: Vec<(f32, f32)> = (1..raw_db.len().min(out_db.len()) - 1)
        .filter(|&i| raw_db[i - 1..=i + 1].iter().all(|&l| l > -45.0))
        .map(|i| {
            let w = |l: &[f32]| (l[i] - (l[i - 1] + l[i + 1]) / 2.0).abs();
            (w(&raw_db), w(&out_db))
        })
        .collect();
    if !wobble.is_empty() {
        let pick = |values: &mut Vec<f32>, p: usize| {
            values.sort_by(f32::total_cmp);
            values[(values.len() - 1) * p / 100]
        };
        let (mut r, mut o): (Vec<f32>, Vec<f32>) = wobble.drain(..).unzip();
        println!(
            "aligned flutter on {} raw speech frames (median / 90%): raw {:.2} / {:.2} dB, processed {:.2} / {:.2} dB",
            r.len(),
            pick(&mut r, 50),
            pick(&mut r, 90),
            pick(&mut o, 50),
            pick(&mut o, 90)
        );
    }
    let mut sorted = raw_db.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[sorted.len() / 10];
    let mean = |p: &[f32]| p.iter().sum::<f32>() / p.len().max(1) as f32;
    let (mut raw_tails, mut out_tails, mut out_early, mut decays) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut i = 5;
    while i + 21 < raw_db.len() {
        let word = mean(&raw_p[i - 4..=i]);
        let next_max = raw_db[i + 1..=i + 20]
            .iter()
            .copied()
            .fold(f32::MIN, f32::max);
        if to_db(word) > -40.0 && next_max < raw_db[i] && raw_db[i + 20] < raw_db[i] - 10.0 {
            raw_tails.push(to_db(mean(&raw_p[i + 3..=i + 20]) / word));
            let out_word = mean(&out_p[i - 4..=i]).max(1e-12);
            out_tails.push(to_db(mean(&out_p[i + 3..=i + 20]) / out_word));
            out_early.push(to_db(mean(&out_p[i + 1..=i + 5]) / out_word));
            // Least-squares slope of the raw decay while above the floor.
            let points: Vec<(f32, f32)> = (1..=15)
                .map(|k| (k as f32 * 0.01, raw_db[i + k]))
                .filter(|&(_, level)| level > floor + 10.0)
                .collect();
            if points.len() >= 4 {
                let (mx, my) = (
                    points.iter().map(|p| p.0).sum::<f32>() / points.len() as f32,
                    points.iter().map(|p| p.1).sum::<f32>() / points.len() as f32,
                );
                let slope = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f32>()
                    / points.iter().map(|p| (p.0 - mx).powi(2)).sum::<f32>();
                if slope < -1.0 {
                    decays.push(-60.0 / slope);
                }
            }
            i += 20;
        } else {
            i += 1;
        }
    }
    let median = |values: &mut Vec<f32>| {
        values.sort_by(f32::total_cmp);
        values.get(values.len() / 2).copied().unwrap_or(f32::NAN)
    };
    println!(
        "word tails: {} endings, 30-200 ms tail vs word end: raw {:.2} dB, processed {:.2} dB (10-50 ms: {:.2} dB)",
        raw_tails.len(),
        median(&mut raw_tails),
        median(&mut out_tails),
        median(&mut out_early)
    );
    if !decays.is_empty() {
        println!(
            "raw apparent RT60 at word endings: median {:.2} s over {} endings",
            median(&mut decays),
            decays.len()
        );
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("wav path");
    let mut reader = hound::WavReader::open(&path).expect("open wav");
    let spec = reader.spec();
    let rate = spec.sample_rate as f32;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(Result::unwrap).collect(),
        hound::SampleFormat::Int => reader
            .samples::<i32>()
            .map(|s| s.unwrap() as f32 / (1 << (spec.bits_per_sample - 1)) as f32)
            .collect(),
    };
    let mono: Vec<f32> = samples
        .chunks(spec.channels as usize)
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect();
    let n = mono.len();
    println!("file: {path}");
    println!(
        "format: {} Hz, {} ch, {:.1} s",
        spec.sample_rate,
        spec.channels,
        n as f32 / rate
    );
    let peak = mono.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
    println!(
        "overall RMS {:.1} dBFS, peak {:.1} dBFS",
        db(rms(&mono)),
        db(peak)
    );
    let near_ceiling = mono.iter().filter(|s| s.abs() > 0.88).count();
    println!("samples at the -1 dBFS ceiling: {near_ceiling}");

    // 10 ms frames.
    let frame = (rate * 0.01) as usize;
    let levels: Vec<f32> = mono.chunks(frame).map(|c| db(rms(c))).collect();
    let speech: Vec<bool> = levels.iter().map(|&l| l > -45.0).collect();
    let speech_frames = speech.iter().filter(|&&s| s).count();
    println!(
        "speech frames (> -45 dBFS): {} of {} ({:.0}%)",
        speech_frames,
        levels.len(),
        100.0 * speech_frames as f32 / levels.len() as f32
    );
    let mut speech_levels: Vec<f32> = levels.iter().copied().filter(|&l| l > -45.0).collect();
    speech_levels.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if !speech_levels.is_empty() {
        let pct = |p: f32| speech_levels[((speech_levels.len() - 1) as f32 * p) as usize];
        println!(
            "speech level: 10% {:.1} / median {:.1} / 90% {:.1} dBFS",
            pct(0.1),
            pct(0.5),
            pct(0.9)
        );
    }

    // Choppiness: short dips (< 80 ms) to near silence between speech frames.
    let mut dips = Vec::new();
    let mut i = 0;
    while i < levels.len() {
        if speech[i] {
            let mut j = i + 1;
            while j < levels.len() && !speech[j] {
                j += 1;
            }
            let gap = j - i - 1;
            if j < levels.len() && (1..8).contains(&gap) {
                let min = levels[i + 1..j].iter().cloned().fold(0.0_f32, f32::min);
                if min < -55.0 {
                    dips.push((i as f32 * 0.01, gap * 10, min));
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    println!(
        "short drops to silence inside speech (choppiness): {}",
        dips.len()
    );
    for (t, ms, min) in dips.iter().take(12) {
        println!("  at {t:.2} s: {ms} ms gap down to {min:.0} dBFS");
    }

    // Gate blips: short bursts (< 150 ms) with silence on both sides.
    let mut blips = Vec::new();
    let mut k = 0;
    while k < levels.len() {
        if speech[k] {
            let start = k;
            while k < levels.len() && speech[k] {
                k += 1;
            }
            let len = k - start;
            let quiet_before = start >= 20 && speech[start - 20..start].iter().all(|s| !s);
            let quiet_after = k + 20 <= levels.len() && speech[k..k + 20].iter().all(|s| !s);
            if len < 15 && quiet_before && quiet_after {
                let peak = levels[start..k].iter().cloned().fold(-120.0_f32, f32::max);
                blips.push((start as f32 * 0.01, len * 10, peak));
            }
        } else {
            k += 1;
        }
    }
    println!("isolated blips (< 150 ms, silence around): {}", blips.len());
    for (t, ms, peak) in blips.iter().take(10) {
        println!("  at {t:.2} s: {ms} ms, peak frame {peak:.0} dBFS");
    }

    // Level jumps between consecutive 10 ms frames during speech.
    let jumps = levels
        .windows(2)
        .filter(|w| w[0] > -40.0 && w[1] > -40.0 && (w[0] - w[1]).abs() > 12.0)
        .count();
    println!("abrupt level jumps (> 12 dB between 10 ms frames in speech): {jumps}");

    // Sample discontinuities (clicks/crackle): big jumps vs local motion.
    let mut crackles = 0;
    for k in 2..n {
        let step = (mono[k] - mono[k - 1]).abs();
        let prev = (mono[k - 1] - mono[k - 2]).abs();
        if step > 0.05 && step > 8.0 * prev.max(0.002) {
            crackles += 1;
        }
    }
    println!("sample discontinuities (possible crackle): {crackles}");

    // Tonal balance during speech frames.
    let bands: [(&str, f32, f32); 6] = [
        ("<150 Hz (rumble)", 0.0, 150.0),
        ("150-500 (body)", 150.0, 500.0),
        ("500-2k (voice)", 500.0, 2_000.0),
        ("2k-5k (presence)", 2_000.0, 5_000.0),
        ("5k-10k (crisp)", 5_000.0, 10_000.0),
        (">10k (air)", 10_000.0, 24_000.0),
    ];
    let speech_samples: Vec<f32> = mono
        .chunks(frame)
        .zip(&speech)
        .filter(|(_, &s)| s)
        .flat_map(|(c, _)| c.iter().copied())
        .collect();
    if !speech_samples.is_empty() {
        println!("tonal balance during speech (relative to 500-2k):");
        let mut energies = Vec::new();
        for (_, lo, hi) in bands {
            // Band via cascaded RBJ low-pass (hi) minus low-pass (lo).
            let mut lp_hi = [
                Biquad::new(EqShape::LowPass, rate, hi.min(rate * 0.49), 0.7),
                Biquad::new(EqShape::LowPass, rate, hi.min(rate * 0.49), 0.7),
            ];
            let mut lp_lo = [
                Biquad::new(EqShape::LowPass, rate, lo.max(1.0), 0.7),
                Biquad::new(EqShape::LowPass, rate, lo.max(1.0), 0.7),
            ];
            let band: Vec<f32> = speech_samples
                .iter()
                .map(|&x| {
                    let a0 = lp_hi[0].process(x);
                    let a = lp_hi[1].process(a0);
                    let b = if lo > 0.0 {
                        let b0 = lp_lo[0].process(x);
                        lp_lo[1].process(b0)
                    } else {
                        0.0
                    };
                    a - b
                })
                .collect();
            energies.push(rms(&band));
        }
        let reference = energies[2];
        for ((name, _, _), e) in bands.iter().zip(&energies) {
            println!("  {name:18} {:+6.1} dB", db(*e / reference));
        }

        // Flutter: frame-to-frame wobble of each band's level inside
        // speech, with the smooth trend removed (|x[i] - avg(neighbours)|).
        // Natural voice is mostly smooth at 10 ms; per-band noise-removal
        // gains that jump around (warbly, "uneasy" sound) show up here.
        println!("flutter during speech (median / 90% wobble per 10 ms):");
        for (name, lo, hi) in &bands[1..] {
            let mut lp_hi = [
                Biquad::new(EqShape::LowPass, rate, hi.min(rate * 0.49), 0.7),
                Biquad::new(EqShape::LowPass, rate, hi.min(rate * 0.49), 0.7),
            ];
            let mut lp_lo = [
                Biquad::new(EqShape::LowPass, rate, *lo, 0.7),
                Biquad::new(EqShape::LowPass, rate, *lo, 0.7),
            ];
            let band: Vec<f32> = mono
                .iter()
                .map(|&x| {
                    let a0 = lp_hi[0].process(x);
                    let a = lp_hi[1].process(a0);
                    let b0 = lp_lo[0].process(x);
                    let b = lp_lo[1].process(b0);
                    a - b
                })
                .collect();
            let band_db: Vec<f32> = band.chunks(frame).map(|c| db(rms(c))).collect();
            let mut wobble: Vec<f32> = (1..band_db.len().min(speech.len()) - 1)
                .filter(|&i| speech[i - 1] && speech[i] && speech[i + 1])
                .map(|i| (band_db[i] - (band_db[i - 1] + band_db[i + 1]) / 2.0).abs())
                .collect();
            wobble.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if !wobble.is_empty() {
                println!(
                    "  {name:18} {:4.1} / {:4.1} dB",
                    wobble[wobble.len() / 2],
                    wobble[(wobble.len() - 1) * 9 / 10]
                );
            }
        }
    }

    // Noise floor in non-speech frames.
    let mut quiet: Vec<f32> = levels.iter().copied().filter(|&l| l <= -45.0).collect();
    quiet.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if !quiet.is_empty() {
        println!(
            "between words: median {:.0} dBFS, loudest 10% {:.0} dBFS",
            quiet[quiet.len() / 2],
            quiet[(quiet.len() - 1) * 9 / 10]
        );
    }
    if let Some(reference) = std::env::args().nth(2) {
        compare_reference(&mono, spec.sample_rate, &reference);
    }
}
