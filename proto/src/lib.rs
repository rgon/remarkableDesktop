//! Wire format between `remvnc run` on the tablet (its stdout, over SSH) and
//! `remvnc-host`. Each message is `tag: u8, len: u32 LE, payload`.
//!
//! The tablet declares virtual input devices, then streams evdev event
//! batches for them; the host recreates the devices with uinput and replays
//! each batch followed by a `SYN_REPORT`.

use std::io::{self, Read, Write};

pub const VERSION: u16 = 2;

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
    /// The tablet is exiting on purpose (X button held, power button).
    Bye,
}

const HELLO: u8 = 1;
const DEVICE: u8 = 2;
const FRAME: u8 = 3;
const BYE: u8 = 4;
const PING: u8 = 5;
const PONG: u8 = 6;

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
                p.extend_from_slice(&(d.name.len() as u16).to_le_bytes());
                p.extend_from_slice(d.name.as_bytes());
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
        if len > 1 << 20 {
            return Err(bad("message too large"));
        }
        let mut p = vec![0u8; len];
        r.read_exact(&mut p)?;
        let mut c = Cursor { buf: &p, at: 0 };
        let msg = match head[0] {
            HELLO => Msg::Hello { version: c.u16()?, width: c.u16()?, height: c.u16()?, rotation: c.u16()? },
            DEVICE => {
                let id = c.u8()?;
                let n = c.u16()? as usize;
                let name = String::from_utf8_lossy(c.take(n)?).into_owned();
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

/// Microseconds on this machine's CLOCK_MONOTONIC (the clock evdev
/// timestamps use once set with EVIOCSCLOCKID).
pub fn monotonic_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
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
