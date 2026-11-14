// Writes the icon at a few sizes to a PNG-less BMP sheet for previewing.
// cargo run --example icon_preview -- <out.bmp>
#[path = "../src/icon_art.rs"]
mod icon_art;

fn main() {
    let out = std::env::args().nth(1).expect("output path");
    let sizes = [256_u32, 64, 32, 16];
    let width: u32 = sizes.iter().sum::<u32>() + 10 * sizes.len() as u32 + 256 + 10;
    let height = 256_u32;
    let mut canvas = vec![[40_u8, 40, 40]; (width * height) as usize];
    let mut x0 = 0;
    for (variant, active) in [(0, true), (1, false)] {
        let list: &[u32] = if variant == 0 { &sizes } else { &[256] };
        for &size in list {
            let rgba = icon_art::render_icon_state(size, active);
            for y in 0..size {
                for x in 0..size {
                    let i = ((y * size + x) * 4) as usize;
                    let a = rgba[i + 3] as f32 / 255.0;
                    let px = &mut canvas[(y * width + x0 + x) as usize];
                    for c in 0..3 {
                        px[c] = (rgba[i + c] as f32 * a + px[c] as f32 * (1.0 - a)) as u8;
                    }
                }
            }
            x0 += size + 10;
        }
    }
    // 24-bit BMP, bottom-up rows.
    let row = (width * 3).div_ceil(4) * 4;
    let mut bmp = Vec::new();
    let size = 54 + row * height;
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&size.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54_u32.to_le_bytes());
    bmp.extend_from_slice(&40_u32.to_le_bytes());
    bmp.extend_from_slice(&(width as i32).to_le_bytes());
    bmp.extend_from_slice(&(height as i32).to_le_bytes());
    bmp.extend_from_slice(&1_u16.to_le_bytes());
    bmp.extend_from_slice(&24_u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    for y in (0..height).rev() {
        let mut line = Vec::new();
        for x in 0..width {
            let [r, g, b] = canvas[(y * width + x) as usize];
            line.extend_from_slice(&[b, g, r]);
        }
        line.resize(row as usize, 0);
        bmp.extend_from_slice(&line);
    }
    std::fs::write(out, bmp).unwrap();
}
