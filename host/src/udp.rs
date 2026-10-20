//! The UDP path of pen and touch (see `remdesktop_proto::sync`): probes the tablet's
//! port, tells it over TCP once an answer came back, then applies the pen
//! datagrams to the virtual pen and touch screen.

use crate::{Devices, Writer, send, stats};
use remdesktop_proto::Datagram;
use remdesktop_proto::{Msg, PEN_DEVICE, TOUCH_DEVICE};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Give up (and stay on TCP) if no probe is answered by then.
const PROBE_FOR: Duration = Duration::from_millis(1500);
const PROBE_EVERY: Duration = Duration::from_millis(100);

pub struct Pen {
    pub tablet: SocketAddr,
    pub key: u32,
    pub devices: Devices,
    pub writer: Writer,
    pub stop: Arc<AtomicBool>,
    pub stats: Arc<Mutex<stats::Stats>>,
    /// Print datagram counters every 2 s.
    pub report: bool,
}

impl Pen {
    /// Repeats (time 0) carry no new event to time.
    fn frame(&self, device: u8, time_us: u64) {
        if time_us != 0 {
            self.stats.lock().unwrap().frame(device, time_us);
        }
    }

    fn report_counters(&self) {
        let mut devices = self.devices.lock().unwrap();
        for (name, id) in [("pen", PEN_DEVICE), ("touch", TOUCH_DEVICE)] {
            let Some(v) = devices.get_mut(&id) else { continue };
            let s = if id == PEN_DEVICE { std::mem::take(&mut v.pen.stats) } else { std::mem::take(&mut v.touch.stats) };
            if s.datagrams > 0 {
                eprintln!(
                    "remdesktop-host: {name} udp {} datagrams, {} lost, {} recovered from history, {} stale, {} steps replayed from tcp",
                    s.datagrams, s.lost, s.recovered, s.stale, s.replays
                );
            }
        }
    }
}

pub fn spawn(p: Pen) -> std::io::Result<std::thread::JoinHandle<()>> {
    let sock = UdpSocket::bind(("0.0.0.0", 0))?;
    sock.connect(p.tablet)?;
    sock.set_read_timeout(Some(Duration::from_millis(50)))?;
    remdesktop_proto::prioritize(sock.as_raw_fd());
    Ok(std::thread::spawn(move || run(sock, p)))
}

fn run(sock: UdpSocket, p: Pen) {
    let started = Instant::now();
    let mut probed: Option<Instant> = None;
    let mut ready = false;
    let mut reported = Instant::now();
    let mut buf = [0u8; 256];
    while !p.stop.load(Ordering::Relaxed) {
        if !ready {
            if started.elapsed() > PROBE_FOR {
                eprintln!("remdesktop-host: the tablet's udp port is unreachable; the pen stays on TCP");
                return;
            }
            if probed.is_none_or(|t| t.elapsed() >= PROBE_EVERY) {
                let _ = sock.send(&Datagram::Probe { key: p.key }.encode());
                probed = Some(Instant::now());
            }
        }
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(_) => continue, // timeout (or an ICMP refusal while probing)
        };
        if p.report && reported.elapsed() >= Duration::from_secs(2) {
            reported = Instant::now();
            p.report_counters();
        }
        match Datagram::decode(&buf[..n]) {
            Some(Datagram::Ack { key }) if key == p.key && !ready => {
                ready = true;
                eprintln!("remdesktop-host: pen over UDP");
                if send(&p.writer, &Msg::UdpReady).is_err() {
                    return;
                }
            }
            Some(Datagram::Pen { key, seq, time_us, states }) if key == p.key && ready => {
                let mut devices = p.devices.lock().unwrap();
                if let Some(v) = devices.get_mut(&PEN_DEVICE) {
                    let batches = v.pen.datagram(seq, &states);
                    v.emit(PEN_DEVICE, batches);
                }
                drop(devices);
                p.frame(PEN_DEVICE, time_us);
            }
            Some(Datagram::Touch { key, seq, time_us, states }) if key == p.key && ready => {
                let mut devices = p.devices.lock().unwrap();
                if let Some(v) = devices.get_mut(&TOUCH_DEVICE) {
                    let batches = v.touch.datagram(seq, &states);
                    v.emit(TOUCH_DEVICE, batches);
                }
                drop(devices);
                p.frame(TOUCH_DEVICE, time_us);
            }
            _ => {}
        }
    }
}
