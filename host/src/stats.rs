//! Input rate and end-to-end latency (tablet kernel event → uinput write).

use remvnc_proto::monotonic_us;
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
        eprintln!("remvnc-host: {line}");
        self.since = now;
    }
}
