//! Canny-style edge detection for the "edge" display mode: outlines are
//! far easier to read on e-paper than the screen's grays are.
//!
//! Gaussian blur, Sobel gradient, non-maximum suppression, then hysteresis
//! thresholding. The result is black lines on white, thickened a little so
//! single-pixel lines survive the panel.

/// Gradient magnitude (|gx| + |gy| of a Sobel over 0..=255 gray, so four
/// times the step in brightness) from which an edge is certain.
const HIGH: i32 = 4 * 60;
/// Gradient magnitude an edge may fade to while still joined to a certain one.
const LOW: i32 = 4 * 24;

/// `gray` is `w`×`h`, one byte per pixel. Returns the same size: 0 on an
/// edge, 255 elsewhere.
pub fn detect(gray: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![255u8; w * h];
    if w < 5 || h < 5 || gray.len() != w * h {
        return out;
    }
    let blurred = blur(gray, w, h);
    let (mag, dir) = gradient(&blurred, w, h);
    let thin = suppress(&mag, &dir, w, h);
    let edges = hysteresis(&thin, w, h);
    // Thicken to 2×2 (down and right), for visibility.
    for y in 0..h {
        for x in 0..w {
            if edges[y * w + x] {
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    if x + dx < w && y + dy < h {
                        out[(y + dy) * w + x + dx] = 0;
                    }
                }
            }
        }
    }
    out
}

/// Separable 5-tap binomial blur (1 4 6 4 1) / 16, borders clamped.
fn blur(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    const K: [u32; 5] = [1, 4, 6, 4, 1];
    let mut tmp = vec![0u8; w * h];
    for y in 0..h {
        let row = &src[y * w..][..w];
        for x in 0..w {
            let mut acc = 0;
            for (i, k) in K.iter().enumerate() {
                acc += k * row[(x + i).saturating_sub(2).min(w - 1)] as u32;
            }
            tmp[y * w + x] = (acc / 16) as u8;
        }
    }
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0;
            for (i, k) in K.iter().enumerate() {
                acc += k * tmp[(y + i).saturating_sub(2).min(h - 1) * w + x] as u32;
            }
            out[y * w + x] = (acc / 16) as u8;
        }
    }
    out
}

/// Which way the gradient points, quantized to the axis it is closest to.
#[derive(Clone, Copy, PartialEq)]
enum Dir {
    /// Gradient along x (a vertical edge): compare left and right.
    X,
    /// Gradient along y: compare up and down.
    Y,
    /// Gradient along the main diagonal (x and y rise together).
    Diag,
    /// Gradient along the anti-diagonal.
    Anti,
}

/// Sobel magnitude and gradient direction; the one-pixel border stays zero.
fn gradient(img: &[u8], w: usize, h: usize) -> (Vec<i32>, Vec<Dir>) {
    let mut mag = vec![0i32; w * h];
    let mut dir = vec![Dir::X; w * h];
    let p = |x: usize, y: usize| img[y * w + x] as i32;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let gx = (p(x + 1, y - 1) + 2 * p(x + 1, y) + p(x + 1, y + 1)) - (p(x - 1, y - 1) + 2 * p(x - 1, y) + p(x - 1, y + 1));
            let gy = (p(x - 1, y + 1) + 2 * p(x, y + 1) + p(x + 1, y + 1)) - (p(x - 1, y - 1) + 2 * p(x, y - 1) + p(x + 1, y - 1));
            let (ax, ay) = (gx.abs(), gy.abs());
            mag[y * w + x] = ax + ay;
            // tan(22.5°) ≈ 0.414 separates the axes from the diagonals.
            dir[y * w + x] = if ay * 1000 < ax * 414 {
                Dir::X
            } else if ax * 1000 < ay * 414 {
                Dir::Y
            } else if (gx > 0) == (gy > 0) {
                Dir::Diag
            } else {
                Dir::Anti
            };
        }
    }
    (mag, dir)
}

/// Keeps only pixels that are a local maximum across the edge, so lines
/// are one pixel wide. Everything below `LOW` is dropped here too.
fn suppress(mag: &[i32], dir: &[Dir], w: usize, h: usize) -> Vec<i32> {
    let mut out = vec![0i32; w * h];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let m = mag[i];
            if m < LOW {
                continue;
            }
            let (a, b) = match dir[i] {
                Dir::X => (i - 1, i + 1),
                Dir::Y => (i - w, i + w),
                Dir::Diag => (i - w - 1, i + w + 1),
                Dir::Anti => (i - w + 1, i + w - 1),
            };
            // `>` on one side and `>=` on the other: a plateau keeps one pixel.
            if m > mag[a] && m >= mag[b] {
                out[i] = m;
            }
        }
    }
    out
}

/// Keeps weak edges (>= `LOW`) only when they connect, through any of the
/// eight neighbors, to a strong one (>= `HIGH`).
fn hysteresis(thin: &[i32], w: usize, h: usize) -> Vec<bool> {
    let mut edge = vec![false; w * h];
    let mut stack: Vec<usize> = (0..w * h).filter(|&i| thin[i] >= HIGH).collect();
    for &i in &stack {
        edge[i] = true;
    }
    while let Some(i) = stack.pop() {
        let (x, y) = (i % w, i / w);
        for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
            for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                let n = ny * w + nx;
                if !edge[n] && thin[n] >= LOW {
                    edge[n] = true;
                    stack.push(n);
                }
            }
        }
    }
    edge
}

#[cfg(test)]
mod tests {
    use super::detect;

    #[test]
    fn flat_image_has_no_edges() {
        let out = detect(&vec![128u8; 32 * 32], 32, 32);
        assert!(out.iter().all(|&v| v == 255));
    }

    #[test]
    fn step_gives_a_thin_line() {
        // Left half black, right half white: one vertical line near x = 16.
        let (w, h) = (32, 32);
        let img: Vec<u8> = (0..w * h).map(|i| if i % w < 16 { 0 } else { 255 }).collect();
        let out = detect(&img, w, h);
        let row = &out[10 * w..][..w];
        let black: Vec<usize> = (0..w).filter(|&x| row[x] == 0).collect();
        assert!(!black.is_empty() && black.len() <= 2, "{black:?}");
        assert!(black.iter().all(|&x| (14..=17).contains(&x)), "{black:?}");
    }

    #[test]
    fn weak_isolated_edge_is_dropped() {
        // A faint step (contrast 20) never reaches the high threshold.
        let (w, h) = (32, 32);
        let img: Vec<u8> = (0..w * h).map(|i| if i % w < 16 { 100 } else { 120 }).collect();
        assert!(detect(&img, w, h).iter().all(|&v| v == 255));
    }
}
