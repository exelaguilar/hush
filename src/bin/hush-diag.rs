//! Hush diagnostics (read-only):
//!
//!   hush-diag                         list audio devices and formats
//!   hush-diag --mic-preview [s] [mic] record the mic and save before/after WAVs
//!
//! Neither command changes anything on the system.

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use hush::virtual_output::{
    is_hush_virtual_input, is_hush_virtual_output, is_third_party_virtual_endpoint,
};

/// `--mic-preview [seconds] [device substring]`: records the physical
/// microphone and writes before.wav / after.wav, where "after" is processed
/// by exactly the chain the Hush APO runs. Changes nothing on the system.
fn mic_preview(seconds: u64, query: Option<&str>) -> Result<()> {
    use hush_apo::dsp::{CleanMic, FRAME};
    use std::sync::{Arc, Mutex};

    let host = cpal::default_host();
    let devices: Vec<cpal::Device> = host.input_devices()?.collect();
    let device = devices
        .into_iter()
        .find(|d| {
            let name = d.name().unwrap_or_default();
            match query {
                Some(q) => name.to_lowercase().contains(&q.to_lowercase()),
                None => {
                    !is_third_party_virtual_endpoint(&name)
                        && !is_hush_virtual_input(&name)
                        && !is_hush_virtual_output(&name)
                }
            }
        })
        .ok_or_else(|| anyhow::anyhow!("no matching physical microphone"))?;
    let name = device.name().unwrap_or_default();
    let supported = device.default_input_config()?;
    let rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.clone().into();
    let recorded = Arc::new(Mutex::new(Vec::<f32>::with_capacity(
        rate as usize * channels * (seconds as usize + 1),
    )));
    let sink = recorded.clone();
    let err = |e| eprintln!("stream error: {e}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| sink.lock().unwrap().extend_from_slice(data),
            err,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| {
                sink.lock()
                    .unwrap()
                    .extend(data.iter().map(|s| *s as f32 / 32768.0))
            },
            err,
            None,
        )?,
        other => anyhow::bail!("unsupported microphone format {other:?}"),
    };
    use cpal::traits::StreamTrait;
    stream.play()?;
    println!("Recording {seconds} s from \"{name}\" ({rate} Hz, {channels} ch). Talk normally, with a pause or two...");
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    drop(stream);
    let before = recorded.lock().unwrap().clone();

    let mut after = before.clone();
    CleanMic::new_inline(rate, channels).process(&mut after, &hush_apo::settings::load());
    // Remove the chain's fixed 10 ms latency so both files line up.
    let shift = (FRAME * channels).min(after.len());
    after.drain(..shift);
    after.extend(std::iter::repeat_n(0.0, shift));

    let dir = std::env::temp_dir().join("hush-preview");
    std::fs::create_dir_all(&dir)?;
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    for (file, data) in [("before.wav", &before), ("after.wav", &after)] {
        let mut writer = hound::WavWriter::create(dir.join(file), spec)?;
        for s in data.iter() {
            writer.write_sample(*s)?;
        }
        writer.finalize()?;
    }

    // Noise floor = quietest 10% of 10 ms blocks; level = loudest 10%.
    let block_levels = |data: &[f32]| -> Vec<f32> {
        let mut levels: Vec<f32> = data
            .chunks(FRAME * channels)
            .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len().max(1) as f32).sqrt())
            .collect();
        levels.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        levels
    };
    let db = |v: f32| 20.0 * v.max(1e-9).log10();
    let summary = |data: &[f32]| {
        let levels = block_levels(data);
        let n = levels.len().max(1);
        let floor = levels[..(n / 10).max(1)].iter().sum::<f32>() / (n / 10).max(1) as f32;
        let loud = levels[n - (n / 10).max(1)..].iter().sum::<f32>() / (n / 10).max(1) as f32;
        (db(floor), db(loud))
    };
    let (floor_before, loud_before) = summary(&before);
    let (floor_after, loud_after) = summary(&after);
    println!("Noise floor: {floor_before:6.1} dBFS -> {floor_after:6.1} dBFS");
    println!("Voice level: {loud_before:6.1} dBFS -> {loud_after:6.1} dBFS");
    println!(
        "Saved {} and {}",
        dir.join("before.wav").display(),
        dir.join("after.wav").display()
    );
    Ok(())
}

fn describe(config: &cpal::SupportedStreamConfig) -> String {
    format!(
        "{} Hz, {} ch, {:?}",
        config.sample_rate().0,
        config.channels(),
        config.sample_format()
    )
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--mic-preview") {
        let seconds = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(6);
        return mic_preview(seconds, args.get(2).map(String::as_str));
    }
    let host = cpal::default_host();
    println!("Microphones:");
    for device in host.input_devices()? {
        let name = device.name().unwrap_or_default();
        let format = device
            .default_input_config()
            .map(|c| describe(&c))
            .unwrap_or_else(|_| "format unavailable".into());
        println!("  {name} - {format}");
    }
    println!("\nPlayback devices:");
    for device in host.output_devices()? {
        let name = device.name().unwrap_or_default();
        let format = device
            .default_output_config()
            .map(|c| describe(&c))
            .unwrap_or_else(|_| "format unavailable".into());
        println!("  {name} - {format}");
    }
    Ok(())
}
