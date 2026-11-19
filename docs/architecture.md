# How Hush works

## Components

| File | Role |
| --- | --- |
| `hush_apo.dll` (`apo/`) | The audio plugin. Windows' audio engine (`audiodg.exe`) loads it on one microphone and runs every captured buffer through it. |
| `hush.exe` (`src/`) | Tray app: settings, live meters, Mic Test. It never processes audio for other apps. |
| `hush-setup.exe` (`src/bin/hush-setup.rs`) | Installs and removes the plugin on a microphone, reports status. |
| `Hush-Setup-<version>.exe` (`installer/`) | Wizard that embeds the three files above. Built by `package.ps1`. |

The app and plugin share `%ProgramData%\Hush`:

- `mic.conf` holds the settings, written by the app and polled by the plugin
  every 250 ms. Unknown keys are ignored and missing keys use defaults, so
  older and newer settings files load safely.
- `status.txt` is a heartbeat the plugin writes once a second while audio
  flows.
- `live.bin` is a memory-mapped file the plugin updates 20 times a second
  with meter levels.
- `installed.txt` records every registry value `hush-setup` wrote.

## App modules (`src/`)

- `ui/`: the window. `mod.rs` holds the app state and the update loop,
  `chrome.rs` the sidebar, header, health banner and the recording toast,
  and `pages/` one file per page (Microphone, Noise & Gate, Voice, Mic Test,
  Settings, About). `theme.rs`: shared widgets and colors (light and dark
  palettes).
- `endpoints.rs`: the one place that maps microphone friendly names to
  capture endpoint ids (registry, read only): plugged in, Hush's FX value
  present, and "Audio enhancements" switched off. `install_state.rs`,
  `health.rs` and `mute.rs` all use it.
- `dev.rs`: developer aids (`HUSH_SHOT`, `HUSH_PAGE`, `HUSH_THEME`, ...).
  They exist only in debug builds; a release build never reads them.
- `tray.rs`: tray icon and menu (Open Hush, Mic Test, Clean Mic, Mute microphone, Quit).
- `mute.rs`: the optional global mute hotkey (`RegisterHotKey` on its own
  thread) and the Windows capture-endpoint mute. Muting never touches the
  plugin or `mic.conf`. One worker thread keeps the endpoint volume of the
  selected microphone and gets mute changes from Windows through an
  `IAudioEndpointVolumeCallback`; a 10 s refresh is only a safety net. It
  looks the device up again when the selection or the device list changes.
  If the selected microphone is not plugged in, toggling reports
  "Microphone not found." instead of muting another device.
- `health.rs`: the microphone health watch. A background thread registers an
  `IMMNotificationClient` for capture device changes and probes the
  microphone about once a second (registry: is the endpoint active, is
  Hush's FX value on it; `IAudioSessionManager2`: is an app capturing, and
  from which microphone). `Monitor` is the pure decision logic: input
  snapshot and time in, health state, repair action and notification out.
  Automatic repairs are limited to one per 2 minutes and 3 per 30 minutes,
  and never run while "Hold to hear original" is pressed.
  Health states: Disconnected, Missing (reconnected without Hush attached),
  Not processing (an app records but `live.bin` is stale for more than 4 s),
  Bypassed (still not processing 15 s after a restart), Failing, Enhancements
  off, and an informational "app uses another microphone" hint.
  Enhancements off means Windows' "Audio enhancements" switch
  (`{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5` = 1, in the endpoint's
  Properties or FxProperties key) is off. Windows then skips every plugin
  on that microphone, so Hush shows a banner with a **Sound settings** button
  and never restarts audio or reports a bypass for it.
- `notify.rs`: Windows toasts for unpackaged apps (AppUserModelId
  `Exel.Hush` registered under HKCU, PNG icon written by a small built-in
  encoder). Toasts use protocol activation: the installed copy (running from
  `%ProgramFiles%\Hush`) registers a per-user `hush:` URL protocol
  (`HKCU\Software\Classes\hush`) that runs `hush.exe --show`, so clicking a
  toast opens the window through the normal second-launch path. Dev builds do
  not register it.
- `level_watch.rs`: raw input level warnings (clipping, too quiet).
- `diagnostics.rs`: builds the text behind **Copy diagnostics**.
- `settings_io.rs`: tuning snapshot, settings export and import.
- `updates.rs`: **Check for updates**. This is the only network call in
  Hush. It happens once per click on that button, never in the background,
  and is hidden when the build has no GitHub repository URL.

## Repair tasks

`hush-setup.exe` creates two on-demand scheduled tasks that run as SYSTEM:
`\Hush\Repair Clean Mic` (`repair --quiet`) and `\Hush\Restart Clean Mic`
(`repair --restart-audio --quiet`). A standard user may start them. The app
runs exactly `schtasks.exe /run /tn <task>` and waits for a newer
`time_unix` in `%ProgramData%\Hush\repair.txt`. The commands are fixed and
only run on demand. If the task is missing, the app falls back once to the
normal elevated `hush-setup.exe repair` (a UAC prompt), and only when the
user clicked **Fix now**. Automatic repairs never show a UAC prompt.

## Signal chain

Processing is mono: the channels are averaged, processed once, and written
back to every channel.

1. **Rumble filter.** 70 Hz high-pass.
2. **Noise removal.** DeepFilterNet 3 at 48 kHz in 10 ms frames, on a worker
   thread at MMCSS "Pro Audio" priority. The real-time thread only moves
   samples through wait-free ring buffers. Other sample rates are converted
   with 10 ms FFT blocks, or bounded sinc blocks when a 10 ms block would
   contain a fractional sample. If the model fails to load, the worker
   cannot start, or the worker panics, audio falls back to the input. A
   watchdog also latches to input after 400 ms of continuous underruns.
   While the model loads (about 150 ms), the stream outputs silence; input
   queued during the load is dropped rather than played back fast.
   - Strength 0–100% maps to a removal limit of 6–30 dB.
   - **Speech protection.** While you talk, removal is capped at 6 dB, so
     weak syllables that the model mistakes for noise dip a little instead
     of fluttering. An RNNoise detector supplies only a speech probability;
     its audio is discarded. Full removal returns after both detectors agree
     on a 60 ms pause. See the
     [0.4.6 evaluation](denoiser-evaluation-0.4.6.md).
   - **Backup cleaning.** A noise profile learned in pauses cleans the raw
     signal that speech protection mixes back in. DeepFilterNet's own output
     is left untouched. See the [0.4.7 evaluation](denoiser-evaluation-0.4.7.md).
   - **Voice gate.** It opens on two consecutive speech frames (20 ms), with
     a one-frame look-ahead so the first syllable is kept. It holds 250 ms
     (350 ms with *On the desk*) and fades to −40 dB between words.
3. **Click ducking.** While you are talking, a spike above 1.5 kHz that
   stands well above the voice's own recent pulses ducks the highs by 12 dB
   for a few milliseconds, with a 2 ms look-ahead.
4. **Pop guard.** A plosive briefly raises the high-pass to 150 Hz.
5. **Voice EQ.** Bass (150 Hz shelf, with an automatic 350 Hz mud trim),
   Clarity (4 kHz) and Air (9 kHz shelf).
6. **Auto level.** Steers speech towards −23 dBFS RMS, with a 120 ms hold
   through brief gaps in speech detection. Gain adapts at +3 dB/s up and
   12 dB/s down. It adds up to +12 dB of boost, or +18 dB
   with *On the desk*. A vocal compressor follows: 5 ms RMS detection,
   3.5:1 from −31 dBFS with an 8 dB soft knee, 5 ms attack and 120 ms release.
7. **Saturation** (UI label; `warmth` in settings). Parallel tape-style
   saturation. Off at 0.
8. **De-esser.** 5–9 kHz detection; it reduces only the highs.
9. **Output volume and limiter.** A 1.5 ms look-ahead limiter with a
   −1 dBFS ceiling.

The APO reports a 60 ms latency estimate at every supported sample rate.
The worker cushion and skip threshold scale with the native rate (20 ms and
50 ms). The actual signal delay also includes resampler and 3.5 ms click and
limiter delays, so it can differ from the reported estimate.

## Installation details

`hush-setup install --mic <name>`:

- copies `hush_apo.dll` to `%ProgramFiles%\Hush`;
- registers the COM class and APO entry under `HKLM\SOFTWARE\Classes`;
- adds the CLSID and processing mode to the microphone's `FxProperties`
  endpoint-effect (EFX) slot, but only if those values are empty. Another
  product's effect is never replaced;
- creates `%ProgramData%\Hush`, granting write access to Users, the audio
  service and the write-restricted token `audiodg.exe` runs with;
- restarts Windows Audio and Audio Endpoint Builder, which caches effect
  settings.

`uninstall` identifies Hush-owned slots by their live CLSID and removes the
matching mode value with the slot. `installed.txt` is a status record. When
Setup switches microphones, it attaches by endpoint ID and rolls back
endpoint writes if switching or saving the record fails. The installer
registers the *Installed apps* entry before
attaching to the microphone. If attaching fails, Hush is still installed and
removable, and the app can retry from its Microphone page.

Design notes:

- **EFX, not GFX.** If a plugin fails to initialise in the legacy GFX slot,
  the whole microphone becomes unusable. In the EFX slot, Windows simply
  skips it.
- **COM aggregation.** The audio engine creates APOs aggregated, and
  `windows-rs` `#[implement]` objects cannot be aggregated, so
  `apo/src/apo.rs` hand-writes the COM object. `apo/tests/com.rs` reproduces
  the engine's creation sequence.
- **Panics.** An unwinding DSP panic restores the original input, including
  in-place buffers, and latches passthrough until the next stream lock.
  The normal callback uses preallocated storage. The fault flag hands
  Application event logging to the settings watcher.

## History

The project started as a virtual microphone driver. That needs Microsoft
attestation signing to load with Secure Boot on, so it was replaced by the
in-place plugin.
