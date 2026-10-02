//! Streams a captured screen to the tablet: only the changed rectangle of
//! the newest frame, and only once the tablet acknowledged the previous one,
//! so no queue (and no latency) builds up on the slow e-ink side.

use crate::capture::{Capture, Source};
use remdesktop_proto::{Msg, gray4};
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
    /// Newest frame as gray levels, the generation it belongs to, and when
    /// it was read from the capture pipeline.
    frame: Option<(u64, Vec<u8>, Instant)>,
    fresh: bool,
    acked: bool,
    /// Set when a capture (re)started; carries the virtual monitor's name.
    started: Option<Option<String>>,
    stop: bool,
    /// The update in flight: id, capture time, size in bytes.
    in_flight: Option<(u32, Instant, usize)>,
    stats: Option<DisplayStats>,
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

/// Bounding box of differences between two same-sized images, as
/// (x, y, w, h), or None if identical.
fn changed(a: &[u8], b: &[u8], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let rows: Vec<usize> = (0..h).filter(|&r| a[r * w..][..w] != b[r * w..][..w]).collect();
    let (&y0, &y1) = (rows.first()?, rows.last()?);
    let (mut x0, mut x1) = (w, 0);
    for &r in &rows {
        let (ra, rb) = (&a[r * w..][..w], &b[r * w..][..w]);
        if let Some(i) = (0..x0).find(|&i| ra[i] != rb[i]) {
            x0 = i;
        }
        if let Some(i) = (x1..w).rev().find(|&i| ra[i] != rb[i]) {
            x1 = i;
        }
    }
    // Even x and width: whole bytes in the 4-bit packing.
    let x0 = x0 & !1;
    let x1 = (x1 + 2).min(w) & !1;
    Some((x0, y0, x1.max(x0 + 2).min(w) - x0, y1 + 1 - y0))
}

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
    pub fn start(logical: (u32, u32), rotation: u16, stats: bool, mut send: Send) -> Streamer {
        let stats = stats.then(DisplayStats::default);
        let shared = Arc::new((Mutex::new(Shared { acked: true, stats, ..Default::default() }), Condvar::new()));

        let cap_shared = shared.clone();
        let capture = std::thread::spawn(move || {
            let (lock, cv) = &*cap_shared;
            let mut current: Option<(u64, Capture)> = None;
            let mut raw = vec![];
            loop {
                let (wanted, generation, shown) = {
                    let s = lock.lock().unwrap();
                    if s.stop {
                        return;
                    }
                    (s.source.clone(), s.generation, s.shown)
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
                        let at = Instant::now();
                        whiten_bars(&mut raw, logical, rotation, shown);
                        let levels: Vec<u8> = raw.iter().map(|&v| gray4::level(v)).collect();
                        let mut s = lock.lock().unwrap();
                        s.frame = Some((generation, levels, at));
                        s.fresh = true;
                        cv.notify_all();
                    }
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("remdesktop-host: capture ended: {e}");
                        current = None;
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        });

        let send_shared = shared.clone();
        let sender = std::thread::spawn(move || {
            let (lock, cv) = &*send_shared;
            let (pw, ph) = if rotation % 180 == 90 { (logical.1, logical.0) } else { logical };
            let (w, h) = (pw as usize, ph as usize);
            let mut last: Option<(u64, Vec<u8>)> = None;
            let mut id = 0u32;
            loop {
                let (generation, frame, captured) = {
                    let mut s = lock.lock().unwrap();
                    while !(s.stop || (s.fresh && s.acked)) {
                        s = cv.wait(s).unwrap();
                    }
                    if s.stop {
                        return;
                    }
                    s.fresh = false;
                    s.frame.clone().unwrap()
                };
                if frame.len() != w * h {
                    continue;
                }
                // A new source starts from a full frame.
                let rect = match &last {
                    Some((g, prev)) if *g == generation => changed(prev, &frame, w, h),
                    _ => Some((0, 0, w, h)),
                };
                let Some((x, y, rw, rh)) = rect else { continue };
                let packed = gray4::pack(&frame, w, x, y, rw, rh);
                let data = lz4_flex::block::compress_prepend_size(&packed);
                id = id.wrapping_add(1);
                {
                    let mut s = lock.lock().unwrap();
                    s.acked = false;
                    s.in_flight = Some((id, captured, data.len()));
                    if let Some(st) = &mut s.stats {
                        st.pixels += rw * rh;
                    }
                }
                let msg = Msg::Rect { id, x: x as u16, y: y as u16, w: rw as u16, h: rh as u16, data };
                if send(&msg).is_err() {
                    return;
                }
                last = Some((generation, frame));
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
    use super::changed;

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

    #[test]
    fn bounding_box_of_changes() {
        let a = vec![0u8; 8 * 4];
        let mut b = a.clone();
        b[1 * 8 + 3] = 5;
        b[2 * 8 + 5] = 5;
        assert_eq!(changed(&a, &b, 8, 4), Some((2, 1, 4, 2)));
        assert_eq!(changed(&a, &a, 8, 4), None);
    }
}
