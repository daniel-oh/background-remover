//! Facts about a cutout, so a caller can judge or explain it without
//! decoding the image: how much of the picture is subject, in how many
//! pieces, how much of it is see-through, and where it is.
//!
//! They are measured on a copy of the mask no larger than [`GRID`] on its
//! long side: shares and fractions do not depend on the size, and a 24
//! megapixel mask need not be walked pixel by pixel.

/// The long side of the grid the facts are measured on.
pub const GRID: usize = 512;

/// Alpha above this is subject (one half).
const SOLID: u8 = 127;
/// A piece smaller than this share of the picture is not counted.
const MIN_PART: f64 = 0.005;

/// What a mask says about the cutout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Facts {
    /// Share of the picture that is subject (alpha above one half), 0 to 1.
    pub coverage: f64,
    /// Separate pieces of subject, each at least half a percent of the
    /// picture.
    pub parts: u32,
    /// Share of the picture that is partly see-through (alpha between 0.1
    /// and 0.9): high for hair, glass and smoke.
    pub soft: f64,
    /// The subject's box as fractions of the picture, `(x, y, w, h)`; `None`
    /// when there is no subject.
    pub bbox: Option<(f64, f64, f64, f64)>,
}

impl Facts {
    /// A result that is probably wrong: almost nothing kept, almost
    /// everything kept, or the subject in many pieces. The thresholds are
    /// the ones the caller who asked for `model=auto` proposed (#34).
    pub fn looks_wrong(&self) -> bool {
        self.coverage < 0.02 || self.coverage > 0.85 || self.parts > 5
    }
}

/// Measure a mask of `w` by `h`.
pub fn facts(alpha: &[u8], w: usize, h: usize) -> Facts {
    assert_eq!(alpha.len(), w * h, "mask size");
    if w == 0 || h == 0 {
        return Facts {
            coverage: 0.0,
            parts: 0,
            soft: 0.0,
            bbox: None,
        };
    }
    // Nearest-neighbour onto the grid: fine for shares and boxes.
    let step = (w.max(h) as f64 / GRID as f64).max(1.0);
    let gw = ((w as f64 / step).round() as usize).max(1);
    let gh = ((h as f64 / step).round() as usize).max(1);
    let grid: Vec<u8> = (0..gh)
        .flat_map(|y| {
            let sy = (((y as f64 + 0.5) * h as f64 / gh as f64) as usize).min(h - 1);
            (0..gw).map(move |x| {
                let sx = (((x as f64 + 0.5) * w as f64 / gw as f64) as usize).min(w - 1);
                alpha[sy * w + sx]
            })
        })
        .collect();
    let n = grid.len() as f64;
    let solid = grid.iter().filter(|&&a| a > SOLID).count();
    let soft = grid.iter().filter(|&&a| a > 25 && a < 230).count();

    let (mut x0, mut y0, mut x1, mut y1) = (gw, gh, 0, 0);
    for y in 0..gh {
        for x in 0..gw {
            if grid[y * gw + x] > SOLID {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    let bbox = (solid > 0).then(|| {
        (
            x0 as f64 / gw as f64,
            y0 as f64 / gh as f64,
            (x1 - x0 + 1) as f64 / gw as f64,
            (y1 - y0 + 1) as f64 / gh as f64,
        )
    });

    Facts {
        coverage: solid as f64 / n,
        parts: parts(&grid, gw, gh, ((n * MIN_PART).ceil() as usize).max(1)),
        soft: soft as f64 / n,
        bbox,
    }
}

/// Pieces of subject (8-connected) of at least `min` cells.
fn parts(grid: &[u8], w: usize, h: usize, min: usize) -> u32 {
    let mut seen = vec![false; grid.len()];
    let mut stack = Vec::new();
    let mut count = 0;
    for start in 0..grid.len() {
        if seen[start] || grid[start] <= SOLID {
            continue;
        }
        seen[start] = true;
        stack.push(start);
        let mut size = 0;
        while let Some(i) = stack.pop() {
            size += 1;
            let (x, y) = ((i % w) as isize, (i / w) as isize);
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                        continue;
                    }
                    let j = ny as usize * w + nx as usize;
                    if !seen[j] && grid[j] > SOLID {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        if size >= min {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(w: usize, h: usize, fill: impl Fn(usize, usize) -> u8) -> Vec<u8> {
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| fill(x, y))
            .collect()
    }

    #[test]
    fn one_square_subject() {
        // A 50 by 50 square in a 200 by 100 picture.
        let a = canvas(200, 100, |x, y| {
            if (50..100).contains(&x) && (25..75).contains(&y) {
                255
            } else {
                0
            }
        });
        let f = facts(&a, 200, 100);
        assert!((f.coverage - 0.125).abs() < 0.01, "{f:?}");
        assert_eq!(f.parts, 1);
        assert_eq!(f.soft, 0.0);
        let (x, y, w, h) = f.bbox.unwrap();
        assert!((x - 0.25).abs() < 0.01 && (y - 0.25).abs() < 0.01, "{f:?}");
        assert!((w - 0.25).abs() < 0.01 && (h - 0.5).abs() < 0.01, "{f:?}");
        assert!(!f.looks_wrong());
    }

    #[test]
    fn many_pieces_and_specks() {
        // Eight 10 by 10 blobs (each 1% of 100 by 100) and a lot of single
        // specks, which are too small to count.
        let a = canvas(100, 100, |x, y| {
            let blob = y < 10 && x % 12 < 10 && x < 96;
            let speck = y > 50 && x % 7 == 0 && y % 7 == 0;
            if blob || speck {
                255
            } else {
                0
            }
        });
        let f = facts(&a, 100, 100);
        assert_eq!(f.parts, 8, "{f:?}");
        assert!(f.looks_wrong(), "more than five pieces");
    }

    #[test]
    fn empty_full_and_soft() {
        let empty = facts(&vec![0; 64 * 64], 64, 64);
        assert_eq!((empty.coverage, empty.parts, empty.bbox), (0.0, 0, None));
        assert!(empty.looks_wrong());
        let full = facts(&vec![255; 64 * 64], 64, 64);
        assert_eq!(full.coverage, 1.0);
        assert!(full.looks_wrong(), "the whole picture kept");
        let soft = facts(&vec![128; 64 * 64], 64, 64);
        assert_eq!(soft.soft, 1.0);
    }

    #[test]
    fn a_large_mask_is_measured_on_the_grid_with_the_same_answer() {
        let a = canvas(
            3000,
            2000,
            |x, y| if x < 1500 && y < 1000 { 255 } else { 0 },
        );
        let f = facts(&a, 3000, 2000);
        assert!((f.coverage - 0.25).abs() < 0.01, "{f:?}");
        assert_eq!(f.parts, 1);
    }
}
