//! The ghosting model: the host's estimate of what the e-ink panel really
//! shows, pixel by pixel, kept on the GPU (`ghost_drive` / `ghost_err` in
//! `edges.wgsl`) and used to decide which tiles need a quality refresh.
//! `docs/GHOSTING.md` has the reasoning, the sources and how to calibrate it.
//!
//! Every pixel has `est` (16-bit fixed point, 0 black .. 65535 white) and
//! `shown` (the 4-bit level the tablet's framebuffer holds there, which is
//! what the vendor engine compares with to decide whether a pixel is driven).
//! An update that was SENT drives the pixels of its rectangles:
//!
//! * fast mode (binary, the DU-like `Mode::Pen`): a pixel whose black/white
//!   side changed is driven toward that side, incompletely: the paint
//!   (white to black) and the erase (black to white) each with its own
//!   share `k`, and each for the gray view and the edge view; the others
//!   keep their estimate;
//! * settle mode (the grayscale `Mode::Ui` / `Content`): every pixel of the
//!   rectangle goes toward its level, `est += k_settle * (level - est)`.
//!
//! The residual error `|level - est|` (minus a dead band) is summed per tile
//! (what the streamer schedules on) and per 8×8 cell (a map for a later step
//! that boxes ghosts tightly). All arithmetic is integers, so the shader and
//! `Model` below agree to the bit.

use std::time::Duration;

/// Fixed-point units of a full swing (white).
#[cfg(test)]
pub const FULL: u32 = 65535;
/// Units of one of the 15 gray steps (`FULL / 15`).
pub const LEVEL: u32 = 4369;
/// Edge of a cell of the per-cell error map, in pixels (also the grain of
/// the cells an update drives, and of the GPU's rectangles).
#[cfg(test)]
pub const CELL: usize = crate::tiles::CELL;
/// A cell whose error is at least this (1 unit = 1/256 of a pixel that is
/// wholly wrong; a quarter of a pixel) is in the boxes the GPU draws around
/// the ghosts (`ghost_boxes`). Must equal `CELL_ERR` in `edges.wgsl`. Guess.
#[cfg(test)]
pub const CELL_ERR: u32 = 64;

// ---- Tunables: one place. "Guess" means no source gives the number; see
// docs/GHOSTING.md for what each stands for and how to measure it. ----

/// Share of the way to black/white a fast-mode drive gets a pixel, in 1/256,
/// per direction and per kind of picture (see `Params`). Guesses: the sources
/// say a fast waveform is a short pulse train that does not saturate, and
/// that what is left depends on the transition and on the drive history; no
/// source gives the numbers. Erasing (black to white) is taken as the weaker
/// drive, which is where the edge view's trails come from.
pub const K_PAINT: u32 = 230; // gray view, white to black: 0.90
pub const K_ERASE: u32 = 205; // gray view, black to white: 0.80
pub const K_PAINT_EDGE: u32 = 230; // edge view (1-bit), white to black: 0.90
pub const K_ERASE_EDGE: u32 = 154; // edge view, black to white: 0.60
/// The same for a settle (GL16/GC16-like) drive. Guess: near complete.
pub const K_SETTLE: u32 = 248; // 0.97
/// A fast drive is toward white from this level up, toward black below
/// (the level the vendor engine's binary mode splits on). Guess.
pub const THRESHOLD: u32 = 8;
/// Error below this (in `FULL` units) counts as no error: one gray step, the
/// finest the 4-bit frame can tell apart. Guess.
pub const DEAD_BAND: u32 = LEVEL;

/// A tile is settled once its error reaches this (1 unit = 1/256 of a pixel
/// that is wholly wrong; 2048 = eight such pixels), when it has been quiet
/// for `QUIET`. Guess.
pub const TILE_ERR: u32 = 2048;
/// The same for the edge view, whose ghosts are thin trails: a few dozen
/// pixels of residue are enough. Guess.
pub const TILE_ERR_EDGE: u32 = 768;
/// The `--settle-ms` timer, the upper bound, only settles a tile whose error
/// is at least this much (a pixel's worth) and drops it otherwise. Guess.
pub const TILE_ERR_WEAK: u32 = 256;
/// A tile is not settled while it still changes: a settle would be undone
/// by the next fast drive.
pub const QUIET: Duration = Duration::from_millis(150);
/// Settle budget: tiles per second on average, and the most that may go in
/// one burst (a 1872×1404 panel has 44×59 tiles; this is a fifth of it per
/// second). Guess; it keeps the panel from being busy refreshing all the time.
pub const TILES_PER_S: f64 = 500.0;
pub const BURST_TILES: f64 = 300.0;

/// What the shader needs, as `Params` words.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub k_paint: u32,
    pub k_erase: u32,
    pub k_paint_edge: u32,
    pub k_erase_edge: u32,
    pub k_settle: u32,
    pub threshold: u32,
    pub dead_band: u32,
}

impl Default for Params {
    fn default() -> Self {
        Params { k_paint: K_PAINT, k_erase: K_ERASE, k_paint_edge: K_PAINT_EDGE, k_erase_edge: K_ERASE_EDGE, k_settle: K_SETTLE, threshold: THRESHOLD, dead_band: DEAD_BAND }
    }
}

impl Params {
    #[cfg(test)]
    /// (paint, erase) for a kind of picture.
    pub fn fast_k(&self, kind: Kind) -> (u32, u32) {
        match kind {
            Kind::Gray4 => (self.k_paint, self.k_erase),
            Kind::Bits => (self.k_paint_edge, self.k_erase_edge),
        }
    }
}

/// How a picture is packed (the two wire formats).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Gray4 = 0,
    Bits = 1,
}

impl Kind {
    /// Tile error that gets a tile settled.
    pub fn tile_err(self) -> u32 {
        match self {
            Kind::Gray4 => TILE_ERR,
            Kind::Bits => TILE_ERR_EDGE,
        }
    }
}

/// What an update did to the panel (the shader's `g_mode`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Drive {
    Fast = 1,
    Settle = 2,
    /// The panel's state is unknown: assume white.
    Reset = 3,
    /// A second fast pass on the pixels that should be white and are
    /// estimated not to be (the targeted correction of the edge view's
    /// trails; a hypothesis that needs the tablet's help, see
    /// docs/GHOSTING.md): an erase drive whether or not their level changed.
    #[allow(dead_code)] // the edge view's targeted correction is not sent yet
    Redrive = 4,
}

#[cfg(test)]
fn approach(est: u32, goal: u32, k: u32) -> u32 {
    (est as i32 + (goal as i32 - est as i32) * k as i32 / 256) as u32
}

/// The model on the CPU, pixel by pixel: the reference the shader is
/// checked against. `levels` are 0..=15, one per pixel.
#[cfg(test)]
pub struct Model {
    w: usize,
    h: usize,
    p: Params,
    kind: Kind,
    est: Vec<u32>,
    shown: Vec<u32>,
}

#[cfg(test)]
impl Model {
    pub fn new(w: usize, h: usize, p: Params, kind: Kind) -> Model {
        Model { w, h, p, kind, est: vec![FULL; w * h], shown: vec![15; w * h] }
    }

    /// An update of the pixels in the marked cells (`cols` cells per row)
    /// whose content is `levels`.
    pub fn drive(&mut self, levels: &[u8], mode: Drive, marked: &[bool], cols: usize) {
        let (paint, erase) = self.p.fast_k(self.kind);
        for y in 0..self.h {
            for x in 0..self.w {
                if !marked[(y / CELL) * cols + x / CELL] {
                    continue;
                }
                let i = y * self.w + x;
                let tl = levels[i] as u32;
                let to_white = tl >= self.p.threshold;
                match mode {
                    Drive::Reset => {
                        self.est[i] = FULL;
                        self.shown[i] = 15;
                        continue;
                    }
                    Drive::Settle => self.est[i] = approach(self.est[i], tl * LEVEL, self.p.k_settle),
                    Drive::Fast => {
                        if to_white != (self.shown[i] >= self.p.threshold) {
                            self.est[i] = if to_white { approach(self.est[i], FULL, erase) } else { approach(self.est[i], 0, paint) };
                        }
                    }
                    Drive::Redrive => {
                        if to_white && self.est[i] + self.p.dead_band < FULL {
                            self.est[i] = approach(self.est[i], FULL, erase);
                        }
                        continue;
                    }
                }
                self.shown[i] = tl;
            }
        }
    }

    /// Per tile: the error of the estimate against `levels`, summed (1 unit =
    /// 1/256 of a wholly wrong pixel), dead band taken off each pixel.
    pub fn errors(&self, levels: &[u8], cols: usize, rows: usize) -> Vec<u32> {
        self.sums(levels, crate::tiles::TILE, cols, rows)
    }

    /// The same per `CELL`×`CELL` cell.
    pub fn cell_errors(&self, levels: &[u8]) -> Vec<u32> {
        self.sums(levels, CELL, self.w.div_ceil(CELL), self.h.div_ceil(CELL))
    }

    fn sums(&self, levels: &[u8], size: usize, cols: usize, rows: usize) -> Vec<u32> {
        let mut out = vec![0u32; cols * rows];
        for y in 0..self.h {
            for x in 0..self.w {
                let i = y * self.w + x;
                let d = (self.est[i] as i32 - levels[i] as i32 * LEVEL as i32).unsigned_abs();
                if d > self.p.dead_band {
                    out[(y / size) * cols + x / size] += (d - self.p.dead_band) >> 8;
                }
            }
        }
        out
    }
}

/// How many settled tiles the panel may be given: tokens come in at
/// `TILES_PER_S` up to `BURST_TILES`.
pub struct Budget {
    tokens: f64,
    at: std::time::Instant,
}

impl Budget {
    pub fn new(now: std::time::Instant) -> Budget {
        Budget { tokens: BURST_TILES, at: now }
    }

    fn refill(&mut self, now: std::time::Instant) {
        let dt = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + dt * TILES_PER_S).min(BURST_TILES);
        self.at = now;
    }

    /// Whole tiles that may go now.
    pub fn available(&mut self, now: std::time::Instant) -> usize {
        self.refill(now);
        self.tokens as usize
    }

    pub fn spend(&mut self, tiles: usize) {
        self.tokens = (self.tokens - tiles as f64).max(0.0);
    }

    /// How long until one more tile may go.
    pub fn until_one(&self) -> Duration {
        Duration::from_secs_f64(((1.0 - self.tokens) / TILES_PER_S).max(0.0))
    }
}

/// The `limit` candidates with the largest error.
pub fn pick(err: &[u32], candidates: &[bool], limit: usize) -> Vec<bool> {
    let mut order: Vec<usize> = (0..err.len()).filter(|&t| candidates[t]).collect();
    order.sort_by(|&a, &b| err[b].cmp(&err[a]).then(a.cmp(&b)));
    let mut out = vec![false; err.len()];
    order.into_iter().take(limit).for_each(|t| out[t] = true);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_fast_drive_is_partial_and_only_where_the_side_changed() {
        let p = Params::default();
        let mut m = Model::new(32, 32, p, Kind::Gray4);
        let mut levels = vec![15u8; 32 * 32];
        levels[0] = 0; // white to black: driven
        levels[1] = 12; // white to light gray: same side, not driven
        m.drive(&levels, Drive::Fast, &[true; 16], 4);
        assert_eq!(m.est[0], approach(FULL, 0, p.k_paint));
        assert_eq!(m.est[1], FULL);
        // The way back is partial too, from where it was, and weaker.
        levels[0] = 15;
        m.drive(&levels, Drive::Fast, &[true; 16], 4);
        assert_eq!(m.est[0], approach(approach(FULL, 0, p.k_paint), FULL, p.k_erase));
        assert!(m.est[0] < FULL);
    }

    #[test]
    fn a_gray_drawn_fast_is_an_error_until_it_settles() {
        let mut m = Model::new(32, 32, Params::default(), Kind::Gray4);
        let gray = vec![7u8; 32 * 32]; // darker than the threshold: driven toward black
        m.drive(&gray, Drive::Fast, &[true; 16], 4);
        let before = m.errors(&gray, 1, 1)[0];
        assert!(before > TILE_ERR, "{before}");
        m.drive(&gray, Drive::Settle, &[true; 16], 4);
        m.drive(&gray, Drive::Settle, &[true; 16], 4);
        assert!(m.errors(&gray, 1, 1)[0] < TILE_ERR_WEAK, "settled");
    }

    #[test]
    fn a_moved_thin_line_leaves_a_trail_that_a_redrive_shrinks() {
        let mut m = Model::new(32, 32, Params::default(), Kind::Bits);
        let line = |x: usize| -> Vec<u8> { (0..32 * 32).map(|i| if i % 32 == x { 0 } else { 15 }).collect() };
        m.drive(&line(10), Drive::Fast, &[true; 16], 4);
        m.drive(&line(11), Drive::Fast, &[true; 16], 4);
        // Column 10 was erased once, incompletely; column 11 is painted.
        let (trail, painted) = (m.est[10], m.est[11]);
        assert!(trail < FULL - DEAD_BAND && painted < FULL / 4, "{trail} {painted}");
        // The cell map has the error only in the cells holding the trail (column 1 of cells).
        let cells = m.cell_errors(&line(11));
        assert!(cells.iter().enumerate().all(|(i, &c)| (c > 0) == (i % 4 == 1)), "{cells:?}");
        let before = m.errors(&line(11), 1, 1)[0];
        m.drive(&line(11), Drive::Redrive, &[true; 16], 4);
        assert!(m.errors(&line(11), 1, 1)[0] < before);
        assert_eq!(m.est[11], painted, "black pixels are left alone");
    }

    #[test]
    fn unsent_tiles_keep_their_estimate() {
        let mut m = Model::new(64, 32, Params::default(), Kind::Gray4);
        let black = vec![0u8; 64 * 32];
        // The left four cells of each row of cells (the first tile), not the right four.
        let left: Vec<bool> = (0..32).map(|c| c % 8 < 4).collect();
        m.drive(&black, Drive::Fast, &left, 8);
        assert!(m.est[0] < FULL && m.est[40] == FULL);
        m.drive(&black, Drive::Reset, &[true; 32], 8);
        assert!(m.est.iter().all(|&e| e == FULL));
    }

    #[test]
    fn the_budget_limits_a_burst_and_refills() {
        let t0 = Instant::now();
        let mut b = Budget::new(t0);
        assert_eq!(b.available(t0), BURST_TILES as usize);
        b.spend(BURST_TILES as usize);
        assert_eq!(b.available(t0), 0);
        assert!(b.until_one() > Duration::ZERO);
        let later = t0 + Duration::from_millis(100);
        assert_eq!(b.available(later), (TILES_PER_S * 0.1) as usize);
    }

    #[test]
    fn the_largest_errors_are_picked_first() {
        let err = [5, 90, 40, 90, 0];
        let all = [true; 5];
        assert_eq!(pick(&err, &all, 2), [false, true, false, true, false]);
        assert_eq!(pick(&err, &[true, false, true, false, true], 2), [true, false, true, false, false]);
        assert_eq!(pick(&err, &all, 0), [false; 5]);
    }
}
