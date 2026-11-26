//! Renders deterministic synthetic input for comparison with the released DSP.

use hush_apo::dsp::{CleanMic, MicSettings};
use std::{
    env,
    fs::File,
    io::{self, BufWriter, Write},
};

fn main() -> io::Result<()> {
    let output = env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "expected an output PCM file")
    })?;
    let mut writer = BufWriter::new(File::create(output)?);
    let mut seed = 0x624a_63b7_u32;
    for rate in [48_000, 44_100, 22_050] {
        for enabled in [true, false] {
            let mut dsp = CleanMic::new_inline(rate, 1);
            let settings = MicSettings {
                enabled,
                ..MicSettings::default()
            };
            let mut samples: Vec<f32> = (0..rate)
                .map(|index| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = (seed >> 21) as f32 / 32_768.0 - 0.03125;
                    let phase = index % (rate / 170);
                    let voice = phase as f32 / (rate / 170) as f32 - 0.5;
                    noise
                        + if index / (rate / 5) % 2 == 0 {
                            voice * 0.25
                        } else {
                            0.0
                        }
                })
                .collect();
            for buffer in samples.chunks_mut((rate / 100) as usize) {
                dsp.process(buffer, &settings);
            }
            for sample in samples {
                writer.write_all(&sample.to_le_bytes())?;
            }
        }
    }
    writer.flush()
}
