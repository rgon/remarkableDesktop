//! Pen ink: a fast local preview of what the pen draws.
//!
//! While the tip is down, black is written straight into the panel buffer
//! and refreshed in the fast mode, so strokes show up long before the
//! host's own rendering of them arrives. Every inked pixel remembers what it
//! covered. A streamed update that overwrites an area drops the ink there
//! (the host's drawing replaces the preview); `restore` wipes the rest, which
//! is how ink is cleared when nothing is being streamed.

use crate::display::Rect;
use crate::epd::ImageInfo;
use crate::input::{ABS_X, ABS_Y, BTN_LEFT, BTN_TOOL_RUBBER, BTN_TOUCH, EV_ABS, EV_KEY};
use remdesktop_proto::Event;

const BLACK: u32 = 0xff00_0000;
/// Brush radius in pixels (a 3 px square).
const BRUSH: i32 = 1;

#[derive(Default)]
pub struct Ink {
    pub on: bool,
    /// What each inked pixel covered, by panel pixel index (y * width + x).
    /// Only meaningful where `mask` has the bit set.
    saved: Vec<u32>,
    /// One bit per panel pixel: inked.
    mask: Vec<u64>,
    width: usize,
    /// Bounding box of the ink (may be larger than the ink itself once
    /// streamed updates drop parts of it).
    dirty: Option<Rect>,
    /// Drawn since the last `take_pending`.
    pending: Option<Rect>,
    pos: [i32; 2],
    tip: bool,
    rubber: bool,
    /// End of the last segment, while a stroke continues.
    last: Option<[i32; 2]>,
}

fn grow(r: Option<Rect>, add: Rect) -> Option<Rect> {
    Some(r.map_or(add, |r| r.union(add)))
}

impl Ink {
    pub fn new(on: bool) -> Self {
        Self { on, ..Self::default() }
    }

    /// Allocates the snapshot on first use (about 10 MB on an rM2).
    fn ensure(&mut self, fb: &ImageInfo) {
        let n = fb.width as usize * fb.height as usize;
        if self.saved.len() != n {
            self.saved = vec![0; n];
            self.mask = vec![0; n.div_ceil(64)];
            self.width = fb.width as usize;
        }
    }

    /// Clears the mask bits of `[start, end)`.
    fn clear_bits(&mut self, start: usize, end: usize) {
        let mut i = start;
        while i < end {
            let (word, bit) = (i / 64, i % 64);
            let n = (64 - bit).min(end - i);
            let m = if n == 64 { u64::MAX } else { ((1u64 << n) - 1) << bit };
            self.mask[word] &= !m;
            i += n;
        }
    }

    /// Calls `f` with the index of every set bit in `[start, end)`.
    fn for_each_set(&self, start: usize, end: usize, mut f: impl FnMut(usize)) {
        let mut i = start;
        while i < end {
            let (word, bit) = (i / 64, i % 64);
            let n = (64 - bit).min(end - i);
            let mut w = self.mask[word] >> bit;
            if n < 64 {
                w &= (1u64 << n) - 1;
            }
            while w != 0 {
                let t = w.trailing_zeros() as usize;
                f(i + t);
                w &= w - 1;
            }
            i += n;
        }
    }

    /// Tracks one pen frame (events as `Pen::feed` makes them). Returns the
    /// pen position (device units) if the frame leaves the pen inking.
    pub fn feed(&mut self, events: &[Event], mouse: bool) -> Option<[i32; 2]> {
        for e in events {
            match (e.kind, e.code) {
                (EV_ABS, ABS_X) => self.pos[0] = e.value,
                (EV_ABS, ABS_Y) => self.pos[1] = e.value,
                (EV_KEY, BTN_TOOL_RUBBER) => self.rubber = e.value != 0,
                // Mouse mode reports the tip as the left button; the eraser is the right one.
                (EV_KEY, BTN_TOUCH) if !mouse => self.tip = e.value != 0,
                (EV_KEY, BTN_LEFT) if mouse => self.tip = e.value != 0,
                _ => {}
            }
        }
        if self.on && self.tip && !self.rubber {
            Some(self.pos)
        } else {
            self.last = None;
            None
        }
    }

    /// Ends the stroke (the pen went over our buttons).
    pub fn lift(&mut self) {
        self.last = None;
    }

    /// Draws to panel pixel `p`, joined to the previous point of the stroke.
    pub fn stroke(&mut self, fb: &ImageInfo, p: [i32; 2]) {
        self.ensure(fb);
        let from = self.last.unwrap_or(p);
        self.last = Some(p);
        let (mut x, mut y) = (from[0], from[1]);
        let (dx, dy) = ((p[0] - x).abs(), -(p[1] - y).abs());
        let (sx, sy) = (if x < p[0] { 1 } else { -1 }, if y < p[1] { 1 } else { -1 });
        let mut err = dx + dy;
        loop {
            self.stamp(fb, x, y);
            if (x, y) == (p[0], p[1]) {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
        let (x0, y0) = (from[0].min(p[0]) - BRUSH, from[1].min(p[1]) - BRUSH);
        let (x1, y1) = (from[0].max(p[0]) + BRUSH + 1, from[1].max(p[1]) + BRUSH + 1);
        let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(fb.width), y1.min(fb.height));
        if x1 > x0 && y1 > y0 {
            let r = Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
            self.pending = grow(self.pending, r);
            self.dirty = grow(self.dirty, r);
        }
    }

    fn stamp(&mut self, fb: &ImageInfo, cx: i32, cy: i32) {
        for y in (cy - BRUSH).max(0)..=(cy + BRUSH).min(fb.height - 1) {
            for x in (cx - BRUSH).max(0)..=(cx + BRUSH).min(fb.width - 1) {
                let px = unsafe { fb.pixels.add(y as usize * fb.stride as usize + x as usize * 4) as *mut u32 };
                let i = y as usize * self.width + x as usize;
                let bit = 1u64 << (i % 64);
                if self.mask[i / 64] & bit == 0 {
                    self.mask[i / 64] |= bit;
                    self.saved[i] = unsafe { px.read_unaligned() };
                }
                unsafe { px.write_unaligned(BLACK) };
            }
        }
    }

    /// The area drawn since the last call, to refresh.
    pub fn take_pending(&mut self) -> Option<Rect> {
        self.pending.take()
    }

    /// A streamed update overwrote `r`: the ink in it is gone.
    pub fn overwritten(&mut self, _fb: &ImageInfo, r: Rect) {
        let Some(d) = self.dirty.filter(|d| d.intersects(r)) else { return };
        let (x0, x1) = (r.x.max(d.x) as usize, (r.x + r.w).min(d.x + d.w) as usize);
        for y in r.y.max(d.y)..(r.y + r.h).min(d.y + d.h) {
            let row = y as usize * self.width;
            self.clear_bits(row + x0, row + x1);
        }
    }

    /// Forgets the ink without touching the panel buffer (it was redrawn).
    pub fn forget(&mut self) {
        if let Some(d) = self.dirty.take() {
            for y in d.y..d.y + d.h {
                let row = y as usize * self.width;
                self.clear_bits(row + d.x as usize, row + (d.x + d.w) as usize);
            }
        }
        self.pending = None;
        self.last = None;
    }

    /// Puts back everything the ink covered. Returns the area to refresh.
    pub fn restore(&mut self, fb: &ImageInfo) -> Option<Rect> {
        let d = self.dirty?;
        let w = self.width;
        let mut bounds: Option<(i32, i32, i32, i32)> = None;
        for y in d.y..d.y + d.h {
            let row = y as usize * w;
            let mut hit: Option<(usize, usize)> = None;
            self.for_each_set(row + d.x as usize, row + (d.x + d.w) as usize, |i| {
                let x = i - row;
                let px = unsafe { fb.pixels.add(y as usize * fb.stride as usize + x * 4) as *mut u32 };
                unsafe { px.write_unaligned(self.saved[i]) };
                hit = Some(hit.map_or((x, x), |(a, b)| (a.min(x), b.max(x))));
            });
            if let Some((a, b)) = hit {
                let (a, b) = (a as i32, b as i32);
                bounds = Some(bounds.map_or((a, y, b, y), |(x0, y0, x1, _)| (x0.min(a), y0, x1.max(b), y)));
            }
        }
        self.forget();
        bounds.map(|(x0, y0, x1, y1)| Rect { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: i32 = 16;
    const H: i32 = 12;

    fn buffer() -> (Vec<u32>, ImageInfo) {
        let mut px = vec![0xffff_ffffu32; (W * H) as usize];
        let fb = ImageInfo { pixels: px.as_mut_ptr().cast(), width: W, height: H, stride: W * 4, format: crate::epd::FORMAT_RGB32 };
        (px, fb)
    }

    fn black(px: &[u32]) -> usize {
        px.iter().filter(|&&p| p == BLACK).count()
    }

    #[test]
    fn restore_puts_back_the_background() {
        let (mut px, fb) = buffer();
        px[5 * W as usize + 5] = 0xff80_8080;
        let before = px.clone();
        let mut ink = Ink::new(true);
        ink.stroke(&fb, [4, 5]);
        ink.stroke(&fb, [9, 5]);
        assert!(black(&px) > 10);
        assert_eq!(ink.take_pending(), Some(Rect { x: 3, y: 4, w: 8, h: 3 }));
        assert!(ink.restore(&fb).is_some());
        assert_eq!(px, before);
        assert!(ink.restore(&fb).is_none());
    }

    #[test]
    fn overwritten_area_drops_its_ink_only() {
        let (mut px, fb) = buffer();
        let mut ink = Ink::new(true);
        ink.stroke(&fb, [2, 2]);
        ink.lift();
        ink.stroke(&fb, [12, 8]);
        // The host drew over the first dot: its pixels are no longer ours to restore.
        let left = Rect { x: 0, y: 0, w: 6, h: 6 };
        ink.overwritten(&fb, left);
        for y in 0..6 {
            for x in 0..6 {
                px[(y * W + x) as usize] = 0xff11_1111;
            }
        }
        let area = ink.restore(&fb).unwrap();
        assert_eq!(area, Rect { x: 11, y: 7, w: 3, h: 3 });
        assert_eq!(px[(2 * W + 2) as usize], 0xff11_1111);
        assert_eq!(px[(8 * W + 12) as usize], 0xffff_ffff);
    }

    #[test]
    fn feed_follows_the_tip() {
        let ev = |kind, code, value| Event { kind, code, value };
        let mut ink = Ink::new(true);
        let down = [ev(EV_ABS, ABS_X, 7), ev(EV_ABS, ABS_Y, 9), ev(EV_KEY, BTN_TOUCH, 1)];
        assert_eq!(ink.feed(&down, false), Some([7, 9]));
        assert_eq!(ink.feed(&[ev(EV_KEY, BTN_TOUCH, 0)], false), None);
        // Mouse mode: only the left button inks.
        assert_eq!(ink.feed(&[ev(EV_KEY, BTN_LEFT, 1)], true), Some([7, 9]));
        ink.on = false;
        assert_eq!(ink.feed(&[], true), None);
    }
}
