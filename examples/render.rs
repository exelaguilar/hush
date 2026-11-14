// Offline A/B renderer: runs an unprocessed Mic Test recording (Clean Mic
// switched off) through the Hush chain with fixed settings.
//
// cargo run --release --example render -- raw.wav [out-dir] [experiments [variant]]
// cargo run --release --example render -- raw.wav out-dir saved
// cargo run --release --example render -- raw.wav out-dir room
// cargo run --release --example render -- raw.wav out-dir probe
// cargo run --release --example render -- raw.wav out-dir clarity

use hush_apo::denoise::DenoiseOptions;
use hush_apo::dsp::{CleanMic, MicSettings, MicTrace};

#[cfg(windows)]
fn cpu_seconds() -> Option<f64> {
    use windows::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
        .ok()?;
    }
    let seconds = |time: FILETIME| {
        (((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64) as f64 * 1e-7
    };
    Some(seconds(kernel) + seconds(user))
}

#[cfg(not(windows))]
fn cpu_seconds() -> Option<f64> {
    None
}

fn read(path: &str) -> (u32, Vec<f32>) {
    let mut reader = hound::WavReader::open(path).expect("open wav");
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

fn write(path: &std::path::Path, rate: u32, samples: &[f32]) {
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

fn render(
    rate: u32,
    input: &[f32],
    settings: &MicSettings,
    mut options: DenoiseOptions,
) -> Vec<f32> {
    if options.model_gain_db > 0.0 {
        let peak = input.iter().map(|x| x.abs()).fold(0.0_f32, f32::max);
        let allowed = (20.0 * (0.8 / peak.max(1e-12)).log10()).max(0.0);
        if allowed < options.model_gain_db {
            eprintln!("limited offline model boost to {allowed:.1} dB for peak headroom");
            options.model_gain_db = allowed;
        }
    }
    let mut chain = CleanMic::new_inline_with_options(rate, 1, options);
    let mut out = input.to_vec();
    let block = (rate / 100) as usize;
    for chunk in out.chunks_mut(block) {
        chain.process(chunk, settings);
    }
    out
}

fn main() {
    let mut args = std::env::args().skip(1);
    let input = args.next().expect("raw wav path");
    let out_dir = args
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(&input)
                .parent()
                .unwrap()
                .join("Hush compare")
        });
    std::fs::create_dir_all(&out_dir).unwrap();
    let mode = args.next();
    let only = args.next();
    let (rate, raw) = read(&input);
    // Fixed settings (not the app's live ones), so each file differs from
    // the others in exactly one way.
    let full = MicSettings {
        enabled: true,
        ..MicSettings::default()
    };
    let noise_only = MicSettings {
        high_pass: false,
        leveler: false,
        gate: false,
        deesser: false,
        click_suppression: false,
        warmth: 0.0,
        bass_db: 0.0,
        clarity_db: 0.0,
        air_db: 0.0,
        suppression: 0.8,
        ..full
    };
    let standard: Vec<(&str, MicSettings)> = vec![
        ("1-noise-removal-only", noise_only),
        (
            "2-noise-removal-only-50",
            MicSettings {
                suppression: 0.5,
                ..noise_only
            },
        ),
        ("3-full-chain", full),
        (
            "4-full-chain-no-auto-level",
            MicSettings {
                leveler: false,
                ..full
            },
        ),
    ];
    let options = |model_gain_db, protection_cleaning| DenoiseOptions {
        model_gain_db,
        protection_cleaning,
        ..DenoiseOptions::default()
    };
    if mode.as_deref() == Some("room") {
        room(&out_dir, rate, &raw, full, noise_only);
        return;
    }
    if matches!(mode.as_deref(), Some("probe" | "probe-tails")) {
        probe(
            &out_dir,
            rate,
            &raw,
            full,
            noise_only,
            mode.as_deref() == Some("probe-tails"),
        );
        return;
    }
    if mode.as_deref() == Some("tails") {
        let saved = MicSettings {
            enabled: true,
            ..hush_apo::settings::load()
        };
        let desk = MicSettings {
            desk_mic: true,
            ..saved
        };
        let base = DenoiseOptions::default();
        let variants = [
            ("n0-0.4.7", noise_only, base),
            ("n1-tail-6dB", noise_only, pause_tail(6.0)),
            ("f0-0.4.7", full, base),
            ("f1-tail-3dB", full, pause_tail(3.0)),
            ("f2-tail-6dB", full, pause_tail(6.0)),
            ("f3-saved", saved, base),
            ("f4-saved-desk", desk, base),
            (
                "f5-saved-desk-50",
                MicSettings {
                    gate_sensitivity: 0.5,
                    ..desk
                },
                base,
            ),
            ("f6-saved-tail-6dB", saved, pause_tail(6.0)),
            ("f7-leveler-8ms", full, leveler_detector(8.0)),
            ("f8-leveler-12ms", full, leveler_detector(12.0)),
            ("f9-leveler-20ms", full, leveler_detector(20.0)),
            ("fa-saved-leveler-12ms", saved, leveler_detector(12.0)),
            ("n2-leveler-12ms", noise_only, leveler_detector(12.0)),
        ];
        comparison(&out_dir, rate, &raw, variants, false);
        return;
    }
    if mode.as_deref() == Some("clarity") {
        clarity(&out_dir, rate, &raw, full, noise_only);
        return;
    }
    if mode.as_deref() == Some("stages") {
        // Warmed full chain with one stage switched off at a time, to find
        // which stage after noise removal adds level wobble.
        let stages = [
            ("s0-full", full),
            (
                "s1-no-auto-level",
                MicSettings {
                    leveler: false,
                    ..full
                },
            ),
            (
                "s2-no-gate",
                MicSettings {
                    gate: false,
                    ..full
                },
            ),
            (
                "s3-no-deesser",
                MicSettings {
                    deesser: false,
                    ..full
                },
            ),
            (
                "s4-no-click-ducking",
                MicSettings {
                    click_suppression: false,
                    ..full
                },
            ),
            (
                "s5-no-warmth",
                MicSettings {
                    warmth: 0.0,
                    ..full
                },
            ),
            (
                "s6-flat-eq",
                MicSettings {
                    bass_db: 0.0,
                    clarity_db: 0.0,
                    air_db: 0.0,
                    ..full
                },
            ),
            ("s7-noise-only", noise_only),
        ];
        for (name, settings) in stages {
            let out = render_warm(rate, &raw, &settings, DenoiseOptions::default());
            write(&out_dir.join(format!("{name}.wav")), rate, &out);
        }
        write(&out_dir.join("raw.wav"), rate, &raw);
        return;
    }
    let variants: Vec<(&str, MicSettings, DenoiseOptions)> = match mode.as_deref() {
        Some("experiments") => [
            ("1-baseline-noise-only", noise_only, 0.0, 0.0),
            ("2-model-plus-6dB", noise_only, 6.0, 0.0),
            ("3-model-plus-12dB", noise_only, 12.0, 0.0),
            ("4-model-plus-18dB", noise_only, 18.0, 0.0),
            ("5-clean-backup-gentle", noise_only, 0.0, 0.35),
            ("6-clean-backup-medium", noise_only, 0.0, 0.65),
            ("7-clean-backup-strong", noise_only, 0.0, 1.0),
            ("8-baseline-full", full, 0.0, 0.0),
            ("9-clean-backup-full", full, 0.0, 0.65),
            ("10-conditioned-full", full, 6.0, 0.0),
            ("11-conditioned-gentle", noise_only, 6.0, 0.35),
            ("12-conditioned-medium", noise_only, 6.0, 0.65),
            ("13-conditioned-strong", noise_only, 6.0, 1.0),
            ("14-combined-full", full, 6.0, 0.65),
            ("15-clean-backup-strong-full", full, 0.0, 1.0),
        ]
        .into_iter()
        .map(|(name, settings, gain, clean)| (name, settings, options(gain, clean)))
        .collect(),
        Some("saved") => {
            let saved = MicSettings {
                enabled: true,
                ..hush_apo::settings::load()
            };
            std::fs::write(
                out_dir.join("settings.conf"),
                hush_apo::settings::format(&saved),
            )
            .unwrap();
            vec![
                ("A-saved-baseline", saved, options(0.0, 0.0)),
                ("B-saved-clean-backup", saved, DenoiseOptions::default()),
            ]
        }
        None => standard
            .into_iter()
            .map(|(name, settings)| (name, settings, DenoiseOptions::default()))
            .collect(),
        _ => panic!("expected experiments, saved or room mode"),
    };
    if let Some(only) = only.as_deref() {
        assert!(
            variants.iter().any(|(name, _, _)| *name == only),
            "unknown variant {only}"
        );
    }
    write(&out_dir.join("0-raw.wav"), rate, &raw);
    for (name, settings, options) in variants {
        if only.as_deref().is_some_and(|only| name != only) {
            continue;
        }
        let start = std::time::Instant::now();
        let cpu_start = cpu_seconds();
        let out = render(rate, &raw, &settings, options);
        let elapsed = start.elapsed().as_secs_f64();
        let cpu_elapsed = cpu_start.zip(cpu_seconds()).map(|(a, b)| b - a);
        let path = out_dir.join(format!("{name}.wav"));
        write(&path, rate, &out);
        println!(
            "wrote {} ({elapsed:.3} s for {:.2} s audio, {:.2}% of real-time budget including model load)",
            path.display(),
            raw.len() as f64 / rate as f64,
            elapsed * rate as f64 / raw.len() as f64 * 100.0
        );
        if let Some(cpu) = cpu_elapsed {
            println!(
                "  CPU: {cpu:.3} s, {:.2}% of one core including model load",
                cpu * rate as f64 / raw.len() as f64 * 100.0
            );
        }
    }
}

/// Level of the active speech: RMS over the 10 ms frames within 20 dB of
/// the loudest 5% of frames. Used to match variants for listening.
fn speech_level(samples: &[f32], rate: u32) -> f32 {
    let frame = (rate / 100) as usize;
    let mut powers: Vec<f32> = samples
        .chunks_exact(frame)
        .map(|c| c.iter().map(|x| x * x).sum::<f32>() / frame as f32)
        .collect();
    powers.sort_by(f32::total_cmp);
    let loud = powers[(powers.len() - 1) * 95 / 100];
    let active: Vec<f32> = powers.into_iter().filter(|&p| p > loud * 0.01).collect();
    (active.iter().sum::<f32>() / active.len().max(1) as f32).sqrt()
}

fn raw_masked_level(raw: &[f32], processed: &[f32], rate: u32) -> f32 {
    let delay = align(raw, processed, rate);
    let frame = (rate / 100) as usize;
    let threshold = 10.0_f32.powf(-45.0 / 10.0);
    let (mut power, mut count) = (0.0_f64, 0);
    for (input, output) in raw
        .chunks_exact(frame)
        .zip(processed[delay..].chunks_exact(frame))
    {
        let input_power = input.iter().map(|x| x * x).sum::<f32>() / frame as f32;
        if input_power > threshold {
            power += output.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>();
            count += frame;
        }
    }
    assert!(count != 0, "clarity comparison needs speech above -45 dBFS");
    (power / count as f64).sqrt() as f32
}

/// Renders the take once from a fresh stream, and from a warmed stream:
/// the take repeated four times (40 s), keeping the last repetition.
fn render_warm(
    rate: u32,
    raw: &[f32],
    settings: &MicSettings,
    options: DenoiseOptions,
) -> Vec<f32> {
    let repeated: Vec<f32> = raw.iter().copied().cycle().take(raw.len() * 4).collect();
    render(rate, &repeated, settings, options)[raw.len() * 3..].to_vec()
}

fn pause_tail(db: f32) -> DenoiseOptions {
    DenoiseOptions {
        pause_tail_db: db,
        ..DenoiseOptions::default()
    }
}

fn leveler_detector(ms: f32) -> DenoiseOptions {
    DenoiseOptions {
        leveler_detector_ms: ms,
        ..DenoiseOptions::default()
    }
}

/// Room-sound experiment: does the remaining roominess come from the
/// protective dry blend or from reverberation DeepFilterNet keeps, and does
/// a late-reverberation post-filter on the model output remove it?
/// Writes unmodified renders for `analyze` and loudness-matched copies for
/// listening.
fn room(
    out_dir: &std::path::Path,
    rate: u32,
    raw: &[f32],
    full: MicSettings,
    noise_only: MicSettings,
) {
    let base = DenoiseOptions::default();
    let reverb = |rt60| DenoiseOptions {
        late_reverb_rt60: rt60,
        ..base
    };
    let unprotected = DenoiseOptions {
        speech_protection_db: 100.0,
        ..base
    };
    let variants: [(&str, MicSettings, DenoiseOptions); 8] = [
        ("n0-0.4.7", noise_only, base),
        ("n1-no-protection", noise_only, unprotected),
        ("n2-dereverb-0.5s", noise_only, reverb(0.5)),
        ("n3-dereverb-0.8s", noise_only, reverb(0.8)),
        ("f0-0.4.7", full, base),
        ("f1-no-protection", full, unprotected),
        ("f2-dereverb-0.5s", full, reverb(0.5)),
        ("f3-dereverb-0.8s", full, reverb(0.8)),
    ];
    comparison(out_dir, rate, raw, variants, false);
}

fn comparison<const N: usize>(
    out_dir: &std::path::Path,
    rate: u32,
    raw: &[f32],
    variants: [(&str, MicSettings, DenoiseOptions); N],
    match_raw_speech: bool,
) {
    let analysis = out_dir.join("analysis");
    let listening = out_dir.join("listening");
    std::fs::create_dir_all(&analysis).unwrap();
    std::fs::create_dir_all(&listening).unwrap();
    write(&analysis.join("raw.wav"), rate, raw);
    let mut references = std::collections::HashMap::new();
    for (stream, warm) in [("fresh", false), ("warm", true)] {
        for (name, settings, options) in variants {
            let cpu_start = cpu_seconds();
            let out = if warm {
                render_warm(rate, raw, &settings, options)
            } else {
                render(rate, raw, &settings, options)
            };
            let cpu = cpu_start.zip(cpu_seconds()).map(|(a, b)| b - a);
            write(&analysis.join(format!("{stream}-{name}.wav")), rate, &out);
            // Match each family to its 0.4.7 reference's speech level.
            let family = &name[..1];
            let level = if match_raw_speech {
                raw_masked_level(raw, &out, rate)
            } else {
                speech_level(&out, rate)
            };
            let reference = *references.entry((stream, family)).or_insert(level);
            let gain = reference / level.max(1e-9);
            let matched: Vec<f32> = out.iter().map(|x| x * gain).collect();
            write(
                &listening.join(format!("{stream}-{name}.wav")),
                rate,
                &matched,
            );
            println!(
                "{stream}-{name}: matched by {:+.2} dB{}",
                20.0 * gain.log10(),
                cpu.map(|c| format!(", CPU {c:.3} s")).unwrap_or_default()
            );
        }
    }
    // The raw take at the full chain's speech level, for orientation.
    let raw_level = if match_raw_speech {
        raw_masked_level(raw, raw, rate)
    } else {
        speech_level(raw, rate)
    };
    let level = references[&("fresh", "f")] / raw_level.max(1e-9);
    let peak = raw.iter().fold(0.0_f32, |m, x| m.max(x.abs())) * level;
    let gain = if peak > 0.98 {
        level * 0.98 / peak
    } else {
        level
    };
    let matched: Vec<f32> = raw.iter().map(|x| x * gain).collect();
    write(&listening.join("raw-level-matched.wav"), rate, &matched);
}

fn clarity(
    out_dir: &std::path::Path,
    rate: u32,
    raw: &[f32],
    full: MicSettings,
    noise_only: MicSettings,
) {
    let saved = MicSettings {
        enabled: true,
        ..hush_apo::settings::load()
    };
    std::fs::write(
        out_dir.join("settings.conf"),
        hush_apo::settings::format(&saved),
    )
    .unwrap();
    let base = DenoiseOptions::default();
    let detector = |mix| DenoiseOptions {
        pre_eq_detector_mix: mix,
        ..base
    };
    let deesser = |mix| DenoiseOptions {
        deesser_reduction_mix: mix,
        ..base
    };
    let variants = [
        ("n0-0.4.7", noise_only, base),
        ("n1-pre-eq-100", noise_only, detector(1.0)),
        ("f0-0.4.7", full, base),
        ("f1-pre-eq-25", full, detector(0.25)),
        ("f2-pre-eq-50", full, detector(0.5)),
        ("f3-pre-eq-100", full, detector(1.0)),
        ("s0-saved", saved, base),
        ("s1-pre-eq-25", saved, detector(0.25)),
        ("s2-pre-eq-50", saved, detector(0.5)),
        ("s3-pre-eq-100", saved, detector(1.0)),
        ("s4-deess-75", saved, deesser(0.75)),
        ("s5-deess-50", saved, deesser(0.5)),
        (
            "s6-deess-off",
            MicSettings {
                deesser: false,
                ..saved
            },
            base,
        ),
        (
            "s7-bass-plus-1",
            MicSettings {
                bass_db: (saved.bass_db + 1.0).min(12.0),
                ..saved
            },
            base,
        ),
    ];
    comparison(out_dir, rate, raw, variants, true);
    for (name, options) in [("s0-saved", base), ("s3-pre-eq-100", detector(1.0))] {
        trace_clarity(out_dir, rate, raw, &saved, options, name);
    }
}

fn trace_clarity(
    out_dir: &std::path::Path,
    rate: u32,
    raw: &[f32],
    settings: &MicSettings,
    options: DenoiseOptions,
    name: &str,
) {
    use std::io::Write;

    let block = (rate / 100) as usize;
    let mut chain = CleanMic::new_inline_with_options(rate, 1, options);
    let delays = chain.trace_delays(settings);
    let warmup = raw.len() * 3;
    let mut samples: Vec<_> = raw.iter().copied().cycle().take(raw.len() * 4).collect();
    let mut trace = Vec::with_capacity(raw.len());
    for (index, chunk) in samples.chunks_mut(block).enumerate() {
        let skip = warmup.saturating_sub(index * block);
        if skip >= chunk.len() {
            chain.process(chunk, settings);
        } else {
            let mut frame = 0;
            chain.process_traced(chunk, settings, |sample| {
                if frame >= skip {
                    trace.push(sample);
                }
                frame += 1;
            });
        }
    }
    let samples = &samples[warmup..];
    let reference = read(
        out_dir
            .join("analysis")
            .join(format!("warm-{name}.wav"))
            .to_str()
            .unwrap(),
    )
    .1;
    assert!(
        samples
            .iter()
            .map(|x| x.to_bits())
            .eq(reference.iter().map(|x| x.to_bits())),
        "traced and standard renders differ"
    );
    let final_delay = align(raw, samples, rate);
    let count = raw.len().min(samples.len() - final_delay);
    let offset = |stage: usize| final_delay - (delays[7] - delays[stage]);
    let trace_dir = out_dir.join("traces").join(name);
    std::fs::create_dir_all(&trace_dir).unwrap();
    for (index, stage) in MicTrace::STAGES.iter().enumerate() {
        let first = offset(index);
        let signal: Vec<_> = trace[first..first + count]
            .iter()
            .map(|s| s.audio[index])
            .collect();
        write(&trace_dir.join(format!("{stage}.wav")), rate, &signal);
    }
    let path = trace_dir.join("gains.tsv");
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    write!(file, "raw_start_ms").unwrap();
    for gain in MicTrace::GAINS {
        write!(file, "\t{gain}_mean_db\t{gain}_min_db\t{gain}_reduced_pct").unwrap();
    }
    writeln!(file).unwrap();
    let gain_stages = [1, 2, 4, 4, 6, 7];
    for start in (0..count.saturating_sub(block - 1)).step_by(block) {
        write!(file, "{:.2}", start as f64 * 1000.0 / f64::from(rate)).unwrap();
        for (gain, stage) in gain_stages.into_iter().enumerate() {
            let first = offset(stage) + start;
            let values = trace[first..first + block].iter().map(|s| s.gain_db[gain]);
            let mean = values.clone().sum::<f32>() / block as f32;
            let min = values.clone().fold(f32::INFINITY, f32::min);
            let reduced = values.filter(|&db| db < -0.1).count() as f32 * 100.0 / block as f32;
            write!(file, "\t{mean:.4}\t{min:.4}\t{reduced:.2}").unwrap();
        }
        writeln!(file).unwrap();
    }
    file.flush().unwrap();
}

/// Synthetic late reverberation: a four-line feedback delay network
/// (Hadamard feedback) with a 20 ms pre-delay, so the first echoes arrive
/// after about 50 ms and decay 60 dB per `rt60`. Early reflections are
/// deliberately absent: only late reverberation is probed.
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

fn seeded_noise(len: usize, seed: u32, low_passed: bool) -> Vec<f32> {
    let (mut state, mut smoothed) = (seed, 0.0_f32);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let white = state as f32 / 2_147_483_648.0 - 1.0;
            smoothed = 0.85 * smoothed + 0.15 * white;
            if low_passed {
                smoothed
            } else {
                white
            }
        })
        .collect()
}

fn scale_to_db(signal: &mut [f32], db: f32) {
    let rms = (signal.iter().map(|x| x * x).sum::<f32>() / signal.len() as f32).sqrt();
    let gain = 10.0_f32.powf(db / 20.0) / rms.max(1e-12);
    signal.iter_mut().for_each(|x| *x *= gain);
}

/// Delay (samples) that best aligns `processed` with `raw`.
fn align(raw: &[f32], processed: &[f32], rate: u32) -> usize {
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
    let max = (rate as usize / 4).min(processed.len() - 1);
    let coarse = (0..=max)
        .step_by(16)
        .max_by(|&a, &b| correlation(a).total_cmp(&correlation(b)))
        .unwrap();
    (coarse.saturating_sub(16)..=(coarse + 16).min(max))
        .max_by(|&a, &b| correlation(a).total_cmp(&correlation(b)))
        .unwrap()
}

/// Controlled probe: how loud a known added signal (late reverberation or
/// steady noise) comes out, relative to how loud the speech comes out.
/// Renders take + probe and take - probe and projects half the difference
/// onto the probe over the raw take's speech windows (> -45 dBFS, 100 ms).
/// Lower is cleaner. Like the noise probe, this measures only the part of
/// the response that tracks the probe in a nonlinear chain.
fn probe(
    out_dir: &std::path::Path,
    rate: u32,
    raw: &[f32],
    full: MicSettings,
    noise_only: MicSettings,
    tails: bool,
) {
    let base = DenoiseOptions::default();
    let reverb = |rt60| DenoiseOptions {
        late_reverb_rt60: rt60,
        ..base
    };
    let unprotected = DenoiseOptions {
        speech_protection_db: 100.0,
        ..base
    };
    let room_variants = [
        ("noise-only 0.4.7", noise_only, base),
        ("noise-only no protection", noise_only, unprotected),
        ("noise-only dereverb 0.5 s", noise_only, reverb(0.5)),
        ("noise-only dereverb 0.8 s", noise_only, reverb(0.8)),
        ("full 0.4.7", full, base),
        ("full dereverb 0.5 s", full, reverb(0.5)),
    ];
    let tail_variants = [
        ("full 0.4.7", full, base),
        ("full pause tail 3 dB", full, pause_tail(3.0)),
        ("full pause tail 6 dB", full, pause_tail(6.0)),
    ];
    let variants = if tails {
        &tail_variants[..]
    } else {
        &room_variants[..]
    };
    let mut report = String::from(
        "stream\tvariant\tspeech gain dB\tlate reverb dB\tlow-passed noise dB\twhite noise dB\n",
    );
    println!("Leakage relative to speech gain (dB, lower is cleaner):");
    for (stream, repeats) in [("fresh", 1_usize), ("warm", 4)] {
        let take: Vec<f32> = raw
            .iter()
            .copied()
            .cycle()
            .take(raw.len() * repeats)
            .collect();
        let start = raw.len() * (repeats - 1);
        // Probes at fixed levels: late reverb 10 dB below the take, noise
        // at -55 dBFS (the level of the existing noise probe).
        let mut room = late_reverb(&take, rate, 0.6);
        let take_db = 10.0 * (take.iter().map(|x| x * x).sum::<f32>() / take.len() as f32).log10();
        scale_to_db(&mut room, take_db - 10.0);
        let mut low = seeded_noise(take.len(), 0x47c_11a9, true);
        scale_to_db(&mut low, -55.0);
        let mut white = seeded_noise(take.len(), 0x5a7_91f3, false);
        scale_to_db(&mut white, -55.0);
        let window = rate as usize / 10;
        let energy = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>();
        let speech: Vec<usize> = (0..raw.len() / window)
            .filter(|&w| {
                let x = &raw[w * window..(w + 1) * window];
                10.0 * (energy(x) / window as f32).log10() > -45.0
            })
            .collect();
        for &(name, settings, options) in variants {
            let clean = render(rate, &take, &settings, options);
            let delay = align(raw, &clean[start..], rate);
            let at = |w: usize| start + w * window + delay..start + (w + 1) * window + delay;
            let usable: Vec<usize> = speech
                .iter()
                .copied()
                .filter(|&w| at(w).end <= clean.len())
                .collect();
            let mut gains: Vec<f32> = usable
                .iter()
                .map(|&w| {
                    let x = &raw[w * window..(w + 1) * window];
                    10.0 * (energy(&clean[at(w)]) / energy(x).max(1e-20)).log10()
                })
                .collect();
            gains.sort_by(f32::total_cmp);
            let speech_gain = gains[gains.len() / 2];
            let mut leak = Vec::new();
            for added in [&room, &low, &white] {
                let plus: Vec<f32> = take.iter().zip(added.iter()).map(|(x, a)| x + a).collect();
                let minus: Vec<f32> = take.iter().zip(added.iter()).map(|(x, a)| x - a).collect();
                let plus = render(rate, &plus, &settings, options);
                let minus = render(rate, &minus, &settings, options);
                let (mut cross, mut power) = (0.0_f64, 0.0_f64);
                for &w in &usable {
                    for (k, i) in at(w).enumerate() {
                        let a = added[start + w * window + k] as f64;
                        cross += 0.5 * (plus[i] - minus[i]) as f64 * a;
                        power += a * a;
                    }
                }
                leak.push(20.0 * (cross / power).abs().log10() as f32 - speech_gain);
            }
            println!(
                "  {stream:5} {name:26} speech gain {speech_gain:+6.2}  late reverb {:+6.2}  low-passed noise {:+6.2}  white noise {:+6.2}",
                leak[0], leak[1], leak[2]
            );
            report.push_str(&format!(
                "{stream}\t{name}\t{speech_gain:.2}\t{:.2}\t{:.2}\t{:.2}\n",
                leak[0], leak[1], leak[2]
            ));
        }
    }
    std::fs::write(out_dir.join("probe.tsv"), report).unwrap();
}
