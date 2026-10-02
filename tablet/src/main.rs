/// Logs to stderr, ignoring failures: stderr is the SSH channel, which may
/// already be gone while we restore the tablet (eprintln! would panic).
macro_rules! log {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($t)*);
    }};
}

mod daemon;
mod display;
mod epd;
mod gfx;
mod hid;
mod hid_report;
mod input;
mod link;
mod session;
mod takeover;
mod ui;

use embedded_graphics::mono_font::{MonoTextStyle, ascii::FONT_10X20};
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyleBuilder, Rectangle};
use embedded_graphics::text::Text;
use epd::{Epd, Mode};
use gfx::{Canvas, Scaled};
use input::{PalmConfig, PenMode, TouchMode};
use session::{Session, quitting};
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  remvnc start [options]   Start the instance (detached): takes over the panel,
                           adds the USB HID function, waits for remvnc-host.
  remvnc stop              End it (hold X or press power on the tablet also works).
  remvnc session           Print \"port token\" for remvnc-host, if running.
  remvnc daemon [options]  Run the instance in the foreground.
  remvnc probe             Draw test patterns for a few seconds.
  remvnc restore           Kill stray instances and restart xochitl.
  remvnc hid-restore       Remove our USB HID function from the gadget.
options: [--rotate 0|90|180|270] [--touch touchpad|touchscreen] [--pen tablet|mouse]
         [--palm-major N] [--pen-grace-ms N] [--log-touch] [--no-usb]
         [--fast-mode M] [--settle-mode M] [--settle-ms N]
         (modes: pen, mono, animation, ui, content)";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("guardian") if args.len() == 5 => {
            let n = |i: usize| args[i].parse::<u64>().expect("guardian: bad argument");
            takeover::run_guardian(n(2) as i32, n(3), n(4) as i32);
            Ok(())
        }
        Some("restore") => {
            takeover::manual_restore();
            Ok(())
        }
        Some("probe") => probe(),
        Some("hid-test") => hid::test(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10)),
        Some("hid-restore") => {
            hid::restore();
            Ok(())
        }
        Some("daemon") => parse_run(&args[2..]).and_then(daemon::run),
        Some("start") => parse_run(&args[2..]).and_then(|_| start(&args[2..])),
        Some("stop") => stop(),
        Some("session") => session_info(),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = result {
        log!("remvnc: {e}");
        std::process::exit(1);
    }
}

fn parse_run(args: &[String]) -> Result<daemon::Args, String> {
    let (mut rotation, mut touch, mut palm, mut usb) = (90, TouchMode::Touchpad, PalmConfig::default(), true);
    let mut pen = PenMode::Tablet;
    let mut refresh = display::Refresh::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--rotate" => {
                rotation = value()?.parse().ok().filter(|r| [0, 90, 180, 270].contains(r)).ok_or("--rotate: 0, 90, 180 or 270")?
            }
            "--touch" => {
                touch = match value()?.as_str() {
                    "touchpad" => TouchMode::Touchpad,
                    "touchscreen" => TouchMode::Touchscreen,
                    _ => return Err("--touch: touchpad or touchscreen".into()),
                }
            }
            "--pen" => {
                pen = match value()?.as_str() {
                    "tablet" => PenMode::Tablet,
                    "mouse" => PenMode::Mouse,
                    _ => return Err("--pen: tablet or mouse".into()),
                }
            }
            "--fast-mode" => refresh.fast = Mode::parse(&value()?).ok_or("--fast-mode: unknown mode")?,
            "--settle-mode" => refresh.settle = Mode::parse(&value()?).ok_or("--settle-mode: unknown mode")?,
            "--settle-ms" => {
                refresh.settle_after = Duration::from_millis(value()?.parse().map_err(|_| "--settle-ms: a number")?)
            }
            "--palm-major" => palm.max_major = value()?.parse().map_err(|_| "--palm-major: a number")?,
            "--pen-grace-ms" => palm.pen_grace_us = value()?.parse::<u64>().map_err(|_| "--pen-grace-ms: a number")? * 1000,
            "--log-touch" => palm.log = true,
            "--no-usb" => usb = false,
            _ => return Err(format!("unknown argument {a}\n{USAGE}")),
        }
    }
    Ok(daemon::Args { refresh, rotation, touch, pen, palm, usb })
}

/// Pumps Qt events for `ms`; false if we were asked to quit.
fn wait(epd: &Epd, ms: u64) -> bool {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end && !quitting() {
        epd.process_events(20);
        std::thread::sleep(Duration::from_millis(10));
    }
    !quitting()
}


const UNIT: &str = "remvnc";

fn unit_active() -> bool {
    std::process::Command::new("systemctl").args(["is-active", "--quiet", UNIT]).status().is_ok_and(|s| s.success())
}

/// Starts the instance as a transient systemd unit, so it outlives the SSH
/// session that started it (USB re-enumeration drops that session).
fn start(options: &[String]) -> Result<(), String> {
    if unit_active() {
        println!("running");
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = std::process::Command::new("systemd-run")
        .args(["--quiet", "--collect", &format!("--unit={UNIT}")])
        .arg(exe)
        .arg("daemon")
        .args(options)
        .status()
        .map_err(|e| format!("systemd-run: {e}"))?;
    if !status.success() {
        return Err("systemd-run failed".into());
    }
    println!("started");
    Ok(())
}

fn stop() -> Result<(), String> {
    let _ = std::process::Command::new("systemctl").args(["stop", UNIT]).status();
    Ok(())
}

fn session_info() -> Result<(), String> {
    if !unit_active() {
        return Err("not running".into());
    }
    let s = std::fs::read_to_string(link::SESSION_FILE).map_err(|_| "starting".to_string())?;
    print!("{s}");
    Ok(())
}

fn probe() -> Result<(), String> {
    let (session, epd) = Session::begin(50)?;
    let mut canvas = Canvas::new(&epd, vec![epd.fb]);
    let text = MonoTextStyle::new(&FONT_10X20, Gray8::BLACK);
    let panel = PrimitiveStyleBuilder::new().fill_color(Gray8::WHITE).stroke_color(Gray8::BLACK).stroke_width(6).build();
    let mut ok = true;
    for (i, mode) in Mode::ALL.into_iter().enumerate() {
        if !ok {
            break;
        }
        let y = 360 + i as i32 * 200;
        let _ = Rectangle::new(Point::new(100, y), Size::new(1204, 160)).into_styled(panel).draw(&mut canvas);
        let _ = Text::new(&format!("{i} {mode:?}"), Point::new(15, y / 3 + 35), text)
            .draw(&mut Scaled { target: &mut canvas, factor: 3 });
        let t = Instant::now();
        canvas.flush(mode);
        log!("remvnc: swap {mode:?} returned in {:?}", t.elapsed());
        ok = wait(&epd, 1200);
    }
    drop(canvas);
    session.end(&epd)
}
