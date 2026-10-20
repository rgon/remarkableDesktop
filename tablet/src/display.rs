//! Showing the host's screen: `Rect` updates are written into the panel
//! buffer and refreshed at once, in the fast mode or, when the host marks
//! them, in the quality mode that clears the fast mode's ghosting. The host
//! decides which and when; it knows what changed and what has been still.

use crate::epd::{Epd, ImageInfo, Mode};
use remdesktop_proto::{RECT_BITS1, RECT_SETTLE, bits1, gray4};

/// Refresh modes for streamed content.
#[derive(Debug, Clone, Copy)]
pub struct Refresh {
    /// Used for updates as they change. `Pen` is the engine's
    /// lowest-latency (black/white) mode.
    pub fast: Mode,
    /// Used for the host's settle updates, to restore grays and clear
    /// ghosting. `Ui` doesn't flash, so it never blocks for long; `Content`
    /// is a flashing full-quality refresh.
    pub settle: Mode,
}

impl Default for Refresh {
    fn default() -> Self {
        Self { fast: Mode::Pen, settle: Mode::Ui }
    }
}

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

/// Writes `Rect` payloads into the framebuffer. Runs on the reader thread.
pub struct Decoder {
    /// Packed byte (two levels) → its two RGB32 pixels.
    pairs: [[u32; 2]; 256],
    /// Decompressed update, reused between updates.
    scratch: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        let px = |l: usize| {
            let g = gray4::gray(l as u8) as u32;
            0xff00_0000 | g << 16 | g << 8 | g
        };
        let pairs = std::array::from_fn(|b| [px(b >> 4), px(b & 15)]);
        Self { pairs, scratch: Vec::new() }
    }

    /// Writes one update into the framebuffer (no refresh yet).
    pub fn write(&mut self, fb: &ImageInfo, r: Rect, flags: u8, packed_lz4: &[u8]) -> Result<Rect, String> {
        if r.x < 0 || r.y < 0 || r.w <= 0 || r.h <= 0 || r.x + r.w > fb.width || r.y + r.h > fb.height {
            return Err(format!("update {r:?} outside the panel"));
        }
        let bits = flags & RECT_BITS1 != 0;
        let rb = if bits { bits1::row_bytes(r.w as usize) } else { gray4::row_bytes(r.w as usize) };
        let want = rb * r.h as usize;
        // lz4_flex's size-prepended format: the length, then the block.
        let (head, block) = packed_lz4.split_first_chunk::<4>().ok_or("lz4: truncated")?;
        let got = u32::from_le_bytes(*head) as usize;
        if got != want {
            return Err(format!("update {r:?}: {got} bytes, expected {want}"));
        }
        self.scratch.resize(want, 0);
        lz4_flex::block::decompress_into(block, &mut self.scratch).map_err(|e| format!("lz4: {e}"))?;
        let (w, pairs) = (r.w as usize, &self.pairs);
        for row in 0..r.h as usize {
            let line = unsafe { fb.pixels.add((r.y as usize + row) * fb.stride as usize + r.x as usize * 4) as *mut u32 };
            let packed = &self.scratch[row * rb..][..rb];
            if bits {
                for x in 0..w {
                    let white = packed[x / 8] >> (7 - x % 8) & 1 != 0;
                    unsafe { line.add(x).write_unaligned(if white { 0xffff_ffff } else { 0xff00_0000 }) };
                }
                continue;
            }
            for (i, &b) in packed.iter().enumerate() {
                let [hi, lo] = pairs[b as usize];
                unsafe {
                    line.add(2 * i).write_unaligned(hi);
                    if 2 * i + 1 < w {
                        line.add(2 * i + 1).write_unaligned(lo);
                    }
                }
            }
        }
        Ok(r)
    }
}

/// Refreshes screen updates in the mode the host asked for. Runs on the
/// input thread.
pub struct Display {
    refresh: Refresh,
    pub frames: u32,
}

impl Display {
    pub fn new(refresh: Refresh) -> Self {
        Self { refresh, frames: 0 }
    }

    /// The mode for updates that must show up at once.
    pub fn fast(&self) -> Mode {
        self.refresh.fast
    }

    /// The mode that clears the fast mode's ghosting.
    pub fn settle(&self) -> Mode {
        self.refresh.settle
    }

    /// Refreshes freshly written pixels, in the quality mode if `flags` has `RECT_SETTLE`.
    pub fn show(&mut self, epd: &Epd, r: Rect, flags: u8) {
        let mode = if flags & RECT_SETTLE != 0 { self.refresh.settle } else { self.refresh.fast };
        epd.swap(r.x, r.y, r.w, r.h, mode, 0);
        epd.process_events(0);
        self.frames += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_writes_odd_widths_and_rejects_bad_sizes() {
        let (w, h) = (5usize, 3usize);
        let levels: Vec<u8> = (0..w * h).map(|i| (i % 16) as u8).collect();
        let packed = gray4::pack(&levels, w, 0, 0, w, h);
        let lz4 = lz4_flex::block::compress_prepend_size(&packed);
        let mut px = vec![0u32; 8 * 4];
        let fb = ImageInfo { pixels: px.as_mut_ptr().cast(), width: 8, height: 4, stride: 8 * 4, format: crate::epd::FORMAT_RGB32 };
        let mut d = Decoder::new();
        let r = Rect { x: 2, y: 1, w: w as i32, h: h as i32 };
        assert_eq!(d.write(&fb, r, 0, &lz4), Ok(r));
        for y in 0..h {
            for x in 0..w {
                let g = gray4::gray(levels[y * w + x]) as u32;
                assert_eq!(px[(y + 1) * 8 + x + 2], 0xff00_0000 | g << 16 | g << 8 | g);
            }
            // The pixel after the row's last one is untouched.
            assert_eq!(px[(y + 1) * 8 + 2 + w], 0);
        }
        let wrong = Rect { w: 4, ..r };
        assert!(d.write(&fb, wrong, 0, &lz4).is_err());
        assert!(d.write(&fb, r, 0, &[1, 2]).is_err());
    }

    #[test]
    fn decoder_expands_one_bit_updates() {
        let (w, h) = (10usize, 2usize);
        let levels: Vec<u8> = (0..w * h).map(|i| if i % 3 == 0 { 15 } else { 0 }).collect();
        let lz4 = lz4_flex::block::compress_prepend_size(&bits1::pack(&levels, w, 0, 0, w, h));
        let mut px = vec![0u32; w * h];
        let fb = ImageInfo { pixels: px.as_mut_ptr().cast(), width: w as i32, height: h as i32, stride: w as i32 * 4, format: crate::epd::FORMAT_RGB32 };
        let r = Rect { x: 0, y: 0, w: w as i32, h: h as i32 };
        assert_eq!(Decoder::new().write(&fb, r, RECT_BITS1, &lz4), Ok(r));
        for i in 0..w * h {
            assert_eq!(px[i], if levels[i] > 7 { 0xffff_ffff } else { 0xff00_0000 });
        }
    }
}
