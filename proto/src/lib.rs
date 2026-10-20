//! Wire format between `remdesktop run` on the tablet (its stdout, over SSH) and
//! `remdesktop-host`. Each message is `tag: u8, len: u32 LE, payload`.
//!
//! The tablet declares virtual input devices, then streams evdev event
//! batches for them; the host recreates the devices with uinput and replays
//! each batch followed by a `SYN_REPORT`.

use std::io::{self, Read, Write};

pub mod pen;
pub mod sync;
pub mod touch;
pub use pen::{PEN_DEVICE, PenState};
pub use sync::{Datagram, Snapshot};
pub use touch::{TOUCH_DEVICE, TouchState};

pub const VERSION: u16 = 11;

/// What the tablet's panel shows of the host's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    /// The screen in gray.
    #[default]
    Normal,
    /// Nothing streamed (input only).
    None,
    /// Just the outlines (edge detection), black on white, which stay
    /// legible on e-paper.
    Edge,
}

impl DisplayMode {
    pub const ALL: [DisplayMode; 3] = [DisplayMode::Normal, DisplayMode::None, DisplayMode::Edge];

    /// The mode after this one, in the order of `ALL`.
    pub fn next(self) -> DisplayMode {
        Self::ALL[(self as usize + 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Abs {
    pub code: u16,
    pub min: i32,
    pub max: i32,
    pub resolution: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: u8,
    pub name: String,
    /// Bitmask of INPUT_PROP_* bits.
    pub props: u32,
    pub keys: Vec<u16>,
    pub abs: Vec<Abs>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Event {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// `view` is the label of the view the tablet last had selected (see
    /// `Screens`), so the host can resume it; "" for the default.
    Hello { version: u16, width: u16, height: u16, rotation: u16, view: String },
    Device(Device),
    /// Events for one device, ending in an implicit SYN_REPORT. `time_us`
    /// is the kernel timestamp of the batch (tablet CLOCK_MONOTONIC).
    Frame { device: u8, time_us: u64, events: Vec<Event> },
    /// Host → tablet clock probe; answered with `Pong`.
    Ping { host_us: u64 },
    Pong { host_us: u64, tablet_us: u64 },
    /// Tablet → host over SSH: connect to `addr:port` and send `token` to
    /// move the session to a TCP_NODELAY socket.
    Direct { addr: String, port: u16, token: [u8; 16] },
    /// Host → tablet: labels of the views the screen button cycles through;
    /// index 0 is the default (e.g. "ALL" or "VIRT").
    Screens { names: Vec<String> },
    /// Tablet → host: switch to view `index` of `Screens`.
    Target { index: u8 },
    /// Host → tablet: new pixels for a rectangle of the panel, in panel
    /// (portrait) coordinates. `data` is LZ4 (size-prepended block) of
    /// 4-bit gray, two pixels per byte (high nibble first), rows padded to
    /// whole bytes.
    Rect { id: u32, x: u16, y: u16, w: u16, h: u16, data: Vec<u8> },
    /// Tablet → host: `Rect` `id` is on its way to the panel; send the next.
    Ack { id: u32 },
    /// Host → tablet: coordinate correction for a device's USB HID reports
    /// (the host corrects uinput input itself). Fractions of the device
    /// range ×65535: the part of the tablet `shown` (x0, x1, y0, y1) is
    /// stretched onto `to`.
    Mapping { device: u8, shown: [u16; 4], to: [u16; 4] },
    /// Tablet → host: whether input goes out over USB HID (the host's
    /// uinput devices then idle), and the current pen and touch modes, which
    /// decide how the host maps the HID devices.
    Status { usb_hid: bool, pen_mouse: bool, touchpad: bool },
    /// Tablet → host: its addresses other than the USB link (e.g. Wi-Fi),
    /// for the host to test whether it could reach the tablet without USB.
    Addresses { addrs: Vec<String> },
    /// Host → tablet: an address the host verified it can reach the tablet
    /// on without USB ("" if none). Enables the tablet's go-remote button.
    RemoteOk { addr: String },
    /// Tablet → host: the user asked to go remote; reconnect over `RemoteOk`'s address.
    GoRemote,
    /// Host → tablet: Linux key codes that type `z` and `y` in the user's
    /// keyboard layout, for the undo/redo buttons (Ctrl + these).
    Keys { undo: u16, redo: u16 },
    /// Tablet → host: what to show of the screen (sent after `Hello` and on
    /// each change).
    Display { mode: DisplayMode },
    /// The tablet is exiting on purpose (X button held, power button).
    Bye,
    /// Tablet → host, for hosts on Wi-Fi: the pen can also go over UDP to
    /// this port (see `pen`); `key` goes in every datagram.
    UdpOffer { port: u16, key: u32 },
    /// Host → tablet: a probe was answered, so datagrams get through; the
    /// pen now goes over UDP.
    UdpReady,
    /// Tablet → host: a pen snapshot whose buttons, tool or contact changed,
    /// for the host to apply if the datagrams never delivered it.
    PenState { seq: u32, time_us: u64, state: PenState },
    /// The same for the touch screen: a change in which contacts are down.
    TouchState { seq: u32, time_us: u64, state: TouchState },
}

const HELLO: u8 = 1;
const DEVICE: u8 = 2;
const FRAME: u8 = 3;
const BYE: u8 = 4;
const PING: u8 = 5;
const PONG: u8 = 6;
const DIRECT: u8 = 7;
const SCREENS: u8 = 8;
const TARGET: u8 = 9;
const RECT: u8 = 10;
const ACK: u8 = 11;
const MAPPING: u8 = 12;
const STATUS: u8 = 13;
const ADDRESSES: u8 = 14;
const REMOTE_OK: u8 = 15;
const GO_REMOTE: u8 = 16;
const KEYS: u8 = 17;
const DISPLAY: u8 = 18;
const UDP_OFFER: u8 = 19;
const UDP_READY: u8 = 20;
const PEN_STATE: u8 = 21;
const TOUCH_STATE: u8 = 22;

impl Msg {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        // Tag and length are reserved up front so the message leaves in one
        // write: on an unbuffered socket (NODELAY) three writes are three
        // packets, each costing Wi-Fi airtime.
        let mut p = vec![0u8; 5];
        let tag = match self {
            Msg::Hello { version, width, height, rotation, view } => {
                for v in [version, width, height, rotation] {
                    p.extend_from_slice(&v.to_le_bytes());
                }
                put_str(&mut p, view);
                HELLO
            }
            Msg::Device(d) => {
                p.push(d.id);
                put_str(&mut p, &d.name);
                p.extend_from_slice(&d.props.to_le_bytes());
                p.extend_from_slice(&(d.keys.len() as u16).to_le_bytes());
                for k in &d.keys {
                    p.extend_from_slice(&k.to_le_bytes());
                }
                p.extend_from_slice(&(d.abs.len() as u16).to_le_bytes());
                for a in &d.abs {
                    p.extend_from_slice(&a.code.to_le_bytes());
                    p.extend_from_slice(&a.min.to_le_bytes());
                    p.extend_from_slice(&a.max.to_le_bytes());
                    p.extend_from_slice(&a.resolution.to_le_bytes());
                }
                DEVICE
            }
            Msg::Frame { device, time_us, events } => {
                p.push(*device);
                p.extend_from_slice(&time_us.to_le_bytes());
                for e in events {
                    p.extend_from_slice(&e.kind.to_le_bytes());
                    p.extend_from_slice(&e.code.to_le_bytes());
                    p.extend_from_slice(&e.value.to_le_bytes());
                }
                FRAME
            }
            Msg::Bye => BYE,
            Msg::Ping { host_us } => {
                p.extend_from_slice(&host_us.to_le_bytes());
                PING
            }
            Msg::Pong { host_us, tablet_us } => {
                p.extend_from_slice(&host_us.to_le_bytes());
                p.extend_from_slice(&tablet_us.to_le_bytes());
                PONG
            }
            Msg::Direct { addr, port, token } => {
                put_str(&mut p, addr);
                p.extend_from_slice(&port.to_le_bytes());
                p.extend_from_slice(token);
                DIRECT
            }
            Msg::Screens { names } => {
                p.push(names.len() as u8);
                for n in names {
                    put_str(&mut p, n);
                }
                SCREENS
            }
            Msg::Target { index } => {
                p.push(*index);
                TARGET
            }
            Msg::Rect { id, x, y, w, h, data } => {
                p.extend_from_slice(&id.to_le_bytes());
                for v in [x, y, w, h] {
                    p.extend_from_slice(&v.to_le_bytes());
                }
                p.extend_from_slice(data);
                RECT
            }
            Msg::Ack { id } => {
                p.extend_from_slice(&id.to_le_bytes());
                ACK
            }
            Msg::Mapping { device, shown, to } => {
                p.push(*device);
                for v in shown.iter().chain(to) {
                    p.extend_from_slice(&v.to_le_bytes());
                }
                MAPPING
            }
            Msg::Status { usb_hid, pen_mouse, touchpad } => {
                p.push(*usb_hid as u8 | (*pen_mouse as u8) << 1 | (*touchpad as u8) << 2);
                STATUS
            }
            Msg::Addresses { addrs } => {
                p.push(addrs.len() as u8);
                for a in addrs {
                    put_str(&mut p, a);
                }
                ADDRESSES
            }
            Msg::RemoteOk { addr } => {
                put_str(&mut p, addr);
                REMOTE_OK
            }
            Msg::GoRemote => GO_REMOTE,
            Msg::Keys { undo, redo } => {
                p.extend_from_slice(&undo.to_le_bytes());
                p.extend_from_slice(&redo.to_le_bytes());
                KEYS
            }
            Msg::Display { mode } => {
                p.push(*mode as u8);
                DISPLAY
            }
            Msg::UdpOffer { port, key } => {
                p.extend_from_slice(&port.to_le_bytes());
                p.extend_from_slice(&key.to_le_bytes());
                UDP_OFFER
            }
            Msg::UdpReady => UDP_READY,
            Msg::PenState { seq, time_us, state } => {
                p.extend_from_slice(&seq.to_le_bytes());
                p.extend_from_slice(&time_us.to_le_bytes());
                state.put(&mut p);
                PEN_STATE
            }
            Msg::TouchState { seq, time_us, state } => {
                p.extend_from_slice(&seq.to_le_bytes());
                p.extend_from_slice(&time_us.to_le_bytes());
                state.put(&mut p);
                TOUCH_STATE
            }
        };
        p[0] = tag;
        let len = (p.len() - 5) as u32;
        p[1..5].copy_from_slice(&len.to_le_bytes());
        w.write_all(&p)
    }

    /// Reads one message; `Ok(None)` on a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Msg>> {
        let mut head = [0u8; 5];
        match r.read_exact(&mut head) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            other => other?,
        }
        let len = u32::from_le_bytes(head[1..].try_into().unwrap()) as usize;
        if len > 8 << 20 {
            return Err(bad("message too large"));
        }
        let mut p = vec![0u8; len];
        r.read_exact(&mut p)?;
        let mut c = Cursor { buf: &p, at: 0 };
        let msg = match head[0] {
            // Fields added later are optional, so an older peer's Hello still
            // parses and the version mismatch can be reported as such.
            HELLO => Msg::Hello {
                version: c.u16()?,
                width: c.u16()?,
                height: c.u16()?,
                rotation: c.u16()?,
                view: if c.at < len { c.str()? } else { String::new() },
            },
            DEVICE => {
                let id = c.u8()?;
                let name = c.str()?;
                let props = c.u32()?;
                let keys = (0..c.u16()?).map(|_| c.u16()).collect::<io::Result<_>>()?;
                let abs = (0..c.u16()?)
                    .map(|_| Ok(Abs { code: c.u16()?, min: c.i32()?, max: c.i32()?, resolution: c.i32()? }))
                    .collect::<io::Result<_>>()?;
                Msg::Device(Device { id, name, props, keys, abs })
            }
            FRAME => {
                let device = c.u8()?;
                let time_us = c.u64()?;
                let mut events = Vec::with_capacity(len.saturating_sub(9) / 8);
                while c.at < len {
                    events.push(Event { kind: c.u16()?, code: c.u16()?, value: c.i32()? });
                }
                Msg::Frame { device, time_us, events }
            }
            BYE => Msg::Bye,
            PING => Msg::Ping { host_us: c.u64()? },
            PONG => Msg::Pong { host_us: c.u64()?, tablet_us: c.u64()? },
            DIRECT => Msg::Direct { addr: c.str()?, port: c.u16()?, token: c.take(16)?.try_into().unwrap() },
            SCREENS => Msg::Screens { names: (0..c.u8()?).map(|_| c.str()).collect::<io::Result<_>>()? },
            TARGET => Msg::Target { index: c.u8()? },
            RECT => {
                let (id, x, y, w, h) = (c.u32()?, c.u16()?, c.u16()?, c.u16()?, c.u16()?);
                Msg::Rect { id, x, y, w, h, data: c.take(len - c.at)?.to_vec() }
            }
            ACK => Msg::Ack { id: c.u32()? },
            MAPPING => {
                let device = c.u8()?;
                let mut v = [0u16; 8];
                for x in &mut v {
                    *x = c.u16()?;
                }
                Msg::Mapping { device, shown: v[..4].try_into().unwrap(), to: v[4..].try_into().unwrap() }
            }
            STATUS => {
                let f = c.u8()?;
                Msg::Status { usb_hid: f & 1 != 0, pen_mouse: f & 2 != 0, touchpad: f & 4 != 0 }
            }
            ADDRESSES => Msg::Addresses { addrs: (0..c.u8()?).map(|_| c.str()).collect::<io::Result<_>>()? },
            REMOTE_OK => Msg::RemoteOk { addr: c.str()? },
            GO_REMOTE => Msg::GoRemote,
            KEYS => Msg::Keys { undo: c.u16()?, redo: c.u16()? },
            DISPLAY => Msg::Display {
                mode: *DisplayMode::ALL.get(c.u8()? as usize).ok_or_else(|| bad("unknown display mode"))?,
            },
            UDP_OFFER => Msg::UdpOffer { port: c.u16()?, key: c.u32()? },
            UDP_READY => Msg::UdpReady,
            PEN_STATE => Msg::PenState {
                seq: c.u32()?,
                time_us: c.u64()?,
                state: PenState::get(c.take(PenState::SIZE)?).ok_or_else(|| bad("short pen state"))?,
            },
            TOUCH_STATE => Msg::TouchState {
                seq: c.u32()?,
                time_us: c.u64()?,
                state: TouchState::get(c.take(len - c.at)?).ok_or_else(|| bad("short touch state"))?.0,
            },
            t => return Err(bad(&format!("unknown message tag {t}"))),
        };
        Ok(Some(msg))
    }
}

impl Msg {
    /// Parses one message from the front of `buf`, if it is complete.
    /// Returns the message and the bytes it used.
    pub fn parse(buf: &[u8]) -> io::Result<Option<(Msg, usize)>> {
        if buf.len() < 5 {
            return Ok(None);
        }
        let total = 5 + u32::from_le_bytes(buf[1..5].try_into().unwrap()) as usize;
        if buf.len() < total {
            return Ok(None);
        }
        let mut r = &buf[..total];
        Ok(Msg::read_from(&mut r)?.map(|m| (m, total)))
    }
}

/// 16-level gray, the panel's native depth, packed two pixels per byte.
pub mod gray4 {
    /// 8-bit gray → level 0..=15.
    pub fn level(v: u8) -> u8 {
        ((v as u16 * 15 + 127) / 255) as u8
    }

    /// Level 0..=15 → 8-bit gray.
    pub fn gray(level: u8) -> u8 {
        level * 17
    }

    pub fn row_bytes(w: usize) -> usize {
        w.div_ceil(2)
    }

    /// Packs the `w`×`h` window at (`x`, `y`) of a one-level-per-byte image
    /// with row length `stride`.
    pub fn pack(levels: &[u8], stride: usize, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
        let rb = row_bytes(w);
        let mut out = vec![0u8; rb * h];
        for r in 0..h {
            let src = &levels[(y + r) * stride + x..][..w];
            let dst = &mut out[r * rb..][..rb];
            for (i, pair) in src.chunks(2).enumerate() {
                dst[i] = pair[0] << 4 | pair.get(1).copied().unwrap_or(0);
            }
        }
        out
    }

    /// Calls `put(column, level)` for each pixel of one packed row.
    #[inline]
    pub fn unpack_row(row: &[u8], w: usize, mut put: impl FnMut(usize, u8)) {
        for (i, &b) in row.iter().enumerate() {
            put(2 * i, b >> 4);
            if 2 * i + 1 < w {
                put(2 * i + 1, b & 15);
            }
        }
    }
}

/// Microseconds on this machine's CLOCK_MONOTONIC (the clock evdev
/// timestamps use once set with EVIOCSCLOCKID).
pub fn monotonic_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

/// Latency tuning for the session socket (both ends): no Nagle, no delayed
/// ACKs, and the highest priority class Linux lets unprivileged code ask for
/// (DSCP EF plus socket priority 6), which Wi-Fi maps to its voice queue
/// instead of best effort. Best effort: failures are ignored.
pub fn tune_socket(fd: std::os::fd::RawFd) {
    set_option(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, 1);
    set_option(fd, libc::IPPROTO_TCP, libc::TCP_QUICKACK, 1);
    prioritize(fd);
}

/// The priority half of `tune_socket`, for UDP sockets too.
pub fn prioritize(fd: std::os::fd::RawFd) {
    set_option(fd, libc::IPPROTO_IP, libc::IP_TOS, 0xb8);
    set_option(fd, libc::SOL_SOCKET, libc::SO_PRIORITY, 6);
}

fn set_option(fd: i32, level: i32, name: i32, value: i32) {
    unsafe { libc::setsockopt(fd, level, name, &value as *const i32 as *const libc::c_void, 4) };
}

fn put_str(p: &mut Vec<u8>, s: &str) {
    p.extend_from_slice(&(s.len() as u16).to_le_bytes());
    p.extend_from_slice(s.as_bytes());
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let s = self.buf.get(self.at..self.at + n).ok_or_else(|| bad("truncated message"))?;
        self.at += n;
        Ok(s)
    }
    fn str(&mut self) -> io::Result<String> {
        let n = self.u16()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gray4_round_trip() {
        // 3x2 window out of a 4-wide image, odd width to exercise padding.
        let levels = [0, 1, 2, 3, 4, 5, 6, 7];
        let packed = gray4::pack(&levels, 4, 1, 0, 3, 2);
        assert_eq!(packed, vec![0x12, 0x30, 0x56, 0x70]);
        let mut row = vec![];
        gray4::unpack_row(&packed[2..4], 3, |i, l| row.push((i, l)));
        assert_eq!(row, vec![(0, 5), (1, 6), (2, 7)]);
        assert_eq!(gray4::level(255), 15);
        assert_eq!(gray4::gray(15), 255);
    }

    #[test]
    fn round_trip() {
        let msgs = vec![
            Msg::Hello { version: VERSION, width: 1872, height: 1404, rotation: 90, view: "2".into() },
            Msg::Device(Device {
                id: 1,
                name: "remdesktop pen".into(),
                props: 0,
                keys: vec![0x140, 0x14a],
                abs: vec![Abs { code: 0, min: 0, max: 20967, resolution: 100 }],
            }),
            Msg::Frame { device: 1, time_us: 123_456_789, events: vec![Event { kind: 3, code: 0, value: -5 }] },
            Msg::UdpOffer { port: 40000, key: 0xdead_beef },
            Msg::UdpReady,
            Msg::TouchState {
                seq: 4,
                time_us: 8,
                state: {
                    let mut t = TouchState::default();
                    t.slots[3] = Some(touch::Contact { id: 7, x: 1, y: -2, pressure: 3, major: 4, minor: 5 });
                    t.edges = 2;
                    t
                },
            },
            Msg::PenState { seq: 9, time_us: 77, state: PenState { flags: 5, x: -1, y: 2, pressure: 3, distance: -4, tilt: [5, -6], edges: 11 } },
            Msg::Ping { host_us: 42 },
            Msg::Pong { host_us: 42, tablet_us: 7 },
            Msg::Direct { addr: "10.11.99.1".into(), port: 4242, token: [9; 16] },
            Msg::Screens { names: vec!["DP-6".into(), "HDMI-2".into()] },
            Msg::Target { index: 2 },
            Msg::Rect { id: 3, x: 8, y: 16, w: 3, h: 2, data: vec![1, 2, 3] },
            Msg::Ack { id: 3 },
            Msg::Mapping { device: 1, shown: [0, 65535, 100, 65000], to: [1, 2, 3, 4] },
            Msg::Status { usb_hid: true, pen_mouse: false, touchpad: true },
            Msg::Addresses { addrs: vec!["192.168.1.5".into()] },
            Msg::RemoteOk { addr: "192.168.1.5".into() },
            Msg::GoRemote,
            Msg::Keys { undo: 53, redo: 20 },
            Msg::Display { mode: DisplayMode::Edge },
            Msg::Bye,
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            m.write_to(&mut buf).unwrap();
        }
        let mut r = &buf[..];
        for m in &msgs {
            assert_eq!(Msg::read_from(&mut r).unwrap().as_ref(), Some(m));
        }
        assert_eq!(Msg::read_from(&mut r).unwrap(), None);
        // Incremental parsing sees nothing until a message is complete.
        assert!(Msg::parse(&buf[..3]).unwrap().is_none());
        let (first, used) = Msg::parse(&buf).unwrap().unwrap();
        assert_eq!(first, msgs[0]);
        assert!(used > 5);
    }
}
