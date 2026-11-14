# Clicks, ambience and room sound under speech (0.4.10)

Offline evaluation only. Nothing shipped: both prototypes were removed from
the plugin after a level-matched listening test in which the current chain
sounded best and both alternatives sounded clearly worse. The measurements
are kept so the same ideas are not tried again without new evidence. The
click guard and the zero-latency room filter no longer exist in the tree;
`clicklab` still reproduces the baseline and the `protect`, `supp` and
`dereverb` comparisons.

## Method

`examples/clicklab.rs` mixes a known signal over a speech take and measures
what the chain does to it. The added signal is either the recorded
background take (room tone with 11 keyboard and mouse transients, no speech)
or a synthetic late-reverberation tail of the speech itself, 10 dB below it.

- **Residue**: energy of (processed mix − processed speech alone) in the
  event window, relative to the added signal, at the model output and at the
  end of the chain. It includes everything the addition changed, speech
  included.
- **Projection**: the part of that difference that is correlated with the
  added signal. Used for steady noise and reflections, where the speech's
  own reaction to the addition is larger than the addition.
- Clicks are scored separately when they land on speech and when they land
  in the first 300 ms after a word. Even-numbered events were used for
  tuning, odd-numbered ones held out. Six placements, at the recorded level
  and at +6 dB.
- Speech guardrails on the speech-only render: active level, worst-10% dip,
  p90 frame-to-frame flutter, short drops, word tails, the quietest fifth of
  speech frames, and frames dominated by energy above 4 kHz.

Speech takes: the new FIFINE take, the beta-audit take and clarity take 2.
Settings: a copy of the saved `mic.conf` with Clean Mic on (suppression 40,
gate sensitivity 8, bass +5, clarity −3, saturation 22, desk position).

## Findings

**Clicks.** The recorded clicks peak at −36 to −49 dBFS in the upper band.

- On voiced speech their 1 ms peaks sit a median 6–12 dB *below* the voice's
  own peaks within ±50 ms, in every band, on all three takes. No detector
  that looks at level can separate them there.
- DeepFilterNet does not remove them under speech. With speech protection
  off and suppression at 100%, the click at the model output dropped by a
  further 1–2 dB on speech and 3–8 dB between words, while p90 flutter rose
  from 2.42 to 5.89 dB.
- The existing click ducking stage changed nothing on any event.
- The prototype click guard (removed) limited bands that the model
  judged to be noise to their level before a sudden broadband rise, using
  the model's own look-ahead (no added delay). Held-out clicks between
  words, 1.5–8 kHz, end of chain, recorded level:

| Take | Current | Click guard | Change |
| --- | ---: | ---: | ---: |
| New take | −0.40 dB | −1.24 dB | 0.8 dB better |
| Beta-audit take | −0.52 dB | −0.57 dB | none |
| Clarity take 2 | −2.11 dB | −1.37 dB | 0.7 dB worse |

  Clicks on speech did not change. p90 flutter rose on two takes (2.25 →
  2.38 and 2.79 → 2.99 dB). The target was 3 dB. By ear it sounded worse.

**Steady noise.** Projected gain of the added room tone at the model output,
new take: −5.0 dB during speech, −7.2 dB in the first 150 ms after a word,
−21 dB at 150–400 ms, −39 dB later; the take alone leaves the chain at
−88.7 dBFS. In the first 150 ms after a word the finished signal sits at
−30.3 dBFS with or without the added room tone: the level there is the
voice's own room tail, about 25 dB above the steady noise. Removing more
steady noise under speech would not be audible, so nothing was changed.

**Room sound.** The late-reverberation gains of `dereverb.rs` can be applied
inside the model step, because libDF exposes the spectrum it has just
written (`DfTract::spec_buf`, read after `process`). This matches the earlier
prototype without its extra 10 ms. Injected reflections, assumed RT60 0.5 s:

| Take | Removed during speech | Removed 0–150 ms after a word | Speech level | Dip | p90 flutter | Quiet syllables |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| New take | 1.4 dB | 3.4 dB | −0.37 dB | +0.17 dB | 0.00 dB | −0.03 dB |
| Beta-audit take | 2.0 dB | 3.5 dB | −0.24 dB | +0.29 dB | +0.21 dB | −0.21 dB |
| Clarity take 2 | 1.8 dB | 3.8 dB | −0.35 dB | +0.18 dB | −0.12 dB | −0.55 dB |

No short drops, 53.27 ms delay in every case. With the real background take
the room tone right after a word fell from −7.2 to −10.2 dB. In the finished
signal the level right after words fell by 0.6–1.2 dB before level matching.
These numbers measure removal of the added reflections, not of the real
room. By ear it sounded worse than the current chain despite these numbers,
so the removal of injected reflections is not a usable proxy for preference
on this microphone.

Processing cost: 1.27–1.33 s of CPU for 30 s of audio for all three
variants; the difference is within run-to-run variation.

## Reproduce

```powershell
cargo build --release --example clicklab
$lab = ".\target\release\examples\clicklab.exe"
# events, then scores for the current chain, the click guard and the room filter
& $lab events <speech.wav> <background.wav> <settings.conf> <out-dir>
& $lab score  <speech.wav> <background.wav> <settings.conf> <out-dir> base
& $lab score  <speech.wav> <background.wav> <settings.conf> <out-dir> protect=100+supp=1.0
& $lab score  <speech.wav> reflect          <settings.conf> <out-dir> dereverb=0.5
# level-matched listening files (placement offset in samples, added gain in dB)
& $lab clips  <speech.wav> <background.wav> <settings.conf> <out-dir> dereverb=0.5 357555 0
```

Variants: `protect=100` (speech protection off), `supp=1.0`, `dereverb=0.5`
(the +10 ms filter). Parts combine with `+`. The `click` and `inline`
variants in the tables above need the removed prototypes.

## Guardrails

With the defaults, `1-noise-removal-only.wav` and `2-noise-removal-only-50.wav`
equal the beta-audit baseline delayed by 96 samples with zero residual, and
`4-full-chain-no-auto-level.wav` is byte-identical. Warm full chain: dip
3.09 dB, p90 flutter 2.67 dB, one short drop, tail −1.58 dB. Fresh: 7.51 dB,
2.73 dB, one, −4.29 dB.
