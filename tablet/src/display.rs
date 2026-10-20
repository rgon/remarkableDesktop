//! Showing the host's screen: `Rect` updates are written into the panel
//! buffer and refreshed at once, in the fast mode or, when the host marks
//! them, in the quality mode that clears the fast mode's ghosting. The host
//! decides which and when; it knows what changed and what has been still.

use crate::epd::{Epd, FORMAT_GRAYSCALE8, FORMAT_RGB16, ImageInfo, Mode};
use remdesktop_proto::{RECT_BITS1, RECT_CLEAR, RECT_SETTLE, bits1, gray4};
use std::time::{Duration, Instant};

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

/// What the clear sequence needs of the panel: the framebuffer and the
/// refresh call. A trait so the order of the steps can be tested.
trait Panel {
    /// The pixels of `r`, as bytes of the framebuffer.
    fn save(&mut self, r: Rect) -> Vec<u8>;
    fn fill(&mut self, r: Rect, gray: u8);
    fn restore(&mut self, r: Rect, saved: &[u8]);
    /// Refreshes `r` and waits for the engine to take it.
    fn swap(&mut self, r: Rect, mode: Mode);
}

/// Flashes a rectangle black, then white, then draws it again, each step in
/// the flashing full-quality mode, which drives every pixel that differs
/// from the step before: black first, because a white fill over white is
/// not a drive. The rectangle already holds the frame (the host sent it with
/// the clear), which is put back after the flashes. Nothing else writes the
/// framebuffer meanwhile: the host sends no other update until this one is
/// acked, so the saved pixels are still the newest ones at the end.
fn clear_rect(p: &mut impl Panel, r: Rect) {
    let saved = p.save(r);
    for gray in [0, 255] {
        p.fill(r, gray);
        p.swap(r, Mode::Content);
    }
    p.restore(r, &saved);
    p.swap(r, Mode::Content);
}

fn bytes_per_pixel(fb: &ImageInfo) -> usize {
    match fb.format {
        FORMAT_GRAYSCALE8 => 1,
        FORMAT_RGB16 => 2,
        _ => 4,
    }
}

/// The real panel: the framebuffer and the engine's refresh.
struct EpdPanel<'a>(&'a Epd);

impl Panel for EpdPanel<'_> {
    fn save(&mut self, r: Rect) -> Vec<u8> {
        save_rect(&self.0.fb, r)
    }

    fn fill(&mut self, r: Rect, gray: u8) {
        unsafe { self.0.fb.fill_gray(r.x, r.y, r.x + r.w, r.y + r.h, gray) };
    }

    fn restore(&mut self, r: Rect, saved: &[u8]) {
        restore_rect(&self.0.fb, r, saved);
    }

    fn swap(&mut self, r: Rect, mode: Mode) {
        self.0.swap(r.x, r.y, r.w, r.h, mode, 0);
        self.0.process_events(0);
    }
}

/// Copies the rows of `r` out of the framebuffer (`r` is inside it: the
/// decoder checked).
fn save_rect(fb: &ImageInfo, r: Rect) -> Vec<u8> {
    let (bpp, row) = (bytes_per_pixel(fb), r.w as usize * bytes_per_pixel(fb));
    let mut out = Vec::with_capacity(row * r.h as usize);
    for y in r.y..r.y + r.h {
        let line = unsafe { fb.pixels.add(y as usize * fb.stride as usize + r.x as usize * bpp) };
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(line, row) });
    }
    out
}

fn restore_rect(fb: &ImageInfo, r: Rect, saved: &[u8]) {
    let (bpp, row) = (bytes_per_pixel(fb), r.w as usize * bytes_per_pixel(fb));
    for (i, y) in (r.y..r.y + r.h).enumerate() {
        let line = unsafe { fb.pixels.add(y as usize * fb.stride as usize + r.x as usize * bpp) };
        unsafe { std::ptr::copy_nonoverlapping(saved[i * row..][..row].as_ptr(), line, row) };
    }
}

/// Refreshes screen updates in the mode the host asked for. Runs on the
/// input thread.
pub struct Display {
    refresh: Refresh,
    pub frames: u32,
    /// Time spent in the engine's refresh calls, logged every few seconds.
    timing: Timing,
}

struct Timing {
    since: Instant,
    calls: u32,
    total: Duration,
    max: Duration,
}

impl Display {
    pub fn new(refresh: Refresh) -> Self {
        Self { refresh, frames: 0, timing: Timing { since: Instant::now(), calls: 0, total: Duration::ZERO, max: Duration::ZERO } }
    }

    /// The mode for updates that must show up at once.
    pub fn fast(&self) -> Mode {
        self.refresh.fast
    }

    /// The mode that clears the fast mode's ghosting.
    pub fn settle(&self) -> Mode {
        self.refresh.settle
    }

    /// Refreshes freshly written pixels, in the quality mode if `flags` has
    /// `RECT_SETTLE`; with `RECT_CLEAR` the rectangle is flashed clean first.
    pub fn show(&mut self, epd: &Epd, r: Rect, flags: u8) {
        let started = Instant::now();
        if flags & RECT_CLEAR != 0 {
            clear_rect(&mut EpdPanel(epd), r);
        } else {
            let mode = if flags & RECT_SETTLE != 0 { self.refresh.settle } else { self.refresh.fast };
            epd.swap(r.x, r.y, r.w, r.h, mode, 0);
            epd.process_events(0);
        }
        self.frames += 1;
        // How long the engine keeps us in a refresh call (the panel's pace).
        let took = started.elapsed();
        let t = &mut self.timing;
        t.calls += 1;
        t.total += took;
        t.max = t.max.max(took);
        if t.since.elapsed() >= Duration::from_secs(2) {
            log!("remdesktop: {} refreshes in {:?}: {:.0} ms each on average, {:.0} ms at most", t.calls, t.since.elapsed(), t.total.as_secs_f64() * 1000.0 / t.calls as f64, t.max.as_secs_f64() * 1000.0);
            *t = Timing { since: Instant::now(), calls: 0, total: Duration::ZERO, max: Duration::ZERO };
        }
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

    /// Records what the clear sequence does.
    struct Fake {
        fb: Vec<u8>,
        log: Vec<String>,
    }

    impl Panel for Fake {
        fn save(&mut self, r: Rect) -> Vec<u8> {
            self.log.push("save".into());
            vec![self.fb[(r.y * 4 + r.x) as usize]]
        }
        fn fill(&mut self, r: Rect, gray: u8) {
            self.log.push(format!("fill {gray}"));
            self.fb[(r.y * 4 + r.x) as usize] = gray;
        }
        fn restore(&mut self, r: Rect, saved: &[u8]) {
            self.log.push("restore".into());
            self.fb[(r.y * 4 + r.x) as usize] = saved[0];
        }
        fn swap(&mut self, _: Rect, mode: Mode) {
            self.log.push(format!("swap {mode:?} {}", self.fb[0]));
        }
    }

    #[test]
    fn a_clear_flashes_black_then_white_and_puts_the_frame_back() {
        let mut p = Fake { fb: vec![77; 16], log: vec![] };
        clear_rect(&mut p, Rect { x: 0, y: 0, w: 1, h: 1 });
        assert_eq!(p.log, ["save", "fill 0", "swap Content 0", "fill 255", "swap Content 255", "restore", "swap Content 77"]);
        assert_eq!(p.fb[0], 77);
    }

    #[test]
    fn a_rect_is_saved_and_restored_with_its_stride() {
        // 6 × 3 RGB32 pixels with a padded stride; a 2 × 2 window at (3, 1).
        let (w, h, stride) = (6usize, 3usize, 6 * 4 + 8);
        let mut mem: Vec<u8> = (0..stride * h).map(|i| i as u8).collect();
        let fb = ImageInfo { pixels: mem.as_mut_ptr(), width: w as i32, height: h as i32, stride: stride as i32, format: crate::epd::FORMAT_RGB32 };
        let r = Rect { x: 3, y: 1, w: 2, h: 2 };
        let saved = save_rect(&fb, r);
        assert_eq!(saved.len(), 2 * 2 * 4);
        assert_eq!(saved[0], (stride + 12) as u8);
        assert_eq!(saved[8], (2 * stride + 12) as u8);
        unsafe { fb.fill_gray(3, 1, 5, 3, 0) };
        assert_eq!(mem[stride + 12], 0);
        let neighbour = mem[stride + 20];
        restore_rect(&fb, r, &saved);
        assert_eq!(mem[stride + 12], (stride + 12) as u8);
        assert_eq!(mem[stride + 20], neighbour);
        // 16-bit and 8-bit buffers are told apart.
        let mut g = vec![0u8; 4];
        let fb8 = ImageInfo { pixels: g.as_mut_ptr(), width: 4, height: 1, stride: 4, format: FORMAT_GRAYSCALE8 };
        assert_eq!(save_rect(&fb8, Rect { x: 1, y: 0, w: 2, h: 1 }).len(), 2);
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
