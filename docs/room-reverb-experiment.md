# What is left after 0.4.7? A room-sound experiment

Baseline: Hush 0.4.7 (`ea8b129`), on the same 10-second, 48 kHz desk-mic
take used in [0.4.6](denoiser-evaluation-0.4.6.md) and
[0.4.7](denoiser-evaluation-0.4.7.md). The take is not published.

The question was which of these accounts for the remaining gap: room
reflections, noise that overlaps speech, dynamics or tone after suppression,
or competing voices. This experiment measures each one under the same
conditions instead of tuning suppression further.

## Why reverberation was the first suspect

- **DeepFilterNet keeps much of the room by design.** In libDF's training
  augmentation (`RandReverbSim` in `libDF/src/augmentations.rs`, v0.5.6), the
  target speech keeps the direct sound and the first 5 ms of reflections. Its
  later tail is shortened to an RT60 of 0.5 s by default. The model is trained
  to treat a moderate room tail as part of the voice.
- **Commercial tools treat echo as a separate problem.** NVIDIA documents
  "Room Echo Removal" in Broadcast as a filter for echo and reverb in small
  rooms, separate from Noise Removal. [Broadcast FAQ][nv-faq] Its developer
  SDK lists `denoiser`, `dereverb` and a combined "Denoise and Dereverb"
  effect, and says the combined effect performs much better than running the
  two separately. [Maxine AFX SDK][nv-afx] SteelSeries describes ClearCast AI
  Noise Cancellation in Sonar as an AI noise-cancellation effect with
  adjustable intensity. [SteelSeries support][ss-anc] The page refused
  automated reads, so its details are unverified here. I found no SteelSeries
  documentation of a separate room-echo control.
- **Unknown.** Neither vendor documents its models, training targets or
  how much reverberation it removes. Any statement about how they do it here
  would be a guess.

## Measurements

All results are relative to each variant's own median speech gain. A
0 dB result means the injected signal experiences the same gain as speech.
The probe starts 10 dB below the take for late reverb, so this is a
relative transfer measurement. A **warm** stream
renders the take four times (40 s) and analyzes the last pass. A **fresh**
stream starts cold.

### Controlled probes

`render ... probe` adds a known signal to the take, renders take + probe and
take − probe, and projects half the difference onto the probe over the 67
speech windows. The **late reverb** probe is a four-line feedback delay
network. Its first echo arrives after about 50 ms, it has no early
reflections, it decays at an RT60 of 0.6 s, and it is mixed 10 dB below the
take. The noise probes use the existing −55 dBFS levels.

| Warm stream | Late reverb | Low-passed noise | White noise |
| --- | ---: | ---: | ---: |
| Noise removal only, 0.4.7 | **+0.79** | −2.35 | −7.86 |
| Noise removal only, no speech protection | +1.18 | −2.05 | −10.15 |
| Noise removal only, dereverb 0.5 s | −0.85 | −3.41 | −8.92 |
| Noise removal only, dereverb 0.8 s | −1.67 | −3.83 | −9.55 |
| Full chain, 0.4.7 | −3.06 | −6.21 | −10.99 |
| Full chain, dereverb 0.5 s | −4.36 | −6.95 | −11.75 |

Fresh streams follow the same pattern with lower values; see `probe.tsv`.
Late reverberation is the least-suppressed of the three. DeepFilterNet alone
passes it at full strength. The 6 dB speech protection does not add it; the
protected variant is slightly lower. As with the noise probe, this measures
only the part of a nonlinear response that tracks the probe.

### Other explanations

- **Dynamics after suppression.** Flutter is now measured on frames selected
  from the raw take, the same frames in every variant. At the 90th
  percentile, the wobble in 10 ms level is 2.15 dB raw, 2.24 dB after noise
  removal only and 2.67 dB in the full chain. Turning off auto level gives
  2.53 dB. Turning off the gate, de-esser, click ducking or warmth makes no
  measurable difference. Dynamics add about 0.5 dB, so this is a
  small effect. (Measured on the processed file's own speech frames,
  the full chain read 4.5 dB, but most of that came from soft word edges
  counted as speech after the 13 dB gain.)
- **Tone.** During speech, 0.4.7 measures 2–5 kHz at −8.7 dB and 150–500 Hz
  at +0.6 dB relative to 500–2000 Hz. That is close to a typical long-term
  speech spectrum, perhaps 1 dB dark in presence. This is weak evidence for a
  tonal problem.
- **Competing voices.** The take contains one talker, so this could not be
  tested. Neither DeepFilterNet nor Hush's gate distinguishes one voice from
  another.

## Prototype: late-reverberation post-filter

`apo/src/dereverb.rs` estimates late reverberation with the statistical
exponential-decay model (Lebart, Boucher and Denbigh, 2001; Habets, 2007).
In each of DeepFilterNet's 32 ERB bands, the late power estimate is
`10^(−6·0.05/RT60)` times the power 50 ms earlier. The filter applies a
smoothed spectral gain to the model output, limited to 10 dB of reduction.
It uses libDF's STFT, so it adds one 10 ms hop of latency.

A first version that cleaned only the protective dry blend was rejected: it
did not shorten word tails and it increased choppiness, consistent with the
probe showing the room is in the model output.

| Full chain | Speech dip (dB) | 90% flutter (dB) | Pause output (dBFS) | Latency (ms) |
| --- | ---: | ---: | ---: | ---: |
| 0.4.7, fresh | 7.96 | 2.74 | −99.6 | 53.5 |
| Dereverb 0.5 s, fresh | 8.47 | 2.93 | −100.4 | 63.5 |
| 0.4.7, warm | 3.09 | 2.67 | −75.1 | 53.5 |
| Dereverb 0.5 s, warm | 3.20 | 2.79 | −80.5 | 63.5 |
| Dereverb 0.8 s, warm | 3.39 | 2.87 | −84.6 | 63.5 |

With noise removal only (warm), the dip score rises from 0.83 to 1.40 dB at
0.5 s and 2.25 dB at 0.8 s. Short drops in speech rise from 7 to 14 and 16.
In the full chain those drops are covered (1 → 0). The filter costs one more
960-point FFT pair every 10 ms. Offline CPU time for 40-second renders
(1.1–1.5 s) showed no difference above run-to-run variation.

**Verdict: rejected by listening.** On the probe, the prototype removes 1.3–2.5
dB of late reverberation. On the owner's desk-mic take, though, both B
(0.5 s) and C (0.8 s) audibly brought back some of the flutter that 0.4.5–0.4.7
removed. The metrics hinted at this: the 90th-percentile flutter rose from
2.67 to 2.79/2.87 dB, and with noise removal only, the dips rose from 0.83 to
1.40/2.25 dB. Flutter is more objectionable than room tail, so this filter
stays off. Any future dereverberation must hold the full-chain 90%
flutter and the noise-only dip score at the 0.4.7 values. It is not
enabled in the live chain. With all experimental options at their defaults,
renders match 0.4.7 byte for byte.

## Ranked next steps

| Idea | Likely audible benefit | Cost | Risk to quiet syllables |
| --- | --- | --- | --- |
| Better late-reverb suppression: online RT60 estimated from decays in pauses, a DRR-aware estimator, gains limited where RNNoise reports speech | High: the largest measured residual | Low CPU; +10 ms | High: the simple version brought flutter back by ear |
| Dereverberation-trained model (DeepFilterNet fine-tuned with a shorter RT60 target, or a small dedicated model) | Highest | Training data and compute; same runtime | Unknown until trained |
| Noise under speech: extend backup cleaning to the enhanced component during speech only | Medium (low-passed noise is still −2.4 dB in denoiser output) | Low | Medium |
| Gentler auto-level/compressor detector (longer RMS window) | Low (≈0.1–0.2 dB of flutter) | Trivial | Low |
| Tonal "distance" correction | Low on this take | Trivial | Low |
| Target-speaker extraction for background voices | High where other people talk | High (enrollment and a new model) | High |

## Reproduce

```powershell
cargo build --release --example render --example analyze
.\target\release\examples\render.exe raw.wav out room     # renders and level-matched listening copies
.\target\release\examples\render.exe raw.wav out probe    # controlled late-reverb and noise probes
.\target\release\examples\render.exe raw.wav out stages   # one stage off at a time (warm)
.\target\release\examples\analyze.exe out\analysis\warm-f0-0.4.7.wav out\analysis\raw.wav
```

[nv-faq]: https://www.nvidia.com/en-us/geforce/broadcasting/broadcast-app/faq/
[nv-afx]: https://docs.nvidia.com/maxine/afx/latest/index.html
[ss-anc]: https://support.steelseries.com/hc/en-us/articles/41269418834061-What-s-the-difference-between-ANC-and-ClearCast-AI-Noise-Cancellation
