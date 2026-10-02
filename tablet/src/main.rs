/// Logs to stderr, ignoring failures: stderr is the SSH channel, which may
/// already be gone while we restore the tablet (eprintln! would panic).
macro_rules! log {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($t)*);
    }};
}

mod display;
mod epd;
mod gfx;
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
use input::{PalmConfig, Pen, PenMode, Touch, TouchMode};
use ui::Button;
use remvnc_proto::Msg;
use session::{Session, quitting};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  remvnc run [--rotate 0|90|180|270] [--touch touchpad|touchscreen] [--pen tablet|mouse]
             [--palm-major N] [--pen-grace-ms N] [--log-touch] [--ssh-only]
             [--fast-mode M] [--settle-mode M] [--settle-ms N]
             (modes: pen, mono, animation, ui, content)
      Forward pen/touch to remvnc-host (on stdout, or a direct socket it
      is told about) until the X is held.
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
        log!("remvnc: {e}");
        std::process::exit(1);
    }
}

struct RunArgs {
    refresh: display::Refresh,
    rotation: u16,
    touch: TouchMode,
    pen: PenMode,
    palm: PalmConfig,
    ssh_only: bool,
}

fn parse_run(args: &[String]) -> Result<RunArgs, String> {
    let (mut rotation, mut touch, mut palm, mut ssh_only) = (90, TouchMode::Touchpad, PalmConfig::default(), false);
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
            "--ssh-only" => ssh_only = true,
            _ => return Err(format!("unknown argument {a}\n{USAGE}")),
        }
    }
    Ok(RunArgs { refresh, rotation, touch, pen, palm, ssh_only })
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


/// Runs the calling thread under SCHED_FIFO so input forwarding is not
/// delayed behind the engine's threads (which keep their normal policy).
fn realtime_priority() {
    let param = libc::sched_param { sched_priority: 20 };
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) } != 0 {
        log!("remvnc: SCHED_FIFO: {}", std::io::Error::last_os_error());
    }
}

/// Qt only needs servicing for the engine's own bookkeeping, not per event.
const QT_EVERY: Duration = Duration::from_millis(100);

fn mode_label(m: TouchMode) -> &'static str {
    match m {
        TouchMode::Touchpad => "PAD",
        TouchMode::Touchscreen => "TOUCH",
    }
}

fn pen_label(m: PenMode) -> &'static str {
    match m {
        PenMode::Tablet => "PEN",
        PenMode::Mouse => "MOUSE",
    }
}

fn run(args: RunArgs) -> Result<(), String> {
    let RunArgs { refresh, rotation, touch: touch_mode, pen: pen_mode, palm, ssh_only } = args;
    // Before pausing xochitl: the host must answer within a few seconds.
    let mut link = link::Link::open(ssh_only);
    let (session, epd) = Session::begin(0)?;
    let inputs = &session.inputs;
    let mut pen = inputs.pen.as_ref().map(|g| Pen::new(&g.file, rotation, pen_mode)).transpose().map_err(|e| format!("pen: {e}"))?;
    let mut touch = inputs
        .touch
        .as_ref()
        .map(|g| Touch::new(&g.file, rotation, touch_mode, palm))
        .transpose()
        .map_err(|e| format!("touch: {e}"))?;

    let mut ui = ui::Ui::new(&epd, rotation);
    let mut target: u8 = 0;
    // View labels from the host; the screen button cycles through them.
    let mut views: Vec<String> = vec!["ALL".into()];
    let mut display = display::Display::new(refresh);
    ui.set_label(Button::Screen, &views[0]);
    ui.set_label(Button::Mode, mode_label(touch_mode));
    ui.set_label(Button::Pen, pen_label(pen_mode));
    ui.draw_home();
    realtime_priority();

    let out = &mut link.out;
    let mut hello = vec![Msg::Hello { version: remvnc_proto::VERSION, width: ui.size[0] as u16, height: ui.size[1] as u16, rotation }];
    hello.extend(pen.iter().map(|p| Msg::Device(p.device())));
    hello.extend(touch.iter().map(|t| Msg::Device(t.device())));
    let mut alive = hello.iter().try_for_each(|m| m.write_to(out)).and_then(|_| out.flush()).is_ok();

    let grabbed = [inputs.pen.as_ref(), inputs.touch.as_ref(), inputs.power.as_ref()];
    let (incoming, mut wake) = link.spawn_reader(epd.fb).map_err(|e| format!("reader: {e}"))?;
    let mut fds: Vec<i32> = grabbed.iter().map(|g| g.map_or(-1, |g| g.file.as_raw_fd())).collect();
    fds.push(wake.as_raw_fd());
    // With a direct link, ssh's stdin still tells us when the host is gone.
    fds.push(if link.is_direct() { 0 } else { -1 });
    let mut files = grabbed.map(|g| g.and_then(|g| g.file.try_clone().ok()));
    // (button, since, already fired): a hold fires once, then needs a release.
    let mut hold: Option<(Button, Instant, bool)> = None;
    let mut last_qt = Instant::now();
    let mut done = false;

    while alive && !done && !quitting() {
        let mut pfds: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        let mut timeout = if hold.is_some() { 30 } else { QT_EVERY.as_millis() as i32 };
        if let Some(t) = display.next_tick_ms() {
            timeout = timeout.min(t);
        }
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
                let fs = touch.feed(&events, ui.size, |p| ui.button_at(p).map(|b| b as u8));
                frames.extend(fs.into_iter().map(|(t, e)| (input::TOUCH_ID, t, e)));
            }
        }
        let out = &mut link.out;
        for (device, time_us, events) in frames {
            alive &= Msg::Frame { device, time_us, events }.write_to(out).is_ok();
        }

        if let Some(f) = files[2].as_mut() {
            let power = input::read_events(f).unwrap_or_default();
            if power.iter().any(|e| e.kind == input::EV_KEY && e.code == input::KEY_POWER && e.value == 1) {
                log!("remvnc: power button pressed");
                done = true;
            }
        }
        if pfds[4].revents != 0 {
            // ssh closed under a direct link.
            let mut b = [0u8; 64];
            if unsafe { libc::read(0, b.as_mut_ptr().cast(), b.len()) } <= 0 {
                log!("remvnc: ssh session ended");
                break;
            }
        }
        if pfds[3].revents != 0 {
            let mut drain = [0u8; 64];
            let _ = std::io::Read::read(&mut wake, &mut drain);
            let mut closed = false;
            while let Ok(item) = incoming.try_recv() {
                match item {
                    link::Incoming::Msg(Msg::Ping { host_us }) => {
                        let pong = Msg::Pong { host_us, tablet_us: remvnc_proto::monotonic_us() };
                        alive &= pong.write_to(&mut link.out).is_ok();
                    }
                    link::Incoming::Msg(Msg::Screens { names }) if !names.is_empty() => {
                        log!("remvnc: host views: {}", names.join(", "));
                        views = names;
                        target = 0;
                        ui.set_label(Button::Screen, &views[0]);
                    }
                    link::Incoming::Msg(_) => {}
                    link::Incoming::Decoded { id, rect } => {
                        // Ack first: the host prepares the next update while we refresh.
                        alive &= Msg::Ack { id }.write_to(&mut link.out).and_then(|_| link.out.flush()).is_ok();
                        match rect {
                            Ok(r) => {
                                if r.intersects(ui.bar_phys()) {
                                    ui.redraw_buttons();
                                }
                                display.show(&epd, r);
                            }
                            Err(e) => log!("remvnc: {e}"),
                        }
                    }
                    link::Incoming::Closed => closed = true,
                }
            }
            if closed {
                log!("remvnc: host went away");
                break;
            }
        }
        alive &= link.out.flush().is_ok();

        let held = touch.as_ref().and_then(|t| t.holding()).map(|b| Button::ALL[b as usize]);
        match (held, hold) {
            (Some(b), Some((h, since, fired))) if b == h => {
                if !fired {
                    let elapsed = since.elapsed();
                    let steps = (elapsed.as_millis() * ui::HOLD_STEPS as u128 / b.hold().as_millis()) as u32;
                    ui.set_progress(b, steps.min(ui::HOLD_STEPS));
                    if elapsed >= b.hold() {
                        hold = Some((b, since, true));
                        match b {
                            Button::Close => {
                                log!("remvnc: close button held");
                                done = true;
                            }
                            Button::Mode => {
                                if let Some(t) = touch.as_mut() {
                                    let next = match t.mode() {
                                        TouchMode::Touchpad => TouchMode::Touchscreen,
                                        TouchMode::Touchscreen => TouchMode::Touchpad,
                                    };
                                    let now = remvnc_proto::monotonic_us();
                                    if let Some(lift) = t.set_mode(next, now) {
                                        alive &= Msg::Frame { device: input::TOUCH_ID, time_us: now, events: lift }.write_to(&mut link.out).is_ok();
                                    }
                                    alive &= Msg::Device(t.device()).write_to(&mut link.out).is_ok();
                                    log!("remvnc: touch is now {next:?}");
                                    ui.set_label(b, mode_label(next));
                                }
                            }
                            Button::Pen => {
                                if let Some(p) = pen.as_mut() {
                                    let next = match p.mode() {
                                        PenMode::Tablet => PenMode::Mouse,
                                        PenMode::Mouse => PenMode::Tablet,
                                    };
                                    let now = remvnc_proto::monotonic_us();
                                    if let Some(release) = p.set_mode(next) {
                                        alive &= Msg::Frame { device: input::PEN_ID, time_us: now, events: release }.write_to(&mut link.out).is_ok();
                                    }
                                    alive &= Msg::Device(p.device()).write_to(&mut link.out).is_ok();
                                    log!("remvnc: pen is now {next:?}");
                                    ui.set_label(b, pen_label(next));
                                }
                            }
                            Button::Screen => {
                                if views.len() > 1 {
                                    target = (target + 1) % views.len() as u8;
                                    alive &= Msg::Target { index: target }.write_to(&mut link.out).is_ok();
                                    ui.set_label(b, &views[target as usize]);
                                }
                            }
                        }
                        alive &= link.out.flush().is_ok();
                    }
                }
            }
            (Some(b), _) => {
                if let Some((h, _, _)) = hold {
                    ui.set_progress(h, 0);
                }
                hold = Some((b, Instant::now(), false));
            }
            (None, Some((h, _, _))) => {
                hold = None;
                ui.set_progress(h, 0);
            }
            (None, None) => {}
        }
        display.tick(&epd);
        if last_qt.elapsed() >= QT_EVERY {
            epd.process_events(0);
            last_qt = Instant::now();
        }
    }
    if !alive {
        log!("remvnc: lost the host connection");
    }
    let _ = Msg::Bye.write_to(&mut link.out).and_then(|_| link.out.flush());
    drop(link);
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
        log!("remvnc: swap {mode:?} returned in {:?}", t.elapsed());
        ok = wait(&epd, 1200);
    }
    drop(canvas);
    session.end(&epd)
}
