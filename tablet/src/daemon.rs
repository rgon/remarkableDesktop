//! The tablet-side instance. It owns the panel, the inputs and the USB HID
//! function, and keeps running while `remdesktop-host` connects, drops (USB
//! re-enumeration) and reconnects.
//!
//! Input goes out as USB HID reports whenever the computer has enumerated
//! our HID function, so it works with no host software at all. Without USB
//! HID, a connected host replays it through uinput instead. The host is
//! also what streams the screen, so its options are greyed out without it.

use crate::display::{self, Display};
use crate::hid::{self, Gadget, Link as UsbLink};
use crate::hid_report::{self, Encoder, Ranges};
use crate::input::{self, PalmConfig, Pen, PenMode, Touch, TouchMode};
use crate::link::{Incoming, Link, Server};
use crate::session::{Session, quitting};
use crate::ui::{self, Button, Ui};
use remdesktop_proto::{Event, Msg};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

pub struct Args {
    pub refresh: display::Refresh,
    pub rotation: u16,
    /// None: the saved mode (or the default).
    pub touch: Option<TouchMode>,
    pub pen: Option<PenMode>,
    pub palm: PalmConfig,
    /// Add the USB HID function.
    pub usb: bool,
}

/// Idle wake-up interval. The engine hands refresh work through Qt's event
/// loop, so it is serviced on every wake-up (and right after each refresh).
const IDLE_WAKE: Duration = Duration::from_millis(100);

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

/// Runs the calling thread under SCHED_FIFO so input forwarding is not
/// delayed behind the engine's threads (which keep their normal policy).
fn realtime_priority() {
    let param = libc::sched_param { sched_priority: 20 };
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) } != 0 {
        log!("remdesktop: SCHED_FIFO: {}", std::io::Error::last_os_error());
    }
}

/// A connected `remdesktop-host`.
struct Host {
    link: Link,
    incoming: Receiver<Incoming>,
    wake: File,
    /// Its uinput devices are declared (only needed without USB HID).
    declared: bool,
    views: Vec<String>,
    target: u8,
    alive: bool,
    /// Last time anything arrived (the host pings every 100 ms).
    heard: Instant,
    /// Address the host verified it can reach us on without USB.
    remote: Option<String>,
}

impl Host {
    fn send(&mut self, m: &Msg) {
        self.alive &= m.write_to(&mut self.link.out).is_ok();
    }
    fn flush(&mut self) {
        use std::io::Write as _;
        self.alive &= self.link.out.flush().is_ok();
    }
}

/// Coordinate correction the host asked for, per device (HID path only).
#[derive(Clone, Copy)]
struct Mapping {
    shown: [f64; 4],
    to: [f64; 4],
}

impl Default for Mapping {
    fn default() -> Self {
        Self { shown: [0.0, 1.0, 0.0, 1.0], to: [0.0, 1.0, 0.0, 1.0] }
    }
}

impl Mapping {
    fn map(&self, v: i32, a: hid_report::Axis, i: usize) -> i32 {
        let range = (a.max - a.min) as f64;
        let f = (v - a.min) as f64 / range;
        let (s0, s1, t0, t1) = (self.shown[2 * i], self.shown[2 * i + 1], self.to[2 * i], self.to[2 * i + 1]);
        let f = ((f - s0) / (s1 - s0)).clamp(0.0, 1.0);
        (a.min as f64 + (t0 + f * (t1 - t0)) * range).round() as i32
    }

    fn apply(&self, events: &mut [Event], axes: [hid_report::Axis; 2], mt: bool) {
        const ABS_X: u16 = 0x00;
        const ABS_Y: u16 = 0x01;
        const ABS_MT_POSITION_X: u16 = 0x35;
        const ABS_MT_POSITION_Y: u16 = 0x36;
        for e in events.iter_mut().filter(|e| e.kind == input::EV_ABS) {
            match (e.code, mt) {
                (ABS_X, false) | (ABS_MT_POSITION_X, true) => e.value = self.map(e.value, axes[0], 0),
                (ABS_Y, false) | (ABS_MT_POSITION_Y, true) => e.value = self.map(e.value, axes[1], 1),
                _ => {}
            }
        }
    }
}

pub fn run(args: Args) -> Result<(), String> {
    let Args { refresh, rotation, touch: touch_arg, pen: pen_arg, palm, usb } = args;
    let mut state = crate::state::State::load();
    let touch_mode = touch_arg.or(state.touch).unwrap_or(TouchMode::Touchpad);
    let pen_mode = pen_arg.or(state.pen).unwrap_or(PenMode::Tablet);
    state.touch = Some(touch_mode);
    state.pen = Some(pen_mode);
    let (session, epd) = Session::begin(0)?;
    let inputs = &session.inputs;
    let mut pen = inputs.pen.as_ref().map(|g| Pen::new(&g.file, rotation, pen_mode)).transpose().map_err(|e| format!("pen: {e}"))?;
    let mut touch = inputs
        .touch
        .as_ref()
        .map(|g| Touch::new(&g.file, rotation, touch_mode, palm))
        .transpose()
        .map_err(|e| format!("touch: {e}"))?;

    let mut ui = Ui::new(&epd, rotation);
    ui.set_label(Button::Side, "SIDE");
    ui.set_label(Button::Net, "NET");
    ui.set_enabled(Button::Net, false);
    ui.set_label(Button::Screen, "VIEW");
    ui.set_label(Button::Mode, mode_label(touch_mode));
    ui.set_label(Button::Pen, pen_label(pen_mode));
    ui.set_enabled(Button::Screen, false);
    ui.set_status("USB  starting", "HOST not connected");
    ui.draw_home();

    // USB HID: descriptor ranges come from the same declarations uinput uses.
    // (Pen ranges from its tablet-mode declaration, which has every axis.)
    let ranges = match (inputs.pen.as_ref(), &touch) {
        (Some(g), Some(t)) => Pen::new(&g.file, rotation, PenMode::Tablet).ok().map(|p| Ranges::new(&p.device(), &t.device())),
        _ => None,
    };
    let gadget = match (usb, ranges) {
        (true, Some(r)) => {
            let report: &'static [u8] = Box::leak(hid_report::descriptor(&r).into_boxed_slice());
            match Gadget::add(report) {
                Ok(g) => {
                    // Let the computer enumerate before hosts may connect.
                    let t = Instant::now();
                    while g.link() != UsbLink::Up && t.elapsed() < Duration::from_secs(3) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Some(g)
                }
                Err(e) => {
                    log!("remdesktop: usb hid unavailable: {e}");
                    hid::restore();
                    None
                }
            }
        }
        _ => None,
    };
    let server = Server::listen().map_err(|e| format!("listen: {e}"))?;
    server.publish().map_err(|e| format!("session file: {e}"))?;
    realtime_priority();

    let mut encoder = Encoder::default();
    let mut mappings = [Mapping::default(); 2]; // pen, touch
    let mut display = Display::new(refresh);
    let mut host: Option<Host> = None;
    let mut hid_was_up = false;
    // Last status sent to the host (usb hid, pen mouse, touchpad).
    let mut sent_status: Option<(bool, bool, bool)> = None;
    let grabbed = [inputs.pen.as_ref(), inputs.touch.as_ref(), inputs.power.as_ref()];
    let mut files = grabbed.map(|g| g.and_then(|g| g.file.try_clone().ok()));
    let input_fds: Vec<i32> = grabbed.iter().map(|g| g.map_or(-1, |g| g.file.as_raw_fd())).collect();
    // (button, since, already fired): a hold fires once, then needs a release.
    let mut hold: Option<(Button, Instant, bool)> = None;
    let mut done = false;
    // Non-USB addresses, re-read every few seconds and sent to the host on change.
    let mut addresses = crate::link::network_addresses();
    let mut addresses_at = Instant::now();

    while !done && !quitting() {
        let mut fds = input_fds.clone();
        fds.push(server.fd());
        fds.push(host.as_ref().map_or(-1, |h| h.wake.as_raw_fd()));
        let mut pfds: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        let mut timeout = if hold.is_some() { 30 } else { IDLE_WAKE.as_millis() as i32 };
        if let Some(t) = display.next_tick_ms() {
            timeout = timeout.min(t);
        }
        unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, timeout) };

        let hid_up = gadget.as_ref().is_some_and(|g| g.link() == UsbLink::Up);

        // Pen first: its hover state gates touches in the same wake-up.
        let mut frames = vec![];
        if let (Some(pen), Some(f)) = (pen.as_mut(), files[0].as_mut()) {
            let events = input::read_events(f).unwrap_or_default();
            frames.extend(pen.feed(&events).into_iter().map(|(t, e)| (input::PEN_ID, t, e)));
        }
        if let Some(touch) = touch.as_mut() {
            let now = remdesktop_proto::monotonic_us();
            if let Some(lift) = touch.set_pen(pen.as_ref().is_some_and(|p| p.in_range), now) {
                frames.push((input::TOUCH_ID, now, lift));
            }
            if let Some(f) = files[1].as_mut() {
                let events = input::read_events(f).unwrap_or_default();
                let fs = touch.feed(&events, ui.size, |p| ui.button_at(p).filter(|&b| ui.enabled(b)).map(|b| b as u8));
                frames.extend(fs.into_iter().map(|(t, e)| (input::TOUCH_ID, t, e)));
            }
        }

        // Route input: USB HID if the computer enumerated it, else the host.
        if let Some(h) = host.as_mut() {
            if !hid_up && !h.declared {
                let decls: Vec<Msg> = pen.iter().map(|p| Msg::Device(p.device())).chain(touch.iter().map(|t| Msg::Device(t.device()))).collect();
                decls.iter().for_each(|m| h.send(m));
                h.declared = true;
            }
            let status = (
                hid_up,
                pen.as_ref().is_some_and(|p| p.mode() == PenMode::Mouse),
                touch.as_ref().is_some_and(|t| t.mode() == TouchMode::Touchpad),
            );
            if sent_status != Some(status) {
                h.send(&Msg::Status { usb_hid: status.0, pen_mouse: status.1, touchpad: status.2 });
                sent_status = Some(status);
            }
        }
        hid_was_up = hid_up;
        for (device, time_us, mut events) in frames {
            if let (true, Some(g), Some(r)) = (hid_up, gadget.as_ref(), ranges.as_ref()) {
                let report = if device == input::PEN_ID {
                    mappings[0].apply(&mut events, r.pen, false);
                    encoder.pen(&events, pen.as_ref().is_some_and(|p| p.mode() == PenMode::Mouse))
                } else {
                    mappings[1].apply(&mut events, r.touch, true);
                    encoder.touch(&events, touch.as_ref().is_some_and(|t| t.mode() == TouchMode::Touchpad))
                };
                // An empty frame tells the host when this report left, so it
                // can measure USB HID latency against what it receives.
                if g.send(&report) && device == input::PEN_ID {
                    if let Some(h) = host.as_mut() {
                        h.send(&Msg::Frame { device, time_us, events: vec![] });
                    }
                }
            } else if let Some(h) = host.as_mut() {
                h.send(&Msg::Frame { device, time_us, events });
            }
        }

        if let Some(f) = files[2].as_mut() {
            let power = input::read_events(f).unwrap_or_default();
            if power.iter().any(|e| e.kind == input::EV_KEY && e.code == input::KEY_POWER && e.value == 1) {
                log!("remdesktop: power button pressed");
                done = true;
            }
        }

        // A (re)connecting host replaces the current one.
        if pfds[3].revents != 0 {
            if let Some(mut link) = server.accept() {
                if let Some(old) = host.take() {
                    old.link.close();
                }
                match link.spawn_reader(epd.fb) {
                    Ok((incoming, wake)) => {
                        let mut h = Host { link, incoming, wake, declared: false, views: vec![], target: 0, alive: true, heard: Instant::now(), remote: None };
                        h.send(&Msg::Addresses { addrs: addresses.clone() });
                        h.send(&Msg::Hello {
                            version: remdesktop_proto::VERSION,
                            width: ui.size[0] as u16,
                            height: ui.size[1] as u16,
                            rotation,
                            view: state.view.clone(),
                        });
                        sent_status = None; // sent with the next wake-up
                        host = Some(h);
                    }
                    Err(e) => log!("remdesktop: host reader: {e}"),
                }
            }
        }
        let mut host_gone = false;
        if let Some(h) = host.as_mut().filter(|_| pfds[4].revents != 0) {
            let mut drain = [0u8; 64];
            let _ = std::io::Read::read(&mut h.wake, &mut drain);
            h.heard = Instant::now();
            while let Ok(item) = h.incoming.try_recv() {
                match item {
                    Incoming::Msg(Msg::Ping { host_us }) => h.send(&Msg::Pong { host_us, tablet_us: remdesktop_proto::monotonic_us() }),
                    Incoming::Msg(Msg::Screens { names }) if !names.is_empty() => {
                        log!("remdesktop: host views: {}", names.join(", "));
                        h.views = names;
                        // The host resumes the view named in our Hello.
                        h.target = h.views.iter().position(|v| *v == state.view).unwrap_or(0) as u8;
                        ui.set_label(Button::Screen, &h.views[h.target as usize]);
                        ui.set_enabled(Button::Screen, h.views.len() > 1);
                    }
                    Incoming::Msg(Msg::RemoteOk { addr }) => {
                        h.remote = (!addr.is_empty()).then_some(addr);
                        ui.set_enabled(Button::Net, h.remote.is_some() && crate::link::is_usb(h.link.peer));
                    }
                    Incoming::Msg(Msg::Mapping { device, shown, to }) => {
                        let f = |v: [u16; 4]| v.map(|x| x as f64 / 65535.0);
                        let i = if device == input::PEN_ID { 0 } else { 1 };
                        mappings[i] = Mapping { shown: f(shown), to: f(to) };
                    }
                    Incoming::Msg(_) => {}
                    Incoming::Decoded { id, rect } => {
                        // Ack first: the host prepares the next update while we refresh.
                        h.send(&Msg::Ack { id });
                        h.flush();
                        match rect {
                            Ok(r) => {
                                if ui.overlays_phys().iter().any(|o| r.intersects(*o)) {
                                    ui.redraw_overlays();
                                }
                                display.show(&epd, r);
                            }
                            Err(e) => log!("remdesktop: {e}"),
                        }
                    }
                    Incoming::Closed => host_gone = true,
                }
            }
        }
        if let Some(h) = host.as_mut() {
            h.flush();
            host_gone |= !h.alive;
            // Silence means the link is dead (e.g. USB unplugged): TCP alone
            // would not notice for minutes.
            if h.heard.elapsed() > Duration::from_millis(1500) {
                log!("remdesktop: host silent for {:?}", h.heard.elapsed());
                h.link.close();
                host_gone = true;
            }
        }
        if addresses_at.elapsed() >= Duration::from_secs(5) {
            addresses_at = Instant::now();
            let now = crate::link::network_addresses();
            if now != addresses {
                addresses = now;
                if let Some(h) = host.as_mut() {
                    h.send(&Msg::Addresses { addrs: addresses.clone() });
                }
            }
        }
        if host_gone {
            log!("remdesktop: host went away");
            host = None;
            ui.set_enabled(Button::Net, false);
            mappings = [Mapping::default(); 2];
            ui.set_label(Button::Side, "SIDE");
    ui.set_label(Button::Net, "NET");
    ui.set_enabled(Button::Net, false);
    ui.set_label(Button::Screen, "VIEW");
            ui.set_enabled(Button::Screen, false);
            // Streamed content would otherwise stay on the panel.
            ui.draw_home();
        }

        let usb_line = match (&gadget, gadget.as_ref().map(|g| g.link())) {
            (None, _) if !usb => "USB  off",
            (None, _) => "USB  unavailable",
            (_, Some(UsbLink::Up)) => "USB  connected (HID input)",
            (_, Some(UsbLink::Suspended)) => "USB  computer asleep",
            _ => "USB  not connected",
        };
        let host_line = match &host {
            Some(h) if !crate::link::is_usb(h.link.peer) => format!("HOST {} (unplug ok)", h.link.peer),
            Some(_) if hid_up => "HOST connected (display)".into(),
            Some(_) => "HOST connected (input, display)".into(),
            None => "HOST not connected".into(),
        };
        ui.set_status(usb_line, &host_line);

        // Hold buttons.
        let held = touch.as_ref().and_then(|t| t.holding()).map(|b| Button::ALL[b as usize]).filter(|&b| ui.enabled(b));
        match (held, hold) {
            (Some(b), Some((h, since, fired))) if b == h => {
                if !fired {
                    let elapsed = since.elapsed();
                    let steps = (elapsed.as_millis() * ui::HOLD_STEPS as u128 / b.hold().as_millis()) as u32;
                    ui.set_progress(b, steps.min(ui::HOLD_STEPS));
                    if elapsed >= b.hold() {
                        hold = Some((b, since, true));
                        let now = remdesktop_proto::monotonic_us();
                        match b {
                            Button::Close => {
                                log!("remdesktop: close button held");
                                done = true;
                            }
                            Button::Mode => {
                                if let Some(t) = touch.as_mut() {
                                    let next = match t.mode() {
                                        TouchMode::Touchpad => TouchMode::Touchscreen,
                                        TouchMode::Touchscreen => TouchMode::Touchpad,
                                    };
                                    let was_pad = t.mode() == TouchMode::Touchpad;
                                    let lift = t.set_mode(next, now);
                                    if let (true, Some(g)) = (hid_up, gadget.as_ref()) {
                                        g.send(&encoder.release_all(was_pad, false)[0]);
                                    } else if let Some(h) = host.as_mut() {
                                        if let Some(lift) = lift {
                                            h.send(&Msg::Frame { device: input::TOUCH_ID, time_us: now, events: lift });
                                        }
                                        h.send(&Msg::Device(t.device()));
                                    }
                                    log!("remdesktop: touch is now {next:?}");
                                    ui.set_label(b, mode_label(next));
                                    state.touch = Some(next);
                                    state.save();
                                }
                            }
                            Button::Pen => {
                                if let Some(p) = pen.as_mut() {
                                    let next = match p.mode() {
                                        PenMode::Tablet => PenMode::Mouse,
                                        PenMode::Mouse => PenMode::Tablet,
                                    };
                                    let was_mouse = p.mode() == PenMode::Mouse;
                                    let release = p.set_mode(next);
                                    if let (true, Some(g)) = (hid_up, gadget.as_ref()) {
                                        g.send(&encoder.release_all(false, was_mouse)[1]);
                                    } else if let Some(h) = host.as_mut() {
                                        if let Some(release) = release {
                                            h.send(&Msg::Frame { device: input::PEN_ID, time_us: now, events: release });
                                        }
                                        h.send(&Msg::Device(p.device()));
                                    }
                                    log!("remdesktop: pen is now {next:?}");
                                    ui.set_label(b, pen_label(next));
                                    state.pen = Some(next);
                                    state.save();
                                }
                            }
                            Button::Side => {}
                            Button::Net => {
                                if let Some(h) = host.as_mut().filter(|h| h.remote.is_some()) {
                                    log!("remdesktop: going remote via {}", h.remote.as_deref().unwrap_or(""));
                                    h.send(&Msg::GoRemote);
                                }
                            }
                            Button::Screen => {
                                if let Some(h) = host.as_mut().filter(|h| h.views.len() > 1) {
                                    h.target = (h.target + 1) % h.views.len() as u8;
                                    let (target, label) = (h.target, h.views[h.target as usize].clone());
                                    h.send(&Msg::Target { index: target });
                                    ui.set_label(b, &label);
                                    state.view = label;
                                    state.save();
                                }
                            }
                        }
                        if let Some(h) = host.as_mut() {
                            h.flush();
                        }
                    }
                }
            }
            (Some(b), _) => {
                if let Some((h, _, _)) = hold {
                    ui.set_progress(h, 0);
                }
                hold = Some((b, Instant::now(), false));
                if b == Button::Side {
                    // Acts on touch: click the pen's side button where it is.
                    hold = Some((b, Instant::now(), true));
                    ui.set_progress(b, ui::HOLD_STEPS);
                    if let Some(p) = pen.as_ref() {
                        let now = remdesktop_proto::monotonic_us();
                        let mouse = p.mode() == PenMode::Mouse;
                        for mut events in p.side_click() {
                            if let (true, Some(g), Some(r)) = (hid_up, gadget.as_ref(), ranges.as_ref()) {
                                mappings[0].apply(&mut events, r.pen, false);
                                g.send(&encoder.pen(&events, mouse));
                            } else if let Some(h) = host.as_mut() {
                                h.send(&Msg::Frame { device: input::PEN_ID, time_us: now, events });
                            }
                        }
                        if let Some(h) = host.as_mut() {
                            h.flush();
                        }
                    }
                }
            }
            (None, Some((h, _, _))) => {
                hold = None;
                ui.set_progress(h, 0);
            }
            (None, None) => {}
        }
        display.tick(&epd);
        epd.process_events(0);
    }

    // Release anything held, tell the host, take our USB function out.
    log!("remdesktop: shutting down");
    if let (Some(g), true) = (gadget.as_ref(), hid_was_up) {
        for r in encoder.release_all(touch.as_ref().is_some_and(|t| t.mode() == TouchMode::Touchpad), pen.as_ref().is_some_and(|p| p.mode() == PenMode::Mouse)) {
            g.send(&r);
        }
    }
    if let Some(mut h) = host.take() {
        h.send(&Msg::Bye);
        h.flush();
    }
    drop(server);
    if gadget.is_some() {
        log!("remdesktop: removing the usb hid function");
        drop(gadget);
        hid::restore();
    }
    drop(ui);
    log!("remdesktop: restoring the screen");
    let _ = std::io::stderr().flush();
    session.end(&epd)
}
