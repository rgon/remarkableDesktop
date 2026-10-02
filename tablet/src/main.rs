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
use input::{Pen, Touch, TouchMode};
use remvnc_proto::Msg;
use session::{Session, quitting};
use std::io::{BufWriter, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  remvnc run [--rotate 0|90|180|270] [--touch touchpad|touchscreen]
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
        Some("run") => parse_run(&args[2..]).and_then(|(r, t)| run(r, t)),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = result {
        eprintln!("remvnc: {e}");
        std::process::exit(1);
    }
}

fn parse_run(args: &[String]) -> Result<(u16, TouchMode), String> {
    let (mut rotation, mut touch) = (90, TouchMode::Touchpad);
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
            _ => return Err(format!("unknown argument {a}\n{USAGE}")),
        }
    }
    Ok((rotation, touch))
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

fn run(rotation: u16, touch_mode: TouchMode) -> Result<(), String> {
    let (session, epd) = Session::begin(0)?;
    let inputs = &session.inputs;
    let pen = inputs.pen.as_ref().map(|g| Pen::new(&g.file, rotation)).transpose().map_err(|e| format!("pen: {e}"))?;
    let mut touch =
        inputs.touch.as_ref().map(|g| Touch::new(&g.file, rotation, touch_mode)).transpose().map_err(|e| format!("touch: {e}"))?;

    let mut ui = ui::Ui::new(&epd, rotation);
    ui.draw_home();

    let mut out = BufWriter::new(std::io::stdout().lock());
    let mut hello = vec![Msg::Hello { version: remvnc_proto::VERSION, width: ui.size[0] as u16, height: ui.size[1] as u16, rotation }];
    hello.extend(pen.iter().map(|p| Msg::Device(p.device())));
    hello.extend(touch.iter().map(|t| Msg::Device(t.device())));
    let mut alive = hello.iter().try_for_each(|m| m.write_to(&mut out)).and_then(|_| out.flush()).is_ok();

    let grabbed = [inputs.pen.as_ref(), inputs.touch.as_ref(), inputs.power.as_ref()];
    let fds = grabbed.map(|g| g.map_or(-1, |g| g.file.as_raw_fd()));
    let mut files = grabbed.map(|g| g.and_then(|g| g.file.try_clone().ok()));
    let mut pen_pending = vec![];
    let mut hold_start: Option<Instant> = None;
    let mut done = false;

    while alive && !done && !quitting() {
        let mut pfds: Vec<libc::pollfd> =
            fds.iter().chain([&0]).map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        let timeout = if hold_start.is_some() { 30 } else { 250 };
        unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, timeout) };
        epd.process_events(0);

        let mut frames = vec![];
        if let (Some(pen), Some(f)) = (&pen, files[0].as_mut()) {
            let mut fs = vec![];
            pen.feed(&input::read_events(f).unwrap_or_default(), &mut pen_pending, &mut fs);
            frames.extend(fs.into_iter().map(|e| (input::PEN_ID, e)));
        }
        if let (Some(touch), Some(f)) = (touch.as_mut(), files[1].as_mut()) {
            let events = input::read_events(f).unwrap_or_default();
            let fs = touch.feed(&events, ui.size, |p| ui.on_button(p));
            frames.extend(fs.into_iter().map(|e| (input::TOUCH_ID, e)));
        }
        if let Some(f) = files[2].as_mut() {
            let power = input::read_events(f).unwrap_or_default();
            if power.iter().any(|e| e.kind == input::EV_KEY && e.code == input::KEY_POWER && e.value == 1) {
                eprintln!("remvnc: power button pressed");
                done = true;
            }
        }
        if pfds[3].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let mut buf = [0u8; 256];
            if unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) } <= 0 {
                eprintln!("remvnc: host went away");
                break;
            }
        }

        for (device, events) in frames {
            alive &= Msg::Frame { device, events }.write_to(&mut out).is_ok();
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
