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
use std::collections::HashMap;

const BLACK: u32 = 0xff00_0000;
/// Brush radius in pixels (a 3 px square).
const BRUSH: i32 = 1;

#[derive(Default)]
pub struct Ink {
    pub on: bool,
    /// Panel pixel index (y * width + x) → the pixel the ink covers.
    under: HashMap<usize, u32>,
    /// Bounding box of `under`.
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
                self.under.entry(y as usize * fb.width as usize + x as usize).or_insert_with(|| unsafe { px.read_unaligned() });
                unsafe { px.write_unaligned(BLACK) };
            }
        }
    }

    /// The area drawn since the last call, to refresh.
    pub fn take_pending(&mut self) -> Option<Rect> {
        self.pending.take()
    }

    /// A streamed update overwrote `r`: the ink in it is gone.
    pub fn overwritten(&mut self, fb: &ImageInfo, r: Rect) {
        if !self.dirty.is_some_and(|d| d.intersects(r)) {
            return;
        }
        let w = fb.width as usize;
        self.under.retain(|&i, _| {
            let (x, y) = ((i % w) as i32, (i / w) as i32);
            !(x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
        });
        self.dirty = self.bounds(w);
    }

    fn bounds(&self, w: usize) -> Option<Rect> {
        let mut it = self.under.keys().map(|&i| ((i % w) as i32, (i / w) as i32));
        let (x, y) = it.next()?;
        let (x0, y0, x1, y1) = it.fold((x, y, x, y), |(a, b, c, d), (x, y)| (a.min(x), b.min(y), c.max(x), d.max(y)));
        Some(Rect { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1 })
    }

    /// Forgets the ink without touching the panel buffer (it was redrawn).
    pub fn forget(&mut self) {
        self.under.clear();
        self.dirty = None;
        self.pending = None;
        self.last = None;
    }

    /// Puts back everything the ink covered. Returns the area to refresh.
    pub fn restore(&mut self, fb: &ImageInfo) -> Option<Rect> {
        let w = fb.width as usize;
        for (&i, &old) in &self.under {
            let px = unsafe { fb.pixels.add((i / w) * fb.stride as usize + (i % w) * 4) as *mut u32 };
            unsafe { px.write_unaligned(old) };
        }
        let area = self.dirty;
        self.forget();
        area
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
