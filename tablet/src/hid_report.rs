//! The HID report descriptor (pen, touchscreen, touchpad, absolute mouse)
//! and an encoder that turns our input frames (the same evdev-style events
//! sent to uinput) into HID input reports.
//!
//! Every device has its own report ID, so switching pen or touch modes just
//! changes which report we send; the host never re-enumerates.

use remdesktop_proto::{Device, Event};

pub const PEN: u8 = 1;
pub const TOUCHSCREEN: u8 = 2;
pub const TOUCHPAD: u8 = 3;
const TOUCHSCREEN_MAX: u8 = 4;
pub const MOUSE: u8 = 5;
const TOUCHPAD_FEATURES: u8 = 6;

/// Contacts per touch report.
pub const FINGERS: usize = 5;

const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_PRESSURE: u16 = 0x18;
const ABS_TILT_X: u16 = 0x1a;
const ABS_TILT_Y: u16 = 0x1b;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOOL_RUBBER: u16 = 0x141;
const BTN_TOUCH: u16 = 0x14a;
const BTN_STYLUS: u16 = 0x14b;

/// Short-item HID descriptor writer.
struct Desc(Vec<u8>);

impl Desc {
    fn item(&mut self, tag: u8, v: i32) -> &mut Self {
        let bytes = v.to_le_bytes();
        let n = if (-128..=127).contains(&v) {
            1
        } else if (-32768..=32767).contains(&v) {
            2
        } else {
            4
        };
        self.0.push(tag | if n == 4 { 3 } else { n as u8 });
        self.0.extend_from_slice(&bytes[..n]);
        self
    }
    /// Unsigned items (usages, logical max of unsigned fields) mustn't be sign-extended.
    fn uitem(&mut self, tag: u8, v: u32) -> &mut Self {
        let n = if v <= 0xff { 1 } else if v <= 0xffff { 2 } else { 4 };
        self.0.push(tag | if n == 4 { 3 } else { n as u8 });
        self.0.extend_from_slice(&v.to_le_bytes()[..n]);
        self
    }
    fn page(&mut self, p: u32) -> &mut Self {
        self.uitem(0x04, p)
    }
    fn usage(&mut self, u: u32) -> &mut Self {
        self.uitem(0x08, u)
    }
    fn collection(&mut self, kind: u8) -> &mut Self {
        self.0.extend_from_slice(&[0xa1, kind]);
        self
    }
    fn end(&mut self) -> &mut Self {
        self.0.push(0xc0);
        self
    }
    fn id(&mut self, id: u8) -> &mut Self {
        self.uitem(0x84, id as u32)
    }
    fn logical(&mut self, min: i32, max: i32) -> &mut Self {
        self.item(0x14, min);
        if min >= 0 { self.uitem(0x24, max as u32) } else { self.item(0x24, max) }
    }
    fn physical(&mut self, min: i32, max: i32) -> &mut Self {
        self.item(0x34, min).item(0x44, max)
    }
    fn size(&mut self, bits: u32, count: u32) -> &mut Self {
        self.uitem(0x74, bits).uitem(0x94, count)
    }
    fn input(&mut self, flags: u8) -> &mut Self {
        self.uitem(0x80, flags as u32)
    }
    fn feature(&mut self, flags: u8) -> &mut Self {
        self.uitem(0xb0, flags as u32)
    }
    /// Length unit: centimetres with exponent -1, i.e. millimetres.
    fn mm(&mut self) -> &mut Self {
        self.uitem(0x64, 0x11).uitem(0x54, 0x0f)
    }
    fn no_unit(&mut self) -> &mut Self {
        self.uitem(0x64, 0).uitem(0x54, 0)
    }
}

const APPLICATION: u8 = 1;
const PHYSICAL: u8 = 0;
const LOGICAL: u8 = 2;
const VAR: u8 = 0x02;
const CONST: u8 = 0x03;

#[derive(Debug, Clone, Copy, Default)]
pub struct Axis {
    pub min: i32,
    pub max: i32,
    /// Physical length in mm.
    pub mm: i32,
}

fn axis(d: &Device, code: u16) -> Axis {
    let a = d.abs.iter().find(|a| a.code == code);
    let (min, max, res) = a.map_or((0, 1, 0), |a| (a.min, a.max, a.resolution));
    let mm = if res > 0 { (max - min) / res } else { 0 };
    Axis { min, max, mm }
}

/// Coordinate ranges the descriptor advertises.
#[derive(Debug, Clone, Copy)]
pub struct Ranges {
    pub pen: [Axis; 2],
    pub pressure: Axis,
    pub tilt: i32,
    pub touch: [Axis; 2],
}

impl Ranges {
    /// From the device declarations we'd otherwise send to uinput.
    pub fn new(pen: &Device, touch: &Device) -> Self {
        let tilt = pen.abs.iter().find(|a| a.code == ABS_TILT_X).map_or(9000, |a| a.max);
        Ranges {
            pen: [axis(pen, ABS_X), axis(pen, ABS_Y)],
            pressure: axis(pen, ABS_PRESSURE),
            tilt,
            touch: [axis(touch, ABS_MT_POSITION_X), axis(touch, ABS_MT_POSITION_Y)],
        }
    }
}

fn xy(d: &mut Desc, r: [Axis; 2]) {
    d.page(0x01).mm();
    for (usage, a) in [(0x30, r[0]), (0x31, r[1])] {
        d.usage(usage).logical(a.min, a.max).physical(0, a.mm).size(16, 1).input(VAR);
    }
    d.no_unit().physical(0, 0);
}

fn fingers(d: &mut Desc, r: [Axis; 2]) {
    for _ in 0..FINGERS {
        d.page(0x0d).usage(0x22).collection(LOGICAL);
        d.usage(0x42).usage(0x47).logical(0, 1).size(1, 2).input(VAR); // tip, confidence
        d.size(1, 6).input(CONST);
        d.usage(0x51).logical(0, 255).size(8, 1).input(VAR); // contact id
        xy(d, r);
        d.end();
    }
    d.page(0x0d).usage(0x54).logical(0, FINGERS as i32).size(8, 1).input(VAR); // contact count
}

pub fn descriptor(r: &Ranges) -> Vec<u8> {
    let mut d = Desc(vec![]);
    // Pen: tip, barrel, invert, eraser, in range; X, Y; pressure; tilt.
    d.page(0x0d).usage(0x02).collection(APPLICATION).id(PEN);
    d.usage(0x20).collection(PHYSICAL);
    for u in [0x42, 0x44, 0x3c, 0x45, 0x32] {
        d.usage(u);
    }
    d.logical(0, 1).size(1, 5).input(VAR).size(1, 3).input(CONST);
    xy(&mut d, r.pen);
    d.page(0x0d).usage(0x30).logical(r.pressure.min, r.pressure.max).size(16, 1).input(VAR);
    d.usage(0x3d).usage(0x3e).logical(-r.tilt, r.tilt).physical(-90, 90).uitem(0x64, 0x14).size(16, 2).input(VAR);
    d.no_unit().physical(0, 0);
    d.end().end();

    // Touchscreen.
    d.page(0x0d).usage(0x04).collection(APPLICATION).id(TOUCHSCREEN);
    fingers(&mut d, r.touch);
    d.id(TOUCHSCREEN_MAX).usage(0x55).logical(0, FINGERS as i32).size(8, 1).feature(VAR); // contact count maximum
    d.end();

    // Touchpad (non-clickable): fingers plus a button the host may expect.
    d.page(0x0d).usage(0x05).collection(APPLICATION).id(TOUCHPAD);
    fingers(&mut d, r.touch);
    d.page(0x09).usage(0x01).logical(0, 1).size(1, 1).input(VAR).size(1, 7).input(CONST);
    d.id(TOUCHPAD_FEATURES);
    d.page(0x0d).usage(0x55).logical(0, FINGERS as i32).size(8, 1).feature(VAR);
    d.usage(0x59).logical(0, 2).size(8, 1).feature(VAR); // pad type: 2 = not clickable
    d.end();

    // Absolute mouse, for the pen's mouse mode.
    d.page(0x01).usage(0x02).collection(APPLICATION).id(MOUSE);
    d.usage(0x01).collection(PHYSICAL);
    d.page(0x09).uitem(0x18, 1).uitem(0x28, 2).logical(0, 1).size(1, 2).input(VAR).size(1, 6).input(CONST);
    d.page(0x01);
    for (usage, a) in [(0x30, r.pen[0]), (0x31, r.pen[1])] {
        d.usage(usage).logical(a.min, a.max).size(16, 1).input(VAR);
    }
    d.end().end();
    d.0
}

/// Feature reports the host may ask for with GET_REPORT.
pub fn feature(id: u8) -> Option<Vec<u8>> {
    match id {
        TOUCHSCREEN_MAX => Some(vec![TOUCHSCREEN_MAX, FINGERS as u8]),
        TOUCHPAD_FEATURES => Some(vec![TOUCHPAD_FEATURES, FINGERS as u8, 2]),
        _ => None,
    }
}

/// Builds HID reports from input frames, keeping the state the frames
/// update incrementally.
#[derive(Default)]
pub struct Encoder {
    pen: PenState,
    contacts: [Contact; 16],
    slot: usize,
}

#[derive(Default)]
struct PenState {
    tip: bool,
    barrel: bool,
    rubber: bool,
    in_range: bool,
    x: i32,
    y: i32,
    pressure: i32,
    tilt: [i32; 2],
    buttons: u8,
}

#[derive(Default, Clone, Copy)]
struct Contact {
    down: bool,
    /// Lifted this frame: reported once more with tip off.
    lifted: bool,
    x: i32,
    y: i32,
}

impl Encoder {
    /// A pen frame (tablet or mouse mode) → one report.
    pub fn pen(&mut self, events: &[Event], mouse: bool) -> Vec<u8> {
        let p = &mut self.pen;
        for e in events {
            match (e.kind, e.code) {
                (EV_ABS, ABS_X) => p.x = e.value,
                (EV_ABS, ABS_Y) => p.y = e.value,
                (EV_ABS, ABS_PRESSURE) => p.pressure = e.value,
                (EV_ABS, ABS_TILT_X) => p.tilt[0] = e.value,
                (EV_ABS, ABS_TILT_Y) => p.tilt[1] = e.value,
                (EV_KEY, BTN_TOUCH) => p.tip = e.value != 0,
                (EV_KEY, BTN_STYLUS) => p.barrel = e.value != 0,
                (EV_KEY, BTN_TOOL_PEN) => p.in_range = e.value != 0,
                (EV_KEY, BTN_TOOL_RUBBER) => {
                    p.rubber = e.value != 0;
                    p.in_range = e.value != 0;
                }
                (EV_KEY, BTN_LEFT) => p.buttons = p.buttons & !1 | (e.value != 0) as u8,
                (EV_KEY, BTN_RIGHT) => p.buttons = p.buttons & !2 | ((e.value != 0) as u8) << 1,
                _ => {}
            }
        }
        let u16le = |v: i32| (v.clamp(0, 0xffff) as u16).to_le_bytes();
        if mouse {
            let mut r = vec![MOUSE, p.buttons];
            r.extend_from_slice(&u16le(p.x));
            r.extend_from_slice(&u16le(p.y));
            return r;
        }
        // Windows/Linux convention: the eraser end reports invert while
        // hovering and eraser (instead of tip) while touching.
        let flags = (p.tip && !p.rubber) as u8
            | (p.barrel as u8) << 1
            | (p.rubber as u8) << 2
            | ((p.tip && p.rubber) as u8) << 3
            | (p.in_range as u8) << 4;
        let mut r = vec![PEN, flags];
        for v in [p.x, p.y, p.pressure] {
            r.extend_from_slice(&u16le(v));
        }
        for t in p.tilt {
            r.extend_from_slice(&(t.clamp(-32768, 32767) as i16).to_le_bytes());
        }
        r
    }

    /// A touch frame → one report for the touchscreen or touchpad collection.
    pub fn touch(&mut self, events: &[Event], touchpad: bool) -> Vec<u8> {
        for e in events.iter().filter(|e| e.kind == EV_ABS) {
            match e.code {
                ABS_MT_SLOT => self.slot = (e.value.max(0) as usize).min(self.contacts.len() - 1),
                ABS_MT_TRACKING_ID => {
                    let c = &mut self.contacts[self.slot];
                    if e.value < 0 {
                        c.lifted = c.down;
                        c.down = false;
                    } else {
                        c.down = true;
                    }
                }
                ABS_MT_POSITION_X => self.contacts[self.slot].x = e.value,
                ABS_MT_POSITION_Y => self.contacts[self.slot].y = e.value,
                _ => {}
            }
        }
        let mut r = vec![if touchpad { TOUCHPAD } else { TOUCHSCREEN }];
        let mut count = 0u8;
        for (id, c) in self.contacts.iter_mut().enumerate().filter(|(_, c)| c.down || c.lifted).take(FINGERS) {
            let flags = c.down as u8 | 1 << 1; // confidence: palms never get here
            r.push(flags);
            r.push(id as u8);
            r.extend_from_slice(&(c.x.clamp(0, 0xffff) as u16).to_le_bytes());
            r.extend_from_slice(&(c.y.clamp(0, 0xffff) as u16).to_le_bytes());
            c.lifted = false;
            count += 1;
        }
        r.resize(1 + FINGERS * 6, 0);
        r.push(count);
        if touchpad {
            r.push(0); // button
        }
        r
    }

    /// Releases everything (mode switches, shutdown): reports with nothing down.
    pub fn release_all(&mut self, touchpad: bool, mouse: bool) -> Vec<Vec<u8>> {
        for c in self.contacts.iter_mut().filter(|c| c.down) {
            c.down = false;
            c.lifted = true;
        }
        let touch = self.touch(&[], touchpad);
        self.pen = PenState { x: self.pen.x, y: self.pen.y, ..Default::default() };
        vec![touch, self.pen(&[], mouse)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remdesktop_proto::Abs;

    fn dev(codes: &[(u16, i32)]) -> Device {
        Device {
            id: 1,
            name: "t".into(),
            props: 0,
            keys: vec![],
            abs: codes.iter().map(|&(code, max)| Abs { code, min: 0, max, resolution: 100 }).collect(),
        }
    }

    #[test]
    fn reports_have_descriptor_sizes() {
        let pen = dev(&[(ABS_X, 20966), (ABS_Y, 15725), (ABS_PRESSURE, 4095), (ABS_TILT_X, 9000)]);
        let touch = dev(&[(ABS_MT_POSITION_X, 1403), (ABS_MT_POSITION_Y, 1871)]);
        let r = Ranges::new(&pen, &touch);
        let d = descriptor(&r);
        assert!(d.len() < 2048 && d.ends_with(&[0xc0, 0xc0]), "descriptor is {} bytes", d.len());
        let mut e = Encoder::default();
        // id + flags + x, y, pressure + 2 tilts
        assert_eq!(e.pen(&[Event { kind: EV_ABS, code: ABS_X, value: 5 }], false).len(), 1 + 1 + 6 + 4);
        assert_eq!(e.pen(&[], true).len(), 1 + 1 + 4);
        let down = [
            Event { kind: EV_ABS, code: ABS_MT_SLOT, value: 0 },
            Event { kind: EV_ABS, code: ABS_MT_TRACKING_ID, value: 7 },
            Event { kind: EV_ABS, code: ABS_MT_POSITION_X, value: 100 },
        ];
        let t = e.touch(&down, false);
        assert_eq!(t.len(), 1 + FINGERS * 6 + 1);
        assert_eq!((t[1], t[2], t[3], *t.last().unwrap()), (0b11, 0, 100, 1));
        // Lift: reported once with tip off, then gone.
        let up = [Event { kind: EV_ABS, code: ABS_MT_TRACKING_ID, value: -1 }];
        let t = e.touch(&up, false);
        assert_eq!((t[1], *t.last().unwrap()), (0b10, 1));
        assert_eq!(*e.touch(&[], false).last().unwrap(), 0);
    }
}
