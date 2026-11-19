# Changelog

## 1.0.1

**Fixes**
- If *Audio enhancements* are switched off for your microphone, Windows skips
  Clean Mic entirely. Hush now says so, with a *Sound settings* button, instead
  of restarting Windows audio and blaming an app.
- The mute hotkey no longer falls back to another microphone when yours is not
  plugged in. It says *Microphone not found.* instead.
- The live meters recover when Clean Mic is reinstalled while Hush is running.
- Mic Test: the level card is now *What others hear* and shows *Silent* in a
  quiet room instead of -100 dB.
- Light theme: clearer switches, slider rails, badges and warning text.

**Notifications**
- Clicking a notification opens Hush.

**Lighter on your PC**
- The mute state updates from Windows' own change notifications instead of
  polling every second, the tray tooltip is only refreshed when it changes, and
  Hush reads the install status less often.

**Behind the scenes**
- One shared microphone lookup, the window code split into smaller files, and
  the screenshot and theme developer switches exist only in debug builds.

## 1.0.0

**Clean Mic stays on**
- Hush watches your microphone. If it is unplugged, you get a notification,
  and when it comes back without Clean Mic (Windows dropped the effect, or the
  mic reappeared on another USB port), Hush puts Clean Mic back by itself.
- If an app is recording from your mic but Clean Mic is not processing it,
  Hush restarts Windows audio once to reload it, then tells you if an app is
  still bypassing it.
- Repairs run through two on-demand scheduled tasks created by Setup, so they
  need no administrator prompt. Automatic repairs never show one; *Fix now*
  may, on installs without the tasks.
- New settings: *Repair Clean Mic automatically* and *Notifications*.
- The device list refreshes when microphones are plugged in or removed, and
  has a *Refresh* button.

**App**
- New logo.
- Warning on the Microphone page when your mic clips or is too quiet.
- *Mute hotkey* (off by default): mutes your microphone in every app, with the
  state shown in the tray.
- *Copy diagnostics* on Settings and About, replacing the terminal commands
  for bug reports.
- *Save tuning* / *Restore tuning*, and *Export* / *Import* of mic settings.
- *Open recordings folder* on Mic Test, and *Mic Test* in the tray menu.
- *Check for updates* on About, only when clicked.
- Copy buttons work (they silently did nothing before).
- Tidier layout: logo in the sidebar, aligned slider values, more spacing.

## 0.4.8 (first public beta)

**Sound**
- No more fast-forward blip when an app opens the mic. Audio that piled up
  while the noise model loaded was played back at double speed, about 0.2 s
  at the start of every call or Mic Test. That audio is now dropped, and the
  stream starts cleanly once the model is ready.
- Noise removal itself is unchanged from 0.4.7. Offline renders match it
  byte for byte.

**Reliability**
- If Clean Mic processing ever crashes, the microphone keeps working
  unprocessed instead of taking down all Windows audio. The failure is logged
  to the Application event log.
- If Setup cannot attach Clean Mic to the chosen microphone, Hush still
  installs, appears in *Installed apps*, and explains how to turn Clean Mic on
  from the app. Before, a failure here left files behind with no uninstall
  entry.
- Mic Test recordings are saved to your real Documents folder, including
  when it is redirected to OneDrive.
- Setup switches Hush to the selected microphone on upgrade. Endpoint
  changes roll back if attachment or record writing fails.

**App**
- *Mic position* options are now *Near my mouth* and *On the desk*.
- *Reset to defaults* asks for a second click before discarding your
  settings.
- Switches show a focus ring for keyboard users, and screen readers
  receive the switch name and on/off state.
- The welcome card asks where you normally speak into the microphone.
- The tone labels no longer overlap: *Bass* is "body and fullness", and the
  *Warmth* slider is now *Saturation*.
- About has a *Licenses* button.

**Project**
- MIT license, third-party notices (also installed next to the app), issue
  templates, and a rewritten README.
- Offline tools: a room-reverb experiment, controlled late-reverb probes, and
  an aligned flutter metric. See `docs/room-reverb-experiment.md`.
