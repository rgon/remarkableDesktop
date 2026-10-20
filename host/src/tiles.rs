//! Change detection: which parts of the screen changed, as a few rectangles
//! instead of one bounding box. A stroke drawn across the screen then costs
//! the pixels it touched, not the whole diagonal's box, on the wire and on
//! the panel (whose refresh time grows with the area).
//!
//! The GPU finds the rectangles itself, on a grid of 8x8 cells (`edges.wgsl`,
//! passes `rc_*`): connected components of changed cells, each with its
//! bounding box, a component that fills little of its box cut into bands.
//! `finish` turns those boxes into the few rectangles of one update. The
//! 32x32 tile grid (`Grid`) is the CPU fallback, for a frame the GPU has no
//! answer for, and the grid of the ghosting model's error sums.

/// Tile edge in pixels. Even, so rectangles stay on whole bytes of the
/// 4-bit packing.
pub const TILE: usize = 32;
/// Cell edge in pixels, the grain of the GPU's rectangles. The least that
/// keeps rectangles on whole bytes of both packings: `Picture::pack` needs x
/// to be a multiple of 2 pixels for `gray4` and of 8 for `bits1`. Widths and
/// heights may be anything, so the last cell of a row or column may be cut
/// short by the panel's edge.
pub const CELL: usize = 8;
/// Most rectangles per update: each is a separate panel refresh.
const MAX_RECTS: usize = 8;
/// More runs than this and merging them pair by pair costs more than it saves.
const MAX_RUNS: usize = 160;
/// Boxes the GPU lists at most (the last is the union of the ones that did
/// not fit, so nothing changed is left out).
pub const BOXES: usize = 64;
/// Changed cells this many empty cells apart still belong together.
#[cfg(test)]
const GAP: usize = 1;
/// A component whose box has fewer cells than this is never cut.
#[cfg(test)]
const SPLIT_AREA: usize = 16;
/// A cut component has at most this many bands.
#[cfg(test)]
const MAX_BANDS: usize = 16;
/// Bands are joined again while this percent of the joined box is changed.
#[cfg(test)]
const MERGE_FILL: usize = 60;

/// x, y, w, h in pixels.
pub type Rect = (usize, usize, usize, usize);

/// The rectangles of one update from boxes that together cover what changed
/// (the GPU's, or any other list): the pair that wastes the least is joined
/// while that is nearly free or there are more than `MAX_RECTS`; smallest
/// first (the smallest are likely a pen or cursor, which should reach the
/// panel first).
pub fn finish(mut runs: Vec<Rect>) -> Vec<Rect> {
    if runs.len() > MAX_RUNS {
        runs = runs.into_iter().reduce(union).into_iter().collect();
    }
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
        finish(runs)
    }

    /// Rectangles for the marked tiles, tightened by `boxes` (boxes around
    /// what really needs the update, from the GPU): the part of each box that
    /// lies in marked tiles, and the usual rectangles for the marked tiles no
    /// box reaches. A box over unmarked tiles does not grow the update, so
    /// what a budget on tiles allowed stays the most that is refreshed.
    pub fn rects_within(&self, marked: &[bool], boxes: &[Rect]) -> Vec<Rect> {
        let mut pieces = vec![];
        let mut reached = vec![false; self.tiles()];
        for &(bx, by, bw, bh) in boxes {
            let end = (bx + bw).div_ceil(TILE).min(self.cols);
            for ty in by / TILE..(by + bh).div_ceil(TILE).min(self.rows) {
                let (y0, y1) = (by.max(ty * TILE), (by + bh).min((ty + 1) * TILE));
                let mut tx = bx / TILE;
                while tx < end {
                    if !marked[ty * self.cols + tx] {
                        tx += 1;
                        continue;
                    }
                    let start = tx;
                    while tx < end && marked[ty * self.cols + tx] {
                        reached[ty * self.cols + tx] = true;
                        tx += 1;
                    }
                    let (x0, x1) = (bx.max(start * TILE), (bx + bw).min(tx * TILE));
                    pieces.push((x0, y0, x1 - x0, y1 - y0));
                }
            }
        }
        let rest: Vec<bool> = marked.iter().zip(&reached).map(|(&m, &r)| m && !r).collect();
        pieces.extend(self.rects(&rest));
        finish(pieces)
    }

    /// Cells of `CELL`×`CELL` pixels per row, and in all.
    pub fn cell_cols(&self) -> usize {
        self.w.div_ceil(CELL)
    }

    pub fn cells(&self) -> usize {
        self.cell_cols() * self.h.div_ceil(CELL)
    }

    /// The cells the rectangles cover: what an update puts on the panel.
    pub fn covered_cells(&self, rects: &[Rect]) -> Vec<bool> {
        let (cols, rows) = (self.cell_cols(), self.h.div_ceil(CELL));
        let mut out = vec![false; cols * rows];
        for &(x, y, w, h) in rects {
            for cy in y / CELL..(y + h).div_ceil(CELL).min(rows) {
                for cx in x / CELL..(x + w).div_ceil(CELL).min(cols) {
                    out[cy * cols + cx] = true;
                }
            }
        }
        out
    }

    /// The marked tiles and the `n` tiles around each of them.
    pub fn dilate(&self, marked: &[bool], n: usize) -> Vec<bool> {
        let mut out = vec![false; self.tiles()];
        for ty in 0..self.rows {
            for tx in 0..self.cols {
                if !marked[ty * self.cols + tx] {
                    continue;
                }
                for y in ty.saturating_sub(n)..(ty + n + 1).min(self.rows) {
                    for x in tx.saturating_sub(n)..(tx + n + 1).min(self.cols) {
                        out[y * self.cols + x] = true;
                    }
                }
            }
        }
        out
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

/// A box of cells (x0, y0, x1, y1, the far corner exclusive) as pixels of a
/// `w`×`h` frame, cut short at the frame's edge.
pub fn box_rect((x0, y0, x1, y1): (usize, usize, usize, usize), w: usize, h: usize) -> Rect {
    let (x, y) = (x0 * CELL, y0 * CELL);
    (x, y, (x1 * CELL).min(w) - x, (y1 * CELL).min(h) - y)
}

fn area(r: Rect) -> usize {
    r.2 * r.3
}

/// The GPU's rectangle algorithm (`rc_*` in `edges.wgsl`) on the CPU, the
/// reference the tests compare it with. `set` marks cells (`cols` per row);
/// the boxes are (x0, y0, x1, y1) in cells, the far corner exclusive, sorted.
/// Cells within `GAP` empty cells of each other are one component; a
/// component that fills under half of its box, which has at least
/// `SPLIT_AREA` cells, is cut into bands (at most `MAX_BANDS`) along the
/// box's longer side, and neighboring bands are joined while the cells set
/// fill `MERGE_FILL` percent of the joined box.
#[cfg(test)]
pub fn cell_boxes(set: &[bool], cols: usize, rows: usize) -> Vec<(usize, usize, usize, usize)> {
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    let mut parent: Vec<usize> = (0..cols * rows).collect();
    let reach = GAP as i64 + 1;
    for y in 0..rows {
        for x in 0..cols {
            if !set[y * cols + x] {
                continue;
            }
            for dy in 0..=reach {
                for dx in -reach..=reach {
                    let (nx, ny) = (x as i64 + dx, y as i64 - dy);
                    if (dy == 0 && dx >= 0) || nx < 0 || ny < 0 || nx >= cols as i64 {
                        continue;
                    }
                    let n = ny as usize * cols + nx as usize;
                    if set[n] {
                        let (a, b) = (find(&mut parent, y * cols + x), find(&mut parent, n));
                        parent[a.max(b)] = a.min(b);
                    }
                }
            }
        }
    }
    // (min x, min y, max x, max y, count) of a component and of its bands.
    type Acc = Option<(usize, usize, usize, usize, usize)>;
    fn add(a: &mut Acc, x: usize, y: usize) {
        *a = Some(match *a {
            None => (x, y, x, y, 1),
            Some((x0, y0, x1, y1, n)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y), n + 1),
        });
    }
    let mut comps: std::collections::BTreeMap<usize, Acc> = Default::default();
    for c in (0..cols * rows).filter(|&c| set[c]) {
        let r = find(&mut parent, c);
        add(comps.entry(r).or_default(), c % cols, c / cols);
    }
    let mut out = vec![];
    for (&root, acc) in &comps {
        let (x0, y0, x1, y1, n) = acc.unwrap();
        let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
        let by_col = bw > bh;
        let len = if by_col { bw } else { bh };
        let band = len.div_ceil(MAX_BANDS);
        let bands = len.div_ceil(band);
        if n * 2 >= bw * bh || bw * bh < SPLIT_AREA || bands < 2 {
            out.push((x0, y0, x1 + 1, y1 + 1));
            continue;
        }
        let mut subs: Vec<Acc> = vec![None; bands];
        for c in (0..cols * rows).filter(|&c| set[c]) {
            if find(&mut parent, c) == root {
                let (x, y) = (c % cols, c / cols);
                add(&mut subs[(if by_col { x - x0 } else { y - y0 }) / band], x, y);
            }
        }
        let mut group: Acc = None;
        for (sx0, sy0, sx1, sy1, sn) in subs.into_iter().flatten() {
            if let Some((gx0, gy0, gx1, gy1, gn)) = group {
                let (ux0, uy0, ux1, uy1) = (gx0.min(sx0), gy0.min(sy0), gx1.max(sx1), gy1.max(sy1));
                if (gn + sn) * 100 >= MERGE_FILL * (ux1 - ux0 + 1) * (uy1 - uy0 + 1) {
                    group = Some((ux0, uy0, ux1, uy1, gn + sn));
                    continue;
                }
                out.push((gx0, gy0, gx1 + 1, gy1 + 1));
            }
            group = Some((sx0, sy0, sx1, sy1, sn));
        }
        if let Some((gx0, gy0, gx1, gy1, _)) = group {
            out.push((gx0, gy0, gx1 + 1, gy1 + 1));
        }
    }
    out.sort();
    out
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
    fn dilating_adds_a_margin_clamped_to_the_grid() {
        let g = Grid::new(32 * 5, 32 * 4); // 5 × 4 tiles
        let mut marked = vec![false; g.tiles()];
        marked[0] = true; // the corner
        let d = g.dilate(&marked, 1);
        assert_eq!(d.iter().filter(|&&t| t).count(), 4);
        assert!(d[0] && d[1] && d[5] && d[6]);
        marked[0] = false;
        marked[2 * 5 + 2] = true; // in the middle
        assert_eq!(g.dilate(&marked, 1).iter().filter(|&&t| t).count(), 9);
        assert_eq!(g.dilate(&marked, 0), marked);
        // Two tiles two apart join into one block.
        marked[2 * 5 + 4] = true;
        assert_eq!(g.rects(&g.dilate(&marked, 1)), vec![(32, 32, 32 * 4, 32 * 3)]);
    }

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

    fn rand(mut s: u32) -> impl FnMut() -> u32 {
        move || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            s >> 8
        }
    }

    fn covered(boxes: &[(usize, usize, usize, usize)], x: usize, y: usize) -> bool {
        boxes.iter().any(|&(x0, y0, x1, y1)| x >= x0 && x < x1 && y >= y0 && y < y1)
    }

    #[test]
    fn the_boxes_cover_every_set_cell_whatever_the_map() {
        let mut r = rand(1);
        for case in 0..60 {
            let (cols, rows) = (1 + r() as usize % 40, 1 + r() as usize % 30);
            let density = [2, 8, 25, 60, 95][case % 5];
            let set: Vec<bool> = (0..cols * rows).map(|_| r() % 100 < density).collect();
            let boxes = cell_boxes(&set, cols, rows);
            for (c, _) in set.iter().enumerate().filter(|(_, s)| **s) {
                assert!(covered(&boxes, c % cols, c / cols), "case {case}: cell {c} is not covered");
            }
            // And a box is never larger than the map.
            assert!(boxes.iter().all(|&(x0, y0, x1, y1)| x0 < x1 && y0 < y1 && x1 <= cols && y1 <= rows));
            // The few rectangles of an update still cover them, in at most MAX_RECTS.
            let rects = finish(boxes.iter().map(|&b| box_rect(b, cols * CELL, rows * CELL)).collect());
            assert!(rects.len() <= MAX_RECTS);
            for (c, _) in set.iter().enumerate().filter(|(_, s)| **s) {
                let (x, y) = ((c % cols) * CELL, (c / cols) * CELL);
                assert!(rects.iter().any(|&(rx, ry, rw, rh)| x >= rx && x + CELL <= rx + rw && y >= ry && y + CELL <= ry + rh), "case {case}: cell {c} lost in the merge");
            }
        }
    }

    #[test]
    fn near_cells_join_and_far_ones_do_not() {
        let set = |cells: &[(usize, usize)]| {
            let mut s = vec![false; 20 * 20];
            cells.iter().for_each(|&(x, y)| s[y * 20 + x] = true);
            s
        };
        // One empty cell between: one component, and the box holds only the cells set.
        assert_eq!(cell_boxes(&set(&[(2, 2), (4, 2)]), 20, 20), vec![(2, 2, 5, 3)]);
        assert_eq!(cell_boxes(&set(&[(2, 2), (4, 4)]), 20, 20), vec![(2, 2, 5, 5)]);
        // Two between: two.
        assert_eq!(cell_boxes(&set(&[(2, 2), (5, 2)]), 20, 20), vec![(2, 2, 3, 3), (5, 2, 6, 3)]);
        // A cursor-sized blob (2x3 cells) is one box of exactly those cells.
        assert_eq!(cell_boxes(&set(&[(7, 7), (8, 7), (7, 8), (8, 8), (7, 9), (8, 9)]), 20, 20), vec![(7, 7, 9, 10)]);
    }

    #[test]
    fn a_thin_diagonal_is_cut_into_bands_and_a_block_is_not() {
        let (cols, rows) = (64, 64);
        let mut diag = vec![false; cols * rows];
        for i in 0..48 {
            diag[(8 + i) * cols + 8 + i] = true;
        }
        let boxes = cell_boxes(&diag, cols, rows);
        let area = |b: &[(usize, usize, usize, usize)]| b.iter().map(|&(x0, y0, x1, y1)| (x1 - x0) * (y1 - y0)).sum::<usize>();
        // 48 cells long: the whole box is 48x48 = 2304 cells; the bands hold far fewer.
        assert!(boxes.len() > 1 && boxes.len() <= MAX_BANDS && area(&boxes) * 4 < 48 * 48, "{boxes:?}");
        let mut block = vec![false; cols * rows];
        for y in 10..30 {
            for x in 5..40 {
                block[y * cols + x] = true;
            }
        }
        assert_eq!(cell_boxes(&block, cols, rows), vec![(5, 10, 40, 30)]);
        // A block with a hole in the middle is still one box (it is mostly filled).
        block[20 * cols + 20] = false;
        assert_eq!(cell_boxes(&block, cols, rows), vec![(5, 10, 40, 30)]);
    }

    #[test]
    fn a_box_over_unmarked_tiles_does_not_grow_the_update() {
        let g = Grid::new(32 * 10, 32 * 10);
        let mut marked = vec![false; g.tiles()];
        // Tiles 3 and 4 of row 2 are due; a tall thin box (a trail) runs through rows 0 to 5 at x = 104..112, over tile column 3.
        marked[2 * 10 + 3] = true;
        marked[2 * 10 + 4] = true;
        let rects = g.rects_within(&marked, &[(104, 10, 8, 180)]);
        // The trail's part in the marked tile, and the other marked tile in full.
        assert_eq!(rects, vec![(104, 64, 8, 32), (128, 64, 32, 32)]);
        // Without a box that reaches them, the marked tiles are as before.
        assert_eq!(g.rects_within(&marked, &[]), g.rects(&marked));
        // A box over several marked tiles in a column is one box.
        let mut column = vec![false; g.tiles()];
        (1..5).for_each(|r| column[r * 10 + 3] = true);
        assert_eq!(g.rects_within(&column, &[(104, 40, 8, 100)]), vec![(104, 40, 8, 100)]);
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
