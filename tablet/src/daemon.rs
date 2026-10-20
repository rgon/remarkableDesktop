//! The tablet-side instance. It owns the panel, the inputs and the USB HID
//! function, and keeps running while `remdesktop-host` connects, drops (USB
//! re-enumeration) and reconnects.
//!
//! Input goes out as USB HID reports whenever the computer has enumerated
//! our HID function, so it works with no host software at all. Without USB
//! HID, a connected host replays it through uinput instead. The host is
//! also what streams the screen, so its options are greyed out without it.

use crate::display::{self, Display};
use crate::epd::{Epd, Mode};
use crate::hid::{self, Gadget, Link as UsbLink};
use crate::hid_report::{self, Encoder, Ranges};
use crate::ink::Ink;
use crate::input::{self, PalmConfig, Pen, PenMode, Touch, TouchMode};
use crate::link::{Incoming, Link, Server};
use crate::session::{Session, quitting};
use crate::ui::{self, Button, Ui};
use remdesktop_proto::{DisplayMode, Event, Msg};
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

fn display_label(m: DisplayMode) -> &'static str {
    match m {
        DisplayMode::Normal => "NRM",
        DisplayMode::None => "OFF",
        DisplayMode::Edge => "EDGE",
    }
}

fn ink_label(on: bool) -> &'static str {
    if on { "INK*" } else { "INK" }
}

/// Wipes the ink preview off the panel, with the quality refresh that also
/// clears the fast mode's ghosting.
fn clear_ink(ink: &mut Ink, epd: &Epd, display: &Display) {
    if let Some(r) = ink.restore(&epd.fb) {
        epd.swap(r.x, r.y, r.w, r.h, display.settle(), 0);
        epd.process_events(0);
    }
}

/// The CLR button: puts back what the ink covered, then refreshes the whole
/// panel in the flashing full-quality mode, which clears every ghost, not
/// just the ink's.
fn clear_screen(ink: &mut Ink, epd: &Epd) {
    ink.restore(&epd.fb);
    epd.swap(0, 0, epd.width(), epd.height(), Mode::Content, 0);
    epd.process_events(0);
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
    /// Keys typing z and y in the host's keyboard layout. Undo/redo send
    /// Ctrl + these while known, else the dedicated Undo/Redo keys.
    keys: Option<(u16, u16)>,
    /// Pen and touch over UDP, offered to hosts on Wi-Fi.
    udp: Option<UdpPath>,
}

/// One host's UDP path for pen and touch: set up by the host's probe, used
/// from its `UdpReady` on.
struct UdpPath {
    pen: remdesktop_proto::pen::PenTx,
    touch: remdesktop_proto::touch::TouchTx,
    /// Where the host's probe came from.
    peer: Option<std::net::SocketAddr>,
    ready: bool,
}

impl UdpPath {
    fn new(key: u32) -> Self {
        UdpPath { pen: remdesktop_proto::pen::PenTx::new(key), touch: remdesktop_proto::touch::TouchTx::new(key), peer: None, ready: false }
    }

    /// A frame of the pen or touch screen: the datagram to send and the
    /// copy of its step for TCP, or None if it goes another way (it is then
    /// only followed, so the first datagram is true).
    fn route(&mut self, device: u8, usb_hid: bool, time_us: u64, events: &[Event]) -> Option<(std::net::SocketAddr, remdesktop_proto::sync::Packet)> {
        let peer = self.peer.filter(|_| self.ready && !usb_hid);
        let now = remdesktop_proto::monotonic_us();
        match (device, peer) {
            (input::PEN_ID, Some(p)) => Some((p, self.pen.frame(time_us, now, events))),
            (input::TOUCH_ID, Some(p)) => Some((p, self.touch.frame(time_us, now, events))),
            (input::PEN_ID, None) => {
                self.pen.track(events);
                None
            }
            (input::TOUCH_ID, None) => {
                self.touch.track(events);
                None
            }
            _ => None,
        }
    }

    /// Datagrams of due repeats.
    fn repeats(&mut self, usb_hid: bool) -> Vec<(std::net::SocketAddr, Vec<u8>)> {
        let now = remdesktop_proto::monotonic_us();
        let Some(peer) = self.peer.filter(|_| self.ready && !usb_hid) else { return vec![] };
        [self.pen.tick(now), self.touch.tick(now)].into_iter().flatten().map(|d| (peer, d)).collect()
    }

    fn next_in_ms(&self) -> Option<i32> {
        let now = remdesktop_proto::monotonic_us();
        [self.pen.next_in_ms(now), self.touch.next_in_ms(now)].into_iter().flatten().min()
    }
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
    ui.set_label(Button::Undo, "UNDO");
    ui.set_label(Button::Redo, "REDO");
    ui.set_label(Button::Side, "SIDE");
    ui.set_label(Button::Net, "NET");
    ui.set_enabled(Button::Net, false);
    ui.set_label(Button::Screen, "VIEW");
    ui.set_label(Button::Mode, mode_label(touch_mode));
    ui.set_label(Button::Pen, pen_label(pen_mode));
    ui.set_label(Button::Display, display_label(state.display));
    ui.set_label(Button::Ink, ink_label(state.ink));
    ui.set_label(Button::Clear, "CLR");
    ui.set_enabled(Button::Clear, state.ink);
    ui.set_enabled(Button::Screen, false);
    ui.set_battery(crate::link::battery_percent());
    ui.set_status("USB  starting", "HOST not connected", &wifi_line(&crate::link::network_addresses()));
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
    // The pen stays off the computer over our buttons, and presses them instead.
    let mut gate = crate::pen_gate::PenGate::default();
    let mut pen_pressing: Option<Button> = None;
    let pen_axes = ranges.as_ref().map(|r| r.pen);
    let mut mappings = [Mapping::default(); 2]; // pen, touch
    let mut display = Display::new(refresh);
    let mut ink = Ink::new(state.ink);
    let mut host: Option<Host> = None;
    let mut power_save = crate::link::PowerSave::default();
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
    // What the info box last showed: rebuilt only when this changes.
    let mut addresses_gen = 0u32;
    let mut status_key = None;

    while !done && !quitting() {
        let mut fds = input_fds.clone();
        fds.push(server.fd());
        fds.push(host.as_ref().map_or(-1, |h| h.wake.as_raw_fd()));
        fds.push(server.udp_fd());
        let mut pfds: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
        let mut timeout = if hold.is_some() { 30 } else { IDLE_WAKE.as_millis() as i32 };
        if let Some(t) = host.as_ref().and_then(|h| h.udp.as_ref()).and_then(|u| u.next_in_ms()) {
            timeout = timeout.min(t);
        }
        unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, timeout) };

        let hid_up = gadget.as_ref().is_some_and(|g| g.link() == UsbLink::Up);

        // Pen first: its hover state gates touches in the same wake-up.
        let mut frames = vec![];
        if let (Some(pen), Some(f)) = (pen.as_mut(), files[0].as_mut()) {
            let events = input::read_events(f).unwrap_or_default();
            let mouse = pen.mode() == PenMode::Mouse;
            let size = ui.size;
            let range = pen.pos_range();
            let zone = |pos: [i32; 2]| match pen_axes {
                Some(a) => ui.pen_zone([0, 1].map(|i| {
                    ((pos[i] - a[i].min) as i64 * size[i] as i64 / (a[i].max - a[i].min).max(1) as i64) as i32
                })),
                None => crate::pen_gate::Zone::Outside,
            };
            for (t, e) in pen.feed(&events) {
                if let Some(p) = ink.feed(&e, mouse) {
                    let at = [0, 1].map(|i| ((p[i] - range[i].0) as i64 * size[i] as i64 / (range[i].1 - range[i].0).max(1) as i64) as i32);
                    if ui.pen_zone(at) == crate::pen_gate::Zone::Outside {
                        ink.stroke(&epd.fb, ui.to_panel(at));
                    } else {
                        ink.lift();
                    }
                }
                let g = gate.filter(e, mouse, &zone);
                pen_pressing = g.pressing.filter(|&b| ui.enabled(b));
                if let Some(e) = g.events {
                    frames.push((input::PEN_ID, t, e));
                }
            }
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
                let decls: Vec<Msg> = pen
                    .iter()
                    .map(|p| Msg::Device(p.device()))
                    .chain(touch.iter().map(|t| Msg::Device(t.device())))
                    .chain([Msg::Device({
                        let (u, r) = h.keys.unwrap_or((input::KEY_Z, input::KEY_Y));
                        input::keys_device(u, r)
                    })])
                    .collect();
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
            if let Some((peer, packet)) = host.as_mut().and_then(|h| h.udp.as_mut()).and_then(|u| u.route(device, hid_up, time_us, &events)) {
                server.udp_send(peer, &packet.datagram);
                if let Some(h) = host.as_mut() {
                    packet.edge.iter().for_each(|edge| h.send(edge));
                }
                continue;
            }
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

        // Repeats of the latest pen and touch states, and the hosts' probes.
        for (peer, d) in host.as_mut().and_then(|h| h.udp.as_mut()).map(|u| u.repeats(hid_up)).unwrap_or_default() {
            server.udp_send(peer, &d);
        }
        for (from, d) in server.udp_recv() {
            use remdesktop_proto::Datagram;
            if let (Some(h), Datagram::Probe { key }) = (host.as_mut(), d) {
                if let Some(u) = h.udp.as_mut().filter(|u| u.pen.key() == key && from.ip() == h.link.peer) {
                    u.peer = Some(from);
                    server.udp_send(from, &Datagram::Ack { key }.encode());
                }
            }
        }
        // Input leaves before the panel is touched: the refresh call below
        // can take a while.
        if let Some(h) = host.as_mut() {
            h.flush();
        }
        // One refresh per wake-up, whatever number of pen frames arrived.
        if let Some(r) = ink.take_pending() {
            epd.swap(r.x, r.y, r.w, r.h, display.fast(), 0);
            epd.process_events(0);
        }

        if let Some(f) = files[2].as_mut().filter(|_| pfds[2].revents != 0) {
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
                // Wi-Fi power save adds tens of ms of jitter: off while a
                // host is on Wi-Fi, back to what it was otherwise.
                if crate::link::is_usb(link.peer) {
                    power_save.restore();
                } else {
                    power_save.disable();
                }
                match link.spawn_reader(epd.fb) {
                    Ok((incoming, wake)) => {
                        let mut h = Host { link, incoming, wake, declared: false, views: vec![], target: 0, alive: true, heard: Instant::now(), remote: None, keys: None, udp: None };
                        h.send(&Msg::Addresses { addrs: addresses.clone() });
                        h.send(&Msg::Hello {
                            version: remdesktop_proto::VERSION,
                            width: ui.size[0] as u16,
                            height: ui.size[1] as u16,
                            rotation,
                            view: state.view.clone(),
                        });
                        h.send(&Msg::Display { mode: state.display });
                        // Wi-Fi hosts get the pen over UDP if they can reach it.
                        if !crate::link::is_usb(h.link.peer) {
                            let key = crate::link::random_key();
                            h.udp = Some(UdpPath::new(key));
                            h.send(&Msg::UdpOffer { port: server.udp_port(), key });
                        }
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
                    Incoming::Msg(Msg::Keys { undo, redo }) => {
                        if h.keys != Some((undo, redo)) {
                            log!("remdesktop: undo/redo now Ctrl + key codes {undo}/{redo}");
                            h.keys = Some((undo, redo));
                            if h.declared {
                                h.send(&Msg::Device(input::keys_device(undo, redo)));
                            }
                        }
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
                    Incoming::Msg(Msg::UdpReady) => {
                        if let Some(u) = h.udp.as_mut().filter(|u| u.peer.is_some()) {
                            log!("remdesktop: pen and touch over UDP");
                            u.ready = true;
                        }
                    }
                    Incoming::Msg(_) => {}
                    Incoming::Decoded { id, flags, rect } => {
                        match rect {
                            // An update that was already on its way when the screen was switched off.
                            Ok(_) if state.display == DisplayMode::None => {
                                ink.forget();
                                ui.draw_home();
                            }
                            Ok(r) => {
                                ui.redraw_overlays_in(r);
                                display.show(&epd, r, flags);
                                ink.overwritten(&epd.fb, r);
                            }
                            Err(e) => log!("remdesktop: {e}"),
                        }
                        // Acked once the panel has been told, and only for the last
                        // rectangle of an update: the host then sends what is newest
                        // by then, instead of an update that would wait, already
                        // stale, behind the refresh we just started.
                        if flags & remdesktop_proto::RECT_LAST != 0 {
                            h.send(&Msg::Ack { id });
                            h.flush();
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
            ui.set_battery(crate::link::battery_percent());
            let now = crate::link::network_addresses();
            if now != addresses {
                addresses = now;
                addresses_gen += 1;
                if let Some(h) = host.as_mut() {
                    h.send(&Msg::Addresses { addrs: addresses.clone() });
                }
            }
        }
        if host_gone {
            log!("remdesktop: host went away");
            host = None;
            power_save.restore();
            ui.set_enabled(Button::Net, false);
            mappings = [Mapping::default(); 2];
            ui.set_label(Button::Undo, "UNDO");
    ui.set_label(Button::Redo, "REDO");
    ui.set_label(Button::Side, "SIDE");
    ui.set_label(Button::Net, "NET");
    ui.set_enabled(Button::Net, false);
    ui.set_label(Button::Screen, "VIEW");
            ui.set_enabled(Button::Screen, false);
            // Streamed content would otherwise stay on the panel.
            ink.forget();
            ui.draw_home();
        }

        // The host reaches us over the USB network (in the 10.11.99.x range), not Wi-Fi.
        let usb_net = host.as_ref().is_some_and(|h| crate::link::is_usb(h.link.peer));
        let usb_line = match (&gadget, gadget.as_ref().map(|g| g.link()), usb_net) {
            (None, _, true) => "USB  connected (network)",
            (None, _, false) if !usb => "USB  off",
            (None, _, false) => "USB  unavailable",
            (_, Some(UsbLink::Up), true) => "USB  connected (HID input, network)",
            (_, Some(UsbLink::Up), false) => "USB  connected (HID input)",
            (_, Some(UsbLink::Suspended), _) => "USB  computer asleep",
            (_, _, true) => "USB  connected (network)",
            _ => "USB  not connected",
        };
        let key = (usb_line, host.as_ref().map(|h| h.link.peer), hid_up, addresses_gen);
        if status_key != Some(key) {
            let host_line = match &host {
                Some(h) if !crate::link::is_usb(h.link.peer) => format!("HOST {} (unplug ok)", h.link.peer),
                Some(_) if hid_up => "HOST connected (display)".into(),
                Some(_) => "HOST connected (input, display)".into(),
                None => "HOST not connected".into(),
            };
            ui.set_status(usb_line, &host_line, &wifi_line(&addresses));
            status_key = Some(key);
        }

        // Hold buttons.
        // A finger or the pen tip on a button.
        let held = pen_pressing
            .or_else(|| touch.as_ref().and_then(|t| t.holding()).map(|b| Button::ALL[b as usize]))
            .filter(|&b| ui.enabled(b));
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
                            Button::Display => {
                                state.display = state.display.next();
                                log!("remdesktop: display is now {:?}", state.display);
                                ui.set_label(b, display_label(state.display));
                                state.save();
                                if let Some(h) = host.as_mut() {
                                    h.send(&Msg::Display { mode: state.display });
                                }
                                if state.display == DisplayMode::None {
                                    // Streamed content would otherwise stay on the panel.
                                    ink.forget();
                                    ui.draw_home();
                                }
                            }
                            Button::Side | Button::Undo | Button::Redo | Button::Ink | Button::Clear => {}
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
                if matches!(b, Button::Undo | Button::Redo) {
                    // Acts on touch: a Ctrl+Z / Ctrl+Y key press.
                    hold = Some((b, Instant::now(), true));
                    ui.set_progress(b, ui::HOLD_STEPS);
                    let undo = b == Button::Undo;
                    let now = remdesktop_proto::monotonic_us();
                    // Ctrl + the layout's z/y while a host told us its layout;
                    // otherwise the dedicated, layout-independent Undo/Redo keys.
                    match (host.as_ref().and_then(|h| h.keys), hid_up, gadget.as_ref()) {
                        (Some((u, r)), true, Some(g)) => {
                            if let Some(usage) = hid_report::usage_for(if undo { u } else { r }) {
                                g.send(&hid_report::keyboard(hid_report::MOD_LEFT_CTRL, Some(usage)));
                                g.send(&hid_report::keyboard(0, None));
                            }
                        }
                        (None, true, Some(g)) => {
                            g.send(&hid_report::consumer(if undo { hid_report::AC_UNDO } else { hid_report::AC_REDO }));
                            g.send(&hid_report::consumer(0));
                        }
                        (keys, _, _) => {
                            if let Some(h) = host.as_mut() {
                                let frames = match keys {
                                    Some((u, r)) => input::ctrl_chord(if undo { u } else { r }),
                                    None => input::tap(if undo { input::KEY_UNDO } else { input::KEY_REDO }),
                                };
                                for events in frames {
                                    h.send(&Msg::Frame { device: input::KEYS_ID, time_us: now, events });
                                }
                                h.flush();
                            }
                        }
                    }
                }
                if matches!(b, Button::Ink | Button::Clear) {
                    // Act on touch.
                    hold = Some((b, Instant::now(), true));
                    if b == Button::Ink {
                        ink.on = !ink.on;
                        if !ink.on {
                            clear_ink(&mut ink, &epd, &display);
                        }
                        log!("remdesktop: ink is now {}", if ink.on { "on" } else { "off" });
                        ui.set_label(b, ink_label(ink.on));
                        ui.set_enabled(Button::Clear, ink.on);
                        state.ink = ink.on;
                        state.save();
                    } else {
                        clear_screen(&mut ink, &epd);
                    }
                    ui.set_progress(b, ui::HOLD_STEPS);
                }
                if b == Button::Side {
                    // Acts on touch: click the pen's side button where it is.
                    hold = Some((b, Instant::now(), true));
                    ui.set_progress(b, ui::HOLD_STEPS);
                    if let Some(p) = pen.as_ref() {
                        let now = remdesktop_proto::monotonic_us();
                        let mouse = p.mode() == PenMode::Mouse;
                        for mut events in p.side_click(gate.anchor, gate.host_in_range()) {
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
    power_save.restore();
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

/// The info box line with the tablet's Wi-Fi address, to connect a host to.
fn wifi_line(addresses: &[String]) -> String {
    match addresses.first() {
        Some(ip) => ip.clone(),
        None => "NO WIFI".into(),
    }
}
