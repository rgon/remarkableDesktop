//! Streams a captured screen to the tablet: only the changed rectangle of
//! the newest frame, and only once the tablet acknowledged the previous one,
//! so no queue (and no latency) builds up on the slow e-ink side.

use crate::capture::{Capture, Source};
use remvnc_proto::{Msg, gray4};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Shared {
    /// What to capture; `generation` bumps on every change.
    source: Option<Source>,
    generation: u64,
    /// Newest frame as gray levels, and the generation it belongs to.
    frame: Option<(u64, Vec<u8>)>,
    fresh: bool,
    acked: bool,
    /// Set when a capture (re)started; carries the virtual monitor's name.
    started: Option<Option<String>>,
    stop: bool,
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

impl Streamer {
    /// `logical` is the tablet's screen as the user sees it; frames are
    /// produced in panel portrait for `rotation`.
    pub fn start(logical: (u32, u32), rotation: u16, mut send: Send) -> Streamer {
        let shared = Arc::new((Mutex::new(Shared { acked: true, ..Default::default() }), Condvar::new()));

        let cap_shared = shared.clone();
        let capture = std::thread::spawn(move || {
            let (lock, cv) = &*cap_shared;
            let mut current: Option<(u64, Capture)> = None;
            let mut raw = vec![];
            loop {
                let (wanted, generation) = {
                    let s = lock.lock().unwrap();
                    if s.stop {
                        return;
                    }
                    (s.source.clone(), s.generation)
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
                                eprintln!("remvnc-host: capture {src:?}: {e}");
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
                        let levels: Vec<u8> = raw.iter().map(|&v| gray4::level(v)).collect();
                        let mut s = lock.lock().unwrap();
                        s.frame = Some((generation, levels));
                        s.fresh = true;
                        cv.notify_all();
                    }
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("remvnc-host: capture ended: {e}");
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
                let (generation, frame) = {
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
                lock.lock().unwrap().acked = false;
                let msg = Msg::Rect { id, x: x as u16, y: y as u16, w: rw as u16, h: rh as u16, data };
                if send(&msg).is_err() {
                    return;
                }
                last = Some((generation, frame));
            }
        });
        Streamer { shared, threads: vec![capture, sender] }
    }

    /// Switches what is captured (None pauses capture).
    pub fn set_source(&self, source: Option<Source>) {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap();
        s.source = source;
        s.generation += 1;
        s.acked = true;
        cv.notify_all();
    }

    pub fn ack(&self) {
        let (lock, cv) = &*self.shared;
        lock.lock().unwrap().acked = true;
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
    fn bounding_box_of_changes() {
        let a = vec![0u8; 8 * 4];
        let mut b = a.clone();
        b[1 * 8 + 3] = 5;
        b[2 * 8 + 5] = 5;
        assert_eq!(changed(&a, &b, 8, 4), Some((2, 1, 4, 2)));
        assert_eq!(changed(&a, &a, 8, 4), None);
    }
}
