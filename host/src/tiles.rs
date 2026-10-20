//! Tile-based change detection: which parts of the screen changed, as a few
//! rectangles instead of one bounding box. A stroke drawn across the screen
//! then costs the pixels it touched, not the whole diagonal's box, on the
//! wire and on the panel (whose refresh time grows with the area).

/// Tile edge in pixels. Even, so rectangles stay on whole bytes of the
/// 4-bit packing.
pub const TILE: usize = 32;
/// Most rectangles per update: each is a separate panel refresh.
const MAX_RECTS: usize = 8;
/// More runs than this and merging them pair by pair costs more than it saves.
const MAX_RUNS: usize = 160;

/// x, y, w, h in pixels.
pub type Rect = (usize, usize, usize, usize);

pub struct Grid {
    w: usize,
    h: usize,
    cols: usize,
    rows: usize,
}

impl Grid {
    pub fn new(w: usize, h: usize) -> Grid {
        Grid { w, h, cols: w.div_ceil(TILE), rows: h.div_ceil(TILE) }
    }

    pub fn tiles(&self) -> usize {
        self.cols * self.rows
    }

    /// The tiles where two same-sized images differ. The images have
    /// `per_byte` pixels to a byte (1, 2 or 8) with rows padded to bytes;
    /// tiles start on byte boundaries, as `TILE` is a multiple of 8.
    pub fn dirty_packed(&self, a: &[u8], b: &[u8], per_byte: usize) -> Vec<bool> {
        let rb = self.w.div_ceil(per_byte);
        let mut out = vec![false; self.tiles()];
        for ty in 0..self.rows {
            let (y0, y1) = (ty * TILE, ((ty + 1) * TILE).min(self.h));
            for tx in 0..self.cols {
                let (x0, x1) = (tx * TILE / per_byte, ((tx + 1) * TILE).min(self.w).div_ceil(per_byte));
                out[ty * self.cols + tx] = (y0..y1).any(|y| a[y * rb + x0..y * rb + x1] != b[y * rb + x0..y * rb + x1]);
            }
        }
        out
    }

    /// Rectangles covering the marked tiles, smallest first (the smallest
    /// are likely a pen or cursor, which should reach the panel first).
    pub fn rects(&self, marked: &[bool]) -> Vec<Rect> {
        let mut runs: Vec<Rect> = vec![];
        for ty in 0..self.rows {
            let mut tx = 0;
            while tx < self.cols {
                if !marked[ty * self.cols + tx] {
                    tx += 1;
                    continue;
                }
                let start = tx;
                while tx < self.cols && marked[ty * self.cols + tx] {
                    tx += 1;
                }
                let (x, y) = (start * TILE, ty * TILE);
                runs.push((x, y, (tx * TILE).min(self.w) - x, ((ty + 1) * TILE).min(self.h) - y));
            }
        }
        if runs.len() > MAX_RUNS {
            runs = runs.into_iter().reduce(union).into_iter().collect();
        }
        // Join the pair that wastes the least, while that is nearly free or
        // there are too many.
        loop {
            let mut best: Option<(usize, usize, i64)> = None;
            for i in 0..runs.len() {
                for j in i + 1..runs.len() {
                    let waste = area(union(runs[i], runs[j])) as i64 - (area(runs[i]) + area(runs[j])) as i64;
                    if best.is_none_or(|b| waste < b.2) {
                        best = Some((i, j, waste));
                    }
                }
            }
            let Some((i, j, waste)) = best else { break };
            let joined = union(runs[i], runs[j]);
            if runs.len() <= MAX_RECTS && waste * 5 > area(joined) as i64 {
                break;
            }
            runs[i] = joined;
            runs.swap_remove(j);
        }
        runs.sort_by_key(|&r| (area(r), r.1, r.0));
        runs
    }

    /// The tiles the rectangles (from `rects`) cover, which may be more
    /// than the marked ones: what an update actually puts on the panel.
    pub fn covered(&self, rects: &[Rect]) -> Vec<bool> {
        let mut out = vec![false; self.tiles()];
        for &(x, y, w, h) in rects {
            for ty in y / TILE..(y + h).div_ceil(TILE).min(self.rows) {
                for tx in x / TILE..(x + w).div_ceil(TILE).min(self.cols) {
                    out[ty * self.cols + tx] = true;
                }
            }
        }
        out
    }
}

fn area(r: Rect) -> usize {
    r.2 * r.3
}

fn union(a: Rect, b: Rect) -> Rect {
    let (x0, y0) = (a.0.min(b.0), a.1.min(b.1));
    let (x1, y1) = ((a.0 + a.2).max(b.0 + b.2), (a.1 + a.3).max(b.1 + b.3));
    (x0, y0, x1 - x0, y1 - y0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_cover_the_tiles_they_span() {
        let g = Grid::new(100, 70); // 4 × 3 tiles
        let c = g.covered(&[(32, 0, 64, 32), (96, 64, 4, 6)]);
        let want: Vec<bool> = (0..12).map(|t| matches!(t, 1 | 2 | 11)).collect();
        assert_eq!(c, want);
    }

    #[test]
    fn bit_images_are_compared_per_tile_like_byte_images() {
        // 70 wide (last tile 6 px, 1 padded byte), 40 high: 3 × 2 tiles.
        let (w, h) = (70usize, 40usize);
        let g = Grid::new(w, h);
        let rb = w.div_ceil(8);
        let a = vec![0u8; rb * h];
        let mut b = a.clone();
        assert!(g.dirty_packed(&a, &b, 8).iter().all(|&d| !d));
        // A pixel in the last column of the last tile of the second row of tiles.
        b[35 * rb + 8] ^= 0x80;
        let d = g.dirty_packed(&a, &b, 8);
        assert_eq!(d, vec![false, false, false, false, false, true]);
        // One in the middle tile of the first row.
        b[3 * rb + 5] ^= 1;
        assert_eq!(g.dirty_packed(&a, &b, 8), vec![false, true, false, false, false, true]);
    }

    #[test]
    fn nibble_images_are_compared_per_tile_too() {
        // 70 wide: 35 bytes a row, the last tile covering bytes 32..35.
        let (w, h) = (70usize, 40usize);
        let g = Grid::new(w, h);
        let a = vec![0u8; 35 * h];
        let mut b = a.clone();
        b[35 * 34 + 34] = 0x10; // the last pixel's byte, in the second row of tiles
        assert_eq!(g.dirty_packed(&a, &b, 2), vec![false, false, false, false, false, true]);
        b[2 * 35 + 16] = 1; // the second tile's first byte (pixel 32)
        assert_eq!(g.dirty_packed(&a, &b, 2), vec![false, true, false, false, false, true]);
    }

    #[test]
    fn marks_only_tiles_that_changed() {
        let g = Grid::new(100, 70);
        assert_eq!((g.cols, g.rows), (4, 3));
        let a = vec![0u8; 100 * 70];
        let mut b = a.clone();
        assert!(g.dirty_packed(&a, &b, 1).iter().all(|&d| !d));
        b[40 * 100 + 99] = 1; // last column, second row of tiles
        let d = g.dirty_packed(&a, &b, 1);
        assert_eq!(d.iter().filter(|&&d| d).count(), 1);
        assert!(d[4 + 3]);
    }

    #[test]
    fn a_diagonal_stays_small() {
        let g = Grid::new(32 * 20, 32 * 20);
        let mut marked = vec![false; g.tiles()];
        for i in 0..20 {
            marked[i * 20 + i] = true;
        }
        let rects = g.rects(&marked);
        assert!(rects.len() <= MAX_RECTS);
        // The rectangles together cover every marked tile...
        for i in 0..20 {
            assert!(rects.iter().any(|&(x, y, w, h)| (i * 32 >= x && i * 32 < x + w) && (i * 32 >= y && i * 32 < y + h)));
        }
        // ...in far less than the diagonal's bounding box.
        let total: usize = rects.iter().map(|&r| area(r)).sum();
        assert!(total < 640 * 640 * 3 / 5, "{total}");
    }

    #[test]
    fn adjacent_tiles_join_without_waste_and_edges_are_clamped() {
        let g = Grid::new(100, 70);
        let mut marked = vec![false; g.tiles()];
        // A 2×2 block at the bottom-right corner.
        for t in [4 + 2, 4 + 3, 8 + 2, 8 + 3] {
            marked[t] = true;
        }
        assert_eq!(g.rects(&marked), vec![(64, 32, 36, 38)]);
    }

    #[test]
    fn small_rectangles_come_first() {
        let g = Grid::new(32 * 10, 32 * 10);
        let mut marked = vec![false; g.tiles()];
        for x in 0..6 {
            marked[x] = true; // wide strip on top
        }
        marked[9 * 10 + 9] = true; // a lone tile far away
        let rects = g.rects(&marked);
        assert_eq!(rects, vec![(288, 288, 32, 32), (0, 0, 192, 32)]);
    }
}
