// Controlled stationary noise during real speech. First run the release
// renderer to obtain its 0-raw.wav. This probe uses its 48 kHz mono format.
// node examples/noise-probe.mjs <0-raw.wav> <output-directory>
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve, sep } from "node:path";

const [, , input, destination] = process.argv;
assert(input && destination, "usage: noise-probe.mjs <0-raw.wav> <output-directory>");
mkdirSync(destination, { recursive: true });
const out = realpathSync(destination);
const renderer = resolve("target/release/examples/render.exe");
const read = (file) => {
  const bytes = readFileSync(file);
  assert.equal(bytes.toString("ascii", 0, 4), "RIFF");
  let data = 0, size = 0;
  for (let p = 12; p + 8 <= bytes.length;) {
    const id = bytes.toString("ascii", p, p + 4);
    const n = bytes.readUInt32LE(p + 4);
    if (id === "fmt ") {
      assert.equal(bytes.readUInt16LE(p + 10), 1, "mono WAV required");
      assert.equal(bytes.readUInt32LE(p + 12), 48000, "48 kHz WAV required");
      assert.equal(bytes.readUInt16LE(p + 22), 32, "float WAV required");
      const format = bytes.readUInt16LE(p + 8);
      assert(format === 3 || (format === 0xfffe && bytes.readUInt32LE(p + 32) === 3), "float WAV required");
    }
    if (id === "data") { data = p + 8; size = n; break; }
    p += 8 + n + (n % 2);
  }
  assert(data && size && data + size <= bytes.length, "invalid WAV data chunk");
  const samples = Float32Array.from({ length: size / 4 }, (_, i) => bytes.readFloatLE(data + i * 4));
  return { bytes, data, samples };
};
const raw = read(input);
// Verified by `analyze` for the 48 kHz suppression-only path (no EQ or leveler).
const delay = 2471;
const win = 4800;
const speech = [];
for (let i = 0; i + win + delay <= raw.samples.length; i += win) {
  const power = raw.samples.subarray(i, i + win).reduce((sum, x) => sum + x * x, 0) / win;
  if (power > 10 ** (-45 / 10)) speech.push(i);
}
assert(speech.length, "no speech windows above -45 dBFS");

const variants = ["1-baseline-noise-only", "2-model-plus-6dB", "5-clean-backup-gentle",
  "6-clean-backup-medium", "7-clean-backup-strong", "11-conditioned-gentle",
  "12-conditioned-medium", "13-conditioned-strong"];
const report = [];
for (const kind of ["white", "low-passed"]) {
  for (const seed of [0x47c11a9, 0x5a791f3]) {
    let state = seed, smoothed = 0;
    const probe = Float32Array.from(raw.samples, () => {
      state ^= state << 13; state ^= state >>> 17; state ^= state << 5;
      const white = (state >>> 0) / 0x80000000 - 1;
      smoothed = 0.85 * smoothed + 0.15 * white;
      return kind === "white" ? white : smoothed;
    });
    const gain = 10 ** (-55 / 20) / Math.sqrt(probe.reduce((sum, x) => sum + x * x, 0) / probe.length);
    for (let i = 0; i < probe.length; i++) probe[i] *= gain;
    const work = mkdtempSync(join(out, "probe-work-"));
    try {
      for (const sign of [1, -1]) {
        const mix = Buffer.from(raw.bytes);
        for (let i = 0; i < probe.length; i++) mix.writeFloatLE(raw.samples[i] + sign * probe[i], raw.data + i * 4);
        const file = join(work, `${sign}.wav`);
        writeFileSync(file, mix);
        execFileSync(renderer, [file, join(work, `${sign}`), "experiments"], { stdio: "ignore" });
      }
      for (const variant of variants) {
        const plus = read(join(work, "1", `${variant}.wav`)).samples;
        const minus = read(join(work, "-1", `${variant}.wav`)).samples;
        let cross = 0, power = 0, difference = 0;
        for (const start of speech) for (let i = start; i < start + win; i++) {
          const delta = (plus[i + delay] - minus[i + delay]) * 0.5;
          cross += delta * probe[i];
          power += probe[i] * probe[i];
          difference += delta * delta;
        }
        report.push({ kind, seed, variant, speechWindows: speech.length,
          coherentLeakageDb: 20 * Math.log10(Math.abs(cross / power)),
          totalResponseDb: 10 * Math.log10(difference / power) });
        if (kind === "low-passed" && seed === 0x47c11a9) {
          writeFileSync(join(out, `${variant}-with-noise.wav`), readFileSync(join(work, "1", `${variant}.wav`)));
        }
      }
      console.log(`Measured ${kind} noise, seed ${seed.toString(16)}`);
    } finally {
      const checked = realpathSync(work);
      assert(checked.startsWith(join(out, "probe-work-").replaceAll("/", sep)), "temporary path escaped output directory");
      rmSync(checked, { recursive: true });
    }
  }
}
writeFileSync(join(out, "noise-probe.json"), JSON.stringify(report, null, 2) + "\n");
