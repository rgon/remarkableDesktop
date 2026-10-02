//! Showing the host's screen: `Rect` updates are written into the panel
//! buffer and refreshed in a fast mode at once; areas that then stay still
//! get a full-quality refresh, which clears the fast mode's ghosting.

use crate::epd::{Epd, Mode};
use remvnc_proto::gray4;
use std::time::{Duration, Instant};

/// How long an area must be still before its quality refresh.
const SETTLE: Duration = Duration::from_millis(450);

/// Refresh used while content changes.
const FAST: Mode = Mode::Animation;
/// Refresh once it settles.
const QUALITY: Mode = Mode::Content;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn union(self, o: Rect) -> Rect {
        let (x0, y0) = (self.x.min(o.x), self.y.min(o.y));
        let (x1, y1) = ((self.x + self.w).max(o.x + o.w), (self.y + self.h).max(o.y + o.h));
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    pub fn intersects(self, o: Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
}

pub struct Display {
    /// Level → RGB32 pixel.
    lut: [u32; 16],
    /// Updated in fast mode, waiting for a quality refresh.
    unsettled: Option<Rect>,
    last_update: Instant,
    pub frames: u32,
}

impl Display {
    pub fn new() -> Self {
        let lut = std::array::from_fn(|l| {
            let g = gray4::gray(l as u8) as u32;
            0xff00_0000 | g << 16 | g << 8 | g
        });
        Self { lut, unsettled: None, last_update: Instant::now(), frames: 0 }
    }

    /// Writes one update into the framebuffer (no refresh yet).
    pub fn write(&mut self, epd: &Epd, r: Rect, packed_lz4: &[u8]) -> Result<Rect, String> {
        let fb = epd.fb;
        if r.x < 0 || r.y < 0 || r.w <= 0 || r.h <= 0 || r.x + r.w > fb.width || r.y + r.h > fb.height {
            return Err(format!("update {r:?} outside the panel"));
        }
        let packed = lz4_flex::block::decompress_size_prepended(packed_lz4).map_err(|e| format!("lz4: {e}"))?;
        let rb = gray4::row_bytes(r.w as usize);
        if packed.len() != rb * r.h as usize {
            return Err(format!("update {r:?}: {} bytes, expected {}", packed.len(), rb * r.h as usize));
        }
        for row in 0..r.h as usize {
            let line = unsafe { fb.pixels.add((r.y as usize + row) * fb.stride as usize + r.x as usize * 4) as *mut u32 };
            let lut = &self.lut;
            gray4::unpack_row(&packed[row * rb..][..rb], r.w as usize, |i, l| unsafe { line.add(i).write(lut[l as usize]) });
        }
        Ok(r)
    }

    /// Refreshes freshly written pixels in the fast mode.
    pub fn show(&mut self, epd: &Epd, r: Rect) {
        epd.swap(r.x, r.y, r.w, r.h, FAST, 0);
        self.unsettled = Some(self.unsettled.map_or(r, |u| u.union(r)));
        self.last_update = Instant::now();
        self.frames += 1;
    }

    /// Gives settled areas their quality refresh. Call regularly.
    pub fn tick(&mut self, epd: &Epd) {
        if let Some(u) = self.unsettled {
            if self.last_update.elapsed() >= SETTLE {
                epd.swap(u.x, u.y, u.w, u.h, QUALITY, 0);
                self.unsettled = None;
            }
        }
    }

    /// Milliseconds until `tick` has work, for the poll timeout.
    pub fn next_tick_ms(&self) -> Option<i32> {
        self.unsettled.map(|_| SETTLE.saturating_sub(self.last_update.elapsed()).as_millis() as i32 + 1)
    }
}
