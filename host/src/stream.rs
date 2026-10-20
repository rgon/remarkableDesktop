//! Streams a captured screen to the tablet: only the changed tiles of the
//! newest frame (as a few rectangles, smallest first), and only once the
//! tablet acknowledged the previous update, so no queue (and no latency)
//! builds up on the slow e-ink side. Tiles that then stay still are sent
//! again for the tablet's quality refresh, which clears the fast mode's
//! ghosting; the host decides which tiles and when, as it knows what changed.

use crate::capture::{Capture, Source};
use crate::edges;
use crate::tiles::{self, Grid};
use remdesktop_proto::{DisplayMode, Msg, RECT_BITS1, RECT_LAST, RECT_SETTLE, bits1, gray4};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Shared {
    /// What to capture; `generation` bumps on every change.
    source: Option<Source>,
    /// Where the picture sits on the tablet (fractions of the logical
    /// screen); the rest is letterbox, painted white.
    shown: [f64; 4],
    generation: u64,
    /// What the tablet shows. Capture keeps running while it is `None`
    /// (stopping it would remove the virtual monitor); nothing is sent.
    mode: DisplayMode,
    /// Newest frame as gray levels, the epoch it belongs to (it changes with
    /// the source and the mode, and the next update is then a whole frame),
    /// when it was read from the capture pipeline, and the mode it was
    /// rendered for.
    frame: Option<Published>,
    /// Sequence number of the frame the sender took last.
    sent_seq: u64,
    fresh: bool,
    acked: bool,
    /// Set when a capture (re)started; carries the virtual monitor's name.
    started: Option<Option<String>>,
    stop: bool,
    /// The update in flight: id, capture time, size in bytes.
    in_flight: Option<(u32, Instant, usize)>,
    /// When the update in flight was sent, and how old its frame was then.
    sent: Option<(Instant, Duration)>,
    stats: Option<DisplayStats>,
}

/// A rendered frame: gray levels (4 bits used, one per byte), or, for the
/// edge view, one bit per pixel as the GPU packs it (`bits1`'s layout).
#[derive(Clone)]
enum Picture {
    Gray(Vec<u8>),
    Bits(Vec<u8>),
}

impl Picture {
    /// Whether it has the size of a `w`×`h` frame.
    fn fits(&self, w: usize, h: usize) -> bool {
        match self {
            Picture::Gray(v) => v.len() == w * h,
            Picture::Bits(v) => v.len() == bits1::row_bytes(w) * h,
        }
    }

    /// The tiles that differ from `prev` (all of them if it is another kind).
    fn dirty(&self, prev: &Picture, grid: &Grid) -> Vec<bool> {
        match (self, prev) {
            (Picture::Gray(a), Picture::Gray(b)) => grid.dirty(b, a),
            (Picture::Bits(a), Picture::Bits(b)) => grid.dirty_bits(b, a),
            _ => vec![true; grid.tiles()],
        }
    }

    /// The `rw`×`rh` window at (`x`, `y`) of a `w`-wide frame as it goes on
    /// the wire, and whether that is 1-bit (`RECT_BITS1`). Rectangles start
    /// on tile boundaries, so a bit window is whole bytes of the packed rows.
    fn pack(&self, w: usize, x: usize, y: usize, rw: usize, rh: usize) -> (bool, Vec<u8>) {
        match self {
            Picture::Gray(v) => (false, gray4::pack(v, w, x, y, rw, rh)),
            Picture::Bits(v) if x % 8 == 0 => {
                let (rb, wb) = (bits1::row_bytes(w), bits1::row_bytes(rw));
                let mut out = Vec::with_capacity(wb * rh);
                for r in 0..rh {
                    out.extend_from_slice(&v[(y + r) * rb + x / 8..][..wb]);
                }
                (true, out)
            }
            // Not on a byte boundary (never from the tile grid): the slow way.
            Picture::Bits(v) => {
                let rb = bits1::row_bytes(w);
                let levels: Vec<u8> = (0..w * (y + rh)).map(|i| if v[(i / w) * rb + (i % w) / 8] >> (7 - (i % w) % 8) & 1 != 0 { 15 } else { 0 }).collect();
                (true, bits1::pack(&levels, w, x, y, rw, rh))
            }
        }
    }
}

/// A frame as the capture thread hands it over.
#[derive(Clone)]
struct Published {
    epoch: u64,
    picture: Picture,
    captured: Instant,
    /// Counts the frames made.
    seq: u64,
    /// The tiles that differ from frame number `.0`, as the GPU found out
    /// (known only when frames came out of it one after the other).
    dirty: Option<(u64, Vec<bool>)>,
}

/// Capture → tablet-ack latency of screen updates, reported every 2 s.
#[derive(Default)]
struct DisplayStats {
    since: Option<Instant>,
    latencies_ms: Vec<f64>,
    /// Capture → sent, and sent → ack: where the latency goes.
    ages_ms: Vec<f64>,
    acks_ms: Vec<f64>,
    bytes: usize,
    pixels: usize,
}

impl DisplayStats {
    fn record(&mut self, latency: Duration, bytes: usize, age: Duration, ack: Duration) {
        let since = *self.since.get_or_insert_with(Instant::now);
        self.latencies_ms.push(latency.as_secs_f64() * 1000.0);
        self.ages_ms.push(age.as_secs_f64() * 1000.0);
        self.acks_ms.push(ack.as_secs_f64() * 1000.0);
        self.bytes += bytes;
        if since.elapsed() >= Duration::from_secs(2) {
            let n = self.latencies_ms.len();
            let pct = |v: &mut Vec<f64>, p: f64| {
                v.sort_by(|a, b| a.total_cmp(b));
                v[((n - 1) as f64 * p) as usize]
            };
            eprintln!(
                "remdesktop-host: display {:.1}/s, {:.0} KB/update ({:.0} kpx), capture→ack p50 {:.1} p99 {:.1} max {:.1} ms (capture→sent p50 {:.1}, sent→ack p50 {:.1} p99 {:.1})",
                n as f64 / since.elapsed().as_secs_f64(),
                self.bytes as f64 / n as f64 / 1024.0,
                self.pixels as f64 / n as f64 / 1000.0,
                pct(&mut self.latencies_ms, 0.5),
                pct(&mut self.latencies_ms, 0.99),
                pct(&mut self.latencies_ms, 1.0),
                pct(&mut self.ages_ms, 0.5),
                pct(&mut self.acks_ms, 0.5),
                pct(&mut self.acks_ms, 0.99),
            );
            *self = DisplayStats::default();
        }
    }
}

pub struct Streamer {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

pub type Send = Box<dyn FnMut(&Msg) -> std::io::Result<()> + std::marker::Send>;

/// Paints the letterbox bars white (GStreamer pads with black). Only rows
/// and columns that are entirely black and within the expected bar size
/// (plus rounding slack) are touched, so dark content is left alone.
fn whiten_bars(frame: &mut [u8], logical: (u32, u32), rotation: u16, shown: [f64; 4]) {
    let (lw, lh) = (logical.0 as f64, logical.1 as f64);
    // Bar thickness in logical pixels: (left/right, top/bottom).
    let bars = (((shown[0] * lw).round() as usize + 2), ((shown[2] * lh).round() as usize + 2));
    if bars == (2, 2) {
        return;
    }
    // In the panel's portrait layout a 90°/270° rotation swaps the axes.
    let (pw, ph) = if rotation % 180 == 90 { (logical.1 as usize, logical.0 as usize) } else { (logical.0 as usize, logical.1 as usize) };
    let (bar_x, bar_y) = if rotation % 180 == 90 { (bars.1, bars.0) } else { bars };
    if frame.len() != pw * ph {
        return;
    }
    let row_black = |f: &[u8], y: usize| f[y * pw..][..pw].iter().all(|&v| v == 0);
    let col_black = |f: &[u8], x: usize| (0..ph).all(|y| f[y * pw + x] == 0);
    for y in (0..bar_y.min(ph)).take_while(|&y| row_black(frame, y)).collect::<Vec<_>>() {
        frame[y * pw..][..pw].fill(255);
    }
    for y in (ph.saturating_sub(bar_y)..ph).rev().take_while(|&y| row_black(frame, y)).collect::<Vec<_>>() {
        frame[y * pw..][..pw].fill(255);
    }
    for x in (0..bar_x.min(pw)).take_while(|&x| col_black(frame, x)).collect::<Vec<_>>() {
        (0..ph).for_each(|y| frame[y * pw + x] = 255);
    }
    for x in (pw.saturating_sub(bar_x)..pw).rev().take_while(|&x| col_black(frame, x)).collect::<Vec<_>>() {
        (0..ph).for_each(|y| frame[y * pw + x] = 255);
    }
}

impl Streamer {
    /// `logical` is the tablet's screen as the user sees it; frames are
    /// produced in panel portrait for `rotation`.
    pub fn start(logical: (u32, u32), rotation: u16, stats: bool, settle_after: Duration, mut send: Send) -> Streamer {
        let stats = stats.then(DisplayStats::default);
        let shared = Arc::new((Mutex::new(Shared { acked: true, stats, ..Default::default() }), Condvar::new()));

        let cap_shared = shared.clone();
        let capture = std::thread::spawn(move || {
            let (lock, cv) = &*cap_shared;
            let mut current: Option<(u64, Capture)> = None;
            let mut raw = vec![];
            let mut detector = edges::Detector::new();
            // `raw` holds a frame of this generation that is not yet rendered.
            let mut raw_new = false;
            let mut raw_for: Option<u64> = None;
            let mut rendered_as: Option<(u64, DisplayMode)> = None;
            let mut epoch = 0u64;
            let mut captured_at = Instant::now();
            // Frames made, and what the GPU says changed since the one the
            // sender last took: the changes of every frame since, joined.
            let mut seq = 0u64;
            let mut gpu_seq: Option<u64> = None;
            let mut acc: Vec<bool> = vec![];
            let mut acc_base: Option<u64> = None;
            // When the last frame was rendered.
            let mut rendered_at = Instant::now();
            loop {
                let (wanted, generation, shown, mode) = {
                    let s = lock.lock().unwrap();
                    if s.stop {
                        return;
                    }
                    (s.source.clone(), s.generation, s.shown, s.mode)
                };
                if current.as_ref().map(|c| c.0) != Some(generation) {
                    current = None; // stops the old stream (and removes a virtual monitor)
                    if let Some(src) = &wanted {
                        match Capture::start(src, logical, rotation) {
                            Ok(c) => {
                                let mut s = lock.lock().unwrap();
                                s.started = Some(c.virtual_name.clone());
                                current = Some((generation, c));
                            }
                            Err(e) => {
                                eprintln!("remdesktop-host: capture {src:?}: {e}");
                                std::thread::sleep(Duration::from_secs(1));
                                continue;
                            }
                        }
                    } else {
                        let s = lock.lock().unwrap();
                        let _ = cv.wait_timeout(s, Duration::from_millis(200));
                        continue;
                    }
                }
                let (_, cap) = current.as_mut().unwrap();
                // A frame waiting for the sender to be free is polled for more often.
                let wait = if raw_new { Duration::from_millis(8) } else { Duration::from_millis(100) };
                match cap.next_frame(&mut raw, wait) {
                    Ok(true) => {
                        captured_at = Instant::now();
                        whiten_bars(&mut raw, logical, rotation, shown);
                        raw_for = Some(generation);
                        raw_new = true;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("remdesktop-host: capture ended: {e}");
                        current = None;
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
                // Render a new frame, or the last one again after a mode change
                // (Mutter sends frames only when the screen changes).
                let wanted_look = (generation, mode);
                // The panel takes a few updates per second and the sender
                // drops what it cannot send, so a frame is only worth
                // rendering when the sender is free (or it has been quite a
                // while): the screen changes at every pointer move, and
                // rendering all of them would load the CPU and the GPU the
                // compositor shares for nothing.
                let sender_free = {
                    let s = lock.lock().unwrap();
                    s.acked && !s.fresh
                };
                let render_new = raw_new && (sender_free || rendered_at.elapsed() >= Duration::from_millis(250));
                if raw_for == Some(generation) && (render_new || rendered_as != Some(wanted_look)) {
                    rendered_at = Instant::now();
                    if rendered_as != Some(wanted_look) {
                        rendered_as = Some(wanted_look);
                        epoch += 1;
                    }
                    raw_new = false;
                    let (pw, ph) = if rotation % 180 == 90 { (logical.1 as usize, logical.0 as usize) } else { (logical.0 as usize, logical.1 as usize) };
                    let (picture, on_gpu, tiles_changed) = match mode {
                        DisplayMode::Edge => {
                            let d = detector.detect(&raw, pw, ph);
                            (Picture::Bits(d.bits), d.on_gpu, d.dirty)
                        }
                        _ => (Picture::Gray(raw.iter().map(|&v| gray4::level(v)).collect()), false, None),
                    };
                    seq += 1;
                    let mut s = lock.lock().unwrap();
                    let mut dirty = None;
                    match tiles_changed {
                        // Only against the frame right before, which the GPU also made.
                        Some(d) if gpu_seq == Some(seq - 1) => {
                            if acc_base != Some(s.sent_seq) {
                                // The sender took a frame since: count from it, if it was the one before.
                                if s.sent_seq == seq - 1 {
                                    acc.clear();
                                    acc.resize(d.len(), false);
                                    acc_base = Some(s.sent_seq);
                                } else {
                                    acc_base = None;
                                }
                            }
                            if let Some(base) = acc_base {
                                acc.iter_mut().zip(&d).for_each(|(a, &b)| *a |= b);
                                dirty = Some((base, acc.clone()));
                            }
                        }
                        _ => acc_base = None,
                    }
                    gpu_seq = on_gpu.then_some(seq);
                    s.frame = Some(Published { epoch, picture, captured: captured_at, seq, dirty });
                    s.fresh = true;
                    cv.notify_all();
                }
            }
        });

        let send_shared = shared.clone();
        let sender = std::thread::spawn(move || {
            let (lock, cv) = &*send_shared;
            let (pw, ph) = if rotation % 180 == 90 { (logical.1, logical.0) } else { logical };
            let (w, h) = (pw as usize, ph as usize);
            let grid = Grid::new(w, h);
            // The epoch, the content and the sequence number of the frame sent last.
            let mut last: Option<(u64, Picture, u64)> = None;
            // When each tile last changed, until it got its quality refresh.
            let mut unsettled: Vec<Option<Instant>> = vec![None; grid.tiles()];
            let mut id = 0u32;
            // Sends the rectangles of one update, smallest first (so a pen or
            // cursor reaches the panel before bulk changes); the last one is
            // acked. `captured` is when the frame was read, for the latency
            // stats (None: not reported). False if the link broke.
            let mut send_rects = |rects: &[tiles::Rect], frame: &Picture, flags: u8, captured: Option<Instant>| -> bool {
                let (mut pixels, mut bytes) = (0, 0);
                let msgs: Vec<Msg> = rects
                    .iter()
                    .enumerate()
                    .map(|(i, &(x, y, rw, rh))| {
                        let (bits, packed) = frame.pack(w, x, y, rw, rh);
                        let flags = if bits { flags | RECT_BITS1 } else { flags };
                        let flags = if i + 1 == rects.len() { flags | RECT_LAST } else { flags };
                        let data = lz4_flex::block::compress_prepend_size(&packed);
                        id = id.wrapping_add(1);
                        pixels += rw * rh;
                        bytes += data.len();
                        Msg::Rect { id, x: x as u16, y: y as u16, w: rw as u16, h: rh as u16, flags, data }
                    })
                    .collect();
                {
                    let mut s = lock.lock().unwrap();
                    s.acked = false;
                    s.in_flight = captured.map(|c| (id, c, bytes));
                    // Before the first byte goes out, so a quick ack cannot beat it.
                    s.sent = captured.map(|c| (Instant::now(), c.elapsed()));
                    if let (Some(st), Some(_)) = (&mut s.stats, captured) {
                        st.pixels += pixels;
                    }
                }
                msgs.iter().all(|m| send(m).is_ok())
            };
            loop {
                let settle_at = unsettled.iter().flatten().min().map(|t| *t + settle_after);
                enum Work {
                    Frame(Published),
                    Settle,
                }
                let work = {
                    let mut s = lock.lock().unwrap();
                    loop {
                        if s.stop {
                            return;
                        }
                        if s.fresh && s.acked {
                            s.fresh = false;
                            if s.mode == DisplayMode::None {
                                // Nothing is sent; whatever comes after starts from a whole frame.
                                last = None;
                                unsettled.fill(None);
                                continue;
                            }
                            let frame = s.frame.clone().unwrap();
                            s.sent_seq = frame.seq;
                            break Work::Frame(frame);
                        }
                        let now = Instant::now();
                        match settle_at {
                            Some(t) if s.acked && now >= t => break Work::Settle,
                            Some(t) if s.acked => s = cv.wait_timeout(s, t - now).unwrap().0,
                            _ => s = cv.wait(s).unwrap(),
                        }
                    }
                };
                match work {
                    Work::Frame(Published { epoch: generation, picture: frame, captured, seq, dirty: gpu_dirty }) => {
                        if !frame.fits(w, h) {
                            continue;
                        }
                        // A new source starts from a full frame. The GPU's
                        // answer is good when it is against the frame we sent last.
                        let dirty = match (&last, gpu_dirty) {
                            (Some((g, _, sent)), Some((base, d))) if *g == generation && *sent == base && d.len() == grid.tiles() => d,
                            (Some((g, prev, _)), _) if *g == generation => frame.dirty(prev, &grid),
                            _ => vec![true; grid.tiles()],
                        };
                        let rects = if dirty.iter().all(|&d| d) { vec![(0, 0, w, h)] } else { grid.rects(&dirty) };
                        if rects.is_empty() {
                            // Same picture as the one sent last.
                            if let Some(l) = &mut last {
                                l.2 = seq;
                            }
                            continue;
                        }
                        if !send_rects(&rects, &frame, 0, Some(captured)) {
                            return;
                        }
                        let now = Instant::now();
                        for (t, _) in dirty.iter().enumerate().filter(|(_, d)| **d) {
                            unsettled[t] = Some(now);
                        }
                        last = Some((generation, frame, seq));
                    }
                    Work::Settle => {
                        let Some((_, frame, _)) = &last else {
                            unsettled.fill(None);
                            continue;
                        };
                        let now = Instant::now();
                        let due: Vec<bool> = unsettled.iter().map(|t| t.is_some_and(|t| now >= t + settle_after)).collect();
                        let mode = lock.lock().unwrap().mode;
                        if mode == DisplayMode::None {
                            continue;
                        }
                        let rects = grid.rects(&due);
                        if !send_rects(&rects, frame, RECT_SETTLE, None) {
                            return;
                        }
                        for (t, _) in due.iter().enumerate().filter(|(_, d)| **d) {
                            unsettled[t] = None;
                        }
                    }
                }
            }
        });
        Streamer { shared, threads: vec![capture, sender] }
    }

    /// Switches what is captured (None pauses capture); `shown` is where the
    /// picture lands on the tablet (see `mapping::letterbox`).
    pub fn set_source(&self, source: Option<Source>, shown: [f64; 4]) {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap();
        s.source = source;
        s.shown = shown;
        s.generation += 1;
        s.acked = true;
        cv.notify_all();
    }

    /// Switches between the screen, its edges, and nothing at all.
    pub fn set_mode(&self, mode: DisplayMode) {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap();
        s.mode = mode;
        s.acked = true;
        cv.notify_all();
    }

    pub fn ack(&self, id: u32) {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap();
        let sent_at = s.sent.take();
        if let Some((sent, captured, bytes)) = s.in_flight.take() {
            if sent == id {
                if let (Some(st), Some((at, age))) = (&mut s.stats, sent_at) {
                    st.record(captured.elapsed(), bytes, age, at.elapsed());
                }
            }
        }
        s.acked = true;
        cv.notify_all();
    }

    /// Once per capture start: `Some(virtual monitor name)`.
    pub fn take_started(&self) -> Option<Option<String>> {
        self.shared.0.lock().unwrap().started.take()
    }
}

impl Drop for Streamer {
    fn drop(&mut self) {
        {
            let (lock, cv) = &*self.shared;
            lock.lock().unwrap().stop = true;
            cv.notify_all();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn whitens_only_black_bars() {
        // 4x6 portrait panel, logical 6x4 rotated 90°, picture letterboxed
        // left/right in logical = top/bottom rows in portrait.
        let mut f = vec![0u8; 4 * 6];
        f[2 * 4..4 * 4].fill(100); // content rows 2..4
        super::whiten_bars(&mut f, (6, 4), 90, [0.25, 0.75, 0.0, 1.0]);
        assert!(f[..2 * 4].iter().all(|&v| v == 255));
        assert!(f[4 * 4..].iter().all(|&v| v == 255));
        assert!(f[2 * 4..4 * 4].iter().all(|&v| v == 100));
    }
}
