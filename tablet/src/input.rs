//! Turns the tablet's raw evdev streams into the virtual devices the host
//! recreates: coordinates rotated to how the tablet is held, touches that
//! start on our own UI held back, and optional touchpad emulation.

use remvnc_proto::{Abs, Device, Event};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_ABS: u16 = 3;
pub const SYN_REPORT: u16 = 0;

pub const KEY_POWER: u16 = 116;
const BTN_LEFT: u16 = 0x110;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOOL_RUBBER: u16 = 0x141;
const BTN_TOOL_FINGER: u16 = 0x145;
const BTN_TOUCH: u16 = 0x14a;
const BTN_STYLUS: u16 = 0x14b;
const BTN_STYLUS2: u16 = 0x14c;
const BTN_TOOL_DOUBLETAP: u16 = 0x14d;
const BTN_TOOL_TRIPLETAP: u16 = 0x14e;
const BTN_TOOL_QUADTAP: u16 = 0x14f;
const BTN_TOOL_QUINTTAP: u16 = 0x148;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_PRESSURE: u16 = 0x18;
const ABS_DISTANCE: u16 = 0x19;
const ABS_TILT_X: u16 = 0x1a;
const ABS_TILT_Y: u16 = 0x1b;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_TOUCH_MINOR: u16 = 0x31;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_PRESSURE: u16 = 0x3a;

const INPUT_PROP_POINTER: u32 = 0;
const INPUT_PROP_DIRECT: u32 = 1;

/// Physical size of the rM2 panel in portrait, in mm.
const PANEL_MM: [f32; 2] = [157.8, 210.4];

pub const PEN_ID: u8 = 1;
pub const TOUCH_ID: u8 = 2;

#[derive(Debug, Clone, Copy)]
pub struct RawEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

/// Drains a non-blocking evdev file (32-bit `struct input_event`, 16 bytes).
pub fn read_events(file: &mut File) -> io::Result<Vec<RawEvent>> {
    let mut out = vec![];
    let mut buf = [0u8; 16 * 64];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend(buf[..n].chunks_exact(16).map(|e| RawEvent {
                kind: u16::from_le_bytes([e[8], e[9]]),
                code: u16::from_le_bytes([e[10], e[11]]),
                value: i32::from_le_bytes([e[12], e[13], e[14], e[15]]),
            })),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AbsInfo {
    pub min: i32,
    pub max: i32,
    pub resolution: i32,
}

pub fn abs_info(file: &File, code: u16) -> io::Result<AbsInfo> {
    let mut a = [0i32; 6]; // value, min, max, fuzz, flat, resolution
    let req = 0x8018_4540u32 + code as u32; // EVIOCGABS(code)
    if unsafe { libc::ioctl(file.as_raw_fd(), req as _, a.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(AbsInfo { min: a[1], max: a[2], resolution: a[5] })
}

/// An axis-aligned rotation/reflection: swap axes first, then flip each
/// resulting axis within its range.
#[derive(Debug, Clone, Copy, Default)]
pub struct Xform {
    pub swap: bool,
    pub flip: [bool; 2],
}

impl Xform {
    /// Portrait screen coordinates (as you read the screen) → coordinates
    /// for a tablet rotated clockwise by `degrees`.
    pub fn rotation(degrees: u16) -> Xform {
        match degrees {
            90 => Xform { swap: true, flip: [true, false] },
            180 => Xform { swap: false, flip: [true, true] },
            270 => Xform { swap: true, flip: [false, true] },
            _ => Xform::default(),
        }
    }
}

/// Where each raw axis (0 = X, 1 = Y) ends up, and whether it is reversed.
#[derive(Debug, Clone, Copy)]
pub struct AxisMap {
    pub to: [(usize, bool); 2],
}

impl AxisMap {
    pub fn new(stages: &[Xform]) -> Self {
        let mut to = [(0, false), (1, false)];
        for s in stages {
            for (axis, flipped) in &mut to {
                if s.swap {
                    *axis = 1 - *axis;
                }
                if s.flip[*axis] {
                    *flipped = !*flipped;
                }
            }
        }
        Self { to }
    }

    /// Raw axis feeding output axis `out`.
    pub fn source(&self, out: usize) -> usize {
        if self.to[0].0 == out { 0 } else { 1 }
    }
}

fn to_pixels(range: AbsInfo, v: i32, size: i32) -> i32 {
    ((v - range.min) as i64 * size as i64 / (range.max - range.min).max(1) as i64) as i32
}

fn apply(v: i32, info: AbsInfo, flipped: bool) -> i32 {
    if flipped { info.min + info.max - v } else { v }
}

/// Resolution in units/mm for raw axis `raw`, whose portrait axis is `portrait`.
fn resolution(info: AbsInfo, portrait: usize) -> i32 {
    if info.resolution > 0 {
        info.resolution
    } else {
        (((info.max - info.min) as f32 / PANEL_MM[portrait]).round() as i32).max(1)
    }
}

/// Wacom digitizer → virtual drawing tablet, events mapped one by one.
pub struct Pen {
    map: AxisMap,
    pos: [AbsInfo; 2],
    abs: Vec<Abs>,
}

impl Pen {
    /// The digitizer is mounted rotated: raw Y runs along the screen's
    /// portrait X, raw X against its portrait Y.
    const PORTRAIT: Xform = Xform { swap: true, flip: [false, true] };

    pub fn new(file: &File, rotation: u16) -> io::Result<Self> {
        let map = AxisMap::new(&[Self::PORTRAIT, Xform::rotation(rotation)]);
        let pos = [abs_info(file, ABS_X)?, abs_info(file, ABS_Y)?];
        let mut abs = vec![];
        for out in 0..2 {
            let raw = map.source(out);
            let portrait = AxisMap::new(&[Self::PORTRAIT]).to[raw].0;
            abs.push(Abs { code: [ABS_X, ABS_Y][out], min: pos[raw].min, max: pos[raw].max, resolution: resolution(pos[raw], portrait) });
        }
        for code in [ABS_PRESSURE, ABS_DISTANCE] {
            let i = abs_info(file, code)?;
            abs.push(Abs { code, min: i.min, max: i.max, resolution: 0 });
        }
        for code in [ABS_TILT_X, ABS_TILT_Y] {
            let i = abs_info(file, code)?;
            // Tilt follows the position axes; keep the range symmetric so a flip is a negation.
            let m = i.max.max(-i.min);
            abs.push(Abs { code, min: -m, max: m, resolution: 0 });
        }
        Ok(Self { map, pos, abs })
    }

    pub fn device(&self) -> Device {
        Device {
            id: PEN_ID,
            name: "remvnc pen".into(),
            props: 1 << INPUT_PROP_POINTER,
            keys: vec![BTN_TOOL_PEN, BTN_TOOL_RUBBER, BTN_TOUCH, BTN_STYLUS, BTN_STYLUS2],
            abs: self.abs.clone(),
        }
    }

    /// Maps raw events; returns finished frames (one per SYN_REPORT).
    pub fn feed(&self, events: &[RawEvent], pending: &mut Vec<Event>, frames: &mut Vec<Vec<Event>>) {
        for e in events {
            match (e.kind, e.code) {
                (EV_SYN, SYN_REPORT) => {
                    if !pending.is_empty() {
                        frames.push(std::mem::take(pending));
                    }
                }
                (EV_ABS, ABS_X | ABS_Y) => {
                    let raw = (e.code - ABS_X) as usize;
                    let (out, flipped) = self.map.to[raw];
                    pending.push(Event { kind: EV_ABS, code: [ABS_X, ABS_Y][out], value: apply(e.value, self.pos[raw], flipped) });
                }
                (EV_ABS, ABS_TILT_X | ABS_TILT_Y) => {
                    let raw = (e.code - ABS_TILT_X) as usize;
                    let (out, flipped) = self.map.to[raw];
                    let value = if flipped { -e.value } else { e.value };
                    pending.push(Event { kind: EV_ABS, code: [ABS_TILT_X, ABS_TILT_Y][out], value });
                }
                (EV_ABS, ABS_PRESSURE | ABS_DISTANCE) | (EV_KEY, _) => {
                    pending.push(Event { kind: e.kind, code: e.code, value: e.value });
                }
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchMode {
    /// Relative pointer with tap/scroll gestures (the compositor's touchpad stack).
    Touchpad,
    /// Absolute touches mapped onto a monitor.
    Touchscreen,
}

const SLOTS: usize = 16;

#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    id: i32,
    pos: [i32; 2],
    pressure: i32,
    major: i32,
    minor: i32,
    dirty: bool,
    began: bool,
    /// Started on our UI: never forwarded.
    held: bool,
    /// A held contact that is still over our UI.
    inside: bool,
}

/// Multitouch screen → virtual touchpad/touchscreen. Rebuilds each frame
/// from slot state so held-back contacts never reach the host.
pub struct Touch {
    map: AxisMap,
    raw: [AbsInfo; 2],
    /// Output ranges, for converting to logical pixels.
    out: [AbsInfo; 2],
    mode: TouchMode,
    abs: Vec<Abs>,
    slots: [Slot; SLOTS],
    current: usize,
    last_count: usize,
}

impl Touch {
    /// Touch X matches the screen; Y runs bottom to top.
    const PORTRAIT: Xform = Xform { swap: false, flip: [false, true] };

    pub fn new(file: &File, rotation: u16, mode: TouchMode) -> io::Result<Self> {
        let map = AxisMap::new(&[Self::PORTRAIT, Xform::rotation(rotation)]);
        let raw = [abs_info(file, ABS_MT_POSITION_X)?, abs_info(file, ABS_MT_POSITION_Y)?];
        let out = [raw[map.source(0)], raw[map.source(1)]];
        let mut abs = vec![];
        for o in 0..2 {
            let r = map.source(o);
            let res = resolution(raw[r], r);
            abs.push(Abs { code: [ABS_MT_POSITION_X, ABS_MT_POSITION_Y][o], min: out[o].min, max: out[o].max, resolution: res });
            abs.push(Abs { code: [ABS_X, ABS_Y][o], min: out[o].min, max: out[o].max, resolution: res });
        }
        abs.push(Abs { code: ABS_MT_SLOT, min: 0, max: SLOTS as i32 - 1, resolution: 0 });
        abs.push(Abs { code: ABS_MT_TRACKING_ID, min: 0, max: 65535, resolution: 0 });
        for code in [ABS_MT_PRESSURE, ABS_MT_TOUCH_MAJOR, ABS_MT_TOUCH_MINOR] {
            let i = abs_info(file, code)?;
            abs.push(Abs { code, min: i.min, max: i.max, resolution: 0 });
        }
        let slots = [Slot { id: -1, ..Default::default() }; SLOTS];
        Ok(Self { map, raw, out, mode, abs, slots, current: 0, last_count: 0 })
    }

    pub fn device(&self) -> Device {
        let mut keys = vec![BTN_TOUCH];
        let props = match self.mode {
            TouchMode::Touchpad => {
                keys.extend([BTN_LEFT, BTN_TOOL_FINGER, BTN_TOOL_DOUBLETAP, BTN_TOOL_TRIPLETAP, BTN_TOOL_QUADTAP, BTN_TOOL_QUINTTAP]);
                1 << INPUT_PROP_POINTER
            }
            TouchMode::Touchscreen => 1 << INPUT_PROP_DIRECT,
        };
        Device { id: TOUCH_ID, name: "remvnc touch".into(), props, keys, abs: self.abs.clone() }
    }

    /// True while a contact that started on our UI is down and still on it.
    pub fn holding(&self) -> bool {
        self.slots.iter().any(|s| s.held && s.inside && s.id >= 0)
    }

    /// Feeds raw events. `on_ui(pixel)` decides, in logical screen pixels of
    /// a `size` screen, whether a contact belongs to our own UI. Returns
    /// forwarded frames.
    pub fn feed(&mut self, events: &[RawEvent], size: [i32; 2], on_ui: impl Fn([i32; 2]) -> bool) -> Vec<Vec<Event>> {
        let out = self.out;
        let on_ui = move |pos: [i32; 2]| on_ui([0, 1].map(|i| to_pixels(out[i], pos[i], size[i])));
        let mut frames = vec![];
        for e in events {
            match (e.kind, e.code) {
                (EV_ABS, ABS_MT_SLOT) => self.current = (e.value.max(0) as usize).min(SLOTS - 1),
                (EV_ABS, code) => {
                    let s = &mut self.slots[self.current];
                    s.dirty = true;
                    match code {
                        ABS_MT_TRACKING_ID => {
                            if e.value >= 0 && s.id < 0 {
                                s.began = true;
                            }
                            s.id = e.value;
                        }
                        ABS_MT_POSITION_X | ABS_MT_POSITION_Y => {
                            let raw = (code - ABS_MT_POSITION_X) as usize;
                            let (out, flipped) = self.map.to[raw];
                            s.pos[out] = apply(e.value, self.raw[raw], flipped);
                        }
                        ABS_MT_PRESSURE => s.pressure = e.value,
                        ABS_MT_TOUCH_MAJOR => s.major = e.value,
                        ABS_MT_TOUCH_MINOR => s.minor = e.value,
                        _ => {}
                    }
                }
                (EV_SYN, SYN_REPORT) => {
                    if let Some(f) = self.frame(&on_ui) {
                        frames.push(f);
                    }
                }
                _ => {}
            }
        }
        frames
    }

    fn frame(&mut self, on_ui: &impl Fn([i32; 2]) -> bool) -> Option<Vec<Event>> {
        let abs = |code, value| Event { kind: EV_ABS, code, value };
        let key = |code, down: bool| Event { kind: EV_KEY, code, value: down as i32 };
        let mut ev = vec![];
        for (i, s) in self.slots.iter_mut().enumerate() {
            if !s.dirty {
                continue;
            }
            s.dirty = false;
            if s.began {
                s.began = false;
                s.held = on_ui(s.pos);
            }
            if s.held {
                s.inside = on_ui(s.pos);
                if s.id < 0 {
                    s.held = false;
                }
                continue;
            }
            ev.push(abs(ABS_MT_SLOT, i as i32));
            ev.push(abs(ABS_MT_TRACKING_ID, s.id));
            if s.id >= 0 {
                ev.extend([
                    abs(ABS_MT_POSITION_X, s.pos[0]),
                    abs(ABS_MT_POSITION_Y, s.pos[1]),
                    abs(ABS_MT_PRESSURE, s.pressure),
                    abs(ABS_MT_TOUCH_MAJOR, s.major),
                    abs(ABS_MT_TOUCH_MINOR, s.minor),
                ]);
            }
        }
        // Legacy single-touch state, which touchpad drivers rely on.
        let active: Vec<&Slot> = self.slots.iter().filter(|s| s.id >= 0 && !s.held).collect();
        let count = active.len();
        if let Some(first) = active.first() {
            ev.push(abs(ABS_X, first.pos[0]));
            ev.push(abs(ABS_Y, first.pos[1]));
        }
        if count != self.last_count {
            ev.push(key(BTN_TOUCH, count > 0));
            if self.mode == TouchMode::Touchpad {
                let tools = [BTN_TOOL_FINGER, BTN_TOOL_DOUBLETAP, BTN_TOOL_TRIPLETAP, BTN_TOOL_QUADTAP, BTN_TOOL_QUINTTAP];
                for (n, tool) in tools.into_iter().enumerate() {
                    ev.push(key(tool, count.min(5) == n + 1));
                }
            }
            self.last_count = count;
        }
        if ev.is_empty() { None } else { Some(ev) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_maps_portrait_axes() {
        // 90° clockwise: logical X = H - portrait Y, logical Y = portrait X.
        let m = AxisMap::new(&[Xform::rotation(90)]);
        assert_eq!(m.to, [(1, false), (0, true)]);
        // Pen raw → portrait: portrait X = raw Y, portrait Y = max - raw X.
        let p = AxisMap::new(&[Pen::PORTRAIT]);
        assert_eq!(p.to, [(1, true), (0, false)]);
        // Pen in landscape: raw X → logical X reversed twice = forward.
        let pl = AxisMap::new(&[Pen::PORTRAIT, Xform::rotation(90)]);
        assert_eq!(pl.to, [(0, false), (1, false)]);
    }
}
