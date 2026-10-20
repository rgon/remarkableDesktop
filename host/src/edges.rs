//! Canny-style edge detection for the "edge" display mode: outlines are
//! far easier to read on e-paper than the screen's grays are.
//!
//! Gaussian blur, Sobel gradient, non-maximum suppression, then hysteresis
//! thresholding. The result is one-pixel black lines on white, drawn on the
//! darker side of each edge, so outlines sit inside shapes instead of
//! growing them. Enclosed regions that are dark in the original are then
//! filled solid.

/// Gradient magnitude (|gx| + |gy| of a Sobel over 0..=255 gray, so four
/// times the step in brightness) from which an edge is certain.
const HIGH: i32 = 4 * 60;
/// Gradient magnitude an edge may fade to while still joined to a certain one.
const LOW: i32 = 4 * 24;

/// Mean gray (0..=255) under which an enclosed region is filled black, and
/// the gray under which the naive fill blackens a pixel.
pub(crate) const DARK: u64 = 100;

/// `gray` is `w`×`h`, one byte per pixel. Returns the same size: 0 on an
/// edge, 255 elsewhere. (The CPU way with the region fill; the streamer
/// goes through `Detector`.)
#[cfg(test)]
pub fn detect(gray: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = outline(gray, w, h);
    if usable(gray, w, h) {
        fill_dark(gray, &mut out, w, h);
    }
    out
}

fn usable(gray: &[u8], w: usize, h: usize) -> bool {
    w >= 5 && h >= 5 && gray.len() == w * h
}

/// Edge detection on the GPU where there is one (see `gpu.rs`), on the CPU
/// otherwise. The frame size may change between calls.
pub struct Detector {
    backend: Backend,
    fill: Fill,
}

/// What turns dark areas solid.
#[derive(Clone, Copy, PartialEq)]
enum Fill {
    /// Every pixel darker than `DARK` is black: nothing for the CPU to do.
    Threshold,
    /// Enclosed regions whose mean is darker than `DARK` are black (`fill_dark`, on the CPU).
    Region,
}

/// The naive fill: pixels of `gray` darker than `DARK` turn black in `out`.
pub(crate) fn threshold(gray: &[u8], out: &mut [u8]) {
    for (o, &g) in out.iter_mut().zip(gray) {
        if (g as u64) < DARK {
            *o = 0;
        }
    }
}

enum Backend {
    /// Not tried yet (or the size changed).
    Untried,
    Gpu(crate::gpu::Gpu),
    Cpu,
}

impl Detector {
    /// `REMDESKTOP_EDGE_FILL=region` fills enclosed dark regions on the CPU
    /// instead of the default per-pixel threshold.
    pub fn new() -> Detector {
        let fill = if std::env::var("REMDESKTOP_EDGE_FILL").is_ok_and(|v| v == "region") { Fill::Region } else { Fill::Threshold };
        Detector { backend: Backend::Untried, fill }
    }

    /// The outline `out` of `gray` with the chosen fill applied.
    fn finish(&self, gray: &[u8], out: &mut [u8], w: usize, h: usize) {
        if !usable(gray, w, h) {
            return;
        }
        match self.fill {
            Fill::Threshold => threshold(gray, out),
            Fill::Region => fill_dark(gray, out, w, h),
        }
    }

    pub fn detect(&mut self, gray: &[u8], w: usize, h: usize) -> Vec<u8> {
        if matches!(&self.backend, Backend::Gpu(g) if g.size() != (w, h)) {
            self.backend = Backend::Untried;
        }
        if matches!(self.backend, Backend::Untried) {
            self.backend = if std::env::var_os("REMDESKTOP_NO_GPU").is_some() || !usable(gray, w, h) {
                Backend::Cpu
            } else {
                match crate::gpu::Gpu::new(w, h, if self.fill == Fill::Threshold { DARK as u32 } else { 0 }) {
                    Ok(g) => {
                        eprintln!("remdesktop-host: edge detection on the GPU ({})", g.name());
                        Backend::Gpu(g)
                    }
                    Err(e) => {
                        eprintln!("remdesktop-host: edge detection on the CPU ({e})");
                        Backend::Cpu
                    }
                }
            };
        }
        let gpu_result = match &mut self.backend {
            Backend::Gpu(g) => Some(g.outline(gray)),
            _ => None,
        };
        match gpu_result {
            Some(Ok(mut out)) => {
                // The GPU applied the threshold itself.
                if self.fill == Fill::Region {
                    self.finish(gray, &mut out, w, h);
                }
                out
            }
            Some(Err(e)) => {
                eprintln!("remdesktop-host: the GPU failed ({e}); edge detection on the CPU from now on");
                self.backend = Backend::Cpu;
                self.cpu(gray, w, h)
            }
            None => self.cpu(gray, w, h),
        }
    }

    fn cpu(&self, gray: &[u8], w: usize, h: usize) -> Vec<u8> {
        let mut out = outline(gray, w, h);
        self.finish(gray, &mut out, w, h);
        out
    }
}

/// The one-pixel outlines, before the dark regions are filled.
pub(crate) fn outline(gray: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![255u8; w * h];
    if !usable(gray, w, h) {
        return out;
    }
    let blurred = blur(gray, w, h);
    let (mag, dir) = gradient(&blurred, w, h);
    let thin = suppress(&mag, &dir, w, h);
    let edges = hysteresis(&thin, w, h);
    // One pixel, on the darker side of the edge.
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            if !edges[i] {
                continue;
            }
            let (dx, dy) = dir[i].step();
            // Toward the brighter side.
            let (sx, sy) = if blurred[(y as isize + dy) as usize * w + (x as isize + dx) as usize] >= blurred[(y as isize - dy) as usize * w + (x as isize - dx) as usize] {
                (dx, dy)
            } else {
                (-dx, -dy)
            };
            // The edge pixel itself, or the one behind it if that is darker.
            let at = |k: isize| ((y as isize + k * sy) as usize, (x as isize + k * sx) as usize);
            let (py, px) = at(-1);
            let k = if gray[py * w + px] < gray[i] { -1 } else { 0 };
            let (py, px) = at(k);
            out[py * w + px] = 0;
        }
    }
    out
}

/// Fills the regions the outlines enclose that are dark in the original, so
/// dark shapes and strokes come out solid instead of hollow.
///
/// Regions are the 4-connected runs of non-edge pixels: the one-pixel lines
/// are 8-connected, so a 4-connected fill cannot slip through a diagonal
/// step. A region is black when its mean gray is below `DARK`.
fn fill_dark(gray: &[u8], out: &mut [u8], w: usize, h: usize) {
    // Work on runs of non-edge pixels instead of single pixels: the regions
    // are found by joining the runs of neighboring rows that overlap (4-connectivity).
    let mut runs: Vec<(u32, u32)> = vec![]; // x0..x1 within its row
    let mut row_start = Vec::with_capacity(h + 1);
    let mut sum: Vec<u64> = vec![];
    for y in 0..h {
        row_start.push(runs.len());
        let row = &out[y * w..][..w];
        let g = &gray[y * w..][..w];
        let mut x = 0;
        while x < w {
            if row[x] == 0 {
                x += 1;
                continue;
            }
            let x0 = x;
            let mut total = 0u64;
            while x < w && row[x] != 0 {
                total += g[x] as u64;
                x += 1;
            }
            runs.push((x0 as u32, x as u32));
            sum.push(total);
        }
    }
    row_start.push(runs.len());

    let mut parent: Vec<u32> = (0..runs.len() as u32).collect();
    fn find(parent: &mut [u32], mut i: u32) -> u32 {
        while parent[i as usize] != i {
            parent[i as usize] = parent[parent[i as usize] as usize];
            i = parent[i as usize];
        }
        i
    }
    for y in 1..h {
        let (mut p, p_end) = (row_start[y - 1], row_start[y]);
        let (mut c, c_end) = (row_start[y], row_start[y + 1]);
        while p < p_end && c < c_end {
            let ((px0, px1), (cx0, cx1)) = (runs[p], runs[c]);
            if px0 < cx1 && cx0 < px1 {
                let (a, b) = (find(&mut parent, p as u32), find(&mut parent, c as u32));
                if a != b {
                    parent[b as usize] = a;
                }
            }
            if px1 < cx1 {
                p += 1;
            } else {
                c += 1;
            }
        }
    }

    // Per region: the pixels it has and their gray.
    let mut total = vec![(0u64, 0u64); runs.len()];
    for i in 0..runs.len() {
        let r = find(&mut parent, i as u32) as usize;
        total[r].0 += sum[i];
        total[r].1 += (runs[i].1 - runs[i].0) as u64;
    }
    for y in 0..h {
        for i in row_start[y]..row_start[y + 1] {
            let (gray_sum, count) = total[find(&mut parent, i as u32) as usize];
            if gray_sum < DARK * count {
                out[y * w + runs[i].0 as usize..y * w + runs[i].1 as usize].fill(0);
            }
        }
    }
}

/// Runs `f(first_row, rows_of_a, rows_of_b)` on bands of rows in parallel.
fn par_rows<A: Send, B: Send>(w: usize, h: usize, a: &mut [A], b: &mut [B], f: impl Fn(usize, &mut [A], &mut [B]) + Sync) {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    let rows = h.div_ceil(threads).max(1);
    std::thread::scope(|s| {
        for (i, (a, b)) in a.chunks_mut(rows * w).zip(b.chunks_mut(rows * w)).enumerate() {
            let f = &f;
            s.spawn(move || f(i * rows, a, b));
        }
    });
}

/// Separable 5-tap binomial blur (1 4 6 4 1) / 16, borders clamped.
fn blur(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut tmp = vec![0u8; w * h];
    // Horizontal pass.
    let mut none = vec![(); w * h];
    par_rows(w, h, &mut tmp, &mut none, |y0, rows, _| {
        for (r, dst) in rows.chunks_mut(w).enumerate() {
            let row = &src[(y0 + r) * w..][..w];
            let px = |x: isize| row[x.clamp(0, w as isize - 1) as usize] as u32;
            let at = |x: usize| ((px(x as isize - 2) + 4 * px(x as isize - 1) + 6 * px(x as isize) + 4 * px(x as isize + 1) + px(x as isize + 2)) / 16) as u8;
            for x in 0..2.min(w) {
                dst[x] = at(x);
            }
            for x in w.saturating_sub(2).max(2)..w {
                dst[x] = at(x);
            }
            // Inside, no clamping: a plain loop the compiler can vectorize.
            if w > 4 {
                for (x, d) in dst[2..w - 2].iter_mut().enumerate() {
                    let t = &row[x..x + 5];
                    *d = ((t[0] as u32 + 4 * t[1] as u32 + 6 * t[2] as u32 + 4 * t[3] as u32 + t[4] as u32) / 16) as u8;
                }
            }
        }
    });
    // Vertical pass.
    let mut out = vec![0u8; w * h];
    par_rows(w, h, &mut out, &mut none, |y0, rows, _| {
        for (r, dst) in rows.chunks_mut(w).enumerate() {
            let line = |dy: isize| &tmp[((y0 + r) as isize + dy).clamp(0, h as isize - 1) as usize * w..][..w];
            let (a, b, c, d, e) = (line(-2), line(-1), line(0), line(1), line(2));
            for x in 0..w {
                dst[x] = ((a[x] as u32 + 4 * b[x] as u32 + 6 * c[x] as u32 + 4 * d[x] as u32 + e[x] as u32) / 16) as u8;
            }
        }
    });
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

impl Dir {
    /// A step along the gradient's axis.
    fn step(self) -> (isize, isize) {
        match self {
            Dir::X => (1, 0),
            Dir::Y => (0, 1),
            Dir::Diag => (1, 1),
            Dir::Anti => (1, -1),
        }
    }
}

/// Sobel magnitude and gradient direction; the one-pixel border stays zero.
fn gradient(img: &[u8], w: usize, h: usize) -> (Vec<i32>, Vec<Dir>) {
    let mut mag = vec![0i32; w * h];
    let mut dir = vec![Dir::X; w * h];
    par_rows(w, h, &mut mag, &mut dir, |y0, mag, dir| {
        for (r, (mag, dir)) in mag.chunks_mut(w).zip(dir.chunks_mut(w)).enumerate() {
            let y = y0 + r;
            if y == 0 || y + 1 >= h {
                continue;
            }
            let (up, mid, down) = (&img[(y - 1) * w..][..w], &img[y * w..][..w], &img[(y + 1) * w..][..w]);
            for x in 1..w - 1 {
                let p = |row: &[u8], dx: usize| row[x + dx - 1] as i32;
                let gx = (p(up, 2) + 2 * p(mid, 2) + p(down, 2)) - (p(up, 0) + 2 * p(mid, 0) + p(down, 0));
                let gy = (p(down, 0) + 2 * p(down, 1) + p(down, 2)) - (p(up, 0) + 2 * p(up, 1) + p(up, 2));
                let (ax, ay) = (gx.abs(), gy.abs());
                mag[x] = ax + ay;
                // tan(22.5°) ≈ 0.414 separates the axes from the diagonals.
                dir[x] = if ay * 1000 < ax * 414 {
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
    });
    (mag, dir)
}

/// Keeps only pixels that are a local maximum across the edge, so lines
/// are one pixel wide. Everything below `LOW` is dropped here too.
fn suppress(mag: &[i32], dir: &[Dir], w: usize, h: usize) -> Vec<i32> {
    let mut out = vec![0i32; w * h];
    let mut none = vec![(); w * h];
    par_rows(w, h, &mut out, &mut none, |y0, rows, _| {
        for (r, dst) in rows.chunks_mut(w).enumerate() {
            let y = y0 + r;
            if y == 0 || y + 1 >= h {
                continue;
            }
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
                    dst[x] = m;
                }
            }
        }
    });
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

    fn black_columns(img: &[u8], w: usize, h: usize) -> Vec<usize> {
        let out = detect(img, w, h);
        let row = &out[10 * w..][..w];
        (0..w).filter(|&x| row[x] == 0).collect()
    }

    #[test]
    fn step_gives_a_thin_line_on_its_dark_side() {
        let (w, h) = (32, 32);
        // Left half black, right half white: the line stays in the black half.
        let img: Vec<u8> = (0..w * h).map(|i| if i % w < 16 { 120 } else { 255 }).collect();
        let black = black_columns(&img, w, h);
        assert!(!black.is_empty() && black.len() == 1, "{black:?}");
        assert!(black.iter().all(|&x| (14..=15).contains(&x)), "{black:?}");
        // And mirrored: right half black, line in the right half.
        let img: Vec<u8> = (0..w * h).map(|i| if i % w < 16 { 255 } else { 120 }).collect();
        let black = black_columns(&img, w, h);
        assert!(!black.is_empty() && black.len() == 1, "{black:?}");
        assert!(black.iter().all(|&x| (16..=17).contains(&x)), "{black:?}");
    }

    #[test]
    fn dark_square_is_filled_and_light_one_is_not() {
        let (w, h) = (48, 32);
        // A black square on the left and a light gray one on the right, on white.
        let img: Vec<u8> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                match (x, y) {
                    (6..=17, 8..=23) => 0,
                    (30..=41, 8..=23) => 200,
                    _ => 255,
                }
            })
            .collect();
        let out = detect(&img, w, h);
        assert_eq!(out[16 * w + 12], 0, "dark interior is filled");
        assert_eq!(out[16 * w + 36], 255, "light interior stays white");
        assert_eq!(out[16 * w + 24], 255, "background stays white");
    }

    #[test]
    fn weak_isolated_edge_is_dropped() {
        // A faint step (contrast 20) never reaches the high threshold.
        let (w, h) = (32, 32);
        let img: Vec<u8> = (0..w * h).map(|i| if i % w < 16 { 100 } else { 120 }).collect();
        assert!(detect(&img, w, h).iter().all(|&v| v == 255));
    }
}
