//! Microphone health watch.
//!
//! A background thread listens for audio device changes and, about once a
//! second, probes the microphone Clean Mic is installed on: is it plugged
//! in, is Hush still attached to it, is an app capturing from it. The pure
//! decision logic (`Monitor`) turns those facts into a health state, an
//! optional repair action and an optional notification.
//!
//! Repairs run fixed commands only: the on-demand scheduled tasks created by
//! `hush-setup`, started with `schtasks /run`. Nothing is repaired unless
//! Hush decides it is needed, and never more than a few times per half hour.

use crate::{endpoints, install_state};
use hush_apo::settings;
use std::{
    collections::{hash_map::DefaultHasher, HashMap, VecDeque},
    hash::{Hash, Hasher},
    sync::mpsc,
    time::{Duration, Instant},
};
use windows::{
    core::{implement, Interface, HSTRING, PCWSTR},
    Win32::{
        Foundation::PROPERTYKEY,
        Media::Audio::{
            eCapture, AudioSessionStateActive, EDataFlow, ERole, IAudioSessionControl2,
            IAudioSessionManager2, IMMDeviceEnumerator, IMMNotificationClient,
            IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
        },
    },
};
pub const REPAIR_TASK: &str = r"\Hush\Repair Clean Mic";
pub const RESTART_TASK: &str = r"\Hush\Restart Clean Mic";

/// The live meter file is considered stale after this long without a write.
const STALE_MS: u64 = 4_000;
/// Time after a successful restart in which processing must resume.
const RESTART_VERIFY: Duration = Duration::from_secs(15);
const MIN_REPAIR_GAP: Duration = Duration::from_secs(120);
const ATTEMPT_WINDOW: Duration = Duration::from_secs(30 * 60);
const MAX_ATTEMPTS: usize = 3;
/// A mic that comes back within this time is not worth a notification.
const DISCONNECT_GRACE: Duration = Duration::from_secs(3);
const NOTICE_DEBOUNCE: Duration = Duration::from_secs(60);
const WRONG_MIC_DEBOUNCE: Duration = Duration::from_secs(3);

// -- Decision logic (pure) -------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Probe {
    /// The recorded endpoint of this microphone is active (plugged in).
    pub endpoint_active: bool,
    /// Some active capture endpoint has the microphone's name.
    pub same_name_active: bool,
    /// Hush's FX values are present on an active endpoint with that name.
    pub attached_to_active: bool,
    /// "Audio enhancements" are switched off for the endpoint Hush is on, so
    /// Windows skips every plugin there.
    pub enhancements_off: bool,
    pub effect_conflict: bool,
    /// An app is actively capturing from that endpoint.
    pub capturing_here: bool,
    /// Identifies the set of capturing sessions (to notice changes).
    pub capture_key: u64,
    /// (app, microphone) actively capturing from a different microphone.
    pub capture_other: Option<(String, String)>,
    /// Apps capturing from this microphone, by display name.
    pub apps: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub mic: String,
    /// Clean Mic is installed on some microphone.
    pub installed: bool,
    pub clean_mic_on: bool,
    pub probe: Probe,
    /// Milliseconds since live.bin was written; `None` if it never was.
    pub live_age_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub auto_repair: bool,
    /// "Hold to hear original" is pressed.
    pub hold_pressed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Health {
    #[default]
    Healthy,
    Disconnected,
    /// Reconnected under a new id, or its FX values are gone.
    Missing,
    NotProcessing,
    /// Processing is still missing after a restart.
    Bypassed,
    /// Automatic repair used up its attempts.
    Failing,
    /// "Audio enhancements" are off in Windows, which skips Clean Mic.
    EnhancementsOff,
    EffectConflict,
    Unavailable,
    /// Informational: an app uses another microphone.
    WrongMic {
        app: String,
        other: String,
    },
}

impl Health {
    pub fn needs_attention(&self) -> bool {
        !matches!(self, Health::Healthy | Health::WrongMic { .. })
    }

    /// Stable id for the agent state file (`WrongMic` is stored apart).
    pub fn id(&self) -> &'static str {
        match self {
            Health::Healthy => "healthy",
            Health::Disconnected => "disconnected",
            Health::Missing => "missing",
            Health::NotProcessing => "not_processing",
            Health::Bypassed => "bypassed",
            Health::Failing => "failing",
            Health::EnhancementsOff => "enhancements_off",
            Health::EffectConflict => "effect_conflict",
            Health::Unavailable => "unavailable",
            Health::WrongMic { .. } => "wrong_mic",
        }
    }

    pub fn from_id(id: &str) -> Option<Health> {
        Some(match id {
            "healthy" => Health::Healthy,
            "disconnected" => Health::Disconnected,
            "missing" => Health::Missing,
            "not_processing" => Health::NotProcessing,
            "bypassed" => Health::Bypassed,
            "failing" => Health::Failing,
            "enhancements_off" => Health::EnhancementsOff,
            "effect_conflict" => Health::EffectConflict,
            "unavailable" => Health::Unavailable,
            _ => return None,
        })
    }

    /// Short label for badges and the tray tooltip.
    pub fn short(&self) -> &'static str {
        match self {
            Health::Healthy => "Healthy",
            Health::Disconnected => "Microphone disconnected",
            Health::Missing | Health::NotProcessing | Health::Bypassed | Health::Failing => {
                "Clean Mic not processing"
            }
            Health::EnhancementsOff => "Enhancements off",
            Health::EffectConflict => "Effect slot conflict",
            Health::Unavailable => "Microphone status unavailable",
            Health::WrongMic { .. } => "Other microphone in use",
        }
    }

    pub fn message(&self, mic: &str) -> String {
        match self {
            Health::Healthy => String::new(),
            Health::Disconnected => {
                format!("{mic} is disconnected. Clean Mic will resume when it's back.")
            }
            Health::Missing => {
                format!("Clean Mic is missing from {mic} since it reconnected.")
            }
            Health::NotProcessing => {
                format!("An app is recording from {mic}, but Clean Mic is not processing it.")
            }
            Health::Bypassed => format!(
                "An app is recording from {mic} without Clean Mic. It may be asking Windows for raw, unprocessed audio."
            ),
            Health::Failing => "Automatic repair keeps failing.".into(),
            Health::EnhancementsOff => {
                format!("Audio enhancements are off for {mic}, so Windows skips Clean Mic.")
            }
            Health::EffectConflict => format!("The effect settings for {mic} conflict with Clean Mic. Select another microphone or review its effects in Windows."),
            Health::Unavailable => "Hush is reconnecting to Windows Audio.".into(),
            Health::WrongMic { app, other } => format!("{app} is using {other}, not {mic}."),
        }
    }

    /// The repair a "Fix now" button runs, if any.
    pub fn fix(&self) -> Option<ActionKind> {
        match self {
            Health::Missing => Some(ActionKind::Repair),
            Health::NotProcessing | Health::Bypassed | Health::Failing => Some(ActionKind::Restart),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Re-apply Clean Mic to the microphone (restarts audio if needed).
    Repair,
    /// Re-apply and restart Windows audio.
    Restart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Disconnected(String),
    Restored(String),
    Bypassed(String),
    Failing,
    EnhancementsOff(String),
}

impl Notice {
    /// Toast title and one line of text.
    pub fn text(&self) -> (String, String) {
        match self {
            Notice::Disconnected(mic) => (
                "Microphone disconnected".into(),
                Health::Disconnected.message(mic),
            ),
            Notice::Restored(mic) => (
                "Clean Mic restored".into(),
                format!("Clean Mic restored on {mic}."),
            ),
            Notice::Bypassed(mic) => (
                "Clean Mic is bypassed".into(),
                Health::Bypassed.message(mic),
            ),
            Notice::Failing => (
                "Automatic repair keeps failing".into(),
                "Open Hush and choose Fix now.".into(),
            ),
            Notice::EnhancementsOff(mic) => (
                "Audio enhancements are off".into(),
                Health::EnhancementsOff.message(mic),
            ),
        }
    }
}

#[derive(Debug)]
pub struct Step {
    pub health: Health,
    pub action: Option<ActionKind>,
    pub notice: Option<Notice>,
}

#[derive(Default)]
pub struct Monitor {
    disconnected_since: Option<Instant>,
    disconnected_notified: bool,
    attempts: VecDeque<Instant>,
    last_attempt: Option<Instant>,
    busy: bool,
    restart_done_at: Option<Instant>,
    stale_capture: Option<(u64, Instant, Instant)>,
    bypass_key: Option<u64>,
    bypass_notified: bool,
    failing_notified: bool,
    enhancements_notified: bool,
    restored_pending: Option<Instant>,
    wrong_since: Option<Instant>,
    last_notice: Vec<(&'static str, Instant)>,
}

impl Monitor {
    pub fn reset_capture(&mut self) {
        self.stale_capture = None;
        self.restart_done_at = None;
        self.bypass_key = None;
        self.bypass_notified = false;
        self.disconnected_since = None;
        self.disconnected_notified = false;
        self.wrong_since = None;
    }

    /// A repair started by the user (Fix now) is running.
    pub fn manual_started(&mut self) {
        self.busy = true;
        self.bypass_key = None;
        self.bypass_notified = false;
    }

    /// A repair (automatic or manual) finished.
    pub fn action_finished(&mut self, kind: ActionKind, success: bool, now: Instant) {
        self.busy = false;
        if success {
            self.restored_pending = Some(now);
            if kind == ActionKind::Restart {
                self.restart_done_at = Some(now);
            }
        }
    }

    fn emit(&mut self, tag: &'static str, notice: Notice, now: Instant) -> Option<Notice> {
        if let Some((_, at)) = self.last_notice.iter().find(|(t, _)| *t == tag) {
            if now.saturating_duration_since(*at) < NOTICE_DEBOUNCE {
                return None;
            }
        }
        self.last_notice.retain(|(t, _)| *t != tag);
        self.last_notice.push((tag, now));
        Some(notice)
    }

    /// Whether an automatic action may start now. `Err(true)` means the
    /// attempt budget is used up.
    fn may_act(&mut self, cfg: Config, now: Instant) -> Result<bool, bool> {
        if !cfg.auto_repair || self.busy || cfg.hold_pressed {
            return Ok(false);
        }
        while self
            .attempts
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > ATTEMPT_WINDOW)
        {
            self.attempts.pop_front();
        }
        if self.attempts.len() >= MAX_ATTEMPTS {
            return Err(true);
        }
        if self
            .last_attempt
            .is_some_and(|at| now.saturating_duration_since(at) < MIN_REPAIR_GAP)
        {
            return Ok(false);
        }
        self.attempts.push_back(now);
        self.last_attempt = Some(now);
        self.busy = true;
        Ok(true)
    }

    /// Applies the rate limits to a wanted action and fills `step`.
    fn act(&mut self, kind: ActionKind, step: &mut Step, cfg: Config, now: Instant) {
        match self.may_act(cfg, now) {
            Ok(true) => step.action = Some(kind),
            Ok(false) => {}
            Err(_) => {
                step.health = Health::Failing;
                if !self.failing_notified {
                    self.failing_notified = true;
                    step.notice = self.emit("failing", Notice::Failing, now);
                }
            }
        }
    }

    pub fn step(&mut self, snap: &Snapshot, cfg: Config, now: Instant) -> Step {
        let mut step = Step {
            health: Health::Healthy,
            action: None,
            notice: None,
        };
        let probe = &snap.probe;
        let stale = snap.live_age_ms.is_none_or(|age| age > STALE_MS);
        if !snap.installed
            || !probe.attached_to_active
            || !probe.capturing_here
            || !stale
            || !snap.clean_mic_on
            || cfg.hold_pressed
            || probe.enhancements_off
        {
            self.stale_capture = None;
        }
        if !snap.installed || snap.mic.is_empty() {
            self.disconnected_since = None;
            self.wrong_since = None;
            return step;
        }

        // 1. Unplugged.
        if !probe.endpoint_active && !probe.same_name_active {
            let since = *self.disconnected_since.get_or_insert(now);
            step.health = Health::Disconnected;
            if !self.disconnected_notified
                && now.saturating_duration_since(since) >= DISCONNECT_GRACE
            {
                self.disconnected_notified = true;
                step.notice =
                    self.emit("disconnected", Notice::Disconnected(snap.mic.clone()), now);
            }
            return step;
        }
        self.disconnected_since = None;
        self.disconnected_notified = false;

        if probe.effect_conflict {
            step.health = Health::EffectConflict;
            return step;
        }

        // 2. Back, but Hush is not attached to it.
        if !probe.attached_to_active {
            step.health = Health::Missing;
            self.act(ActionKind::Repair, &mut step, cfg, now);
            return step;
        }

        // 3. Windows skips every plugin while "Audio enhancements" are off.
        // Nothing to repair: the user has to switch them on.
        if probe.enhancements_off && snap.clean_mic_on {
            step.health = Health::EnhancementsOff;
            self.bypass_key = None;
            self.bypass_notified = false;
            self.restart_done_at = None;
            if !self.enhancements_notified {
                self.enhancements_notified = true;
                step.notice = self.emit(
                    "enhancements",
                    Notice::EnhancementsOff(snap.mic.clone()),
                    now,
                );
            }
            return step;
        }
        self.enhancements_notified = false;

        // 4. Attached, an app records, but nothing is processed.
        if self.bypass_key.is_some_and(|key| key != probe.capture_key) {
            self.bypass_key = None;
            self.bypass_notified = false;
            self.restart_done_at = None;
        }
        if self
            .restart_done_at
            .is_some_and(|at| now.saturating_duration_since(at) > Duration::from_secs(60))
        {
            self.restart_done_at = None;
        }
        if probe.capturing_here && stale && snap.clean_mic_on && !cfg.hold_pressed {
            if self.bypass_key == Some(probe.capture_key) {
                step.health = Health::Bypassed;
                return step;
            }
            if let Some(done) = self.restart_done_at {
                if now.saturating_duration_since(done) >= RESTART_VERIFY {
                    self.bypass_key = Some(probe.capture_key);
                    self.restart_done_at = None;
                    step.health = Health::Bypassed;
                    if !self.bypass_notified {
                        self.bypass_notified = true;
                        step.notice =
                            self.emit("bypassed", Notice::Bypassed(snap.mic.clone()), now);
                    }
                } else {
                    step.health = Health::NotProcessing;
                }
                return step;
            }
            step.health = Health::NotProcessing;
            let (key, since, last) =
                self.stale_capture
                    .get_or_insert((probe.capture_key, now, now));
            if *key != probe.capture_key
                || now.saturating_duration_since(*last) > Duration::from_secs(5)
            {
                (*key, *since) = (probe.capture_key, now);
            }
            *last = now;
            if now.saturating_duration_since(*since) >= Duration::from_millis(STALE_MS) {
                self.act(ActionKind::Restart, &mut step, cfg, now);
            }
            return step;
        }

        // Healthy (or nobody is recording).
        self.bypass_key = None;
        self.bypass_notified = false;
        self.failing_notified = false;
        if probe.capturing_here && !stale {
            self.restart_done_at = None;
        }
        if let Some(at) = self.restored_pending {
            if now.saturating_duration_since(at) > Duration::from_secs(300) {
                self.restored_pending = None;
            } else if !self.busy {
                self.restored_pending = None;
                step.notice = self.emit("restored", Notice::Restored(snap.mic.clone()), now);
            }
        }

        // 5. Hint: an app records from a different microphone.
        match (&probe.capture_other, probe.capturing_here) {
            (Some((app, other)), false) => {
                let since = *self.wrong_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= WRONG_MIC_DEBOUNCE {
                    step.health = Health::WrongMic {
                        app: app.clone(),
                        other: other.clone(),
                    };
                }
            }
            _ => self.wrong_since = None,
        }
        step
    }
}

// -- System probes -----------------------------------------------------------------

/// What the probe keeps between runs, so a steady state costs little:
/// the registry scan of capture endpoints (redone on device events and
/// every few seconds), each endpoint's session manager, and process names.
#[derive(Default)]
struct ProbeCache {
    endpoints: Vec<endpoints::Endpoint>,
    scanned: Option<Instant>,
    managers: HashMap<String, IAudioSessionManager2>,
    names: HashMap<u32, Option<String>>,
}

/// Safety-net rescan of the endpoint registry when no event arrived.
const RESCAN: Duration = Duration::from_secs(5);

impl ProbeCache {
    /// Forget everything about devices (a device was added, removed or changed).
    fn invalidate(&mut self) {
        self.scanned = None;
        self.managers.clear();
    }

    fn refresh(&mut self) {
        if self.scanned.is_some_and(|at| at.elapsed() < RESCAN) {
            return;
        }
        self.endpoints = endpoints::capture_endpoints();
        self.scanned = Some(Instant::now());
        let active: Vec<&str> = self
            .endpoints
            .iter()
            .filter(|e| e.active)
            .map(|e| e.id.as_str())
            .collect();
        self.managers.retain(|id, _| active.contains(&id.as_str()));
    }

    /// Display name of a process (`None` for Hush itself), looked up once.
    fn label(&mut self, pid: u32) -> Option<String> {
        if self.names.len() > 64 {
            self.names.clear();
        }
        self.names
            .entry(pid)
            .or_insert_with(|| crate::app_names::label(pid))
            .clone()
    }

    /// Active capture sessions on one endpoint: process ids.
    fn active_sessions(&mut self, enumerator: &IMMDeviceEnumerator, id: &str) -> Option<Vec<u32>> {
        let mut pids = Vec::new();
        if !self.managers.contains_key(id) {
            let full = HSTRING::from(endpoints::full_device_id(id));
            let manager = unsafe {
                enumerator
                    .GetDevice(PCWSTR(full.as_ptr()))
                    .and_then(|device| device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None))
            };
            match manager {
                Ok(manager) => {
                    self.managers.insert(id.to_string(), manager);
                }
                Err(_) => return None,
            }
        }
        let manager = self.managers.get(id)?;
        unsafe {
            let Ok(sessions) = manager.GetSessionEnumerator() else {
                self.managers.remove(id);
                return None;
            };
            for index in 0..sessions.GetCount().ok()? {
                let Ok(control) = sessions.GetSession(index) else {
                    continue;
                };
                if control.GetState().ok() != Some(AudioSessionStateActive) {
                    continue;
                }
                if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                    pids.push(control2.GetProcessId().unwrap_or(0));
                }
            }
        }
        Some(pids)
    }
}

fn probe(enumerator: &IMMDeviceEnumerator, target: &str, cache: &mut ProbeCache) -> Option<Probe> {
    let mut probe = Probe::default();
    cache.refresh();
    let all = cache.endpoints.clone();
    let selected = all.iter().find(|e| e.id.eq_ignore_ascii_case(target));
    let active: Vec<&endpoints::Endpoint> = all.iter().filter(|e| e.active).collect();
    probe.endpoint_active = selected.is_some_and(|e| e.active);
    probe.same_name_active = selected.is_some_and(|selected| {
        active.iter().any(|e| {
            endpoints::names_match(&e.name, &selected.name)
                && selected.container.is_some()
                && e.container == selected.container
        })
    });
    probe.attached_to_active = selected.is_some_and(|e| e.active && e.has_hush);
    probe.effect_conflict = selected.is_some_and(|e| e.active && e.effect_conflict);
    probe.enhancements_off = selected.is_some_and(|e| e.active && e.has_hush && e.enhancements_off);

    let own = std::process::id();
    let mut keys: Vec<u32> = Vec::new();
    for endpoint in &active {
        let (id, name) = (&endpoint.id, &endpoint.name);
        let here = id.eq_ignore_ascii_case(target);
        let pids = match cache.active_sessions(enumerator, id) {
            Some(pids) => pids,
            None if here => return None,
            None => continue,
        };
        if here {
            if !pids.is_empty() {
                probe.capturing_here = true;
            }
            for pid in &pids {
                if *pid == own {
                    continue;
                }
                if let Some(label) = cache.label(*pid) {
                    if !probe.apps.contains(&label) {
                        probe.apps.push(label);
                    }
                }
            }
            keys.extend(pids);
        } else if probe.capture_other.is_none() {
            let other = pids
                .into_iter()
                .filter(|pid| *pid != own)
                .find_map(|pid| cache.label(pid));
            if let Some(app) = other {
                probe.capture_other = Some((app, name.clone()));
            }
        }
    }
    probe.apps.sort();
    keys.sort_unstable();
    let mut hasher = DefaultHasher::new();
    keys.hash(&mut hasher);
    probe.capture_key = hasher.finish();
    Some(probe)
}

// -- Background watcher --------------------------------------------------------------

enum Msg {
    Target(String),
    DeviceEvent,
    Stop,
}

#[implement(IMMNotificationClient)]
struct DeviceEvents {
    tx: mpsc::Sender<Msg>,
}

impl DeviceEvents {
    fn notify(&self, id: &PCWSTR) {
        // Capture endpoint ids start with "{0.0.1".
        let capture = unsafe { id.to_string() }
            .map(|id| id.starts_with("{0.0.1"))
            .unwrap_or(true);
        if capture {
            let _ = self.tx.send(Msg::DeviceEvent);
        }
    }
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for DeviceEvents_Impl {
    fn OnDeviceStateChanged(
        &self,
        pwstrdeviceid: &PCWSTR,
        _dwnewstate: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.notify(pwstrdeviceid);
        Ok(())
    }

    fn OnDeviceAdded(&self, pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        self.notify(pwstrdeviceid);
        Ok(())
    }

    fn OnDeviceRemoved(&self, pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        self.notify(pwstrdeviceid);
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        _pwstrdefaultdeviceid: &PCWSTR,
    ) -> windows::core::Result<()> {
        if flow == eCapture {
            let _ = self.tx.send(Msg::DeviceEvent);
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

pub struct Update {
    pub target: String,
    pub probe: Option<Probe>,
    /// A capture device was added, removed or changed since the last update.
    pub devices_changed: bool,
}

pub struct Watcher {
    commands: mpsc::Sender<Msg>,
    updates: mpsc::Receiver<Update>,
}

impl Watcher {
    /// Starts the watcher. `wake` is called when something changed.
    pub fn new(mic: &str, wake: impl Fn() + Send + 'static) -> Self {
        let (commands, inbox) = mpsc::channel();
        let (outbox, updates) = mpsc::channel();
        let events = commands.clone();
        let mic = mic.to_string();
        std::thread::Builder::new()
            .name("hush-health".into())
            .spawn(move || worker(inbox, events, outbox, mic, wake))
            .ok();
        Self { commands, updates }
    }

    pub fn set_target(&self, mic: &str) {
        let _ = self.commands.send(Msg::Target(mic.to_string()));
    }

    /// Latest results since the last call.
    pub fn poll(&self) -> Option<Update> {
        let mut latest: Option<Update> = None;
        while let Ok(update) = self.updates.try_recv() {
            let changed =
                update.devices_changed || latest.as_ref().is_some_and(|l| l.devices_changed);
            latest = Some(Update {
                devices_changed: changed,
                ..update
            });
        }
        latest
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.commands.send(Msg::Stop);
    }
}

fn worker(
    inbox: mpsc::Receiver<Msg>,
    events: mpsc::Sender<Msg>,
    outbox: mpsc::Sender<Update>,
    mut mic: String,
    wake: impl Fn() + Send + 'static,
) {
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let client: IMMNotificationClient = DeviceEvents { tx: events }.into();
    let mut enumerator: Option<IMMDeviceEnumerator> = None;
    let mut last = None;
    let mut changed = false;
    let mut cache = ProbeCache::default();
    let mut retry_at = Instant::now();
    'watch: loop {
        match inbox.recv_timeout(Duration::from_secs(1)) {
            Ok(Msg::Target(name)) => {
                mic = name;
                changed = true;
            }
            Ok(Msg::DeviceEvent) => changed = true,
            Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break 'watch,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        while let Ok(message) = inbox.try_recv() {
            match message {
                Msg::Target(name) => {
                    mic = name;
                    changed = true;
                }
                Msg::DeviceEvent => changed = true,
                Msg::Stop => break 'watch,
            }
        }
        if changed {
            cache.invalidate();
        }
        if enumerator.is_none() && Instant::now() >= retry_at {
            enumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ok();
            if let Some(device) = &enumerator {
                unsafe {
                    let _ = device.RegisterEndpointNotificationCallback(&client);
                }
                cache.invalidate();
            } else {
                retry_at = Instant::now() + RESCAN;
            }
        }
        let current = enumerator
            .as_ref()
            .and_then(|device| probe(device, &mic, &mut cache));
        if current.is_none() {
            cache.invalidate();
            if let Some(device) = enumerator.take() {
                unsafe {
                    let _ = device.UnregisterEndpointNotificationCallback(&client);
                }
                retry_at = Instant::now() + RESCAN;
            }
        }
        if last.as_ref() != Some(&current) || changed {
            let _ = outbox.send(Update {
                target: mic.clone(),
                probe: current.clone(),
                devices_changed: changed,
            });
            wake();
        }
        last = Some(current);
        changed = false;
    }
    if let Some(device) = enumerator {
        unsafe {
            let _ = device.UnregisterEndpointNotificationCallback(&client);
        }
    }
    drop(cache);
    drop(client);
    if com {
        unsafe { CoUninitialize() };
    }
}

// -- Repairs -------------------------------------------------------------------------

pub use hush::repair_report::RepairReport as RepairFile;

pub fn read_repair_file() -> Option<RepairFile> {
    RepairFile::read()
}

/// Runs a fixed command without a console window.
fn run_hidden(program: &str, args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    hush::windows_setup_paths::system_tool(program)
        .and_then(|mut command| command.args(args).creation_flags(CREATE_NO_WINDOW).output())
        .is_ok_and(|output| output.status.success())
}

/// Whether the two on-demand repair tasks exist (queried on request only).
pub fn repair_tasks_exist() -> (bool, bool) {
    (
        run_hidden("schtasks.exe", &["/query", "/tn", REPAIR_TASK]),
        run_hidden("schtasks.exe", &["/query", "/tn", RESTART_TASK]),
    )
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub success: bool,
    pub message: String,
}

/// Starts a repair on a background thread. The scheduled task (no UAC) is
/// tried first; `allow_uac` permits one fallback to the elevated setup tool.
pub fn run_repair(kind: ActionKind, allow_uac: bool) -> mpsc::Receiver<Outcome> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("hush-repair".into())
        .spawn(move || {
            let outcome = repair_blocking(kind, allow_uac);
            settings::trace(&format!(
                "health: {:?} -> {} ({})",
                kind,
                if outcome.success { "ok" } else { "failed" },
                outcome.message
            ));
            let _ = sender.send(outcome);
        })
        .ok();
    receiver
}

fn repair_blocking(kind: ActionKind, allow_uac: bool) -> Outcome {
    let task = match kind {
        ActionKind::Repair => REPAIR_TASK,
        ActionKind::Restart => RESTART_TASK,
    };
    let generation = read_repair_file().map_or(0, |report| report.generation);
    let operation = if kind == ActionKind::Restart {
        "restart"
    } else {
        "repair"
    };
    let fail = |message: &str| Outcome {
        success: false,
        message: message.to_string(),
    };
    if run_hidden("schtasks.exe", &["/run", "/tn", task]) {
        let deadline = Instant::now() + Duration::from_secs(125);
        while Instant::now() < deadline {
            if let Some(file) =
                read_repair_file().filter(|report| report.matches(generation, operation))
            {
                return Outcome {
                    success: file.succeeded(),
                    message: file.message,
                };
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        return fail("The repair did not finish in time.");
    }
    if !allow_uac {
        return fail("The repair task is not available.");
    }
    let args = match kind {
        ActionKind::Repair => vec!["repair".to_string()],
        ActionKind::Restart => vec!["repair".to_string(), "--restart-audio".to_string()],
    };
    let message = install_state::run_setup(args)
        .recv()
        .unwrap_or_else(|_| "Not completed.".into());
    if let Some(report) = read_repair_file().filter(|report| report.matches(generation, operation))
    {
        Outcome {
            success: report.succeeded(),
            message: report.message,
        }
    } else {
        fail(if message == "Done." {
            "Setup returned without a repair result."
        } else {
            &message
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: Config = Config {
        auto_repair: true,
        hold_pressed: false,
    };

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn healthy() -> Snapshot {
        Snapshot {
            mic: "Mic (USB)".into(),
            installed: true,
            clean_mic_on: true,
            probe: Probe {
                endpoint_active: true,
                same_name_active: true,
                attached_to_active: true,
                enhancements_off: false,
                effect_conflict: false,
                capturing_here: true,
                capture_key: 1,
                capture_other: None,
                apps: Vec::new(),
            },
            live_age_ms: Some(50),
        }
    }

    fn unplugged() -> Snapshot {
        let mut snap = healthy();
        snap.probe = Probe::default();
        snap
    }

    fn stale() -> Snapshot {
        let mut snap = healthy();
        snap.live_age_ms = Some(10_000);
        snap
    }

    #[test]
    fn quick_replug_gives_no_notification() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        let step = monitor.step(&unplugged(), CFG, t0);
        assert_eq!(step.health, Health::Disconnected);
        assert_eq!(step.notice, None);
        let step = monitor.step(&unplugged(), CFG, t0 + secs(2));
        assert_eq!(step.notice, None);
        let step = monitor.step(&healthy(), CFG, t0 + secs(2));
        assert_eq!(step.health, Health::Healthy);
        assert_eq!(step.notice, None);
        assert_eq!(step.action, None);
    }

    #[test]
    fn long_disconnect_notifies_once() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        monitor.step(&unplugged(), CFG, t0);
        let step = monitor.step(&unplugged(), CFG, t0 + secs(4));
        assert_eq!(step.notice, Some(Notice::Disconnected("Mic (USB)".into())));
        let step = monitor.step(&unplugged(), CFG, t0 + secs(70));
        assert_eq!(step.notice, None);
        assert_eq!(step.action, None);
    }

    #[test]
    fn new_id_with_same_name_triggers_repair() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        let mut snap = healthy();
        snap.probe.endpoint_active = false;
        snap.probe.attached_to_active = false;
        let step = monitor.step(&snap, CFG, t0);
        assert_eq!(step.health, Health::Missing);
        assert_eq!(step.action, Some(ActionKind::Repair));
        // While it runs nothing else starts.
        let step = monitor.step(&snap, CFG, t0 + secs(1));
        assert_eq!(step.action, None);
        // After a successful repair the restored notice follows.
        monitor.action_finished(ActionKind::Repair, true, t0 + secs(5));
        let step = monitor.step(&healthy(), CFG, t0 + secs(6));
        assert_eq!(step.notice, Some(Notice::Restored("Mic (USB)".into())));
    }

    #[test]
    fn stale_processing_restarts_then_bypasses() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        let step = monitor.step(&stale(), CFG, t0);
        assert_eq!(step.health, Health::NotProcessing);
        assert_eq!(step.action, None);
        assert_eq!(
            monitor.step(&stale(), CFG, t0 + secs(4)).action,
            Some(ActionKind::Restart)
        );
        monitor.action_finished(ActionKind::Restart, true, t0 + secs(5));
        // Still within the verification time: just waiting.
        let step = monitor.step(&stale(), CFG, t0 + secs(10));
        assert_eq!(step.health, Health::NotProcessing);
        assert_eq!(step.action, None);
        // 15 s after the restart: bypassed, with one notification.
        let step = monitor.step(&stale(), CFG, t0 + secs(21));
        assert_eq!(step.health, Health::Bypassed);
        assert_eq!(step.notice, Some(Notice::Bypassed("Mic (USB)".into())));
        // No retries while the capture sessions stay the same.
        let step = monitor.step(&stale(), CFG, t0 + secs(400));
        assert_eq!(step.health, Health::Bypassed);
        assert_eq!(step.action, None);
        assert_eq!(step.notice, None);
        // A different set of capturing apps allows another try.
        let mut changed = stale();
        changed.probe.capture_key = 2;
        let step = monitor.step(&changed, CFG, t0 + secs(401));
        assert_eq!(step.action, None);
        assert_eq!(
            monitor.step(&changed, CFG, t0 + secs(405)).action,
            Some(ActionKind::Restart)
        );
    }

    #[test]
    fn stale_capture_before_suspend_does_not_count_as_continuous() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        assert_eq!(monitor.step(&stale(), CFG, t0).action, None);
        assert_eq!(monitor.step(&stale(), CFG, t0 + secs(60)).action, None);
        assert_eq!(
            monitor.step(&stale(), CFG, t0 + secs(64)).action,
            Some(ActionKind::Restart)
        );
    }

    #[test]
    fn enhancements_off_is_not_a_restart_or_bypass() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        let mut snap = stale();
        snap.probe.enhancements_off = true;
        let step = monitor.step(&snap, CFG, t0);
        assert_eq!(step.health, Health::EnhancementsOff);
        assert_eq!(step.action, None);
        assert_eq!(
            step.notice,
            Some(Notice::EnhancementsOff("Mic (USB)".into()))
        );
        assert!(step.health.needs_attention());
        // Still no action, no second toast, and never "bypassed".
        for later in [20, 70, 400] {
            let step = monitor.step(&snap, CFG, t0 + secs(later));
            assert_eq!(step.health, Health::EnhancementsOff);
            assert_eq!((step.action, step.notice), (None, None));
        }
        // Turning enhancements back on resumes normal handling.
        snap.probe.enhancements_off = false;
        let step = monitor.step(&snap, CFG, t0 + secs(401));
        assert_eq!(step.health, Health::NotProcessing);
    }

    #[test]
    fn processing_resuming_clears_the_state() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        monitor.step(&stale(), CFG, t0);
        monitor.action_finished(ActionKind::Restart, true, t0 + secs(3));
        let step = monitor.step(&healthy(), CFG, t0 + secs(8));
        assert_eq!(step.health, Health::Healthy);
        assert_eq!(step.notice, Some(Notice::Restored("Mic (USB)".into())));
    }

    #[test]
    fn rate_limits() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        let mut missing = healthy();
        missing.probe.attached_to_active = false;
        assert_eq!(
            monitor.step(&missing, CFG, t0).action,
            Some(ActionKind::Repair)
        );
        monitor.action_finished(ActionKind::Repair, false, t0 + secs(5));
        // At most one attempt per 2 minutes.
        assert_eq!(monitor.step(&missing, CFG, t0 + secs(60)).action, None);
        assert_eq!(
            monitor.step(&missing, CFG, t0 + secs(125)).action,
            Some(ActionKind::Repair)
        );
        monitor.action_finished(ActionKind::Repair, false, t0 + secs(130));
        assert_eq!(
            monitor.step(&missing, CFG, t0 + secs(250)).action,
            Some(ActionKind::Repair)
        );
        monitor.action_finished(ActionKind::Repair, false, t0 + secs(255));
        // Three attempts used: the state turns into "keeps failing".
        let step = monitor.step(&missing, CFG, t0 + secs(400));
        assert_eq!(step.health, Health::Failing);
        assert_eq!(step.action, None);
        assert_eq!(step.notice, Some(Notice::Failing));
        // Not notified again, and no more attempts within 30 minutes.
        let step = monitor.step(&missing, CFG, t0 + secs(900));
        assert_eq!(step.health, Health::Failing);
        assert_eq!((step.action, step.notice), (None, None));
        // The window moves on after 30 minutes.
        let step = monitor.step(&missing, CFG, t0 + secs(2200));
        assert_eq!(step.action, Some(ActionKind::Repair));
    }

    #[test]
    fn no_restart_while_holding_original() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        let held = Config {
            hold_pressed: true,
            ..CFG
        };
        assert_eq!(monitor.step(&stale(), held, t0).action, None);
        assert_eq!(monitor.step(&stale(), CFG, t0 + secs(1)).action, None);
        assert_eq!(
            monitor.step(&stale(), CFG, t0 + secs(5)).action,
            Some(ActionKind::Restart)
        );
    }

    #[test]
    fn auto_repair_off_shows_state_but_does_nothing() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        let off = Config {
            auto_repair: false,
            hold_pressed: false,
        };
        let mut missing = healthy();
        missing.probe.attached_to_active = false;
        for (snap, health) in [(missing, Health::Missing), (stale(), Health::NotProcessing)] {
            let step = monitor.step(&snap, off, t0);
            assert_eq!(step.health, health);
            assert_eq!(step.action, None);
        }
    }

    #[test]
    fn wrong_mic_hint_is_debounced_and_quiet() {
        let mut monitor = Monitor::default();
        let t0 = Instant::now();
        let mut snap = healthy();
        snap.probe.capturing_here = false;
        snap.probe.capture_other = Some(("Discord".into(), "Headset".into()));
        assert_eq!(monitor.step(&snap, CFG, t0).health, Health::Healthy);
        let step = monitor.step(&snap, CFG, t0 + secs(4));
        assert!(matches!(step.health, Health::WrongMic { .. }));
        assert_eq!(step.notice, None);
        assert!(!step.health.needs_attention());
    }

    #[test]
    fn repair_file_is_parsed() {
        let file = RepairFile::parse(
            "time_unix = 1700000000\nresult = repaired\nmessage = Done it.\nendpoint = abc\n",
        )
        .unwrap();
        assert_eq!(file.time_unix, 1_700_000_000);
        assert_eq!(file.result, "repaired");
        assert_eq!(file.message, "Done it.");
        assert_eq!(RepairFile::parse("nothing useful"), None);
    }
}
