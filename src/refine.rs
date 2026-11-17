//! Edge colour clean-up, for cutouts that will sit on a new background.
//!
//! A pixel on the edge of the subject is a mix: some of the subject's colour
//! and some of the old background's. The mask says how much of each
//! (alpha), but the colour stored is still the mix, so on a new background
//! the old one shows as a halo around hair and soft edges.
//!
//! This estimates the subject's own colour for those pixels with the blur
//! fusion of Forte and Pitié, "Approximate Fast Foreground Colour
//! Estimation" (ICIP 2021), run twice as its authors suggest: a wide pass
//! for the colours either side of the edge, then a narrow one for detail.
//!
//! ```text
//! F' = blur(F·α) / blur(α)            the subject's colour near here
//! B' = blur(B·(1−α)) / blur(1−α)      the background's colour near here
//! F  = F' + α·(I − α·F' − (1−α)·B')   what is left of the pixel once the
//!                                     background's share is taken out
//! ```
//!
//! The blurred estimates are computed on a copy no larger than the model's
//! side: the mask carries no more detail than that, and it keeps the work
//! and the memory small whatever the picture's size. The last line runs at
//! full size against the real pixels, so the picture's own detail stays.
//! Fully opaque and fully transparent pixels are left exactly as they were.

use image::RgbImage;

use crate::imageops::SIDE;
use crate::resample::resize_lanczos;

/// Box widths of the two passes, for a working copy of `SIDE` pixels on its
/// long side; scaled down with smaller pictures.
const WIDE: f32 = 90.0;
const NARROW: f32 = 6.0;
const EPS: f32 = 1e-5;

/// Replace the colour of every partly transparent pixel of `rgb` with an
/// estimate of the subject's own colour there. `alpha` is the mask at the
/// picture's size.
pub fn decontaminate(rgb: &mut RgbImage, alpha: &[u8]) {
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    assert_eq!(alpha.len(), w * h, "mask size");
    if w == 0 || h == 0 || !alpha.iter().any(|&a| a > 0 && a < 255) {
        return;
    }

    // The working copy: the picture and the mask, at most SIDE on a side.
    let long = w.max(h);
    let side = SIDE as usize;
    let (lw, lh) = if long > side {
        (
            (w * side).div_ceil(long).max(1),
            (h * side).div_ceil(long).max(1),
        )
    } else {
        (w, h)
    };
    let (small_rgb, small_a) = if (lw, lh) == (w, h) {
        (rgb.as_raw().clone(), alpha.to_vec())
    } else {
        (
            resize_lanczos(rgb.as_raw(), w, h, 3, lw, lh),
            resize_lanczos(alpha, w, h, 1, lw, lh),
        )
    };
    let n = lw * lh;
    let a: Vec<f32> = small_a.iter().map(|&v| v as f32 / 255.0).collect();
    let scale = lw.max(lh) as f32 / side as f32;
    let radius = |width: f32| ((width * scale / 2.0).round() as usize).max(1);

    // Pass one, wide: F and B both start as the picture itself.
    let near_a = box_blur(&a, lw, lh, radius(WIDE));
    // Pass two, narrow, only as far as its blurred estimates: the fusion
    // itself happens at full size below.
    let near_a2 = box_blur(&a, lw, lh, radius(NARROW));
    let mut fg = [vec![0f32; n], vec![0f32; n], vec![0f32; n]];
    let mut bg = [vec![0f32; n], vec![0f32; n], vec![0f32; n]];
    let mut scratch = vec![0f32; n];
    for c in 0..3 {
        let img: Vec<f32> = (0..n)
            .map(|i| small_rgb[i * 3 + c] as f32 / 255.0)
            .collect();

        for i in 0..n {
            scratch[i] = img[i] * a[i];
        }
        let near_f = box_blur(&scratch, lw, lh, radius(WIDE));
        for i in 0..n {
            scratch[i] = img[i] * (1.0 - a[i]);
        }
        let near_b = box_blur(&scratch, lw, lh, radius(WIDE));

        // F1 and B1 of the wide pass, weighted for the narrow blur.
        let mut f1a = vec![0f32; n];
        let mut b1a = vec![0f32; n];
        for i in 0..n {
            let f = near_f[i] / (near_a[i] + EPS);
            let b = near_b[i] / (1.0 - near_a[i] + EPS);
            let f1 = (f + a[i] * (img[i] - a[i] * f - (1.0 - a[i]) * b)).clamp(0.0, 1.0);
            f1a[i] = f1 * a[i];
            b1a[i] = b * (1.0 - a[i]);
        }
        let near_f = box_blur(&f1a, lw, lh, radius(NARROW));
        let near_b = box_blur(&b1a, lw, lh, radius(NARROW));
        for i in 0..n {
            fg[c][i] = near_f[i] / (near_a2[i] + EPS);
            bg[c][i] = near_b[i] / (1.0 - near_a2[i] + EPS);
        }
    }

    // The fusion at full size, on the edge pixels only.
    let (sx, sy) = (lw as f32 / w as f32, lh as f32 / h as f32);
    let raw: &mut [u8] = rgb;
    for y in 0..h {
        let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, (lh - 1) as f32);
        let y0 = fy as usize;
        let y1 = (y0 + 1).min(lh - 1);
        let ty = fy - y0 as f32;
        for x in 0..w {
            let av = alpha[y * w + x];
            if av == 0 || av == 255 {
                continue;
            }
            let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, (lw - 1) as f32);
            let x0 = fx as usize;
            let x1 = (x0 + 1).min(lw - 1);
            let tx = fx - x0 as f32;
            let sample = |plane: &[f32]| {
                let top = plane[y0 * lw + x0] * (1.0 - tx) + plane[y0 * lw + x1] * tx;
                let bottom = plane[y1 * lw + x0] * (1.0 - tx) + plane[y1 * lw + x1] * tx;
                top * (1.0 - ty) + bottom * ty
            };
            let af = av as f32 / 255.0;
            let px = &mut raw[(y * w + x) * 3..(y * w + x) * 3 + 3];
            for c in 0..3 {
                let (f, b) = (sample(&fg[c]), sample(&bg[c]));
                let i = px[c] as f32 / 255.0;
                let out = f + af * (i - af * f - (1.0 - af) * b);
                px[c] = (out.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        }
    }
}

/// The mean over a square of `2·radius + 1` on a side, in two passes of
/// running sums. At the border the mean is of the part inside the picture.
fn box_blur(src: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    let mut mid = vec![0f32; w * h];
    let mut prefix = vec![0f64; w.max(h) + 1];
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        for x in 0..w {
            prefix[x + 1] = prefix[x] + row[x] as f64;
        }
        for x in 0..w {
            let (lo, hi) = (x.saturating_sub(radius), (x + radius + 1).min(w));
            mid[y * w + x] = ((prefix[hi] - prefix[lo]) / (hi - lo) as f64) as f32;
        }
    }
    let mut out = vec![0f32; w * h];
    for x in 0..w {
        for y in 0..h {
            prefix[y + 1] = prefix[y] + mid[y * w + x] as f64;
        }
        for y in 0..h {
            let (lo, hi) = (y.saturating_sub(radius), (y + radius + 1).min(h));
            out[y * w + x] = ((prefix[hi] - prefix[lo]) / (hi - lo) as f64) as f32;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_field_blurs_to_itself() {
        let src = vec![0.25f32; 40 * 30];
        let out = box_blur(&src, 40, 30, 7);
        assert!(out.iter().all(|&v| (v - 0.25).abs() < 1e-6));
    }

    #[test]
    fn the_blur_is_a_mean_and_keeps_the_total_in_the_middle() {
        let mut src = vec![0f32; 21 * 21];
        src[10 * 21 + 10] = 1.0;
        let out = box_blur(&src, 21, 21, 2);
        assert!((out[10 * 21 + 10] - 1.0 / 25.0).abs() < 1e-6);
        assert_eq!(out[0], 0.0);
    }

    /// A red subject on a green background with a soft edge between them,
    /// a few pixels wide as a mask's edges are. The edge pixels are a mix of
    /// red and green; afterwards they must be close to red, and the pixels
    /// that were fully in or fully out must be untouched.
    #[test]
    fn a_soft_edge_loses_the_background_colour() {
        let (w, h) = (256u32, 64u32);
        let mut img = RgbImage::new(w, h);
        let mut alpha = vec![0u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                // Opaque left of 120, transparent right of 128, a ramp between.
                let a = ((128.0 - x as f32) / 8.0).clamp(0.0, 1.0);
                alpha[(y * w + x) as usize] = (a * 255.0).round() as u8;
                let r = (a * 255.0).round() as u8;
                img.put_pixel(x, y, image::Rgb([r, 255 - r, 0]));
            }
        }
        let before = img.clone();
        decontaminate(&mut img, &alpha);
        for y in 0..h {
            for x in 0..w {
                let a = alpha[(y * w + x) as usize];
                let px = img.get_pixel(x, y).0;
                if a == 0 || a == 255 {
                    assert_eq!(px, before.get_pixel(x, y).0, "untouched at {x},{y}");
                } else if a >= 64 {
                    // A visible edge pixel: mostly red, little green left.
                    assert!(px[0] > 200, "red at {x},{y}: {px:?} (alpha {a})");
                    assert!(px[1] < 60, "green at {x},{y}: {px:?} (alpha {a})");
                }
            }
        }
    }

    #[test]
    fn nothing_to_do_is_nothing_done() {
        let mut img = RgbImage::from_pixel(8, 8, image::Rgb([10, 20, 30]));
        let before = img.clone();
        let mut alpha = vec![255u8; 64];
        alpha[..32].fill(0);
        decontaminate(&mut img, &alpha);
        assert_eq!(img, before);
    }

    #[test]
    fn a_large_picture_works_on_a_reduced_copy() {
        // 1500 wide: the working copy is reduced; edges must still be cleaned.
        let (w, h) = (1500u32, 40u32);
        let mut img = RgbImage::new(w, h);
        let mut alpha = vec![0u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let a = ((756.0 - x as f32) / 12.0).clamp(0.0, 1.0);
                alpha[(y * w + x) as usize] = (a * 255.0).round() as u8;
                let r = (a * 255.0).round() as u8;
                img.put_pixel(x, y, image::Rgb([r, 0, 255 - r]));
            }
        }
        decontaminate(&mut img, &alpha);
        let px = img.get_pixel(750, 20).0; // alpha one half
        assert!(px[0] > 200 && px[2] < 60, "{px:?}");
    }
}
