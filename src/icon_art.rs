// Hush app icon, drawn procedurally so the same artwork serves the
// window, the tray, the .exe resource (via build.rs) and the installer at
// any size. White voice bars settling to a dot ("hush") on a teal-blue
// rounded square with a diagonal gradient.
//
// Shared with build.rs via `include!`, so keep it dependency-free.

#[allow(dead_code)]
pub fn render_icon(size: u32) -> Vec<u8> {
    render_icon_state(size, true)
}

/// `active == false` draws the tray "off" state: the same mark on grey.
#[allow(dead_code)]
pub fn render_icon_state(size: u32, active: bool) -> Vec<u8> {
    let n = size as f32;
    let mut rgba = vec![0_u8; (size * size * 4) as usize];

    fn rounded_box(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
        let qx = (px - cx).abs() - (hw - r);
        let qy = (py - cy).abs() - (hh - r);
        let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
        outside + qx.max(qy).min(0.0) - r
    }
    fn segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
        let (pax, pay, bax, bay) = (px - ax, py - ay, bx - ax, by - ay);
        let len = bax * bax + bay * bay;
        let h = if len > 0.0 {
            ((pax * bax + pay * bay) / len).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ((pax - bax * h).powi(2) + (pay - bay * h).powi(2)).sqrt()
    }
    fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ]
    }

    // Voice bars: (full height, opacity). Small icons get fewer, wider bars
    // so they stay crisp at 16 px.
    let (bars, width, gap): (&[(f32, f32)], f32, f32) = if size < 24 {
        (&[(0.44, 1.0), (0.22, 0.92), (0.0, 0.75)], 0.15, 0.085)
    } else {
        (
            &[(0.30, 1.0), (0.50, 1.0), (0.32, 0.95), (0.14, 0.85), (0.0, 0.7)],
            0.09,
            0.058,
        )
    };
    let total = bars.len() as f32 * width + (bars.len() - 1) as f32 * gap;
    let left = 0.5 - total / 2.0 + width / 2.0;

    let (top, bottom) = if active {
        ([56.0, 220.0, 232.0], [10.0, 104.0, 168.0])
    } else {
        ([150.0, 156.0, 166.0], [82.0, 88.0, 100.0])
    };

    for y in 0..size {
        for x in 0..size {
            let (u, v) = ((x as f32 + 0.5) / n, (y as f32 + 0.5) / n);
            let aa = 1.0 / n; // one pixel, for anti-aliasing

            let bg = rounded_box(u, v, 0.5, 0.5, 0.47, 0.47, 0.23);
            let bg_cov = (0.5 - bg / aa).clamp(0.0, 1.0);
            // Diagonal gradient, light top-left to deep bottom-right.
            let mut color = mix(top, bottom, ((u + v) * 0.5).clamp(0.0, 1.0));
            // Soft sheen across the top third.
            let sheen = (0.35 - v).max(0.0) / 0.35 * 0.10;
            color = mix(color, [255.0; 3], sheen);

            let mut glyph = 0.0_f32;
            for (i, &(height, alpha)) in bars.iter().enumerate() {
                let cx = left + i as f32 * (width + gap);
                let half = height / 2.0;
                let d = segment(u, v, cx, 0.5 - half, cx, 0.5 + half) - width / 2.0;
                glyph = glyph.max((0.5 - d / aa).clamp(0.0, 1.0) * alpha);
            }
            color = mix(color, [255.0; 3], glyph);

            let i = ((y * size + x) * 4) as usize;
            rgba[i] = color[0].round() as u8;
            rgba[i + 1] = color[1].round() as u8;
            rgba[i + 2] = color[2].round() as u8;
            rgba[i + 3] = (bg_cov * 255.0).round() as u8;
        }
    }
    rgba
}

/// Builds a .ico file (32-bit BMP entries) containing the given sizes.
#[allow(dead_code)]
pub fn build_ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = sizes
        .iter()
        .map(|&size| {
            let rgba = render_icon(size);
            let mut dib = Vec::new();
            // BITMAPINFOHEADER (height doubled: XOR image + AND mask).
            dib.extend_from_slice(&40_u32.to_le_bytes());
            dib.extend_from_slice(&(size as i32).to_le_bytes());
            dib.extend_from_slice(&((size * 2) as i32).to_le_bytes());
            dib.extend_from_slice(&1_u16.to_le_bytes());
            dib.extend_from_slice(&32_u16.to_le_bytes());
            dib.extend_from_slice(&[0_u8; 24]);
            // BGRA rows, bottom-up.
            for y in (0..size).rev() {
                for x in 0..size {
                    let i = ((y * size + x) * 4) as usize;
                    dib.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
                }
            }
            // AND mask: all zero (alpha channel is used), rows padded to 4 bytes.
            let mask_row = (size.div_ceil(32) * 4) as usize;
            dib.extend(std::iter::repeat_n(0_u8, mask_row * size as usize));
            (size, dib)
        })
        .collect();

    let mut ico = Vec::new();
    ico.extend_from_slice(&0_u16.to_le_bytes());
    ico.extend_from_slice(&1_u16.to_le_bytes());
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, dib) in &images {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0]);
        ico.extend_from_slice(&1_u16.to_le_bytes());
        ico.extend_from_slice(&32_u16.to_le_bytes());
        ico.extend_from_slice(&(dib.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += dib.len() as u32;
    }
    for (_, dib) in images {
        ico.extend_from_slice(&dib);
    }
    ico
}
