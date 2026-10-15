//! BGRA -> NV12 (BT.709, limited range) conversion with optional 2x downscale and cursor overlay.
//! Used by the Windows host, where Desktop Duplication hands us BGRA frames. Kept platform-neutral
//! so it can be tested anywhere.

#![cfg_attr(not(windows), allow(dead_code))]

use rayon::prelude::*;

#[inline(always)]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16).clamp(16, 235) as u8
}

#[inline(always)]
fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    let u = ((-26 * r - 86 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    (u.clamp(16, 240) as u8, v.clamp(16, 240) as u8)
}

#[inline(always)]
fn px(src: &[u8], stride: usize, x: usize, y: usize, scale: usize) -> (i32, i32, i32) {
    if scale == 1 {
        let o = y * stride + x * 4;
        (src[o + 2] as i32, src[o + 1] as i32, src[o] as i32)
    } else {
        let o0 = (y * 2) * stride + x * 8;
        let o1 = o0 + stride;
        let r = src[o0 + 2] as i32 + src[o0 + 6] as i32 + src[o1 + 2] as i32 + src[o1 + 6] as i32;
        let g = src[o0 + 1] as i32 + src[o0 + 5] as i32 + src[o1 + 1] as i32 + src[o1 + 5] as i32;
        let b = src[o0] as i32 + src[o0 + 4] as i32 + src[o1] as i32 + src[o1 + 4] as i32;
        ((r + 2) >> 2, (g + 2) >> 2, (b + 2) >> 2)
    }
}

/// Converts `pairs` column pairs of two output rows. `ox`/`oy` are output-pixel coordinates in
/// the (downscaled) source.
#[inline(always)]
fn row_pair(src: &[u8], stride: usize, scale: usize, ox: usize, oy: usize, y0: &mut [u8], y1: &mut [u8], uv: &mut [u8], pairs: usize) {
    for i in 0..pairs {
        let x = ox + i * 2;
        let (r00, g00, b00) = px(src, stride, x, oy, scale);
        let (r01, g01, b01) = px(src, stride, x + 1, oy, scale);
        let (r10, g10, b10) = px(src, stride, x, oy + 1, scale);
        let (r11, g11, b11) = px(src, stride, x + 1, oy + 1, scale);
        y0[i * 2] = luma(r00, g00, b00);
        y0[i * 2 + 1] = luma(r01, g01, b01);
        y1[i * 2] = luma(r10, g10, b10);
        y1[i * 2 + 1] = luma(r11, g11, b11);
        let (u, v) = chroma((r00 + r01 + r10 + r11 + 2) >> 2, (g00 + g01 + g10 + g11 + 2) >> 2, (b00 + b01 + b10 + b11 + 2) >> 2);
        uv[i * 2] = u;
        uv[i * 2 + 1] = v;
    }
}

/// Output size for a source of `sw` x `sh` at `scale` (1 or 2): even dimensions.
pub fn output_size(sw: usize, sh: usize, scale: usize) -> (usize, usize) {
    ((sw / scale) & !1, (sh / scale) & !1)
}

/// Converts a whole frame. `dst` holds the Y plane (`w*h`) followed by interleaved UV (`w*h/2`).
pub fn bgra_to_nv12(src: &[u8], stride: usize, sw: usize, sh: usize, scale: usize, dst: &mut [u8]) {
    let (w, h) = output_size(sw, sh, scale);
    let (ys, uvs) = dst[..w * h * 3 / 2].split_at_mut(w * h);
    ys.par_chunks_mut(w * 2).zip(uvs.par_chunks_mut(w)).enumerate().for_each(|(i, (yy, uv))| {
        let (y0, y1) = yy.split_at_mut(w);
        row_pair(src, stride, scale, 0, i * 2, y0, y1, uv, w / 2);
    });
}

/// A mouse pointer image: straight-alpha BGRA plus an "invert" flag per pixel.
#[derive(Clone, Default)]
pub struct CursorImage {
    pub w: usize,
    pub h: usize,
    pub bgra: Vec<u8>,
    pub xor: Vec<bool>,
}

/// Builds a cursor image from a DXGI pointer shape (type 1 monochrome, 2 color, 4 masked color).
pub fn cursor_from_dxgi(kind: u32, w: usize, h_raw: usize, pitch: usize, data: &[u8]) -> Option<CursorImage> {
    let h = if kind == 1 { h_raw / 2 } else { h_raw };
    if w == 0 || h == 0 {
        return None;
    }
    let mut c = CursorImage { w, h, bgra: vec![0; w * h * 4], xor: vec![false; w * h] };
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let o = &mut c.bgra[i * 4..i * 4 + 4];
            match kind {
                1 => {
                    let bit = |row: usize| -> Option<bool> {
                        let b = *data.get(row * pitch + x / 8)?;
                        Some(b & (0x80 >> (x % 8)) != 0)
                    };
                    let (and, xr) = (bit(y)?, bit(y + h)?);
                    match (and, xr) {
                        (true, false) => {}
                        (false, false) => o.copy_from_slice(&[0, 0, 0, 255]),
                        (false, true) => o.copy_from_slice(&[255, 255, 255, 255]),
                        (true, true) => {
                            o.copy_from_slice(&[255, 255, 255, 255]);
                            c.xor[i] = true;
                        }
                    }
                }
                2 | 4 => {
                    let s = data.get(y * pitch + x * 4..y * pitch + x * 4 + 4)?;
                    o.copy_from_slice(s);
                    if kind == 4 {
                        if s[3] == 0xFF {
                            c.xor[i] = true;
                        }
                        o[3] = 255;
                    }
                }
                _ => return None,
            }
        }
    }
    Some(c)
}

/// Draws the cursor (top-left at `cx`, `cy`) into a BGRA buffer.
pub fn blend_cursor(dst: &mut [u8], stride: usize, dw: usize, dh: usize, c: &CursorImage, cx: i32, cy: i32) {
    for y in 0..c.h {
        let ty = cy + y as i32;
        if ty < 0 || ty >= dh as i32 {
            continue;
        }
        for x in 0..c.w {
            let tx = cx + x as i32;
            if tx < 0 || tx >= dw as i32 {
                continue;
            }
            let i = y * c.w + x;
            let s = &c.bgra[i * 4..i * 4 + 4];
            let o = ty as usize * stride + tx as usize * 4;
            let d = &mut dst[o..o + 4];
            if c.xor[i] {
                d[0] ^= s[0];
                d[1] ^= s[1];
                d[2] ^= s[2];
            } else if s[3] == 255 {
                d[..3].copy_from_slice(&s[..3]);
            } else if s[3] > 0 {
                let a = s[3] as u32;
                for k in 0..3 {
                    d[k] = ((s[k] as u32 * a + d[k] as u32 * (255 - a) + 127) / 255) as u8;
                }
            }
        }
    }
}

/// Converts a frame and draws the cursor on top, touching only the pixels under the cursor.
#[allow(clippy::too_many_arguments)]
pub fn convert_with_cursor(
    src: &[u8],
    stride: usize,
    sw: usize,
    sh: usize,
    scale: usize,
    dst: &mut [u8],
    cursor: Option<(&CursorImage, i32, i32)>,
    patch: &mut Vec<u8>,
) {
    bgra_to_nv12(src, stride, sw, sh, scale, dst);
    let Some((c, cx, cy)) = cursor else { return };
    let (w, h) = output_size(sw, sh, scale);
    let align = (2 * scale) as i32;
    // Patch rectangle in source pixels, aligned so it maps onto whole chroma blocks.
    let x0 = (cx.max(0) / align) * align;
    let y0 = (cy.max(0) / align) * align;
    let x1 = ((cx + c.w as i32 + align - 1) / align * align).min((w * scale) as i32);
    let y1 = ((cy + c.h as i32 + align - 1) / align * align).min((h * scale) as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let (pw, ph) = ((x1 - x0) as usize, (y1 - y0) as usize);
    patch.clear();
    patch.resize(pw * ph * 4, 0);
    for r in 0..ph {
        let s = (y0 as usize + r) * stride + x0 as usize * 4;
        patch[r * pw * 4..(r + 1) * pw * 4].copy_from_slice(&src[s..s + pw * 4]);
    }
    blend_cursor(patch, pw * 4, pw, ph, c, cx - x0, cy - y0);
    let (ys, uvs) = dst[..w * h * 3 / 2].split_at_mut(w * h);
    let (ox, oy) = (x0 as usize / scale, y0 as usize / scale);
    let (ow, oh) = (pw / scale, ph / scale);
    for i in 0..oh / 2 {
        let row = oy + i * 2;
        let (a, b) = ys.split_at_mut((row + 1) * w);
        let y0r = &mut a[row * w + ox..row * w + ox + ow];
        let y1r = &mut b[ox..ox + ow];
        let uvr = &mut uvs[(row / 2) * w + ox..(row / 2) * w + ox + ow];
        row_pair(patch, pw * 4, scale, 0, i * 2, y0r, y1r, uvr, ow / 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, bgra: [u8; 4]) -> Vec<u8> {
        bgra.iter().copied().cycle().take(w * h * 4).collect()
    }

    #[test]
    fn colors() {
        for (bgra, y, u, v) in [
            ([255, 255, 255, 255], 235, 128, 128),
            ([0, 0, 0, 255], 16, 128, 128),
            ([0, 0, 255, 255], 63, 102, 240), // red
        ] {
            let src = solid(4, 4, bgra);
            let mut dst = vec![0; 24];
            bgra_to_nv12(&src, 16, 4, 4, 1, &mut dst);
            assert!((dst[0] as i32 - y).abs() <= 1, "y {} vs {y}", dst[0]);
            assert!((dst[16] as i32 - u).abs() <= 2, "u {} vs {u}", dst[16]);
            assert!((dst[17] as i32 - v).abs() <= 2, "v {} vs {v}", dst[17]);
        }
    }

    #[test]
    fn downscale_and_cursor() {
        let (w, h) = (64, 48);
        let src = solid(w, h, [0, 0, 0, 255]);
        let mut dst = vec![0; (w / 2) * (h / 2) * 3 / 2];
        let cur = CursorImage { w: 4, h: 4, bgra: solid(4, 4, [255, 255, 255, 255]), xor: vec![false; 16] };
        let mut patch = Vec::new();
        convert_with_cursor(&src, w * 4, w, h, 2, &mut dst, Some((&cur, 10, 6)), &mut patch);
        let ow = w / 2;
        // Cursor covers source (10..14, 6..10) -> output (5..7, 3..5).
        assert_eq!(dst[3 * ow + 5], 235);
        assert_eq!(dst[4 * ow + 6], 235);
        assert_eq!(dst[0], 16);
        assert_eq!(dst[3 * ow + 8], 16);
    }

    #[test]
    fn monochrome_cursor() {
        // 8x2 cursor: row 0 = black, row 1 = transparent.
        let and = [0x00u8, 0xFF];
        let xor = [0x00u8, 0x00];
        let data = [and[0], and[1], xor[0], xor[1]];
        let c = cursor_from_dxgi(1, 8, 4, 1, &data).unwrap();
        assert_eq!((c.w, c.h), (8, 2));
        assert_eq!(&c.bgra[0..4], &[0, 0, 0, 255]);
        assert_eq!(c.bgra[8 * 4 + 3], 0);
    }
}
