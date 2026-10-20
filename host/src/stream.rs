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
    stats: Option<DisplayStats>,
}

/// A frame as the capture thread hands it over.
#[derive(Clone)]
struct Published {
    epoch: u64,
    levels: Vec<u8>,
    captured: Instant,
    mode: DisplayMode,
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
    bytes: usize,
    pixels: usize,
}

impl DisplayStats {
    fn record(&mut self, latency: Duration, bytes: usize) {
        let since = *self.since.get_or_insert_with(Instant::now);
        self.latencies_ms.push(latency.as_secs_f64() * 1000.0);
        self.bytes += bytes;
        if since.elapsed() >= Duration::from_secs(2) {
            let n = self.latencies_ms.len();
            self.latencies_ms.sort_by(|a, b| a.total_cmp(b));
            let pct = |p: f64| self.latencies_ms[((n - 1) as f64 * p) as usize];
            eprintln!(
                "remdesktop-host: display {:.1}/s, {:.0} KB/update ({:.0} kpx), capture→ack p50 {:.1} p99 {:.1} max {:.1} ms",
                n as f64 / since.elapsed().as_secs_f64(),
                self.bytes as f64 / n as f64 / 1024.0,
                self.pixels as f64 / n as f64 / 1000.0,
                pct(0.5),
                pct(0.99),
                pct(1.0)
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
                match cap.next_frame(&mut raw, Duration::from_millis(100)) {
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
                if raw_for == Some(generation) && (raw_new || rendered_as != Some(wanted_look)) {
                    if rendered_as != Some(wanted_look) {
                        rendered_as = Some(wanted_look);
                        epoch += 1;
                    }
                    raw_new = false;
                    let (pw, ph) = if rotation % 180 == 90 { (logical.1 as usize, logical.0 as usize) } else { (logical.0 as usize, logical.1 as usize) };
                    let (levels, on_gpu, tiles_changed): (Vec<u8>, bool, Option<Vec<bool>>) = match mode {
                        DisplayMode::Edge => {
                            let d = detector.detect(&raw, pw, ph);
                            (d.pixels.iter().map(|&v| gray4::level(v)).collect(), d.on_gpu, d.dirty)
                        }
                        _ => (raw.iter().map(|&v| gray4::level(v)).collect(), false, None),
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
                    s.frame = Some(Published { epoch, levels, captured: captured_at, mode, seq, dirty });
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
            let mut last: Option<(u64, Vec<u8>, u64)> = None;
            // When each tile last changed, until it got its quality refresh.
            let mut unsettled: Vec<Option<Instant>> = vec![None; grid.tiles()];
            let mut id = 0u32;
            // Sends the rectangles of one update, smallest first (so a pen or
            // cursor reaches the panel before bulk changes); the last one is
            // acked. `captured` is when the frame was read, for the latency
            // stats (None: not reported). False if the link broke.
            let mut send_rects = |rects: &[tiles::Rect], frame: &[u8], mode: DisplayMode, flags: u8, captured: Option<Instant>| -> bool {
                let (mut pixels, mut bytes) = (0, 0);
                let msgs: Vec<Msg> = rects
                    .iter()
                    .enumerate()
                    .map(|(i, &(x, y, rw, rh))| {
                        let (flags, packed) = if mode == DisplayMode::Edge {
                            (flags | RECT_BITS1, bits1::pack(frame, w, x, y, rw, rh))
                        } else {
                            (flags, gray4::pack(frame, w, x, y, rw, rh))
                        };
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
                    Work::Frame(Published { epoch: generation, levels: frame, captured, mode, seq, dirty: gpu_dirty }) => {
                        if frame.len() != w * h {
                            continue;
                        }
                        // A new source starts from a full frame. The GPU's
                        // answer is good when it is against the frame we sent last.
                        let dirty = match (&last, gpu_dirty) {
                            (Some((g, _, sent)), Some((base, d))) if *g == generation && *sent == base && d.len() == grid.tiles() => d,
                            (Some((g, prev, _)), _) if *g == generation => grid.dirty(prev, &frame),
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
                        if !send_rects(&rects, &frame, mode, 0, Some(captured)) {
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
                        if !send_rects(&rects, frame, mode, RECT_SETTLE, None) {
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
        if let Some((sent, captured, bytes)) = s.in_flight.take() {
            if sent == id {
                if let Some(st) = &mut s.stats {
                    st.record(captured.elapsed(), bytes);
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
