//! Pillow's resampler, ported: `libImaging/Resample.c` for 8-bit channels
//! with the Lanczos filter. The alpha the old service made was a mask
//! Pillow had resized, so this does exactly what Pillow does: the same
//! coefficient table (22-bit fixed point, rounded the same way), the same
//! horizontal-then-vertical pass order, the same 8-bit intermediate and the
//! same clipping. The output matches Pillow's to the bit.

const PRECISION_BITS: i32 = 32 - 8 - 2;
const LANCZOS_SUPPORT: f64 = 3.0;

#[inline]
fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

#[inline]
fn lanczos(x: f64) -> f64 {
    if (-3.0..3.0).contains(&x) {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// One axis of Pillow's `precompute_coeffs` + `normalize_coeffs_8bpc`.
/// Returns, per output index, the first input index and its integer taps.
fn coefficients(in_size: usize, out_size: usize) -> (Vec<usize>, usize, Vec<i32>) {
    let scale = in_size as f64 / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = LANCZOS_SUPPORT * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let ss = 1.0 / filterscale;
    let mut bounds = vec![0usize; out_size];
    let mut kk = vec![0i32; out_size * ksize];
    let mut taps = vec![0f64; ksize];
    for xx in 0..out_size {
        let center = (xx as f64 + 0.5) * scale;
        let xmin = ((center - support + 0.5) as i64).max(0) as usize;
        let xmax = ((center + support + 0.5) as i64).min(in_size as i64) as usize - xmin;
        let mut ww = 0.0;
        for (x, tap) in taps.iter_mut().enumerate().take(xmax) {
            let w = lanczos((x as f64 + xmin as f64 - center + 0.5) * ss);
            *tap = w;
            ww += w;
        }
        if ww != 0.0 {
            for tap in taps.iter_mut().take(xmax) {
                *tap /= ww;
            }
        }
        for tap in taps.iter_mut().skip(xmax) {
            *tap = 0.0;
        }
        bounds[xx] = xmin;
        let row = &mut kk[xx * ksize..(xx + 1) * ksize];
        for (slot, &tap) in row.iter_mut().zip(taps.iter()) {
            let k = tap * (1i64 << PRECISION_BITS) as f64;
            *slot = if k < 0.0 {
                (-0.5 + k) as i32
            } else {
                (0.5 + k) as i32
            };
        }
    }
    (bounds, ksize, kk)
}

#[inline]
fn clip8(v: i64) -> u8 {
    (v >> PRECISION_BITS).clamp(0, 255) as u8
}

/// Resize an interleaved 8-bit image with `channels` channels the way
/// Pillow's `Image.resize(..., Image.LANCZOS)` does.
pub fn resize_lanczos(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    channels: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    assert_eq!(src.len(), src_w * src_h * channels, "source buffer size");
    let init = 1i64 << (PRECISION_BITS - 1);

    // Horizontal pass, into a temp of dst_w by src_h.
    let horizontal: Vec<u8> = if dst_w != src_w {
        let (bounds, ksize, kk) = coefficients(src_w, dst_w);
        let mut out = vec![0u8; dst_w * src_h * channels];
        for y in 0..src_h {
            let row = &src[y * src_w * channels..(y + 1) * src_w * channels];
            for xx in 0..dst_w {
                let xmin = bounds[xx];
                let k = &kk[xx * ksize..(xx + 1) * ksize];
                for c in 0..channels {
                    let mut ss = init;
                    for (x, &w) in k.iter().enumerate() {
                        if w == 0 {
                            continue;
                        }
                        let idx = xmin + x;
                        if idx >= src_w {
                            break;
                        }
                        ss += row[idx * channels + c] as i64 * w as i64;
                    }
                    out[(y * dst_w + xx) * channels + c] = clip8(ss);
                }
            }
        }
        out
    } else {
        src.to_vec()
    };

    if dst_h == src_h {
        return horizontal;
    }

    // Vertical pass, from the temp into the destination.
    let (bounds, ksize, kk) = coefficients(src_h, dst_h);
    let mut out = vec![0u8; dst_w * dst_h * channels];
    let stride = dst_w * channels;
    for yy in 0..dst_h {
        let ymin = bounds[yy];
        let k = &kk[yy * ksize..(yy + 1) * ksize];
        for xx in 0..stride {
            let mut ss = init;
            for (y, &w) in k.iter().enumerate() {
                if w == 0 {
                    continue;
                }
                let idx = ymin + y;
                if idx >= src_h {
                    break;
                }
                ss += horizontal[idx * stride + xx] as i64 * w as i64;
            }
            out[yy * stride + xx] = clip8(ss);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanczos_kernel_is_pillows() {
        assert_eq!(lanczos(0.0), 1.0);
        assert!(lanczos(3.0).abs() < 1e-12);
        assert!((lanczos(1.0)).abs() < 1e-12);
        assert!(lanczos(0.5) > 0.6 && lanczos(0.5) < 0.7);
    }

    #[test]
    fn identity_is_untouched() {
        let src: Vec<u8> = (0..=255).collect();
        assert_eq!(resize_lanczos(&src, 16, 16, 1, 16, 16), src);
    }

    #[test]
    fn coefficients_sum_to_one() {
        let (_, ksize, kk) = coefficients(1200, 1024);
        for xx in 0..1024 {
            let sum: i64 = kk[xx * ksize..(xx + 1) * ksize]
                .iter()
                .map(|&k| k as i64)
                .sum();
            let one = 1i64 << PRECISION_BITS;
            assert!(
                (sum - one).abs() <= ksize as i64,
                "row {xx} sums to {sum}, expected about {one}"
            );
        }
    }

    #[test]
    fn flat_stays_flat_both_ways() {
        let src = vec![77u8; 30 * 50 * 3];
        let down = resize_lanczos(&src, 30, 50, 3, 12, 20);
        assert!(down.iter().all(|&v| v == 77));
        let up = resize_lanczos(&src, 30, 50, 3, 61, 99);
        assert!(up.iter().all(|&v| v == 77));
    }
}
