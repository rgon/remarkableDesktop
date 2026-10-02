//! `remvnc-host`: runs `remvnc run` on the tablet over SSH and replays its
//! pen and touch input through local uinput devices.

mod capture;
mod displayconfig;
mod mapping;
mod outputs;
mod stats;
mod stream;
mod uinput;

use mapping::Mapper;
use remvnc_proto::{Device, Msg};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const USAGE: &str = "usage: remvnc-host [--ssh root@10.11.99.1] [--remote /home/root/remvnc]
                   [--deploy <local remvnc binary>] [--stats] [--ssh-only] [--list-screens]
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
            "--log-touch" | "--ssh-only" => a.run_args.push(flag),
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

/// Moves the session to a direct TCP connection with Nagle disabled.
fn connect_direct(addr: &str, port: u16, token: &[u8; 16]) -> std::io::Result<TcpStream> {
    let ip: std::net::IpAddr = addr.parse().map_err(std::io::Error::other)?;
    let tcp = TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(2))?;
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

fn remap(devices: &mut HashMap<u8, Virtual>, screens: &[outputs::Output], target: &Target) {
    let name = target.output.as_ref().map_or("all screens".to_string(), |o| format!("{} ({}x{})", o.name, o.w, o.h));
    eprintln!("remvnc-host: input now maps to {name}");
    for v in devices.values_mut() {
        v.mapper = Mapper::new(&v.decl, screens, target.output.as_ref(), target.shown);
    }
}

fn run() -> Result<(), String> {
    let args = parse()?;
    if let Some(local) = &args.deploy {
        let status = Command::new("scp")
            .args(["-q", "-O", local, &format!("{}:{}", args.ssh, args.remote)])
            .status()
            .map_err(|e| format!("scp: {e}"))?;
        if !status.success() {
            return Err("deploy failed".into());
        }
    }

    let remote_cmd = std::iter::once(format!("{} run", args.remote)).chain(args.run_args.iter().cloned()).collect::<Vec<_>>().join(" ");
    // ssh's stdin stays open for the whole session: the tablet exits (and
    // hands the screen back) as soon as it closes.
    let mut child = Command::new("ssh")
        .args(["-T", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2", &args.ssh, &remote_cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))?;
    let ssh_in = child.stdin.take().unwrap();
    let mut ssh_out = BufReader::new(child.stdout.take().unwrap());

    let first = Msg::read_from(&mut ssh_out).map_err(|e| format!("protocol: {e}"))?.ok_or("tablet closed the connection")?;
    let (mut stream, writer, mut pending): (Box<dyn Read + Send>, Writer, Option<Msg>) = match first {
        Msg::Direct { addr, port, token } => match connect_direct(&addr, port, &token) {
            Ok(tcp) => {
                eprintln!("remvnc-host: direct link to {addr}:{port}");
                let r = tcp.try_clone().map_err(|e| e.to_string())?;
                (Box::new(BufReader::new(r)), Arc::new(Mutex::new(Box::new(tcp))), None)
            }
            Err(e) => {
                // The tablet gives up waiting and carries on over SSH.
                eprintln!("remvnc-host: direct link failed ({e}), staying on ssh");
                (Box::new(ssh_out), Arc::new(Mutex::new(Box::new(ssh_in))), None)
            }
        },
        other => (Box::new(ssh_out), Arc::new(Mutex::new(Box::new(ssh_in))), Some(other)),
    };

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
    let result = loop {
        // Pongs arrive every 100 ms, so this runs often enough.
        if let Some(Some(name)) = streamer.as_ref().and_then(|s| s.take_started()) {
            // A virtual monitor appeared: point input at it.
            screens = outputs::list().unwrap_or_default();
            target = Target { output: screens.iter().find(|o| o.name == name).cloned(), shown: mapping::FULL };
            remap(&mut devices, &screens, &target);
        }
        let msg = match pending.take() {
            Some(m) => Ok(Some(m)),
            None => Msg::read_from(&mut stream),
        };
        match msg {
            Ok(Some(Msg::Hello { version, width, height, rotation })) => {
                if version != remvnc_proto::VERSION {
                    break Err(format!("tablet speaks protocol {version}, we speak {}", remvnc_proto::VERSION));
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
                    s.set_source(Some(capture::Source::Virtual));
                    streamer = Some(s);
                }
            }
            Ok(Some(Msg::Device(d))) => {
                // Same id again: the tablet changed the device (e.g. touch mode).
                devices.remove(&d.id);
                match uinput::VirtualDevice::create(&d) {
                    Ok(dev) => {
                        eprintln!("remvnc-host: created \"{}\"", d.name);
                        let mapper = Mapper::new(&d, &screens, target.output.as_ref(), target.shown);
                        devices.insert(d.id, Virtual { decl: d, dev, mapper });
                    }
                    Err(e) => break Err(format!("creating \"{}\": {e}", d.name)),
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
                    // The virtual monitor's mapping is set once it exists (below).
                    (Some(s), None) => s.set_source(Some(capture::Source::Virtual)),
                    (Some(s), Some(m)) => s.set_source(Some(capture::Source::Monitor(m.name.clone()))),
                    (None, _) => {}
                }
                if streamer.is_none() || monitor.is_some() {
                    // Mirroring a real monitor removes the virtual one.
                    if let Ok(s) = outputs::list() {
                        screens = s;
                    }
                    let shown = match (&streamer, &monitor) {
                        (Some(_), Some(m)) => mapping::letterbox(logical, m),
                        _ => mapping::FULL,
                    };
                    target = Target { output: monitor, shown };
                    remap(&mut devices, &screens, &target);
                }
            }
            Ok(Some(Msg::Ack { id })) => {
                if let Some(s) = &streamer {
                    s.ack(id);
                }
            }
            Ok(Some(Msg::Pong { host_us, tablet_us })) => stats.pong(host_us, tablet_us),
            Ok(Some(Msg::Bye)) => {
                eprintln!("remvnc-host: tablet closed the session");
                break Ok(());
            }
            Ok(Some(_)) => {}
            Ok(None) => break Err("connection closed".into()),
            Err(e) => break Err(format!("protocol: {e}")),
        }
    };
    drop(streamer);
    drop(devices);
    // Ending ssh ends the session on the tablet; the pinger then fails and exits.
    let _ = child.kill();
    let _ = child.wait();
    drop(stream);
    if let Ok(mut w) = writer.lock() {
        let _ = w.flush();
    }
    drop(writer);
    let _ = pinger.join();
    result
}
