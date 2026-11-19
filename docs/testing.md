# How Hush was tested

Hush's sound was tuned by ear in level-matched A/B comparisons and backed
by offline measurements. Its code has 89 automated tests, and its failure
paths were tested by deliberately breaking them. It runs daily on real PCs
in Discord calls and games.

## Where it runs

| | |
| --- | --- |
| PCs | The developer's PC and a second user's PC |
| Microphone | fifine SC3 USB mic on a desk |
| Apps | Discord, games, and alongside SteelSeries Sonar |
| Rooms | Ordinary rooms with room tone, keyboard and mouse |

## How sound changes are decided

Changes to the sound are not shipped on numbers alone.

1. **Offline renders.** Every candidate change is rendered from the same
   10-second, 48 kHz recording of real speech at a desk. The recording is
   not published, because it is a real voice. `examples/render.rs` produces
   the renders, and `examples/analyze.rs` measures them against the raw
   input, aligned for delay:
   - speech dips, which catch over-suppressed syllables;
   - level in pauses, which shows how much noise is left;
   - frame-to-frame flutter;
   - word tails;
   - delay.
2. **Regression check.** Unchanged settings must reproduce the previous
   release's output byte for byte. That proves a refactor didn't quietly
   change the sound.
3. **Level-matched listening.** The candidate and the current version are
   adjusted to the same loudness and compared by ear. Louder almost always
   sounds better, so matching the levels keeps the comparison fair. Only a
   change that sounds better ships.

Several models and approaches were evaluated along the way, including
neural noise removal, speech detection, speech protection, click guards and
dereverb. Ideas that looked good on paper but lost by ear were rejected.
Their reports stay in the repo so they are not retried without new
evidence:

- [0.4.6 denoiser evaluation](denoiser-evaluation-0.4.6.md): speech
  protection, which stops quiet syllables from fluttering.
- [0.4.7 evaluation](denoiser-evaluation-0.4.7.md): cleaning the noise that
  speech protection mixes back in.
- [Room sound experiment](room-reverb-experiment.md): what is left after
  noise removal. Room reverb measured as the biggest remaining factor, and a
  simple dereverb brought flutter back, so it did not ship.
- [Clicks and ambience under speech (0.4.10)](click-ambience-evaluation-0.4.10.md):
  two prototypes, a click guard and a zero-latency room filter. Both
  measured well on some metrics and sounded clearly worse, so both were
  removed.

Measurements guide the work, but the final decision is always made by ear.

## Automated tests

`cargo test --workspace --exclude hush-installer --all-targets` runs 89
tests. CI runs them on every push on a GitHub-hosted Windows machine.

| Area | Tests | Examples |
| --- | --- | --- |
| Audio plugin | 46 | Noise and click removal; a single click never opens the gate; soft word endings keep it open; limiter never exceeds its ceiling; other sample rates; real-time worker pacing; stream start without a fast-forward; watchdog and panic fallback; settings that survive garbage and read failures |
| COM integration | 3 | Recreates how Windows' audio engine creates and aggregates the plugin |
| App | 31 | Health watch decisions and rate limits; level warnings; diagnostics; settings import validation; version comparison; notification XML |
| Setup tool | 9 | Repair planning: an unplugged mic, a mic back under a new ID, two mics with the same name; install record and repair task XML |

## Failure testing

When something breaks, your mic should keep working and Windows audio should
not go down with it.

- **Processing crash.** A panic is injected mid-buffer, including after
  in-place writes. The test checks that the original audio is restored and
  that the stream then passes audio through unprocessed. A crash inside
  `audiodg.exe` would otherwise take down all Windows audio.
- **Model worker failure.** An injected worker panic falls back to the
  unprocessed mic. A watchdog does the same after 400 ms of continuous
  underruns. Both have tests.
- **Setup.** Switching microphones attaches the new one before releasing the
  old one, and rolls back registry changes if any step fails. Hush never
  replaces another product's effect on a microphone.
- **Byte-for-byte audits.** The [0.4.8 beta audit](beta-audit-0.4.8.md)
  re-checked start-up, recovery and setup, and confirmed the sound matched
  0.4.7 exactly.

## Tried it on your setup?

[Open an issue](../../../issues/new/choose) whether it works or not, and
paste the output of **Copy diagnostics** (Settings). A short "works on a
Blue Yeti with Windows 10" helps too.
