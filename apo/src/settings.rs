//! Live settings shared between the Hush app and the APO.
//!
//! The app writes `%ProgramData%\Hush\mic.conf` (simple `key = value`
//! lines). Inside the audio engine a background thread polls the file and
//! publishes the values through atomics, which the real-time thread reads
//! without locking.

use crate::dsp::MicSettings;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        OnceLock,
    },
    time::{Duration, SystemTime},
};

pub const FILE_NAME: &str = "mic.conf";

pub fn settings_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("Hush")
}

pub fn settings_path() -> PathBuf {
    settings_dir().join(FILE_NAME)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

fn parse_db(value: &str, limit: f32) -> Option<f32> {
    value
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(-limit, limit))
}

/// Unknown keys and malformed values are ignored (the default is kept), so a
/// half-written or newer file never disables the microphone.
pub fn parse(text: &str) -> MicSettings {
    let mut s = MicSettings::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "enabled" => s.enabled = parse_bool(value).unwrap_or(s.enabled),
            "noise_suppression" => {
                s.noise_suppression = parse_bool(value).unwrap_or(s.noise_suppression)
            }
            "suppression" => {
                if let Some(v) = parse_db(value, 1000.0) {
                    s.suppression = (v / 100.0).clamp(0.0, 1.0);
                }
            }
            "high_pass" => s.high_pass = parse_bool(value).unwrap_or(s.high_pass),
            "leveler" => s.leveler = parse_bool(value).unwrap_or(s.leveler),
            "gain_db" => s.gain_db = parse_db(value, 24.0).unwrap_or(s.gain_db),
            "bass_db" => s.bass_db = parse_db(value, 12.0).unwrap_or(s.bass_db),
            "clarity_db" => s.clarity_db = parse_db(value, 12.0).unwrap_or(s.clarity_db),
            "air_db" => s.air_db = parse_db(value, 12.0).unwrap_or(s.air_db),
            "gate" => s.gate = parse_bool(value).unwrap_or(s.gate),
            "deesser" => s.deesser = parse_bool(value).unwrap_or(s.deesser),
            "desk_mic" => s.desk_mic = parse_bool(value).unwrap_or(s.desk_mic),
            "warmth" => {
                if let Some(v) = parse_db(value, 1000.0) {
                    s.warmth = (v / 100.0).clamp(0.0, 1.0);
                }
            }
            "click_suppression" => {
                s.click_suppression = parse_bool(value).unwrap_or(s.click_suppression)
            }
            "gate_sensitivity" => {
                if let Some(v) = parse_db(value, 1000.0) {
                    s.gate_sensitivity = (v / 100.0).clamp(0.0, 1.0);
                }
            }
            "preset" => s.preset = Preset::parse(value).unwrap_or(s.preset),
            _ => {}
        }
    }
    s
}

pub fn format(s: &MicSettings) -> String {
    let flag = |b: bool| if b { "on" } else { "off" };
    format!(
        "# Hush clean-mic settings. Changes apply live.\n\
         enabled = {}\n\
         noise_suppression = {}\n\
         # 0-100: noise removal strength\n\
         suppression = {}\n\
         high_pass = {}\n\
         leveler = {}\n\
         gain_db = {:.1}\n\
         # voice EQ in dB (-12..12); preset is natural, warm, broadcast or custom\n\
         preset = {}\n\
         bass_db = {:.1}\n\
         clarity_db = {:.1}\n\
         air_db = {:.1}\n\
         # voice gate: silence between words; sensitivity 0-100 (higher opens on quieter speech)\n\
         gate = {}\n\
         gate_sensitivity = {}\n\
         deesser = {}\n\
         click_suppression = {}\n\
         # 0-100: tape-style warmth\n\
         warmth = {}\n\
         # mic position: on = on the desk (more auto boost, gate holds through soft word endings)\n\
         desk_mic = {}\n",
        flag(s.enabled),
        flag(s.noise_suppression),
        (s.suppression * 100.0).round(),
        flag(s.high_pass),
        flag(s.leveler),
        s.gain_db,
        s.preset.name(),
        s.bass_db,
        s.clarity_db,
        s.air_db,
        flag(s.gate),
        (s.gate_sensitivity * 100.0).round(),
        flag(s.deesser),
        flag(s.click_suppression),
        (s.warmth * 100.0).round(),
        flag(s.desk_mic),
    )
}

/// Writes the settings file atomically (temp file + rename) so the audio
/// engine never reads a half-written file.
pub fn save(settings: &MicSettings) -> std::io::Result<()> {
    let path = settings_path();
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, format(settings))?;
    std::fs::rename(&temp, &path)
}

pub fn load() -> MicSettings {
    std::fs::read_to_string(settings_path())
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// Voice tone presets (what Sonar calls "presets"): values for the three EQ
/// controls. `Custom` means the user moved a slider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    Natural,
    #[default]
    Warm,
    Broadcast,
    Custom,
}

impl Preset {
    pub const ALL: [Preset; 4] = [
        Preset::Natural,
        Preset::Warm,
        Preset::Broadcast,
        Preset::Custom,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Preset::Natural => "natural",
            Preset::Warm => "warm",
            Preset::Broadcast => "broadcast",
            Preset::Custom => "custom",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Preset::Natural => "Natural",
            Preset::Warm => "Warm",
            Preset::Broadcast => "Broadcast (crisp)",
            Preset::Custom => "Custom",
        }
    }

    pub fn parse(text: &str) -> Option<Preset> {
        Preset::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(text.trim()))
    }

    /// (bass, clarity, air) in dB; `None` for Custom.
    pub fn eq(self) -> Option<(f32, f32, f32)> {
        match self {
            Preset::Natural => Some((1.0, 0.5, 0.0)),
            Preset::Warm => Some((3.5, 2.0, 1.0)),
            Preset::Broadcast => Some((3.0, 3.0, 1.0)),
            Preset::Custom => None,
        }
    }

    pub fn apply(self, settings: &mut MicSettings) {
        settings.preset = self;
        if let Some((bass, clarity, air)) = self.eq() {
            settings.bass_db = bass;
            settings.clarity_db = clarity;
            settings.air_db = air;
        }
    }
}

// Lock-free snapshot storage for the real-time thread: one atomic per value.
const F_ENABLED: usize = 0;
const F_NS: usize = 1;
const F_SUPPRESSION: usize = 2;
const F_HIGH_PASS: usize = 3;
const F_LEVELER: usize = 4;
const F_GAIN: usize = 5;
const F_BASS: usize = 6;
const F_CLARITY: usize = 7;
const F_AIR: usize = 8;
const F_GATE: usize = 9;
const F_GATE_SENSITIVITY: usize = 10;
const F_DEESSER: usize = 11;
const F_CLICK: usize = 12;
const F_WARMTH: usize = 13;
const F_DESK: usize = 14;
const FIELDS: usize = 15;

static SHARED: [AtomicU32; FIELDS] = [const { AtomicU32::new(0) }; FIELDS];
static SETTINGS_VERSION: AtomicU32 = AtomicU32::new(0);

fn pack(settings: &MicSettings) -> [u32; FIELDS] {
    let b = |v: bool| v as u32;
    [
        b(settings.enabled),
        b(settings.noise_suppression),
        settings.suppression.to_bits(),
        b(settings.high_pass),
        b(settings.leveler),
        settings.gain_db.to_bits(),
        settings.bass_db.to_bits(),
        settings.clarity_db.to_bits(),
        settings.air_db.to_bits(),
        b(settings.gate),
        settings.gate_sensitivity.to_bits(),
        b(settings.deesser),
        b(settings.click_suppression),
        settings.warmth.to_bits(),
        b(settings.desk_mic),
    ]
}

fn publish(settings: &MicSettings) {
    SETTINGS_VERSION.fetch_add(1, Ordering::SeqCst);
    for (slot, value) in SHARED.iter().zip(pack(settings)) {
        slot.store(value, Ordering::SeqCst);
    }
    SETTINGS_VERSION.fetch_add(1, Ordering::SeqCst);
}

/// Take one bounded attempt so a preempted writer cannot stall audio.
pub fn current() -> Option<MicSettings> {
    let version = SETTINGS_VERSION.load(Ordering::SeqCst);
    if version == 0 || version % 2 != 0 {
        return None;
    }
    let v: [u32; FIELDS] = std::array::from_fn(|i| SHARED[i].load(Ordering::SeqCst));
    if SETTINGS_VERSION.load(Ordering::SeqCst) != version {
        return None;
    }
    Some(MicSettings {
        enabled: v[F_ENABLED] != 0,
        noise_suppression: v[F_NS] != 0,
        suppression: f32::from_bits(v[F_SUPPRESSION]),
        high_pass: v[F_HIGH_PASS] != 0,
        leveler: v[F_LEVELER] != 0,
        gain_db: f32::from_bits(v[F_GAIN]),
        bass_db: f32::from_bits(v[F_BASS]),
        clarity_db: f32::from_bits(v[F_CLARITY]),
        air_db: f32::from_bits(v[F_AIR]),
        gate: v[F_GATE] != 0,
        gate_sensitivity: f32::from_bits(v[F_GATE_SENSITIVITY]),
        deesser: v[F_DEESSER] != 0,
        click_suppression: v[F_CLICK] != 0,
        warmth: f32::from_bits(v[F_WARMTH]),
        desk_mic: v[F_DESK] != 0,
        preset: Preset::Custom,
    })
}

pub const TRACE_FILE_NAME: &str = "apo-trace.log";

pub fn trace_path() -> PathBuf {
    settings_dir().join(TRACE_FILE_NAME)
}

/// Writes to the Windows Application event log under the source
/// "Hush" (event id 1). Works from inside the audio service, whose
/// write-restricted token may block file writes. Read it with:
/// `Get-WinEvent -FilterHashtable @{LogName='Application'; ProviderName='Hush'}`
#[cfg(windows)]
fn event_log(message: &str) {
    use windows::Win32::System::EventLog::{
        DeregisterEventSource, RegisterEventSourceW, ReportEventW, EVENTLOG_INFORMATION_TYPE,
    };
    use windows_core::{w, HSTRING, PCWSTR};
    let text = HSTRING::from(message);
    unsafe {
        if let Ok(handle) = RegisterEventSourceW(PCWSTR::null(), w!("Hush")) {
            let strings = [PCWSTR(text.as_ptr())];
            let _ = ReportEventW(
                handle,
                EVENTLOG_INFORMATION_TYPE,
                0,
                1,
                None,
                0,
                Some(&strings),
                None,
            );
            let _ = DeregisterEventSource(handle);
        }
    }
}

/// Diagnostic tracing is off unless `%ProgramData%\Hush\debug.flag`
/// exists (checked once per process), so normal use writes nothing.
fn debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| settings_dir().join("debug.flag").exists())
}

/// Always recorded: something the user may need to know went wrong.
pub fn trace_error(message: &str) {
    #[cfg(windows)]
    event_log(message);
    if debug_enabled() {
        trace_file(message);
    }
}

/// Configuration-time diagnostics (never call from the real-time thread).
/// Only recorded when debugging is enabled (see [`debug_enabled`]).
pub fn trace(message: &str) {
    if !debug_enabled() {
        return;
    }
    #[cfg(windows)]
    event_log(message);
    trace_file(message);
}

/// Appends to `apo-trace.log` when writable (capped at 512 KB).
fn trace_file(message: &str) {
    use std::io::Write;
    let path = trace_path();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 512 * 1024) {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            file,
            "{} pid {} {}",
            unix_seconds(),
            std::process::id(),
            message
        );
    }
}

pub const STATUS_FILE_NAME: &str = "status.txt";

pub fn status_path() -> PathBuf {
    settings_dir().join(STATUS_FILE_NAME)
}

/// Counters updated by the real-time thread (atomics only) and written to
/// `status.txt` by the watcher thread, so the app and installer can confirm
/// the APO is really running inside the audio engine.
static PROCESSED_FRAMES: AtomicU64 = AtomicU64::new(0);
static STREAM_RATE: AtomicU32 = AtomicU32::new(0);
static STREAM_CHANNELS: AtomicU32 = AtomicU32::new(0);

static FAULTED: AtomicBool = AtomicBool::new(false);
static WORKER_FALLBACK: AtomicBool = AtomicBool::new(false);

/// Called from the real-time thread when processing panicked: the watcher
/// thread logs it (no I/O on the real-time thread).
pub fn report_fault() {
    FAULTED.store(true, Ordering::Relaxed);
}

/// Called from the real-time thread when denoising permanently falls back.
/// The settings watcher records the event outside the callback.
pub fn report_worker_fallback() {
    WORKER_FALLBACK.store(true, Ordering::Relaxed);
}

/// Called from the real-time thread.
pub fn record_processed(frames: u32, rate: u32, channels: u32) {
    PROCESSED_FRAMES.fetch_add(frames as u64, Ordering::Relaxed);
    STREAM_RATE.store(rate, Ordering::Relaxed);
    STREAM_CHANNELS.store(channels, Ordering::Relaxed);
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parsed `status.txt`.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineStatus {
    pub process_id: u32,
    pub processed_frames: u64,
    pub rate: u32,
    pub channels: u32,
    pub updated_unix: u64,
    /// Times the denoiser worker was late (a short gap was played).
    pub underruns: u64,
    /// Samples skipped to keep latency bounded.
    pub skipped: u64,
}

pub fn parse_status(text: &str) -> Option<EngineStatus> {
    let mut status = EngineStatus {
        process_id: 0,
        processed_frames: 0,
        rate: 0,
        channels: 0,
        updated_unix: 0,
        underruns: 0,
        skipped: 0,
    };
    let mut seen = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "process_id" => status.process_id = value.parse().ok()?,
            "processed_frames" => status.processed_frames = value.parse().ok()?,
            "rate" => status.rate = value.parse().ok()?,
            "channels" => status.channels = value.parse().ok()?,
            "underruns" => status.underruns = value.parse().ok()?,
            "skipped" => status.skipped = value.parse().ok()?,
            "updated_unix" => {
                status.updated_unix = value.parse().ok()?;
                seen = true;
            }
            _ => {}
        }
    }
    seen.then_some(status)
}

fn write_status() {
    let text = format!(
        "# Written by the Hush APO inside audiodg.exe\n\
         process_id = {}\n\
         processed_frames = {}\n\
         rate = {}\n\
         channels = {}\n\
         updated_unix = {}\n\
         underruns = {}\n\
         skipped = {}\n",
        std::process::id(),
        PROCESSED_FRAMES.load(Ordering::Relaxed),
        STREAM_RATE.load(Ordering::Relaxed),
        STREAM_CHANNELS.load(Ordering::Relaxed),
        unix_seconds(),
        crate::denoise::UNDERRUNS.load(Ordering::Relaxed),
        crate::denoise::SKIPPED.load(Ordering::Relaxed)
    );
    // Best effort: write to a temp file and rename so readers never see a
    // half-written file.
    let path = status_path();
    let temp = path.with_extension("tmp");
    let result = std::fs::write(&temp, text).and_then(|_| std::fs::rename(&temp, &path));
    if let Err(error) = result {
        static REPORTED: AtomicBool = AtomicBool::new(false);
        if !REPORTED.swap(true, Ordering::Relaxed) {
            trace(&format!("cannot write {}: {error}", path.display()));
        }
    }
}

/// Starts (once per process) the thread that keeps [`current`] in sync with
/// the settings file and publishes `status.txt`. If the settings file is
/// missing, defaults apply.
pub fn start_watcher() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let path = settings_path();
        let mut last: Option<SystemTime> = None;
        let mut last_frames = u64::MAX;
        let mut ticks = 0_u32;
        let mut load = move || {
            if let Some(settings) = load_if_changed(&path, &mut last, |path| {
                std::fs::read_to_string(path)
            }) {
                publish(&settings);
            }
        };
        load();
        let _ = std::thread::Builder::new()
            .name("hush-settings".into())
            .spawn(move || {
                let mut live = LiveWriter::open();
                let mut live_frames = u64::MAX;
                loop {
                    // 20 Hz: live meter; 4 Hz: settings; 1 Hz: status file
                    // (only while audio flows, plus a heartbeat every ~30 s).
                    std::thread::sleep(Duration::from_millis(50));
                    ticks = ticks.wrapping_add(1);
                    let frames = PROCESSED_FRAMES.load(Ordering::Relaxed);
                    if let Some(live) = live.as_mut() {
                        if frames != live_frames {
                            live_frames = frames;
                            live.write(&LiveLevels {
                                input: f32::from_bits(IN_PEAK.swap(0, Ordering::Relaxed)),
                                output: f32::from_bits(OUT_PEAK.swap(0, Ordering::Relaxed)),
                                voice: crate::denoise::GATE_OPEN.load(Ordering::Relaxed),
                                updated_ms: unix_millis(),
                            });
                        }
                    }
                    if ticks.is_multiple_of(5) {
                        load();
                    }
                    if FAULTED.swap(false, Ordering::Relaxed) {
                        trace_error(
                            "Clean Mic processing failed; this stream now passes the microphone through unprocessed",
                        );
                    }
                    if WORKER_FALLBACK.swap(false, Ordering::Relaxed) {
                        trace_error(
                            "Denoiser worker stalled or failed; this stream now passes microphone input through",
                        );
                    }
                    if ticks.is_multiple_of(20)
                        && (frames != last_frames || ticks.is_multiple_of(600))
                    {
                        last_frames = frames;
                        write_status();
                    }
                }
            });
    });
}

fn load_if_changed(
    path: &std::path::Path,
    last: &mut Option<SystemTime>,
    mut read: impl FnMut(&std::path::Path) -> std::io::Result<String>,
) -> Option<MicSettings> {
    let modified = match std::fs::metadata(path) {
        Ok(metadata) => match metadata.modified() {
            Ok(modified) => Some(modified),
            Err(_) => return None,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    if modified == *last && last.is_some() {
        return None;
    }
    match modified {
        Some(modified) => match read(path) {
            Ok(text) => {
                *last = Some(modified);
                Some(parse(&text))
            }
            // Keep the last good snapshot and retry on the next poll.
            Err(_) => None,
        },
        None => {
            *last = None;
            Some(MicSettings::default())
        }
    }
}

// ---------------------------------------------------------------------------
// Live levels: a tiny memory-mapped file the app reads ~30x a second for its
// meters and voice light, without opening the microphone itself.
// ---------------------------------------------------------------------------

pub const LIVE_FILE_NAME: &str = "live.bin";
const LIVE_MAGIC: u32 = 0x5653_4D4C; // "LMSV"
const LIVE_SIZE: u64 = 32;

pub fn live_path() -> PathBuf {
    settings_dir().join(LIVE_FILE_NAME)
}

static IN_PEAK: AtomicU32 = AtomicU32::new(0);
static OUT_PEAK: AtomicU32 = AtomicU32::new(0);

/// Called from the real-time thread with this buffer's peaks. Non-negative
/// f32 bit patterns order like the values, so `fetch_max` keeps the loudest.
pub fn record_levels(input_peak: f32, output_peak: f32) {
    IN_PEAK.fetch_max(input_peak.max(0.0).to_bits(), Ordering::Relaxed);
    OUT_PEAK.fetch_max(output_peak.max(0.0).to_bits(), Ordering::Relaxed);
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LiveLevels {
    /// Microphone peak before processing (0..1).
    pub input: f32,
    /// Peak of what apps receive (0..1).
    pub output: f32,
    /// Voice gate open (you are talking).
    pub voice: bool,
    pub updated_ms: u64,
}

impl LiveLevels {
    /// Milliseconds since the plugin last processed audio.
    pub fn age_ms(&self) -> u64 {
        unix_millis().saturating_sub(self.updated_ms)
    }
}

// Layout (little endian): magic u32 | seq u32 (odd while writing) |
// input f32 | output f32 | voice u32 | pad u32 | updated_ms u64
struct LiveWriter {
    map: memmap2::MmapMut,
    seq: u32,
}

impl LiveWriter {
    fn open() -> Option<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(live_path())
            .ok()?;
        file.set_len(LIVE_SIZE).ok()?;
        let map = unsafe { memmap2::MmapMut::map_mut(&file) }.ok()?;
        Some(Self { map, seq: 0 })
    }

    fn put_u32(&mut self, offset: usize, value: u32) {
        let pointer = self.map[offset..offset + 4].as_mut_ptr() as *mut u32;
        unsafe { std::ptr::write_volatile(pointer, value.to_le()) };
    }

    fn write(&mut self, levels: &LiveLevels) {
        use std::sync::atomic::{fence, Ordering};
        self.seq = self.seq.wrapping_add(1) | 1;
        self.put_u32(4, self.seq);
        fence(Ordering::Release);
        self.put_u32(0, LIVE_MAGIC);
        self.put_u32(8, levels.input.to_bits());
        self.put_u32(12, levels.output.to_bits());
        self.put_u32(16, levels.voice as u32);
        self.put_u32(24, levels.updated_ms as u32);
        self.put_u32(28, (levels.updated_ms >> 32) as u32);
        fence(Ordering::Release);
        self.seq = self.seq.wrapping_add(1);
        self.put_u32(4, self.seq);
    }
}

/// Reader used by the app: maps `live.bin` once and reads it on demand.
pub struct LiveReader {
    map: Option<memmap2::Mmap>,
}

impl LiveReader {
    pub fn new() -> Self {
        Self { map: None }
    }

    fn get_u32(map: &memmap2::Mmap, offset: usize) -> u32 {
        let pointer = map[offset..offset + 4].as_ptr() as *const u32;
        u32::from_le(unsafe { std::ptr::read_volatile(pointer) })
    }

    /// Latest levels, or `None` if the plugin has never run.
    pub fn read(&mut self) -> Option<LiveLevels> {
        if self.map.is_none() {
            let file = std::fs::File::open(live_path()).ok()?;
            if file.metadata().ok()?.len() < LIVE_SIZE {
                return None;
            }
            self.map = unsafe { memmap2::Mmap::map(&file) }.ok();
        }
        let map = self.map.as_ref()?;
        for _ in 0..4 {
            let before = Self::get_u32(map, 4);
            if before & 1 == 1 {
                continue;
            }
            std::sync::atomic::fence(Ordering::Acquire);
            if Self::get_u32(map, 0) != LIVE_MAGIC {
                return None;
            }
            let levels = LiveLevels {
                input: f32::from_bits(Self::get_u32(map, 8)),
                output: f32::from_bits(Self::get_u32(map, 12)),
                voice: Self::get_u32(map, 16) != 0,
                updated_ms: Self::get_u32(map, 24) as u64 | ((Self::get_u32(map, 28) as u64) << 32),
            };
            std::sync::atomic::fence(Ordering::Acquire);
            if Self::get_u32(map, 4) == before {
                return Some(levels);
            }
        }
        None
    }
}

impl Default for LiveReader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_ignores_garbage() {
        let settings = MicSettings {
            enabled: true,
            noise_suppression: false,
            suppression: 0.5,
            high_pass: false,
            leveler: true,
            gain_db: -3.0,
            bass_db: 2.5,
            clarity_db: -1.0,
            air_db: 6.0,
            gate: false,
            gate_sensitivity: 0.25,
            deesser: false,
            click_suppression: true,
            warmth: 0.4,
            desk_mic: false,
            preset: Preset::Custom,
        };
        assert_eq!(parse(&format(&settings)), settings);
        let mut warm = MicSettings::default();
        Preset::Broadcast.apply(&mut warm);
        assert_eq!(parse(&format(&warm)).preset, Preset::Broadcast);
        let parsed = parse("enabled = maybe\nsuppression = abc\ngain_db = 999\nnew_key = 1\n");
        assert!(parsed.enabled);
        assert_eq!(parsed.suppression, MicSettings::default().suppression);
        assert_eq!(parsed.gain_db, 24.0);
    }

    #[test]
    fn status_round_trips() {
        let text = "process_id = 42\nprocessed_frames = 480000\nrate = 48000\nchannels = 2\nupdated_unix = 1794000000\n";
        let status = parse_status(text).unwrap();
        assert_eq!(status.process_id, 42);
        assert_eq!(status.processed_frames, 480_000);
        assert_eq!(status.rate, 48_000);
        assert_eq!(status.channels, 2);
        assert!(parse_status("garbage").is_none());
    }

    #[test]
    fn read_failure_keeps_last_snapshot_and_retries() {
        let path = std::env::current_exe().unwrap();
        let actual_modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let previous_modified = actual_modified - Duration::from_secs(1);
        let mut last = Some(previous_modified);
        let failed = load_if_changed(&path, &mut last, |_| {
            Err(std::io::ErrorKind::PermissionDenied.into())
        });
        assert_eq!(failed, None);
        assert_eq!(last, Some(previous_modified));
        let loaded = load_if_changed(&path, &mut last, |_| Ok("gain_db = 3".to_string()));
        assert_eq!(loaded.unwrap().gain_db, 3.0);
        assert_eq!(last, Some(actual_modified));
    }
}
