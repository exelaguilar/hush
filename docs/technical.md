# Technical details

The [README](../README.md) covers what Hush does and how to use it. This page
covers how it runs on your PC, what it costs, and how to build and debug
it. For the full signal chain and design notes, see
[architecture.md](architecture.md). For how it was tested, see
[testing.md](testing.md).

## How it runs

Hush installs an audio processing object (APO) on one microphone endpoint.
Windows' audio engine (`audiodg.exe`) loads it and runs every captured buffer
through it, before any app sees the audio:

```text
mic → rumble filter → DeepFilterNet 3 + voice gate → click ducking → pop guard
    → voice EQ → auto level + compressor → saturation → de-esser → limiter → apps
```

Because the cleanup happens inside the audio engine, every app that opens
that microphone gets the cleaned signal. You don't need a virtual device or a
driver, and the audio is processed once however many apps are listening.

| Part | What it does |
| --- | --- |
| `hush_apo.dll` | The audio plugin. Runs inside `audiodg.exe` on the selected mic. |
| `hush.exe` | The app. In the tray it runs the health watch, repairs, mute hotkey and notifications with no window and no graphics. Opening Hush starts a second `hush.exe` for the window (settings, live meters, Mic Test), which exits when you close it. It never processes audio for other apps. |
| `hush-setup.exe` | Attaches the plugin to a microphone, removes it, reports status and repairs it. |
| `Hush-Setup-<version>.exe` | The installer, which embeds the three files above. |

The app and plugin use `%ProgramData%\Hush` for tuning and
`%ProgramData%\HushRuntime` for engine output:

| File | Purpose |
| --- | --- |
| `mic.conf` | Settings. Written by the app and polled by the plugin every 250 ms. |
| `status.txt` | Heartbeat written by the plugin once a second while audio flows, with its own CPU use over that second. |
| `live.bin` | Memory-mapped meter levels, updated 20 times a second. |

Setup stores the installed endpoint record and repair results under
`HKLM\SOFTWARE\Exel\Hush`.

App preferences such as startup, tray, mute hotkey and notifications live in
`%APPDATA%\Hush\app.conf`.

## Requirements

- Windows 10 or 11, 64-bit. Hush has been developed and tested on Windows 11
  only.
- Administrator rights to install.
- **Audio enhancements** turned on for the microphone in **Settings → System →
  Sound → (your mic)**. Windows skips every effect plugin when they are off.
- Works with Secure Boot on, because Hush installs no driver.
- Any capture sample rate. Rates other than 48 kHz are resampled internally.

## Cost

| | |
| --- | --- |
| CPU | About 3% of one core for the audio processing. Measured live by the plugin on the developer's PC during a Discord call: 0.8% to 3.4% of one core. **About → Status** shows the figure for your PC. The tray app adds well under 0.1% while idle. |
| Added delay | About 75 ms, calculated from the model's look-ahead plus a 20 ms safety buffer. Not measured end to end. |
| Memory | The tray app uses about 3 MB. The window adds about 80 MB, mostly the graphics driver, only while it is open. The neural model loads inside `audiodg.exe` while a stream is open. |
| Network | None. The only network call is **Check for updates**, which runs only when you click it. |

When **Clean Mic** is off, the effects are bypassed. The model keeps running
at a nearly inaudible setting so the audio stream's delay stays constant.

## Microphone health and repair

The tray app watches the microphone that Clean Mic is installed on. It
listens for Windows device changes and probes about once a second, reusing
its last device scan until Windows reports a change:

| State | Detected by | Action |
| --- | --- | --- |
| Disconnected | The endpoint is not active, and no endpoint with the same name is active. | Notification after 3 s. Shorter USB blips are ignored. |
| Missing | The mic is back, but Hush's effect is not on it, because Windows dropped it or the mic came back under a new endpoint ID. | Runs the repair task. |
| Not processing | An app is capturing from the mic, but the plugin has reported no audio for 4 s. | Restarts Windows audio once, through the restart task. |
| Bypassed | Still not processing 15 s after a restart. Usually an app is asking Windows for raw audio. | Tells you, and stops retrying. |

Repairs run through two on-demand scheduled tasks that Setup creates. They
run as SYSTEM and have fixed command lines:

- `\Hush\Repair Clean Mic` runs `repair --quiet --scheduled`.
- `\Hush\Restart Clean Mic` runs `repair --restart-audio --quiet --scheduled`.

A standard user may start these tasks but not change them. They are only
created when `hush-setup.exe` runs from `%ProgramFiles%\Hush`. Automatic
repairs are limited to one every 2 minutes and three every 30 minutes, and
they never show an administrator prompt. If the tasks are missing, **Fix
now** can fall back to an elevated repair once.

## Debugging

- **Copy diagnostics** (Settings or About) copies the version, Windows build,
  microphone, engine status, glitch counters, health state, last repair and
  current settings.
- `hush-setup.exe status` prints the same engine information from a terminal.
  `hush-setup.exe list` shows every capture and render endpoint and its
  effects.
- To capture detailed plugin traces, create `%ProgramData%\Hush\debug.flag`
  and restart Windows Audio. Traces then go to
  `%ProgramData%\HushRuntime\apo-trace.log`.
- Errors are logged to the Windows Application event log under the source
  `Hush`:
  ```powershell
  Get-WinEvent -LogName Application -MaxEvents 500 | ? ProviderName -eq Hush | Select TimeCreated, Message
  ```

## Building from source

Requires Rust (stable, MSVC toolchain) on Windows.

```powershell
cargo build --workspace --release
cargo test --workspace --exclude hush-installer --all-targets
powershell -ExecutionPolicy Bypass -File .\package.ps1   # -> dist\Hush-Setup-<version>.exe and .sha256
```

The installer is one file. Its build script (`installer/build.rs`)
brotli-compresses `hush.exe`, `hush-setup.exe` and `hush_apo.dll` from
`SM_PAYLOAD_DIR` (default `target\release`) into `OUT_DIR`, each behind its
original length. At install time the installer unpacks them in memory,
checks every length, and only then touches the system. Run
`cargo test -p hush-installer` for its codec tests, with `SM_PAYLOAD_DIR` set
and `__COMPAT_LAYER=RunAsInvoker` (the installer manifest requires elevation
and the test executable shares it).
`package.ps1` sets `CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_OPT_LEVEL=3` so the
compression step is fast; set it yourself for a plain release build of the
installer.

`Hush-Setup.exe /S` (or `--silent`) installs with no window, and must run
elevated. It attaches Clean Mic to the microphone Hush already uses (else
the first physical one), keeps your saved settings, does not launch the app,
and exits 0 on success (1 if a requested attachment fails or setup fails).
When the install directory is available, details are written to
`%ProgramFiles%\Hush\install.log`.

Releases: push a `vX.Y.Z` tag that matches the version in `Cargo.toml`.
`.github/workflows/release.yml` builds and tests on Windows, then creates the
GitHub release with `Hush-Setup-X.Y.Z.exe`, its `.sha256`, and the matching
`CHANGELOG.md` section as notes (an existing release keeps its notes).
`.github/workflows/ci.yml` builds and tests every push and pull request.

`packaging/winget/` holds the winget manifest templates (`Exel.Hush`) and
`update-manifests.ps1 -Version X.Y.Z`, which fills them from the release hash.
Its README covers submitting them to winget-pkgs.

`hush-setup.exe` manages the plugin directly:

| Command | Does |
| --- | --- |
| `list` | Lists endpoints and their effects. |
| `status` | Shows install, engine and repair status. |
| `install --mic <name>` | Attaches Clean Mic to a microphone. Needs admin. |
| `uninstall` | Removes it. Needs admin. |
| `repair [--restart-audio]` | Puts Clean Mic back where it was recorded. Needs admin. |

Offline tools in `examples/`:

- `render.rs`: renders a recording through the chain.
- `analyze.rs`: measures speech dips, pause levels and delay against the raw
  input.
- `clicklab.rs`: mixes known clicks or reverb over speech and measures what
  the chain does to them.

Developer switches for the app: `HUSH_PAGE=<page>` opens a page, and
`HUSH_SHOT=<file.png>` saves a screenshot and exits. `HUSH_SHOT_SCALE`,
`HUSH_SHOT_SIZE` and `HUSH_SHOT_TEST` adjust the screenshot.
