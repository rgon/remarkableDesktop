mod epd;
mod gfx;
mod input;
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
use input::{PalmConfig, Pen, Touch, TouchMode};
use remvnc_proto::Msg;
use session::{Session, quitting};
use std::io::{BufWriter, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  remvnc run [--rotate 0|90|180|270] [--touch touchpad|touchscreen]
             [--palm-major N] [--pen-grace-ms N] [--log-touch]
      Forward pen/touch to the host over stdout until the X is held.
  remvnc probe     Draw test patterns for a few seconds.
  remvnc restore   Kill stray instances and restart xochitl.";

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
        Some("run") => parse_run(&args[2..]).and_then(run),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = result {
        eprintln!("remvnc: {e}");
        std::process::exit(1);
    }
}

struct RunArgs {
    rotation: u16,
    touch: TouchMode,
    palm: PalmConfig,
}

fn parse_run(args: &[String]) -> Result<RunArgs, String> {
    let (mut rotation, mut touch, mut palm) = (90, TouchMode::Touchpad, PalmConfig::default());
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
            "--palm-major" => palm.max_major = value()?.parse().map_err(|_| "--palm-major: a number")?,
            "--pen-grace-ms" => palm.pen_grace_us = value()?.parse::<u64>().map_err(|_| "--pen-grace-ms: a number")? * 1000,
            "--log-touch" => palm.log = true,
            _ => return Err(format!("unknown argument {a}\n{USAGE}")),
        }
    }
    Ok(RunArgs { rotation, touch, palm })
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

const HOLD: Duration = Duration::from_millis(1500);

/// Runs the calling thread under SCHED_FIFO so input forwarding is not
/// delayed behind the engine's threads (which keep their normal policy).
fn realtime_priority() {
    let param = libc::sched_param { sched_priority: 20 };
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) } != 0 {
        eprintln!("remvnc: SCHED_FIFO: {}", std::io::Error::last_os_error());
    }
}

/// Qt only needs servicing for the engine's own bookkeeping, not per event.
const QT_EVERY: Duration = Duration::from_millis(100);

fn run(args: RunArgs) -> Result<(), String> {
    let RunArgs { rotation, touch: touch_mode, palm } = args;
    let (session, epd) = Session::begin(0)?;
    let inputs = &session.inputs;
    let mut pen = inputs.pen.as_ref().map(|g| Pen::new(&g.file, rotation)).transpose().map_err(|e| format!("pen: {e}"))?;
    let mut touch = inputs
        .touch
        .as_ref()
        .map(|g| Touch::new(&g.file, rotation, touch_mode, palm))
        .transpose()
        .map_err(|e| format!("touch: {e}"))?;

    let mut ui = ui::Ui::new(&epd, rotation);
    ui.draw_home();
    realtime_priority();

    let mut out = BufWriter::new(std::io::stdout().lock());
    let mut hello = vec![Msg::Hello { version: remvnc_proto::VERSION, width: ui.size[0] as u16, height: ui.size[1] as u16, rotation }];
    hello.extend(pen.iter().map(|p| Msg::Device(p.device())));
    hello.extend(touch.iter().map(|t| Msg::Device(t.device())));
    let mut alive = hello.iter().try_for_each(|m| m.write_to(&mut out)).and_then(|_| out.flush()).is_ok();

    let grabbed = [inputs.pen.as_ref(), inputs.touch.as_ref(), inputs.power.as_ref()];
    let fds = grabbed.map(|g| g.map_or(-1, |g| g.file.as_raw_fd()));
    let mut files = grabbed.map(|g| g.and_then(|g| g.file.try_clone().ok()));
    let mut from_host: Vec<u8> = vec![];
    let mut hold_start: Option<Instant> = None;
    let mut last_qt = Instant::now();
    let mut done = false;

    while alive && !done && !quitting() {
        let mut pfds: Vec<libc::pollfd> =
            fds.iter().chain([&0]).map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        let timeout = if hold_start.is_some() { 30 } else { QT_EVERY.as_millis() as i32 };
        unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, timeout) };

        // Pen first: its hover state gates touches in the same wake-up.
        let mut frames = vec![];
        if let (Some(pen), Some(f)) = (pen.as_mut(), files[0].as_mut()) {
            let events = input::read_events(f).unwrap_or_default();
            frames.extend(pen.feed(&events).into_iter().map(|(t, e)| (input::PEN_ID, t, e)));
        }
        if let Some(touch) = touch.as_mut() {
            let now = remvnc_proto::monotonic_us();
            if let Some(lift) = touch.set_pen(pen.as_ref().is_some_and(|p| p.in_range), now) {
                frames.push((input::TOUCH_ID, now, lift));
            }
            if let Some(f) = files[1].as_mut() {
                let events = input::read_events(f).unwrap_or_default();
                let fs = touch.feed(&events, ui.size, |p| ui.on_button(p));
                frames.extend(fs.into_iter().map(|(t, e)| (input::TOUCH_ID, t, e)));
            }
        }
        for (device, time_us, events) in frames {
            alive &= Msg::Frame { device, time_us, events }.write_to(&mut out).is_ok();
        }

        if let Some(f) = files[2].as_mut() {
            let power = input::read_events(f).unwrap_or_default();
            if power.iter().any(|e| e.kind == input::EV_KEY && e.code == input::KEY_POWER && e.value == 1) {
                eprintln!("remvnc: power button pressed");
                done = true;
            }
        }
        if pfds[3].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let mut buf = [0u8; 4096];
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                eprintln!("remvnc: host went away");
                break;
            }
            from_host.extend_from_slice(&buf[..n as usize]);
            while let Ok(Some((msg, used))) = Msg::parse(&from_host) {
                from_host.drain(..used);
                if let Msg::Ping { host_us } = msg {
                    let pong = Msg::Pong { host_us, tablet_us: remvnc_proto::monotonic_us() };
                    alive &= pong.write_to(&mut out).is_ok();
                }
            }
        }
        alive &= out.flush().is_ok();

        let holding = touch.as_ref().is_some_and(|t| t.holding());
        match (holding, hold_start) {
            (true, None) => hold_start = Some(Instant::now()),
            (false, Some(_)) => {
                hold_start = None;
                ui.set_progress(0);
            }
            (true, Some(t)) => {
                let steps = (t.elapsed().as_millis() * ui::HOLD_STEPS as u128 / HOLD.as_millis()) as u32;
                ui.set_progress(steps.min(ui::HOLD_STEPS));
                if t.elapsed() >= HOLD {
                    eprintln!("remvnc: close button held");
                    done = true;
                }
            }
            (false, None) => {}
        }
        if last_qt.elapsed() >= QT_EVERY {
            epd.process_events(0);
            last_qt = Instant::now();
        }
    }
    if !alive {
        eprintln!("remvnc: lost the host connection");
    }
    let _ = Msg::Bye.write_to(&mut out).and_then(|_| out.flush());
    drop(out);
    drop(ui);
    session.end(&epd)
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
        eprintln!("remvnc: swap {mode:?} returned in {:?}", t.elapsed());
        ok = wait(&epd, 1200);
    }
    drop(canvas);
    session.end(&epd)
}
