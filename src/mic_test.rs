//! Microphone test for the Hush window: a live level meter, "hear
//! myself" monitoring through the default playback device, and a 10-second
//! record + playback. The microphone is opened like any other app opens it,
//! so the preview includes the APO when it is installed.

#[path = "mic_test_input.rs"]
mod input;

use anyhow::{anyhow, Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, Stream, StreamConfig,
};
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
};

pub const RECORD_SECONDS: usize = 10;
/// Monitoring keeps at most this much audio queued (latency cap).
const MONITOR_MAX_QUEUE_MS: usize = 60;

#[derive(Default)]
struct Recording {
    active: bool,
    samples: Vec<f32>,
    limit: usize,
}

#[derive(Default)]
struct Playback {
    samples: Vec<f32>,
    position: usize,
    playing: bool,
}

struct Shared {
    /// Peak level of the most recent input buffer (f32 bits).
    peak: AtomicU32,
    monitor: AtomicBool,
    recording: Mutex<Recording>,
    playback: Mutex<Playback>,
    monitor_queue: Mutex<Option<rtrb::Producer<f32>>>,
    input_error: Mutex<Option<String>>,
    output_error: Mutex<Option<String>>,
}

/// Linear-interpolating sample-rate converter for one mono stream.
struct Resampler {
    step: f64,
    position: f64,
    previous: f32,
    next: f32,
}

impl Resampler {
    fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            step: input_rate as f64 / output_rate.max(1) as f64,
            position: 0.0,
            previous: 0.0,
            next: 0.0,
        }
    }

    fn sample(&mut self, mut pull: impl FnMut() -> f32) -> f32 {
        self.position += self.step;
        while self.position >= 1.0 {
            self.position -= 1.0;
            self.previous = self.next;
            self.next = pull();
        }
        self.previous + (self.next - self.previous) * self.position as f32
    }
}

pub struct MicTest {
    input: Option<input::Capture>,
    /// Opened on first use (monitoring or playback), so a meter-only test
    /// never touches the speakers.
    output: RefCell<Option<Stream>>,
    shared: Arc<Shared>,
    input_rate: u32,
}

impl MicTest {
    pub fn start(device_id: &str) -> Result<Self> {
        let shared = Arc::new(Shared {
            peak: AtomicU32::new(0),
            monitor: AtomicBool::new(false),
            recording: Mutex::new(Recording::default()),
            playback: Mutex::new(Playback::default()),
            monitor_queue: Mutex::new(None),
            input_error: Mutex::new(None),
            output_error: Mutex::new(None),
        });
        let input_shared = shared.clone();
        let errors = shared.clone();
        let (input, input_rate) = input::Capture::start(
            device_id,
            move |data, channels, frames| {
                let mut recording = input_shared.recording.try_lock().ok();
                let monitoring = input_shared.monitor.load(Ordering::Relaxed);
                let mut queue = monitoring
                    .then(|| input_shared.monitor_queue.try_lock().ok())
                    .flatten();
                let mut peak = 0.0_f32;
                for i in 0..frames {
                    let mono = if data.is_empty() {
                        0.0
                    } else {
                        data[i * channels..(i + 1) * channels].iter().sum::<f32>() / channels as f32
                    };
                    peak = peak.max(mono.abs());
                    if let Some(producer) = queue.as_deref_mut().and_then(Option::as_mut) {
                        let _ = producer.push(mono);
                    }
                    if let Some(recording) = recording.as_mut() {
                        if recording.active && recording.samples.len() < recording.limit {
                            recording.samples.push(mono);
                        }
                    }
                }
                input_shared.peak.store(peak.to_bits(), Ordering::Relaxed);
            },
            move |error| {
                if let Ok(mut state) = errors.input_error.lock() {
                    *state = Some(error);
                }
            },
        )?;
        Ok(Self {
            input: Some(input),
            output: RefCell::new(None),
            shared,
            input_rate,
        })
    }

    /// Opens the default playback device for monitoring and playback, once.
    fn open_output(&self) -> Result<()> {
        if self.output.borrow().is_some() {
            return Ok(());
        }
        let (producer, mut monitor_queue) =
            rtrb::RingBuffer::<f32>::new(self.input_rate as usize / 2);
        let output = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| anyhow!("no playback device"))?;
        let output_config = output.default_output_config()?;
        if output_config.sample_format() != SampleFormat::F32 {
            return Err(anyhow!(
                "unsupported playback format {:?}",
                output_config.sample_format()
            ));
        }
        let input_rate = self.input_rate;
        let output_rate = output_config.sample_rate().0;
        let out_channels = output_config.channels() as usize;
        let max_queue = input_rate as usize * MONITOR_MAX_QUEUE_MS / 1000;
        let output_shared = self.shared.clone();
        let errors = self.shared.clone();
        let mut monitor_resampler = Resampler::new(input_rate, output_rate);
        let mut playback_resampler = Resampler::new(input_rate, output_rate);
        let output_config: StreamConfig = output_config.into();
        let output_stream = output
            .build_output_stream(
                &output_config,
                move |data: &mut [f32], _| {
                    let monitoring = output_shared.monitor.load(Ordering::Relaxed);
                    if !monitoring {
                        // Drain so re-enabling monitoring starts fresh.
                        while monitor_queue.pop().is_ok() {}
                    } else if monitor_queue.slots() > max_queue {
                        for _ in 0..monitor_queue.slots() - max_queue / 2 {
                            let _ = monitor_queue.pop();
                        }
                    }
                    let mut playback = output_shared.playback.try_lock().ok();
                    for frame in data.chunks_exact_mut(out_channels.max(1)) {
                        let mut value = 0.0;
                        if monitoring {
                            value +=
                                monitor_resampler.sample(|| monitor_queue.pop().unwrap_or(0.0));
                        }
                        if let Some(playback) = playback.as_mut() {
                            if playback.playing {
                                value += playback_resampler.sample(|| {
                                    let sample = playback.samples.get(playback.position).copied();
                                    playback.position += 1;
                                    sample.unwrap_or(0.0)
                                });
                                if playback.position > playback.samples.len() {
                                    playback.playing = false;
                                }
                            }
                        }
                        for sample in frame.iter_mut() {
                            *sample = value.clamp(-1.0, 1.0);
                        }
                    }
                },
                move |error| {
                    if let Ok(mut state) = errors.output_error.lock() {
                        *state = Some(format!("Playback stopped: {error}"));
                    }
                },
                None,
            )
            .context("opening the playback device")?;
        output_stream.play()?;
        *self
            .shared
            .monitor_queue
            .lock()
            .map_err(|_| anyhow!("Monitor queue unavailable"))? = Some(producer);
        *self.output.borrow_mut() = Some(output_stream);
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.input.is_some()
    }

    pub fn stop_input(&mut self) {
        self.input = None;
        self.shared.monitor.store(false, Ordering::Relaxed);
        self.stop_recording();
    }

    pub fn poll_error(&mut self) -> Option<String> {
        let input = self.shared.input_error.lock().ok()?.take();
        if input.is_some() {
            self.stop_input();
        }
        let output = self.shared.output_error.lock().ok()?.take();
        if output.is_some() {
            *self.output.borrow_mut() = None;
            self.shared.monitor.store(false, Ordering::Relaxed);
            self.stop_playback();
        }
        input.or(output)
    }

    /// Peak of the latest input buffer, 0..1.
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.shared.peak.load(Ordering::Relaxed))
    }

    /// Turns "hear myself" on or off (opening the speakers the first time).
    pub fn set_monitor(&self, on: bool) -> Result<()> {
        if on {
            self.open_output()?;
        }
        self.shared.monitor.store(on, Ordering::Relaxed);
        Ok(())
    }

    pub fn start_recording(&self) {
        self.stop_playback();
        let mut recording = self
            .shared
            .recording
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        recording.samples = Vec::with_capacity(self.input_rate as usize * RECORD_SECONDS);
        recording.limit = self.input_rate as usize * RECORD_SECONDS;
        recording.active = true;
    }

    /// Fraction of the recording done, or `None` when not recording.
    pub fn recording_progress(&self) -> Option<f32> {
        let mut recording = self
            .shared
            .recording
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !recording.active {
            return None;
        }
        if recording.samples.len() >= recording.limit {
            recording.active = false;
            return None;
        }
        Some(recording.samples.len() as f32 / recording.limit.max(1) as f32)
    }

    pub fn stop_recording(&self) {
        self.shared
            .recording
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active = false;
    }

    pub fn has_recording(&self) -> bool {
        !self
            .shared
            .recording
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .samples
            .is_empty()
    }

    pub fn play_recording(&self) -> Result<()> {
        self.open_output()?;
        let samples = self
            .shared
            .recording
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .samples
            .clone();
        let mut playback = self
            .shared
            .playback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        playback.samples = samples;
        playback.position = 0;
        playback.playing = true;
        Ok(())
    }

    /// Saves the last recording as a WAV file in Documents\Hush Recordings
    /// and returns its path.
    pub fn save_recording(&self) -> Result<std::path::PathBuf> {
        let samples = self
            .shared
            .recording
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .samples
            .clone();
        if samples.is_empty() {
            return Err(anyhow!("nothing recorded yet"));
        }
        let dir = recordings_dir();
        std::fs::create_dir_all(&dir)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = dir.join(format!("hush-{stamp}.wav"));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: self.input_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec)?;
        for sample in samples {
            writer.write_sample(sample)?;
        }
        writer.finalize()?;
        Ok(path)
    }

    pub fn stop_playback(&self) {
        self.shared
            .playback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .playing = false;
    }

    pub fn is_playing(&self) -> bool {
        self.shared
            .playback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .playing
    }
}

/// `Documents\Hush Recordings` in the user's real Documents folder, which
/// may be redirected (for example to OneDrive).
pub fn recordings_dir() -> std::path::PathBuf {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
    };
    let documents = unsafe {
        SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DEFAULT, None)
            .ok()
            .map(|path| {
                let text = std::ffi::OsString::from_wide(path.as_wide());
                CoTaskMemFree(Some(path.0 as *const _));
                text
            })
    };
    documents
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join("Documents")
        })
        .join("Hush Recordings")
}
