# Cleaning the speech protection blend

Baseline: Hush 0.4.6 (`eff838a`). Input: the local 10-second 48 kHz take.
The baseline denoiser-only WAV matched the previous
release's reference byte for byte. Speech dip and pause scores use the
[same aligned 100 ms windows as 0.4.6](denoiser-evaluation-0.4.6.md).

## What changed

The 6 dB speech protection restores about half of the unprocessed signal.
That protects voice when the model over-suppresses a syllable, but also
restores steady noise. The new worker learns a noise power profile in
DeepFilterNet's frequency bands after both detectors have reported a pause
for 150 ms. It waits for 20 eligible observations and freezes the profile
through speech. A bounded, smoothed subtraction cleans only the restored
signal, with a maximum of 12 dB of extra attenuation on that component.

The worker applies the difference between the cleaned and original backup
using the model's own window and lookahead. It leaves the neural enhanced
component intact, adds no buffering delay, and needs no dependency or second
audio model. Bypass clears the correction tail immediately.

## Input level experiment

The following are suppression-only renders at 80% strength, with the gate
disabled. The input boost was compensated after DeepFilterNet and did not
change the Windows input setting or the output gain.

| Variant | Speech dip (dB) | Pause reduction (dB) |
| --- | ---: | ---: |
| 0.4.6 baseline | 1.86 | 13.52 |
| +6 dB model input | 0.44 | 25.20 |
| +12 dB model input | 0.66 | 21.72 |
| +18 dB model input | 0.62 | 22.53 |
| Backup cleaning, 35% | 1.82 | 13.83 |
| Backup cleaning, 65% | 1.82 | 14.10 |
| Backup cleaning, 100% | 1.81 | 14.42 |
| +6 dB with backup cleaning, 65% | 0.44 | 25.20 |

The level boost looked much better in a fresh stream. After repeating the
take for 40 seconds and analyzing its last 10 seconds, the +6 dB boost moved
the dip score from 0.84 to 0.69 dB, but pause reduction fell from 18.20 to
16.67 dB. It also did not improve controlled noise leakage during speech.
The boost is retained only in the offline renderer, with a headroom limit.

## Controlled noise during speech

The original take has no isolated clean speech track. To measure transmission
of a known noise source, `examples/noise-probe.mjs` adds and subtracts the same
-55 dBFS noise from the take, renders both, and projects half the output
difference onto that noise over the 67 aligned speech windows. Two seeded
realizations each of white and low-passed noise were used. This measures the
component of the response that tracks the added noise. The total response
also includes changes the model makes to the speech when noise is added.

| Variant | White leakage (dB) | Low-passed leakage (dB) |
| --- | ---: | ---: |
| 0.4.6 baseline | -5.24 | -4.07 |
| +6 dB model input | -5.16 | -3.81 |
| Backup cleaning, 65% | -8.52 | -5.47 |
| Backup cleaning, 100% | -11.00 | -6.32 |
| +6 dB with backup cleaning, 100% | -10.90 | -5.84 |

The selected 100% backup cleanup reduces coherent leakage by 5.76 dB for
white noise and 2.25 dB for low-passed noise. Total response also drops by
2.91 and 1.22 dB, respectively. On the low-passed probe listening sample,
the speech dip is 3.19 dB versus 3.26 dB for the baseline. The 100% setting
selects the bounded backup correction; it does not remove 100% of the noise.

This demonstrates an improvement against controlled stationary noise under
speech. It does not measure room reflections or establish how much ambience
is audible in the original take. Listen to the full-chain A/B before judging
the result on the desk microphone.

## Full chain and runtime checks

| Matched full-chain settings | Speech dip, old / new (dB) | Median pause output, old / new (dBFS) |
| --- | ---: | ---: |
| Fixed defaults, fresh stream | 7.91 / 7.96 | -98.40 / -99.58 |
| Fixed defaults, last 10 seconds of a 40-second render | 3.08 / 3.09 | -71.14 / -75.13 |
| Saved microphone settings, fresh stream | 6.84 / 6.89 | -104.11 / -105.26 |

AutoLevel's startup ramp affects the full-chain dip and pause scores, which
is why the warmed run is included. Both releases have the same measured
alignment: 51.48 ms for suppression only and 53.48 ms for the full inline
chain. There are no additional short drops or sample discontinuities on the
original take with the selected settings.

Three alternating 40-second suppression-only timing runs reported a median
of 2.93% of one core for each variant, including model startup. No difference
was resolved at the timing granularity of this test. The profile uses the
existing FFT implementation on the worker; no new runtime dependency or
model was added.

The 37 tests include profile learning/freeze, correction alignment, immediate
bypass, resampling, real-time worker and the Windows COM lifecycle.

## Reproduce

```powershell
cargo run --release --example render -- raw.wav comparison experiments
cargo run --release --example analyze -- comparison/7-clean-backup-strong.wav raw.wav
node examples/noise-probe.mjs comparison/0-raw.wav probe
cargo run --release --example render -- raw.wav saved-comparison saved
```

The last command reads the saved settings and records a copy in its output
directory for comparison. It does not write to the microphone configuration.
