// Summarize spectral balance on the same raw-selected 10 ms windows in
// every render. The labels are acoustic proxies, not phoneme transcriptions.
// node examples/clarity-metrics.mjs <clarity-render-directory>
import assert from "node:assert/strict";
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const root = process.argv[2];
assert(root, "supply the output directory from the clarity renderer");
const stages = ["denoised", "click", "pop", "eq", "level", "saturation", "deess", "limiter"];
const traceNames = ["s0-saved", "s3-pre-eq-100"];

function readWav(path) {
  const bytes = readFileSync(path);
  assert.equal(bytes.toString("ascii", 0, 4), "RIFF");
  let rate = 0;
  for (let p = 12; p + 8 <= bytes.length;) {
    const chunk = bytes.toString("ascii", p, p + 4);
    const size = bytes.readUInt32LE(p + 4);
    if (chunk === "fmt ") {
      const format = bytes.readUInt16LE(p + 8);
      assert(format === 3 || (format === 0xfffe && bytes.readUInt32LE(p + 32) === 3));
      assert.equal(bytes.readUInt16LE(p + 10), 1);
      assert.equal(bytes.readUInt16LE(p + 22), 32);
      rate = bytes.readUInt32LE(p + 12);
    } else if (chunk === "data") {
      assert(rate && size % 4 === 0 && p + 8 + size <= bytes.length);
      const samples = Float32Array.from({ length: size / 4 }, (_, i) => bytes.readFloatLE(p + 8 + 4 * i));
      return { rate, samples };
    }
    p += 8 + size + (size % 2);
  }
  throw new Error("missing WAV data");
}

const { rate, samples: raw } = readWav(join(root, "analysis/raw.wav"));
assert(rate >= 20000, "the 5–9 kHz analysis requires a higher sample rate");
const frame = Math.floor(rate / 100);
const db = (power) => 10 * Math.log10(Math.max(power, 1e-15));
const ratio = (a, b) => db(a) - db(b);
const median = (values) => {
  values.sort((a, b) => a - b);
  if (!values.length) return NaN;
  const i = Math.floor(values.length / 2);
  return (values[i] + values[Math.floor((values.length - 1) / 2)]) / 2;
};

function filter(frequency, highPass = false) {
  const w = 2 * Math.PI * frequency / rate;
  const c = Math.cos(w);
  const a = 1 + Math.sin(w) / Math.SQRT2;
  const b = (highPass ? 1 + c : 1 - c) / (2 * a);
  const middle = (highPass ? -2 : 2) * b;
  const feedback1 = 2 * c / a;
  const feedback2 = -(2 - a) / a;
  let x1 = 0, x2 = 0, y1 = 0, y2 = 0;
  return (x) => {
    const y = b * x + middle * x1 + b * x2 + feedback1 * y1 + feedback2 * y2;
    x2 = x1; x1 = x; y2 = y1; y1 = y;
    return y;
  };
}

function measure(samples) {
  const bands = [[120, 300], [300, 700], [700, 2000], [500, 2000], [2000, 5000], [5000, 9000]]
    .map(([lo, hi]) => [filter(lo, true), filter(hi)]);
  const windows = [];
  for (let i = 0; i + frame <= samples.length; i += frame) {
    const power = Array(bands.length).fill(0);
    let total = 0, peak = 0;
    for (let j = i; j < i + frame; j++) {
      const x = samples[j];
      total += x * x;
      peak = Math.max(peak, x * x);
      bands.forEach(([high, low], k) => { power[k] += low(high(x)) ** 2; });
    }
    windows.push({ power, rmsDb: db(total / frame), crest: ratio(peak, total / frame) });
  }
  return windows;
}

const pitchLowPass = filter(1900);
const stride = 4;
const pitchSignal = [];
for (let i = 0; i < raw.length; i++) {
  const x = pitchLowPass(raw[i]);
  if (i % stride === 0) pitchSignal.push(x);
}
function periodicity(window) {
  const sampleRate = rate / stride;
  const width = Math.round(sampleRate * 0.025);
  const start = Math.round((window + 0.5) * frame / stride - width / 2);
  const minLag = Math.ceil(sampleRate / 360);
  const maxLag = Math.floor(sampleRate / 80);
  if (start < 0 || start + width + maxLag >= pitchSignal.length) return 0;
  let best = 0;
  for (let lag = minLag; lag <= maxLag; lag++) {
    let cross = 0, a = 0, b = 0;
    for (let i = start; i < start + width; i++) {
      const x = pitchSignal[i], y = pitchSignal[i + lag];
      cross += x * y; a += x * x; b += y * y;
    }
    best = Math.max(best, cross / Math.sqrt(Math.max(a * b, 1e-30)));
  }
  return best;
}

const initialTrace = readWav(join(root, "traces", traceNames[0], "limiter.wav")).samples;
const windows = measure(raw.subarray(0, initialTrace.length));
const voiced = windows.map((w, i) => w.rmsDb > -45 && periodicity(i) > 0.65);
const vowels = windows.flatMap((_, i) => voiced[i - 1] && voiced[i] && voiced[i + 1] ? [i] : []);
const consonants = windows.flatMap((w, i) => {
  if (w.rmsDb < -56 || ratio(w.power[4], w.power[3]) < -12 || periodicity(i) >= 0.55) return [];
  const nearby = vowels.filter((v) => Math.abs(v - i) <= 6);
  if (!nearby.length) return [];
  const vowel = nearby.reduce((a, b) => Math.abs(a - i) <= Math.abs(b - i) ? a : b);
  return [[i, vowel]];
});
assert(vowels.length && consonants.length, "too few voiced or unvoiced proxy windows");

const output = ["render\tconsonant_2_5k_vs_500_2k_db\tconsonant_vs_adjacent_vowel_2_5k_db"
  + "\tconsonant_crest_db\tvowel_120_300_vs_700_2k_db\tvowel_300_700_vs_700_2k_db"
  + "\tconsonant_5_9k_median_dbfs\tconsonant_5_9k_p90_dbfs"];
const row = (name, samples) => {
  const w = measure(samples);
  const c = consonants.filter(([i, j]) => i < w.length && j < w.length);
  const v = vowels.filter((i) => i < w.length);
  const highs = c.map(([i]) => db(w[i].power[5] / frame)).sort((a, b) => a - b);
  const values = [
    median(c.map(([i]) => ratio(w[i].power[4], w[i].power[3]))),
    median(c.map(([i, j]) => ratio(w[i].power[4], w[j].power[4]))),
    median(c.map(([i]) => w[i].crest)),
    median(v.map((i) => ratio(w[i].power[0], w[i].power[2]))),
    median(v.map((i) => ratio(w[i].power[1], w[i].power[2]))),
    median(highs),
    highs[Math.ceil(highs.length * 0.9) - 1],
  ];
  output.push([name, ...values.map((x) => x.toFixed(3))].join("\t"));
};

row("raw", raw);
const activity = ["render\tstage\tconsonant_samples_reduced_pct\tvowel_samples_reduced_pct"];
for (const name of traceNames) {
  const folder = join(root, "traces", name);
  for (const stage of stages) row(name + "/" + stage, readWav(join(folder, stage + ".wav")).samples);
  const lines = readFileSync(join(folder, "gains.tsv"), "utf8").trim().split(/\r?\n/);
  const headers = lines.shift().split("\t");
  const gains = lines.map((line) => line.split("\t").map(Number));
  for (const stage of ["click", "pop_blend", "compressor", "deess", "limiter"]) {
    const col = headers.indexOf(stage + "_reduced_pct");
    assert(col >= 0);
    const average = (indices) => indices.reduce((sum, i) => sum + gains[i][col], 0) / indices.length;
    activity.push([name, stage, average(consonants.map(([i]) => i)).toFixed(2), average(vowels).toFixed(2)].join("\t"));
  }
}
const delay = raw.length - initialTrace.length;
for (const name of readdirSync(join(root, "listening")).filter((x) => /^(fresh|warm)-[fs].*\.wav$/.test(x))) {
  row(name, readWav(join(root, "listening", name)).samples.subarray(delay));
}
writeFileSync(join(root, "clarity.tsv"), output.join("\n") + "\n");
writeFileSync(join(root, "gain-activity.tsv"), activity.join("\n") + "\n");
writeFileSync(join(root, "windows.tsv"), [
  "type\tstart_ms\tadjacent_voiced_start_ms",
  ...vowels.map((i) => "vowel_proxy\t" + (i * frame * 1000 / rate).toFixed(2)),
  ...consonants.map(([i, j]) => "consonant_proxy\t" + (i * frame * 1000 / rate).toFixed(2)
    + "\t" + (j * frame * 1000 / rate).toFixed(2)),
].join("\n") + "\n");
console.log(vowels.length + " vowel-core proxies, " + consonants.length + " consonant proxies");
