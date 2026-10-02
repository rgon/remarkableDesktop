//! `remvnc-host`: runs `remvnc run` on the tablet over SSH and replays its
//! pen and touch input through local uinput devices.

mod capture;
mod displayconfig;
mod gsettings;
mod mapping;
mod outputs;
mod stats;
mod stream;
mod uinput;

use mapping::Mapper;
use remvnc_proto::{Device, Msg};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const USAGE: &str = "usage: remvnc-host [--ssh root@10.11.99.1] [--remote /home/root/remvnc]
                   [--deploy <local remvnc binary>] [--stats] [--no-usb] [--list-screens]
                   [--no-display]
                   [--rotate 0|90|180|270] [--touch touchpad|touchscreen] [--pen tablet|mouse]
                   [--palm-major N] [--pen-grace-ms N] [--log-touch]
                   [--fast-mode M] [--settle-mode M] [--settle-ms N]";

struct Args {
    ssh: String,
    remote: String,
    deploy: Option<String>,
    stats: bool,
    /// Stream the screen to the tablet (otherwise input only).
    display: bool,
    run_args: Vec<String>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args { ssh: "root@10.11.99.1".into(), remote: "/home/root/remvnc".into(), deploy: None, stats: false, display: true, run_args: vec![] };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value\n{USAGE}"));
        match flag.as_str() {
            "--ssh" => a.ssh = value()?,
            "--remote" => a.remote = value()?,
            "--deploy" => a.deploy = Some(value()?),
            "--stats" => a.stats = true,
            "--no-display" => a.display = false,
            "--capture-test" => {
                // remvnc-host --capture-test virtual|<connector> <out.pgm>
                let src = value()?;
                let path = it.next().ok_or("--capture-test needs an output path")?;
                let source = if src == "virtual" { capture::Source::Virtual } else { capture::Source::Monitor(src) };
                let mut cap = capture::Capture::start(&source, (1872, 1404), 90)?;
                let mut frame = vec![];
                let t = std::time::Instant::now();
                while !cap.next_frame(&mut frame, Duration::from_secs(5)).map_err(|e| format!("frame: {e}"))? {}
                let first = t.elapsed();
                println!("virtual monitor: {:?}", cap.virtual_name);
                let mut pgm = format!("P5\n{} {}\n255\n", cap.width, cap.height).into_bytes();
                pgm.extend_from_slice(&frame);
                std::fs::write(&path, pgm).map_err(|e| e.to_string())?;
                println!("{}x{} frame in {first:?}; outputs now:", cap.width, cap.height);
                for o in outputs::list()? {
                    println!("  {} {}x{} at {},{}", o.name, o.w, o.h, o.x, o.y);
                }
                std::process::exit(0);
            }
            "--list-screens" => {
                for (i, o) in outputs::list()?.iter().enumerate() {
                    println!("{}: {} {}x{} at {},{}", i + 1, o.name, o.w, o.h, o.x, o.y);
                }
                std::process::exit(0);
            }
            "--rotate" | "--touch" | "--pen" | "--palm-major" | "--pen-grace-ms" | "--fast-mode" | "--settle-mode" | "--settle-ms" => {
                let v = value()?;
                a.run_args.extend([flag, v]);
            }
            "--log-touch" | "--no-usb" => a.run_args.push(flag),
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown argument {flag}\n{USAGE}")),
        }
    }
    Ok(a)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("remvnc-host: {e}");
            ExitCode::FAILURE
        }
    }
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

fn send(w: &Writer, msg: &Msg) -> std::io::Result<()> {
    let mut w = w.lock().unwrap();
    msg.write_to(&mut *w)?;
    w.flush()
}

/// Runs a command on the tablet; its stdout on success.
fn ssh(target: &str, cmd: &str) -> Result<String, String> {
    let out = Command::new("ssh")
        // Keepalives: a call caught by USB re-enumeration fails fast instead of hanging.
        .args(["-T", "-o", "ConnectTimeout=3", "-o", "ServerAliveInterval=1", "-o", "ServerAliveCountMax=2", "-o", "BatchMode=yes", target, cmd])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ssh: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Address, port and token of the running instance, read over SSH (which
/// is what authenticates us). Retries while USB re-enumerates.
fn find_session(args: &Args, wait: Duration) -> Result<(SocketAddr, [u8; 16]), String> {
    let deadline = std::time::Instant::now() + wait;
    let mut last;
    loop {
        match ssh(&args.ssh, &format!("{} session && echo $SSH_CONNECTION", args.remote)) {
            Ok(out) => {
                let mut lines = out.lines();
                let (port, token) = lines.next().and_then(|l| l.split_once(' ')).ok_or("odd session reply")?;
                let server = lines.next().and_then(|l| l.split_whitespace().nth(2)).ok_or("no SSH_CONNECTION")?;
                let ip: std::net::IpAddr = server.trim_start_matches("::ffff:").parse().map_err(|e| format!("{server}: {e}"))?;
                let port: u16 = port.parse().map_err(|_| "bad port")?;
                let mut t = [0u8; 16];
                for (i, b) in t.iter_mut().enumerate() {
                    *b = u8::from_str_radix(token.get(2 * i..2 * i + 2).ok_or("short token")?, 16).map_err(|_| "bad token")?;
                }
                return Ok((SocketAddr::new(ip, port), t));
            }
            Err(e) if e.contains("not running") => return Err("remvnc is not running on the tablet".into()),
            Err(e) => last = e,
        }
        if std::time::Instant::now() > deadline {
            return Err(format!("tablet unreachable: {last}"));
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}

fn connect(addr: SocketAddr, token: &[u8; 16]) -> std::io::Result<TcpStream> {
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    tcp.set_nodelay(true)?;
    (&tcp).write_all(token)?;
    Ok(tcp)
}

/// What the screen button selects: with display, view 0 is a new virtual
/// monitor and view N mirrors monitor N; without, view 0 is the whole
/// desktop and view N confines input to monitor N.
fn view_labels(display: bool, monitors: usize) -> Vec<String> {
    std::iter::once(if display { "VIRT" } else { "ALL" }.to_string()).chain((1..=monitors).map(|i| i.to_string())).collect()
}

/// Input mapping for the current view: the monitor input lands on, and
/// which part of the tablet shows it.
struct Target {
    output: Option<outputs::Output>,
    shown: [f64; 4],
}

struct Virtual {
    decl: Device,
    dev: uinput::VirtualDevice,
    mapper: Mapper,
}

/// uinput ids, as set in uinput.rs.
const VENDOR: u16 = 0x1d50;

/// How GNOME should map a device itself, if it can: drawing tablets and
/// touchscreens have per-device monitor settings; absolute pointers
/// (pen in mouse mode) span the desktop, touchpads are relative.
fn compositor_kind(d: &Device) -> Option<gsettings::Kind> {
    const INPUT_PROP_DIRECT: u32 = 1;
    if d.keys.contains(&0x140) {
        Some(gsettings::Kind::Tablet) // BTN_TOOL_PEN
    } else if d.props & (1 << INPUT_PROP_DIRECT) != 0 {
        Some(gsettings::Kind::Touchscreen)
    } else {
        None
    }
}

/// The tablet's USB gadget ids, which its HID devices carry.
const HID_VENDOR: u16 = 0x04b3;
const HID_PRODUCT: u16 = 0x4010;

/// How the tablet's input reaches us: its own USB HID devices (we can only
/// tell GNOME where they map and send the tablet a correction), or our
/// uinput devices.
#[derive(Default, Clone, Copy, PartialEq)]
struct TabletStatus {
    usb_hid: bool,
    pen_mouse: bool,
    touchpad: bool,
}

fn fractions(v: [f64; 4]) -> [u16; 4] {
    v.map(|f| (f.clamp(0.0, 1.0) * 65535.0).round() as u16)
}

fn remap_hid(w: &Writer, screens: &[outputs::Output], target: &Target, gs: &mut gsettings::Outputs, st: TabletStatus) {
    let edids = displayconfig::edids().unwrap_or_default();
    let edid = target.output.as_ref().and_then(|o| edids.get(&o.name));
    // Pen: GNOME maps a drawing tablet to the monitor; an absolute mouse
    // spans the desktop, so it gets squeezed into the target's share.
    let pen_to = if st.pen_mouse {
        mapping::desktop_span(screens, target.output.as_ref())
    } else {
        gs.map(gsettings::Kind::Tablet, HID_VENDOR, HID_PRODUCT, edid);
        mapping::FULL
    };
    let _ = send(w, &Msg::Mapping { device: 1, shown: fractions(target.shown), to: fractions(pen_to) });
    // Touch: a touchscreen is mapped by GNOME; a touchpad is relative.
    let (shown, to) = if st.touchpad {
        (mapping::FULL, mapping::FULL)
    } else {
        gs.map(gsettings::Kind::Touchscreen, HID_VENDOR, HID_PRODUCT, edid);
        (target.shown, mapping::FULL)
    };
    let _ = send(w, &Msg::Mapping { device: 2, shown: fractions(shown), to: fractions(to) });
}

fn remap(devices: &mut HashMap<u8, Virtual>, screens: &[outputs::Output], target: &Target, gs: &mut gsettings::Outputs) {
    let name = target.output.as_ref().map_or("all screens".to_string(), |o| format!("{} ({}x{})", o.name, o.w, o.h));
    eprintln!("remvnc-host: input now maps to {name}");
    let edids = displayconfig::edids().unwrap_or_default();
    let edid = target.output.as_ref().and_then(|o| edids.get(&o.name));
    for (id, v) in devices.iter_mut() {
        v.mapper = match compositor_kind(&v.decl) {
            Some(kind) if gs.map(kind, VENDOR, 0x6200 + *id as u16, edid) => Mapper::letterboxed(&v.decl, target.shown),
            _ => Mapper::new(&v.decl, screens, target.output.as_ref(), target.shown),
        };
    }
}

fn run() -> Result<(), String> {
    let args = parse()?;
    if let Some(local) = &args.deploy {
        // Copy beside, then rename: the running binary can't be overwritten.
        let tmp = format!("{}.new", args.remote);
        let status = Command::new("scp").args(["-q", "-O", local, &format!("{}:{tmp}", args.ssh)]).status().map_err(|e| format!("scp: {e}"))?;
        if !status.success() {
            return Err("deploy failed".into());
        }
        ssh(&args.ssh, &format!("mv {tmp} {}", args.remote))?;
    }
    let start = std::iter::once(format!("{} start", args.remote)).chain(args.run_args.iter().cloned()).collect::<Vec<_>>().join(" ");
    eprintln!("remvnc-host: tablet: {}", ssh(&args.ssh, &start)?.trim());

    // Settings changes are undone when this drops, across reconnects.
    let mut gs = gsettings::Outputs::default();
    let mut wait = Duration::from_secs(30);
    // After "go remote": the tablet's address off USB, reached without SSH.
    let mut remote: Option<(SocketAddr, [u8; 16])> = None;
    loop {
        let (addr, token) = match remote {
            Some(r) => r,
            None => find_session(&args, wait)?,
        };
        let tcp = match connect(addr, &token) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("remvnc-host: connect {addr}: {e}; retrying");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        eprintln!("remvnc-host: connected to {addr}");
        match session(tcp, addr.port(), &args, &mut gs) {
            Ok(End::Bye) => return Ok(()),
            Ok(End::GoRemote(ip)) => {
                eprintln!("remvnc-host: going remote via {ip}");
                remote = Some((SocketAddr::new(ip, addr.port()), token));
            }
            // USB re-enumeration or a network blip: the instance keeps
            // running, so find it again (directly if we went remote).
            Err(e) => {
                eprintln!("remvnc-host: {e}; reconnecting");
                if let Some((r, _)) = remote {
                    if !wait_reachable(r, Duration::from_secs(30)) {
                        eprintln!("remvnc-host: {r} unreachable; trying over USB");
                        remote = None;
                    }
                }
            }
        }
        wait = Duration::from_secs(20);
    }
}

/// How a session ended without an error.
enum End {
    /// The tablet ended it.
    Bye,
    /// The tablet asked to move the connection off USB, to this address.
    GoRemote(std::net::IpAddr),
}

fn reachable(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

fn wait_reachable(addr: SocketAddr, wait: Duration) -> bool {
    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline {
        if reachable(addr) {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

/// Tests, every few seconds, which of the tablet's non-USB addresses we can
/// reach its instance on, and tells the tablet (enabling its NET button).
fn spawn_prober(
    w: Writer,
    port: u16,
    addrs: Arc<Mutex<Vec<String>>>,
    verified: Arc<Mutex<Option<std::net::IpAddr>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut last: Option<(Option<std::net::IpAddr>, Vec<String>)> = None;
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let candidates = addrs.lock().unwrap().clone();
            let ok = candidates
                .iter()
                .filter_map(|a| a.parse::<std::net::IpAddr>().ok())
                .find(|&ip| reachable(SocketAddr::new(ip, port)));
            if last.as_ref() != Some(&(ok, candidates.clone())) {
                match ok {
                    Some(ip) => eprintln!("remvnc-host: tablet reachable without USB at {ip}"),
                    None if !candidates.is_empty() => eprintln!("remvnc-host: tablet not reachable at {}", candidates.join(", ")),
                    None => {}
                }
                *verified.lock().unwrap() = ok;
                let addr = ok.map_or(String::new(), |ip| ip.to_string());
                if send(&w, &Msg::RemoteOk { addr }).is_err() {
                    return;
                }
                last = Some((ok, candidates));
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

/// One connection to the instance.
fn session(tcp: TcpStream, port: u16, args: &Args, gs: &mut gsettings::Outputs) -> Result<End, String> {
    // The tablet answers our 100 ms pings, so 1.5 s of silence is a dead
    // link (e.g. USB unplugged), which TCP alone would not report.
    tcp.set_read_timeout(Some(Duration::from_millis(1500))).map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(Duration::from_millis(500))).map_err(|e| e.to_string())?;
    let mut stream = BufReader::new(tcp.try_clone().map_err(|e| e.to_string())?);
    let closer = tcp.try_clone().map_err(|e| e.to_string())?;
    let writer: Writer = Arc::new(Mutex::new(Box::new(tcp)));
    let tablet_addrs: Arc<Mutex<Vec<String>>> = Default::default();
    let verified: Arc<Mutex<Option<std::net::IpAddr>>> = Default::default();
    let stop_prober: Arc<std::sync::atomic::AtomicBool> = Default::default();
    spawn_prober(writer.clone(), port, tablet_addrs.clone(), verified.clone(), stop_prober.clone());

    // Clock probes for latency stats, 10 Hz.
    let pinger = {
        let w = writer.clone();
        std::thread::spawn(move || {
            while send(&w, &Msg::Ping { host_us: remvnc_proto::monotonic_us() }).is_ok() {
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };

    let mut stats = stats::Stats::new(args.stats);
    let mut devices: HashMap<u8, Virtual> = HashMap::new();
    // Monitors as they were before we added a virtual one; views index these.
    let mut monitors: Vec<outputs::Output> = vec![];
    // Current layout, for mapping (includes the virtual monitor).
    let mut screens: Vec<outputs::Output> = vec![];
    let mut target = Target { output: None, shown: mapping::FULL };
    let mut logical = (0u32, 0u32);
    let mut streamer: Option<stream::Streamer> = None;
    let mut status = TabletStatus::default();
    let offset: stats::Offset = Default::default();
    let mut hid_probe: Option<stats::HidProbe> = None;
    let result = loop {
        // Pongs arrive every 100 ms, so this runs often enough.
        if let Some(Some(name)) = streamer.as_ref().and_then(|s| s.take_started()) {
            // A virtual monitor appeared: point input at it.
            screens = outputs::list().unwrap_or_default();
            target = Target { output: screens.iter().find(|o| o.name == name).cloned(), shown: mapping::FULL };
            remap(&mut devices, &screens, &target, gs);
            if status.usb_hid {
                remap_hid(&writer, &screens, &target, gs, status);
            }
        }
        match Msg::read_from(&mut stream) {
            Ok(Some(Msg::Hello { version, width, height, rotation })) => {
                if version != remvnc_proto::VERSION {
                    eprintln!("remvnc-host: tablet speaks protocol {version}, we speak {}", remvnc_proto::VERSION);
                    break Ok(End::Bye);
                }
                eprintln!("remvnc-host: tablet ready, {width}x{height} rotated {rotation}°");
                logical = (width as u32, height as u32);
                monitors = outputs::list().unwrap_or_else(|e| {
                    eprintln!("remvnc-host: no monitor list ({e}); input spans all screens");
                    vec![]
                });
                screens = monitors.clone();
                for (i, s) in monitors.iter().enumerate() {
                    eprintln!("remvnc-host: screen {}: {} {}x{} at {},{}", i + 1, s.name, s.w, s.h, s.x, s.y);
                }
                if let Err(e) = send(&writer, &Msg::Screens { names: view_labels(args.display, monitors.len()) }) {
                    break Err(format!("send: {e}"));
                }
                if args.display {
                    let w = writer.clone();
                    let s = stream::Streamer::start(logical, rotation, args.stats, Box::new(move |m| send(&w, m)));
                    s.set_source(Some(capture::Source::Virtual), mapping::FULL);
                    streamer = Some(s);
                }
            }
            Ok(Some(Msg::Status { usb_hid, pen_mouse, touchpad })) => {
                let st = TabletStatus { usb_hid, pen_mouse, touchpad };
                if st.usb_hid != status.usb_hid {
                    eprintln!("remvnc-host: input arrives over {}", if usb_hid { "USB HID" } else { "this connection (uinput)" });
                }
                status = st;
                if usb_hid {
                    remap_hid(&writer, &screens, &target, gs, status);
                    if args.stats && hid_probe.is_none() {
                        hid_probe = stats::HidProbe::start(offset.clone());
                    }
                }
            }
            Ok(Some(Msg::Device(d))) => {
                // Same id again: the tablet changed the device (e.g. touch mode).
                devices.remove(&d.id);
                match uinput::VirtualDevice::create(&d) {
                    Ok(dev) => {
                        eprintln!("remvnc-host: created \"{}\"", d.name);
                        devices.insert(d.id, Virtual { decl: d, dev, mapper: Mapper::default() });
                        remap(&mut devices, &screens, &target, gs);
                    }
                    Err(e) => eprintln!("remvnc-host: creating \"{}\": {e}", d.name),
                }
            }
            Ok(Some(Msg::Frame { device: _, time_us, events })) if events.is_empty() => {
                if let Some(p) = &hid_probe {
                    p.marker(time_us);
                }
            }
            Ok(Some(Msg::Frame { device, time_us, mut events })) => {
                if let Some(v) = devices.get_mut(&device) {
                    v.mapper.apply(&mut events);
                    if let Err(e) = v.dev.emit(&events) {
                        eprintln!("remvnc-host: write to device {device}: {e}");
                    }
                }
                stats.frame(device, time_us);
            }
            Ok(Some(Msg::Target { index })) => {
                let monitor = monitors.get((index as usize).wrapping_sub(1)).cloned();
                match (&streamer, &monitor) {
                    // The virtual monitor's mapping is set once it exists (above).
                    (Some(s), None) => s.set_source(Some(capture::Source::Virtual), mapping::FULL),
                    (Some(s), Some(m)) => s.set_source(Some(capture::Source::Monitor(m.name.clone())), mapping::letterbox(logical, m)),
                    (None, _) => {}
                }
                if streamer.is_none() || monitor.is_some() {
                    // Mirroring a real monitor removes the virtual one.
                    if let Ok(s) = outputs::list() {
                        screens = s;
                    }
                    // Keep the monitor's aspect: the tablet area matches the
                    // (letterboxed) picture of it, shown or not.
                    let shown = monitor.as_ref().map_or(mapping::FULL, |m| mapping::letterbox(logical, m));
                    target = Target { output: monitor, shown };
                    remap(&mut devices, &screens, &target, gs);
                    if status.usb_hid {
                        remap_hid(&writer, &screens, &target, gs, status);
                    }
                }
            }
            Ok(Some(Msg::Ack { id })) => {
                if let Some(s) = &streamer {
                    s.ack(id);
                }
            }
            Ok(Some(Msg::Pong { host_us, tablet_us })) => {
                stats.pong(host_us, tablet_us);
                *offset.lock().unwrap() = stats.clock_offset();
            }
            Ok(Some(Msg::Bye)) => {
                eprintln!("remvnc-host: tablet closed the session");
                break Ok(End::Bye);
            }
            Ok(Some(Msg::Addresses { addrs })) => *tablet_addrs.lock().unwrap() = addrs,
            Ok(Some(Msg::GoRemote)) => {
                if let Some(ip) = *verified.lock().unwrap() {
                    break Ok(End::GoRemote(ip));
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break Err("connection closed".into()),
            Err(e) => break Err(format!("connection lost: {e}")),
        }
    };
    stop_prober.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(streamer);
    drop(devices);
    // The pinger exits once the socket is shut down.
    let _ = closer.shutdown(std::net::Shutdown::Both);
    let _ = pinger.join();
    result
}
