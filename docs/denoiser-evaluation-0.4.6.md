# Denoiser evaluation, 2026-11-08

Input: a local unprocessed 10-second, 48 kHz recording.
Baseline: commit `c2f8e6c` (0.4.5). All renders use the fixed settings in
`examples/render.rs`, not the live `mic.conf`.

## Measurement

`analyze` now accepts an optional raw reference. It estimates processing delay
by normalized waveform correlation, then compares matching 100 ms windows.
Speech windows are selected from the **raw** RMS above -45 dBFS; pauses are
selected below -50 dBFS. Each processed variant therefore uses the same input
labels, rather than relabeling suppressed speech as silence.

The dip score is median processed-minus-raw gain minus its 10th percentile
over speech windows. Smaller is better. This aligned metric is not directly
comparable to the handoff's 6.6 dB value, whose exact measurement procedure
was not recorded. The aligned 0.4.5 baseline is 1.86 dB for suppression alone.

Full-chain results also include AutoLevel's startup gain ramp. Use the
suppression-only comparison to isolate denoiser artifacts; listen to the
full-chain comparison to assess the resulting voice.

There is no isolated clean speech reference. These measurements cannot prove
noise reduction underneath simultaneous speech or establish perceived quality.
The WAV comparisons remain necessary.

## Experiments

Suppression only, 80% strength, gate disabled:

| Candidate | Speech dip (dB) | Pause reduction (dB) | Result |
| --- | ---: | ---: | --- |
| 0.4.5 fixed 6 dB cap | 1.86 | 11.53 | Baseline |
| 50 ms / 100 ms smooth cap | 1.87 | 11.53 | No material change |
| Per-bin smoothing, 12 dB speech cap | 4.02 | 10.07 | Rejected |
| Per-bin smoothing, noise-aware 6 dB floor | 3.54 | 12.68 | Rejected: speech dips |
| Dry-blend formula, neighboring-bin protection | 2.18 | 9.89 | Rejected: louder pauses |
| 500 ms spectral release, fast pause release | 3.04 | 11.91 | Rejected: speech dips |
| RNNoise probability + smooth 6 dB cap | 1.86 | 13.52 | Kept |

The spectral experiments reused libDF's STFT window and lookahead, calculated
enhanced/noisy magnitudes, smoothed the gains, and resynthesized the delayed
noisy spectrum. They did not add an FFT dependency or another neural model.

## Selected change

RNNoise (`nnnoiseless`, default features disabled) receives the same 48 kHz,
480-sample input frame as DeepFilterNet, scaled to signed-16-bit sample units.
Its audio output is discarded. A probability of at least 0.6 protects an onset
before the delayed DeepFilterNet output is available. Either detector can keep
protection active. If probability stays below 0.3 and the existing speech
classifier also reports no speech for 60 ms, the 300 ms hold ends early.
Uncertain frames retain the original hold. Dry-blend protection fades in over
50 ms and out over 100 ms. The voice gate itself is unchanged.

On the supplied take, suppression-only pause reduction improves by 1.99 dB
without increasing the 1.86 dB speech-dip score. The median pause output changes
from -71.07 to -74.43 dBFS (these are separate medians; their difference is not
the median per-window improvement). The full-chain dip and median pause scores
are unchanged: 7.91 dB and -98.40 dBFS. Its loudest tenth of non-speech 10 ms
frames rounds from -47 to -48 dBFS. This is a small pause improvement, not
evidence that room ambience underneath speech or the studio-quality goal is
solved.

Validation includes tests for independent onset protection, agreement before
shortening the hold, uncertain word endings, smooth transitions, and bypass
strength. Existing DSP, resampling, real-time worker, and aggregated COM tests
also pass.

Three alternating baseline/candidate timing runs after compilation finished
processed all four 10-second variants in fresh processes. Median total CPU time
was 1.344 seconds for 0.4.5 and 1.453 seconds for the selected change: 3.36%
versus 3.63% of one logical core, including four model loads and WAV I/O.
The added detector/control cost was about 0.27 percentage points of one core.
These are short offline measurements, not a claim about whole-machine CPU or
long-running `audiodg.exe` utilization. The new standalone DLL is 27,538,432
bytes; the packaged installer is 42,489,344 bytes.

## Reproduce

```powershell
cargo run --release --example render -- <raw.wav> <output-directory>
cargo run --release --example analyze -- <processed.wav> <raw.wav>
```

The renderer reports both elapsed processing time and Windows process CPU
time, including model loading. CPU percentage is relative to one logical
core over the duration of the input; it is not whole-machine utilization.
