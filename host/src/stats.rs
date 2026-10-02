//! Input rate and end-to-end latency (tablet kernel event → uinput write).

use remdesktop_proto::monotonic_us;
use std::collections::HashMap;

/// Clock offset samples are kept from the lowest-RTT pongs in this window.
const OFFSET_WINDOW: usize = 50;
const REPORT_US: u64 = 2_000_000;

#[derive(Default)]
struct Device {
    frames: u32,
    latencies_us: Vec<i64>,
    max_gap_us: u64,
    last_us: u64,
}

pub struct Stats {
    enabled: bool,
    /// (rtt, tablet clock − host clock), newest last.
    probes: Vec<(u64, i64)>,
    devices: HashMap<u8, Device>,
    since: u64,
}

impl Stats {
    pub fn new(enabled: bool) -> Self {
        Self { enabled, probes: vec![], devices: HashMap::new(), since: monotonic_us() }
    }

    pub fn pong(&mut self, host_us: u64, tablet_us: u64) {
        let now = monotonic_us();
        let rtt = now.saturating_sub(host_us);
        // Assume symmetric paths: the tablet read its clock half an RTT after we sent.
        let offset = tablet_us as i64 - (host_us + rtt / 2) as i64;
        self.probes.push((rtt, offset));
        if self.probes.len() > OFFSET_WINDOW {
            self.probes.remove(0);
        }
    }

    /// Tablet clock − host clock, once known.
    pub fn clock_offset(&self) -> Option<i64> {
        self.offset().map(|o| o.1)
    }

    fn offset(&self) -> Option<(u64, i64)> {
        self.probes.iter().min_by_key(|p| p.0).copied()
    }

    pub fn frame(&mut self, device: u8, time_us: u64) {
        if !self.enabled {
            return;
        }
        let now = monotonic_us();
        let offset = self.offset();
        let d = self.devices.entry(device).or_default();
        d.frames += 1;
        if let Some((_, off)) = offset {
            d.latencies_us.push(now as i64 - (time_us as i64 - off));
        }
        // Gaps while moving; longer pauses are the hand being idle.
        let gap = time_us.saturating_sub(d.last_us);
        if d.last_us > 0 && gap < 100_000 {
            d.max_gap_us = d.max_gap_us.max(gap);
        }
        d.last_us = time_us;
        if now - self.since >= REPORT_US {
            self.report(now);
        }
    }

    fn report(&mut self, now: u64) {
        let secs = (now - self.since) as f64 / 1e6;
        let mut line = String::new();
        let mut ids: Vec<u8> = self.devices.keys().copied().collect();
        ids.sort();
        for id in ids {
            let d = self.devices.get_mut(&id).unwrap();
            let name = match id {
                1 => "pen",
                2 => "touch",
                _ => "dev",
            };
            d.latencies_us.sort();
            let pct = |p: f64| d.latencies_us.get(((d.latencies_us.len() - 1) as f64 * p) as usize).copied().unwrap_or(0) as f64 / 1000.0;
            if d.frames > 0 && !d.latencies_us.is_empty() {
                line += &format!(
                    "{name} {:.0}/s lat p50 {:.1} p99 {:.1} max {:.1} ms, max gap {:.1} ms | ",
                    d.frames as f64 / secs,
                    pct(0.5),
                    pct(0.99),
                    pct(1.0),
                    d.max_gap_us as f64 / 1000.0
                );
            }
            *d = Device { last_us: d.last_us, ..Default::default() };
        }
        if let Some((rtt, _)) = self.offset() {
            let worst = self.probes.iter().map(|p| p.0).max().unwrap_or(0);
            line += &format!("rtt min {:.1} max {:.1} ms", rtt as f64 / 1000.0, worst as f64 / 1000.0);
        }
        eprintln!("remdesktop-host: {line}");
        self.since = now;
    }
}

/// Measures pen latency over USB HID: tablet kernel event → this machine's
/// kernel event on the tablet's HID stylus device. The tablet sends one
/// marker (its event timestamp) per HID report it delivered; markers and
/// arrivals are paired in order.
pub struct HidProbe {
    tx: std::sync::mpsc::Sender<u64>,
}

/// Tablet clock − host clock, from the lowest-RTT ping, shared with the probe.
pub type Offset = std::sync::Arc<std::sync::Mutex<Option<i64>>>;

impl HidProbe {
    pub fn start(offset: Offset) -> Option<HidProbe> {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let dev = std::fs::read_dir("/sys/class/input").ok()?.flatten().find(|e| {
            e.file_name().to_string_lossy().starts_with("event")
                && std::fs::read_to_string(e.path().join("device/name")).is_ok_and(|n| n.trim() == "reMarkable 2 Stylus")
        })?;
        let path = format!("/dev/input/{}", dev.file_name().to_string_lossy());
        let mut file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("remdesktop-host: hid latency probe: {path}: {e}");
                return None;
            }
        };
        eprintln!("remdesktop-host: measuring USB HID pen latency on {path}");
        let clock: libc::c_int = libc::CLOCK_MONOTONIC;
        unsafe { libc::ioctl(file.as_raw_fd(), 0x4004_45a0, &clock) }; // EVIOCSCLOCKID
        let (tx, rx) = std::sync::mpsc::channel::<u64>();
        std::thread::spawn(move || {
            let mut markers = std::collections::VecDeque::new();
            let mut arrivals = std::collections::VecDeque::new();
            let mut lat: Vec<f64> = vec![];
            let (mut seen_m, mut seen_a, mut resyncs) = (0usize, 0usize, 0usize);
            let mut since = std::time::Instant::now();
            let mut buf = [0u8; 24 * 64];
            while let Ok(n) = file.read(&mut buf) {
                for ev in buf[..n].chunks_exact(24) {
                    let (kind, code) = (u16::from_ne_bytes([ev[16], ev[17]]), u16::from_ne_bytes([ev[18], ev[19]]));
                    if kind == 0 && code == 0 {
                        let sec = i64::from_ne_bytes(ev[0..8].try_into().unwrap());
                        let usec = i64::from_ne_bytes(ev[8..16].try_into().unwrap());
                        arrivals.push_back((sec * 1_000_000 + usec) as u64);
                        seen_a += 1;
                    }
                }
                let before = markers.len();
                markers.extend(rx.try_iter());
                seen_m += markers.len() - before;
                let Some(off) = *offset.lock().unwrap() else { continue };
                while let (Some(&m), Some(&a)) = (markers.front(), arrivals.front()) {
                    let l = a as i64 - (m as i64 - off);
                    if !(-5_000..200_000).contains(&l) {
                        // Out of step (a report or marker went missing): resync.
                        if resyncs < 3 {
                            eprintln!("remdesktop-host: hid probe resync: {:.1} ms apart", l as f64 / 1000.0);
                        }
                        resyncs += 1;
                        markers.clear();
                        arrivals.clear();
                        break;
                    }
                    lat.push(l as f64 / 1000.0);
                    markers.pop_front();
                    arrivals.pop_front();
                }
                if since.elapsed() >= std::time::Duration::from_secs(2) && lat.is_empty() {
                    eprintln!("remdesktop-host: hid probe: {seen_m} markers, {seen_a} reports, {resyncs} resyncs, offset known: {}", offset.lock().unwrap().is_some());
                    since = std::time::Instant::now();
                }
                if since.elapsed() >= std::time::Duration::from_secs(2) && !lat.is_empty() {
                    lat.sort_by(|a, b| a.total_cmp(b));
                    let p = |q: f64| lat[((lat.len() - 1) as f64 * q) as usize];
                    eprintln!(
                        "remdesktop-host: pen over USB HID {:.0}/s lat p50 {:.1} p99 {:.1} max {:.1} ms",
                        lat.len() as f64 / since.elapsed().as_secs_f64(),
                        p(0.5),
                        p(0.99),
                        p(1.0)
                    );
                    lat.clear();
                    since = std::time::Instant::now();
                }
            }
        });
        Some(HidProbe { tx })
    }

    pub fn marker(&self, tablet_us: u64) {
        let _ = self.tx.send(tablet_us);
    }
}
