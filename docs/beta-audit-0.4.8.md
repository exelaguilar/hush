# Hush 0.4.8 beta audit

The reference is 0.4.7 at `ea8b129`. The audit starts from the beta
preparation at `cb84025`. Standard noise-only and full-chain WAVs matched
the corresponding 0.4.7 reference files byte for byte.

## Findings and fixes

- **Worker startup.** The worker published readiness before removing its
  input backlog. The audio thread now discards input during loading, and
  readiness is published after the worker has allocated its buffers. The
  first accepted samples start fresh resampler, RNNoise and speech protection
  state. A threaded regression test supplies a recognizable stale marker
  during loading and then paces a fresh signal at 48, 44.1 and 16 kHz, followed
  by another 48 kHz start. The gate onset and hold have separate unit coverage.
- **APO panic recovery.** In-place input could be partially overwritten before
  the fallback ran. The callback now uses a scratch buffer allocated at
  stream lock to recover that block. The fallback latches until the next
  lock, which creates fresh DSP state. An injected partial-write fault
  verifies recovery and subsequent passthrough. Normal processing uses
  a bounded copy, and the fault flag is atomic. The panic hook and unwinding
  itself have no real-time guarantee.
- **Installer.** Setup selects by endpoint ID and installs the target before
  removing Hush from the previous capture endpoint. Endpoint edits roll back
  if a write or record replacement fails. Uninstall verifies live ownership
  and only removes recognized Hush slots and their associated mode value.
  Setup stages elevated executables under Program Files, resolves system
  utilities by absolute path, and writes complete payloads before replacing
  installed files. Installed apps registration comes before optional files
  and endpoint work. Install, microphone switching, forced failure and
  uninstall still need an elevated Windows smoke test.
- **App.** The first-run card asks for mic position. It uses the same control
  as the Microphone page. Both UIs have named switches and enable the
  Windows AccessKit bridge. Leaving Settings cancels a pending reset.
  The known-folder path retains its Windows Unicode representation.
  The hero screenshot was deferred by the owner.
- **Public history.** Both branch histories use generic recording paths
  and filenames. The release URL and repository ownership still need to
  be confirmed.

## Speech and room-tail experiments

The local 10-second, 48 kHz take was rendered fresh and as the final pass of
four repetitions. Listening copies are matched by active speech RMS.
The pause fade waits for 80 ms of consecutive quiet decisions from RNNoise,
DeepFilterNet and the gate classifier. It then fades the gate output by up
to 3 or 6 dB. The original gate and noise-only path retain their normal
behavior. The experimental attenuation defaults to zero.

| Full chain | Fresh / warm dip (dB) | Fresh / warm p90 flutter (dB) | Fresh / warm short drops | Fresh / warm tail (dB) |
| --- | ---: | ---: | ---: | ---: |
| 0.4.7 fixed defaults | 7.96 / 3.09 | 2.74 / 2.67 | 1 / 1 | −4.27 / −1.58 |
| Confirmed pause, 3 dB | 7.96 / 3.09 | 2.74 / 2.67 | 1 / 0 | −4.27 / −1.58 |
| Confirmed pause, 6 dB | 7.96 / 3.09 | 2.74 / 2.67 | 1 / 0 | −4.27 / −1.58 |
| Saved settings | 6.89 / 2.43 | 2.75 / 2.85 | 1 / 0 | −4.70 / −1.69 |
| Saved settings and desk mode | 8.37 / 3.01 | 2.79 / 2.89 | 1 / 1 | −4.56 / −1.91 |
| Saved settings, desk mode and sensitivity 50 | 8.37 / 3.01 | 2.79 / 2.89 | 1 / 1 | −4.56 / −1.91 |
| Saved settings and confirmed pause, 6 dB | 6.89 / 2.43 | 2.75 / 2.85 | 1 / 0 | −4.70 / −1.69 |

The matched noise-only result was 1.81 / 0.83 dB dip and 4 / 7 drops for
both the baseline and the 6 dB pause option. Measured alignment was
51.48 ms noise-only and 53.48 ms full chain across these comparisons.

The pause fades preserved speech by these metrics. The seven measured
word tails and the late-reverb probe were unchanged at reported precision.
Warm full-chain relative reverb transfer was −3.06 dB in all three cases.
Warm low-passed transfer was −6.21, −6.22 and −6.23 dB for the baseline,
3 dB and 6 dB variants. This experiment only affects confirmed pauses,
and provides no measured reason to ship it.

Desk mode raised the fresh dip score and added a warm short drop on
this take. Sensitivity 50 left those metrics unchanged. The first-run
choice sets mic position and preserves the user's sensitivity.

## Compressor detector comparison

The detector times below change the compressor's RMS estimate and keep its
attack, release and ratio fixed. The saved setting comparison keeps the
mic position and gate controls as recorded. Each value is fresh / warm.

| Full chain | Dip (dB) | p90 flutter (dB) | Short drops | Tail (dB) |
| --- | ---: | ---: | ---: | ---: |
| 0.4.7, 5 ms reference | 7.96 / 3.09 | 2.74 / 2.67 | 1 / 1 | −4.27 / −1.58 |
| 8 ms | 8.00 / 3.21 | 2.72 / 2.64 | 1 / 1 | −4.31 / −1.58 |
| 12 ms | 7.85 / 3.39 | 2.73 / 2.66 | 1 / 0 | −4.34 / −1.51 |
| 20 ms | 6.83 / 3.66 | 2.69 / 2.62 | 1 / 0 | −4.39 / −1.45 |
| Saved settings, 12 ms | 6.65 / 2.65 | 2.74 / 2.80 | 1 / 0 | −4.77 / −1.51 |

The 20 ms detector yielded the biggest fresh improvement, but the warm dip
increased from 3.09 to 3.66 dB. All three fixed-default detector variants
exceeded the warm dip guardrail. With the saved settings, the 12 ms
detector improved flutter from 2.85 to 2.80 dB warm, which still exceeds
the 2.67 dB limit. The noise-only 12 ms control matched the baseline
dip and drop counts. The beta retains the 5 ms detector.

The full-chain variants aligned at 53.48 ms. On one 40-second render each,
the default used 1.312 s of process CPU. The two pause fades each used
1.312 s, and the slower detectors used 1.297–1.328 s. These short offline
timings include model initialization and do not resolve a CPU difference.
Both experiments use existing model outputs and add zero look-ahead.

To reproduce, build the release `render` and `analyze` examples. Run the
renderer with `tails` for the settings and DSP comparisons, `saved` for
the saved settings reference, and `probe-tails` for the controlled probe.
Use the raw reference with the analyzer on the speech-matched copies.

Listen to the warmed saved settings pair first, with and without desk
mode. For the compressor tradeoff, compare the warmed default with the
20 ms variant, especially on quiet syllables.
