//! `remdesktop-host`: runs `remdesktop run` on the tablet over SSH and replays its
//! pen and touch input through local uinput devices.

mod capture;
mod displayconfig;
mod edges;
mod gpu;
mod gsettings;
mod keymap;
mod mapping;
mod outputs;
mod stats;
mod tiles;
mod stream;
mod udp;
mod uinput;

use mapping::Mapper;
use remdesktop_proto::{Device, DisplayMode, Msg, PEN_DEVICE, TOUCH_DEVICE};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Set while we act as `SSH_ASKPASS` for our own ssh children.
const ASKPASS_ENV: &str = "REMDESKTOP_ASKPASS";
/// SSH password for the tablet, also accepted from the environment. Only
/// used when key login fails: ssh offers keys first.
const PASSWORD_ENV: &str = "REMDESKTOP_SSH_PASSWORD";

static PASSWORD: Mutex<Option<String>> = Mutex::new(None);
/// The password is asked for at most once per run.
static ASKED: AtomicBool = AtomicBool::new(false);

const USAGE: &str = "usage: remdesktop-host [--ssh root@10.11.99.1] [--remote /home/root/remdesktop]
                       [--deploy <local remdesktop binary>] [--stats] [--no-usb] [--list-screens]
                       [--no-display] [--no-udp]
                       [--rotate 0|90|180|270] [--touch touchpad|touchscreen] [--pen tablet|mouse]
                       [--palm-major N] [--pen-grace-ms N] [--palm-rest-ms N] [--log-touch]
                       [--fast-mode M] [--settle-mode M] [--settle-ms N]";

struct Args {
    ssh: String,
    remote: String,
    deploy: Option<String>,
    stats: bool,
    /// Stream the screen to the tablet (otherwise input only).
    display: bool,
    /// Offer the pen over UDP when on Wi-Fi.
    udp: bool,
    /// How long a screen area must be still before it gets a quality refresh.
    settle_ms: u64,
    run_args: Vec<String>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args { ssh: "root@10.11.99.1".into(), remote: "/home/root/remdesktop".into(), deploy: None, stats: false, display: true, udp: true, settle_ms: 1000, run_args: vec![] };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value\n{USAGE}"));
        match flag.as_str() {
            "--ssh" => a.ssh = value()?,
            "--remote" => a.remote = value()?,
            "--deploy" => a.deploy = Some(value()?),
            "--stats" => a.stats = true,
            "--no-display" => a.display = false,
            "--no-udp" => a.udp = false,
            "--settle-ms" => a.settle_ms = value()?.parse().map_err(|_| "--settle-ms: a number")?,
            "--capture-test" => {
                // remdesktop-host --capture-test virtual|<connector> <out.pgm>
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
            "--rotate" | "--touch" | "--pen" | "--palm-major" | "--pen-grace-ms" | "--palm-rest-ms" | "--fast-mode" | "--settle-mode" => {
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

/// Reads a line from the terminal with echo off.
fn prompt_password(prompt: &str) -> Result<String, String> {
    use std::io::{BufRead, BufReader};
    use std::os::fd::AsRawFd;
    let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").map_err(|e| format!("no terminal to ask the password on: {e}"))?;
    let fd = tty.as_raw_fd();
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
        return Err("cannot read terminal settings".into());
    }
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    (&tty).write_all(prompt.as_bytes()).ok();
    let mut line = String::new();
    let read = BufReader::new(&tty).read_line(&mut line);
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    (&tty).write_all(b"\n").ok();
    read.map_err(|e| e.to_string())?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Makes `cmd` (ssh or scp) log in with the password, if there is one: ssh
/// runs this binary as its askpass helper, which prints it back.
fn with_auth(cmd: &mut Command) -> &mut Command {
    match PASSWORD.lock().unwrap().as_ref() {
        Some(pw) => {
            let me = std::env::current_exe().unwrap_or_else(|_| "remdesktop-host".into());
            cmd.env("SSH_ASKPASS", me)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(ASKPASS_ENV, "1")
                .env(PASSWORD_ENV, pw)
                .args(["-o", "BatchMode=no", "-o", "NumberOfPasswordPrompts=1", "-o", "PreferredAuthentications=publickey,password,keyboard-interactive"])
        }
        None => cmd.args(["-o", "BatchMode=yes"]),
    }
}

fn main() -> ExitCode {
    if std::env::var_os(ASKPASS_ENV).is_some() {
        println!("{}", std::env::var(PASSWORD_ENV).unwrap_or_default());
        return ExitCode::SUCCESS;
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("remdesktop-host: {e}");
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

/// Asks for the tablet's password, once, after key login was refused.
/// False if there is nobody to ask or it was asked already.
fn ask_password(target: &str) -> bool {
    if ASKED.swap(true, Ordering::Relaxed) {
        return false;
    }
    eprintln!("remdesktop-host: {target} refused our ssh key");
    match prompt_password(&format!("{target}'s password: ")) {
        Ok(pw) if !pw.is_empty() => {
            *PASSWORD.lock().unwrap() = Some(pw);
            true
        }
        Ok(_) => false,
        Err(e) => {
            eprintln!("remdesktop-host: {e}");
            false
        }
    }
}

/// Runs a command on the tablet; its stdout on success. Logs in with the
/// key, and falls back to a password if the key is refused.
fn ssh(target: &str, cmd: &str) -> Result<String, String> {
    let first = ssh_once(target, cmd);
    let have_password = PASSWORD.lock().unwrap().is_some();
    match first {
        Err(e) if e.contains("Permission denied") && !have_password && ask_password(target) => ssh_once(target, cmd),
        other => other,
    }
}

fn ssh_once(target: &str, cmd: &str) -> Result<String, String> {
    let mut ssh = Command::new("ssh");
    // Keepalives: a call caught by USB re-enumeration fails fast instead of hanging.
    ssh.args(["-T", "-o", "ConnectTimeout=3", "-o", "ServerAliveInterval=1", "-o", "ServerAliveCountMax=2"]);
    let out = with_auth(&mut ssh)
        .args([target, cmd])
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
            Err(e) if e.contains("not running") => return Err("remdesktop is not running on the tablet".into()),
            Err(e) if e.contains("Permission denied") => return Err(format!("ssh login refused: {e}")),
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
    remdesktop_proto::tune_socket(std::os::fd::AsRawFd::as_raw_fd(&tcp));
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
    /// What the pen shows, for the datagrams' snapshots to be diffed against.
    pen: remdesktop_proto::pen::PenSync,
    touch: remdesktop_proto::touch::TouchSync,
}

impl Virtual {
    /// Writes batches of events, each followed by a SYN_REPORT.
    fn emit(&mut self, id: u8, batches: Vec<Vec<remdesktop_proto::Event>>) {
        for mut events in batches {
            self.mapper.apply(&mut events);
            if let Err(e) = self.dev.emit(&events) {
                eprintln!("remdesktop-host: write to device {id}: {e}");
            }
        }
    }
}

/// Shared by the TCP loop and the pen's UDP thread.
type Devices = Arc<Mutex<HashMap<u8, Virtual>>>;

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
    eprintln!("remdesktop-host: input now maps to {name}");
    let edids = displayconfig::edids().unwrap_or_default();
    let edid = target.output.as_ref().and_then(|o| edids.get(&o.name));
    for (id, v) in devices.iter_mut() {
        v.mapper = match compositor_kind(&v.decl) {
            Some(kind) if gs.map(kind, VENDOR, 0x6200 + *id as u16, edid) => Mapper::letterboxed(&v.decl, target.shown),
            _ => Mapper::new(&v.decl, screens, target.output.as_ref(), target.shown),
        };
    }
}

/// Replaces the tablet's binary with the one bundled in the AppImage, using
/// the same installer as `<AppImage> install`.
fn update_tablet(args: &Args) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let script = exe.parent().and_then(|bin| bin.parent()).map(|usr| usr.join("share/remdesktop/install.sh")).filter(|p| p.is_file()).ok_or("no tablet binary is bundled with this build")?;
    eprintln!("remdesktop-host: updating the tablet");
    let mut cmd = Command::new("sh");
    cmd.arg(script).args(["--ssh", &args.ssh, "--remote", &args.remote]);
    if let Some(pw) = PASSWORD.lock().unwrap().clone() {
        cmd.env(PASSWORD_ENV, pw);
    }
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err("updating the tablet failed".into()),
        Err(e) => Err(format!("sh: {e}")),
    }
}

/// Input over the network is replayed through /dev/uinput; say so when we can't write it.
fn warn_uinput() {
    let writable = std::fs::OpenOptions::new().write(true).open("/dev/uinput").is_ok();
    if !writable {
        eprintln!("remdesktop-host: WARNING: /dev/uinput is not writable, so pen and touch won't work without USB.");
        eprintln!("remdesktop-host:          install host/99-remdesktop-uinput.rules into /etc/udev/rules.d/, reload udev and join the input group (run.sh offers to do this).");
    }
}

fn run() -> Result<(), String> {
    let args = parse()?;
    if let Some(pw) = std::env::var(PASSWORD_ENV).ok().filter(|p| !p.is_empty()) {
        *PASSWORD.lock().unwrap() = Some(pw);
    }
    if let Some(local) = &args.deploy {
        // Log in first, so a password is asked for here and scp can reuse it.
        ssh(&args.ssh, "true")?;
        // Copy beside, then rename: the running binary can't be overwritten.
        let tmp = format!("{}.new", args.remote);
        let status = with_auth(Command::new("scp").args(["-q", "-O"])).args([local, &format!("{}:{tmp}", args.ssh)]).status().map_err(|e| format!("scp: {e}"))?;
        if !status.success() {
            return Err("deploy failed".into());
        }
        ssh(&args.ssh, &format!("mv {tmp} {}", args.remote))?;
    }
    let start = std::iter::once(format!("{} start", args.remote)).chain(args.run_args.iter().cloned()).collect::<Vec<_>>().join(" ");
    eprintln!("remdesktop-host: tablet: {}", ssh(&args.ssh, &start)?.trim());
    // An outdated tablet is replaced from the bundled binary once per run.
    let mut updated = false;

    // Settings changes are undone when this drops, across reconnects.
    let mut gs = gsettings::Outputs::default();
    let mut wait = Duration::from_secs(30);
    // The tablet's non-USB addresses, as it last reported them, and the port
    // and token of its instance: enough to find it again over Wi-Fi once USB
    // is unplugged (the token stays valid as long as the instance runs).
    let known: Arc<Mutex<Vec<String>>> = Default::default();
    let mut last: Option<(u16, [u8; 16])> = None;
    // The tablet's USB address, which is always preferred when it is there.
    let usb = usb_address(&args.ssh);
    // After "go remote": the address to connect to next, staying off USB.
    let mut remote: Option<(SocketAddr, [u8; 16])> = None;
    loop {
        let pinned = remote.is_some();
        let (addr, token) = match remote.take() {
            Some(r) => r,
            None => find_tablet(&args, wait, &known, last, usb)?,
        };
        last = Some((addr.port(), token));
        let tcp = match connect(addr, &token) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("remdesktop-host: connect {addr}: {e}; retrying");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        eprintln!("remdesktop-host: connected to {addr}");
        // Off USB, a returning USB link takes the session back, unless we were
        // asked to stay off it.
        let watch_usb = usb.filter(|&u| !pinned && addr.ip() != u);
        match session(tcp, addr.port(), &args, &mut gs, &known, watch_usb) {
            Ok(End::Bye) => return Ok(()),
            Ok(End::Outdated(version)) => {
                eprintln!("remdesktop-host: tablet speaks protocol {version}, we speak {}", remdesktop_proto::VERSION);
                if updated {
                    return Err("the tablet still speaks another protocol after updating it".into());
                }
                updated = true;
                if let Err(e) = update_tablet(&args) {
                    return Err(format!("{e}; update the tablet with `<AppImage> install` (or --deploy)"));
                }
                eprintln!("remdesktop-host: tablet: {}", ssh(&args.ssh, &start)?.trim());
                last = None;
                wait = Duration::from_secs(30);
                continue;
            }
            Ok(End::GoRemote(ip)) => {
                eprintln!("remdesktop-host: going remote via {ip}");
                remote = Some((SocketAddr::new(ip, addr.port()), token));
            }
            Ok(End::UsbBack) => eprintln!("remdesktop-host: USB is back; moving the session to it"),
            // USB unplugged, re-enumeration or a network blip: the instance
            // keeps running, so find it again over USB or Wi-Fi.
            Err(e) => {
                eprintln!("remdesktop-host: {e}; reconnecting");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        wait = Duration::from_secs(20);
    }
}

/// The USB address in an ssh target (`user@10.11.99.1`), if it is one.
fn usb_address(target: &str) -> Option<std::net::IpAddr> {
    let ip: std::net::IpAddr = target.rsplit('@').next()?.parse().ok()?;
    match ip {
        std::net::IpAddr::V4(v4) if v4.octets()[..3] == [10, 11, 99] => Some(ip),
        _ => None,
    }
}

/// Finds the tablet's instance: over SSH first (USB, or wherever `--ssh`
/// points), then, once that has failed for a few seconds, over the Wi-Fi
/// addresses it reported earlier with the token we already have. Keeps
/// trying for `wait`, or for good when there are Wi-Fi addresses to watch.
fn find_tablet(
    args: &Args,
    wait: Duration,
    known: &Arc<Mutex<Vec<String>>>,
    last: Option<(u16, [u8; 16])>,
    usb: Option<std::net::IpAddr>,
) -> Result<(SocketAddr, [u8; 16]), String> {
    let started = std::time::Instant::now();
    let forever = last.is_some() && !known.lock().unwrap().is_empty();
    let mut announced = false;
    loop {
        match find_session(args, Duration::ZERO) {
            Ok(found) => return Ok(found),
            Err(e) if e.contains("not running") || e.contains("login refused") => return Err(e),
            Err(_) => {}
        }
        // Give a USB link that is merely re-enumerating time to come back.
        if let (Some((port, token)), true) = (last, usb.is_none() || started.elapsed() >= Duration::from_secs(4)) {
            let candidates: Vec<std::net::IpAddr> = known.lock().unwrap().iter().filter_map(|a| a.parse().ok()).collect();
            if let Some(ip) = candidates.into_iter().find(|&ip| reachable(SocketAddr::new(ip, port))) {
                eprintln!("remdesktop-host: tablet found over the network at {ip}");
                return Ok((SocketAddr::new(ip, port), token));
            }
        }
        if !forever && started.elapsed() > wait {
            return Err("tablet unreachable over USB".into());
        }
        if forever && !announced {
            announced = true;
            eprintln!("remdesktop-host: waiting for the tablet over USB or Wi-Fi");
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}

/// How a session ended without an error.
enum End {
    /// The tablet ended it.
    Bye,
    /// The tablet runs another protocol version (this one).
    Outdated(u16),
    /// The tablet asked to move the connection off USB, to this address.
    GoRemote(std::net::IpAddr),
    /// USB came back while we were on Wi-Fi, and USB is preferred.
    UsbBack,
}

fn reachable(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
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
                    Some(ip) => eprintln!("remdesktop-host: tablet reachable without USB at {ip}"),
                    None if !candidates.is_empty() => eprintln!("remdesktop-host: tablet not reachable at {}", candidates.join(", ")),
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
fn session(tcp: TcpStream, port: u16, args: &Args, gs: &mut gsettings::Outputs, tablet_addrs: &Arc<Mutex<Vec<String>>>, watch_usb: Option<std::net::IpAddr>) -> Result<End, String> {
    // The tablet answers our 100 ms pings, so 1.5 s of silence is a dead
    // link (e.g. USB unplugged), which TCP alone would not report.
    tcp.set_read_timeout(Some(Duration::from_millis(1500))).map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(Duration::from_millis(500))).map_err(|e| e.to_string())?;
    let tablet_ip = tcp.peer_addr().ok().map(|a| a.ip());
    let mut stream = BufReader::new(tcp.try_clone().map_err(|e| e.to_string())?);
    let closer = tcp.try_clone().map_err(|e| e.to_string())?;
    let writer: Writer = Arc::new(Mutex::new(Box::new(tcp)));
    let tablet_addrs = tablet_addrs.clone();
    let verified: Arc<Mutex<Option<std::net::IpAddr>>> = Default::default();
    let stop_prober: Arc<std::sync::atomic::AtomicBool> = Default::default();
    spawn_prober(writer.clone(), port, tablet_addrs.clone(), verified.clone(), stop_prober.clone());

    // Off USB: notice when the tablet's USB address answers SSH again.
    let usb_back: Arc<std::sync::atomic::AtomicBool> = Default::default();
    if let Some(ip) = watch_usb {
        let (flag, stop) = (usb_back.clone(), stop_prober.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if reachable(SocketAddr::new(ip, 22)) {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    }

    // Clock probes for latency stats, 10 Hz.
    let pinger = {
        let w = writer.clone();
        std::thread::spawn(move || {
            while send(&w, &Msg::Ping { host_us: remdesktop_proto::monotonic_us() }).is_ok() {
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };

    // Undo/redo keys for the current keyboard layout, re-checked now and then
    // (the user may switch input sources). Asking GNOME forks a process and
    // compiles a keymap, which must not stall input forwarding.
    let keys_watcher = {
        let (w, stop) = (writer.clone(), stop_prober.clone());
        std::thread::spawn(move || {
            let mut keys_sent: Option<Vec<u16>> = None;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match keymap::keys_for(&['z', 'y']) {
                    Some((codes, layout)) if keys_sent.as_ref() != Some(&codes) => {
                        eprintln!("remdesktop-host: keyboard layout {layout}: undo/redo on key codes {codes:?}");
                        let _ = send(&w, &Msg::Keys { undo: codes[0], redo: codes[1] });
                        keys_sent = Some(codes);
                    }
                    Some(_) => {}
                    None if keys_sent.is_none() => {
                        eprintln!("remdesktop-host: couldn't read the keyboard layout; undo/redo assume QWERTY");
                        keys_sent = Some(vec![]);
                    }
                    None => {}
                }
                for _ in 0..50 {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
    };

    let stats = Arc::new(Mutex::new(stats::Stats::new(args.stats)));
    let devices: Devices = Default::default();
    let mut udp_pen: Option<std::thread::JoinHandle<()>> = None;
    // Monitors as they were before we added a virtual one; views index these.
    let mut monitors: Vec<outputs::Output> = vec![];
    // Current layout, for mapping (includes the virtual monitor).
    let mut screens: Vec<outputs::Output> = vec![];
    let mut target = Target { output: None, shown: mapping::FULL };
    let mut logical = (0u32, 0u32);
    let mut streamer: Option<stream::Streamer> = None;
    let mut status = TabletStatus::default();
    let mut display_mode = DisplayMode::Normal;
    // Messages we feed ourselves (resuming the tablet's saved view).
    let mut queued: std::collections::VecDeque<Msg> = Default::default();
    let offset: stats::Offset = Default::default();
    let mut hid_probe: Option<stats::HidProbe> = None;
    let result = loop {
        if usb_back.load(std::sync::atomic::Ordering::Relaxed) {
            break Ok(End::UsbBack);
        }
        // Pongs arrive every 100 ms, so this runs often enough.
        if let Some(Some(name)) = streamer.as_ref().and_then(|s| s.take_started()) {
            // A virtual monitor appeared: point input at it.
            screens = outputs::list().unwrap_or_default();
            target = Target { output: screens.iter().find(|o| o.name == name).cloned(), shown: mapping::FULL };
            remap(&mut devices.lock().unwrap(), &screens, &target, gs);
            if status.usb_hid {
                remap_hid(&writer, &screens, &target, gs, status);
            }
        }
        let msg = match queued.pop_front() {
            Some(m) => Ok(Some(m)),
            None => Msg::read_from(&mut stream),
        };
        match msg {
            Ok(Some(Msg::Hello { version, width, height, rotation, view })) => {
                if version != remdesktop_proto::VERSION {
                    break Ok(End::Outdated(version));
                }
                eprintln!("remdesktop-host: tablet ready, {width}x{height} rotated {rotation}°");
                logical = (width as u32, height as u32);
                monitors = outputs::list().unwrap_or_else(|e| {
                    eprintln!("remdesktop-host: no monitor list ({e}); input spans all screens");
                    vec![]
                });
                screens = monitors.clone();
                for (i, s) in monitors.iter().enumerate() {
                    eprintln!("remdesktop-host: screen {}: {} {}x{} at {},{}", i + 1, s.name, s.w, s.h, s.x, s.y);
                }
                let labels = view_labels(args.display, monitors.len());
                // Resume the view the tablet had selected (e.g. after USB was replugged).
                let initial = labels.iter().position(|l| *l == view).unwrap_or(0) as u8;
                if let Err(e) = send(&writer, &Msg::Screens { names: labels }) {
                    break Err(format!("send: {e}"));
                }
                if args.display {
                    let w = writer.clone();
                    let s = stream::Streamer::start(logical, rotation, args.stats, Duration::from_millis(args.settle_ms), Box::new(move |m| send(&w, m)));
                    if initial == 0 {
                        s.set_source(Some(capture::Source::Virtual), mapping::FULL);
                    }
                    s.set_mode(display_mode);
                    streamer = Some(s);
                }
                if initial != 0 {
                    eprintln!("remdesktop-host: resuming view {view}");
                    queued.push_back(Msg::Target { index: initial });
                }
            }
            Ok(Some(Msg::Status { usb_hid, pen_mouse, touchpad })) => {
                let st = TabletStatus { usb_hid, pen_mouse, touchpad };
                if st.usb_hid != status.usb_hid {
                    eprintln!("remdesktop-host: input arrives over {}", if usb_hid { "USB HID" } else { "this connection (uinput)" });
                }
                if !usb_hid {
                    warn_uinput();
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
                // Declared again (mode change): the tablet's step count has moved on.
                let redeclared = devices.lock().unwrap().remove(&d.id).is_some();
                match uinput::VirtualDevice::create(&d) {
                    Ok(dev) => {
                        eprintln!("remdesktop-host: created \"{}\"", d.name);
                        devices.lock().unwrap().insert(d.id, Virtual { decl: d, dev, mapper: Mapper::default(), pen: if redeclared { Default::default() } else { remdesktop_proto::pen::PenSync::synced() }, touch: if redeclared { Default::default() } else { remdesktop_proto::touch::TouchSync::synced() } });
                        remap(&mut devices.lock().unwrap(), &screens, &target, gs);
                    }
                    Err(e) => eprintln!("remdesktop-host: creating \"{}\": {e}", d.name),
                }
            }
            Ok(Some(Msg::Frame { device: _, time_us, events })) if events.is_empty() => {
                if let Some(p) = &hid_probe {
                    p.marker(time_us);
                }
            }
            Ok(Some(Msg::Frame { device, time_us, mut events })) => {
                if let Some(v) = devices.lock().unwrap().get_mut(&device) {
                    match device {
                        PEN_DEVICE => v.pen.track(&events),
                        TOUCH_DEVICE => v.touch.track(&events),
                        _ => {}
                    }
                    v.mapper.apply(&mut events);
                    if let Err(e) = v.dev.emit(&events) {
                        eprintln!("remdesktop-host: write to device {device}: {e}");
                    }
                }
                stats.lock().unwrap().frame(device, time_us);
            }
            Ok(Some(Msg::PenState { seq, time_us: _, state })) => {
                if let Some(v) = devices.lock().unwrap().get_mut(&PEN_DEVICE) {
                    let batches = v.pen.edge(seq, state);
                    v.emit(PEN_DEVICE, batches);
                }
            }
            Ok(Some(Msg::TouchState { seq, time_us: _, state })) => {
                if let Some(v) = devices.lock().unwrap().get_mut(&TOUCH_DEVICE) {
                    let batches = v.touch.edge(seq, state);
                    v.emit(TOUCH_DEVICE, batches);
                }
            }
            Ok(Some(Msg::UdpOffer { port, key })) => {
                if let (true, None, Some(ip)) = (args.udp, &udp_pen, tablet_ip.filter(|i| i.is_ipv4())) {
                    let pen = udp::Pen {
                        tablet: SocketAddr::new(ip, port),
                        key,
                        devices: devices.clone(),
                        writer: writer.clone(),
                        stop: stop_prober.clone(),
                        stats: stats.clone(),
                        report: args.stats,
                    };
                    match udp::spawn(pen) {
                        Ok(t) => udp_pen = Some(t),
                        Err(e) => eprintln!("remdesktop-host: udp: {e}; the pen stays on TCP"),
                    }
                }
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
                    remap(&mut devices.lock().unwrap(), &screens, &target, gs);
                    if status.usb_hid {
                        remap_hid(&writer, &screens, &target, gs, status);
                    }
                }
            }
            Ok(Some(Msg::Display { mode })) => {
                eprintln!("remdesktop-host: display is now {mode:?}");
                display_mode = mode;
                if let Some(s) = &streamer {
                    s.set_mode(mode);
                }
            }
            Ok(Some(Msg::Ack { id })) => {
                if let Some(s) = &streamer {
                    s.ack(id);
                }
            }
            Ok(Some(Msg::Pong { host_us, tablet_us })) => {
                let mut stats = stats.lock().unwrap();
                stats.pong(host_us, tablet_us);
                *offset.lock().unwrap() = stats.clock_offset();
            }
            Ok(Some(Msg::Bye)) => {
                eprintln!("remdesktop-host: tablet closed the session");
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
    if let Some(t) = udp_pen {
        let _ = t.join();
    }
    drop(devices);
    // The pinger exits once the socket is shut down.
    let _ = closer.shutdown(std::net::Shutdown::Both);
    let _ = pinger.join();
    let _ = keys_watcher.join();
    result
}
