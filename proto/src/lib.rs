//! Wire format between `remvnc run` on the tablet (its stdout, over SSH) and
//! `remvnc-host`. Each message is `tag: u8, len: u32 LE, payload`.
//!
//! The tablet declares virtual input devices, then streams evdev event
//! batches for them; the host recreates the devices with uinput and replays
//! each batch followed by a `SYN_REPORT`.

use std::io::{self, Read, Write};

pub const VERSION: u16 = 4;

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
    Hello { version: u16, width: u16, height: u16, rotation: u16 },
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
    /// The tablet is exiting on purpose (X button held, power button).
    Bye,
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

impl Msg {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let mut p = Vec::new();
        let tag = match self {
            Msg::Hello { version, width, height, rotation } => {
                for v in [version, width, height, rotation] {
                    p.extend_from_slice(&v.to_le_bytes());
                }
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
        };
        w.write_all(&[tag])?;
        w.write_all(&(p.len() as u32).to_le_bytes())?;
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
            HELLO => Msg::Hello { version: c.u16()?, width: c.u16()?, height: c.u16()?, rotation: c.u16()? },
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
            Msg::Hello { version: VERSION, width: 1872, height: 1404, rotation: 90 },
            Msg::Device(Device {
                id: 1,
                name: "remvnc pen".into(),
                props: 0,
                keys: vec![0x140, 0x14a],
                abs: vec![Abs { code: 0, min: 0, max: 20967, resolution: 100 }],
            }),
            Msg::Frame { device: 1, time_us: 123_456_789, events: vec![Event { kind: 3, code: 0, value: -5 }] },
            Msg::Ping { host_us: 42 },
            Msg::Pong { host_us: 42, tablet_us: 7 },
            Msg::Direct { addr: "10.11.99.1".into(), port: 4242, token: [9; 16] },
            Msg::Screens { names: vec!["DP-6".into(), "HDMI-2".into()] },
            Msg::Target { index: 2 },
            Msg::Rect { id: 3, x: 8, y: 16, w: 3, h: 2, data: vec![1, 2, 3] },
            Msg::Ack { id: 3 },
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
