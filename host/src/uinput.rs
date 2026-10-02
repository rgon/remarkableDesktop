//! Virtual input devices through /dev/uinput (x86_64 and other 64-bit hosts).

use remvnc_proto::{Device, Event};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const BUS_VIRTUAL: u16 = 0x06;

const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556e;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    value: i32,
    min: i32,
    max: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

pub struct VirtualDevice {
    file: File,
    buf: Vec<u8>,
}

fn ioctl<T>(file: &File, req: libc::c_ulong, arg: T) -> io::Result<()> {
    if unsafe { libc::ioctl(file.as_raw_fd(), req, arg) } < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

impl VirtualDevice {
    pub fn create(d: &Device) -> io::Result<Self> {
        let file = OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open("/dev/uinput").map_err(|e| {
            io::Error::new(e.kind(), format!("/dev/uinput: {e} (see host/99-remvnc-uinput.rules)"))
        })?;
        ioctl(&file, UI_SET_EVBIT, EV_SYN as libc::c_int)?;
        if !d.keys.is_empty() {
            ioctl(&file, UI_SET_EVBIT, EV_KEY as libc::c_int)?;
            for &k in &d.keys {
                ioctl(&file, UI_SET_KEYBIT, k as libc::c_int)?;
            }
        }
        if !d.abs.is_empty() {
            ioctl(&file, UI_SET_EVBIT, EV_ABS as libc::c_int)?;
        }
        for a in &d.abs {
            ioctl(&file, UI_SET_ABSBIT, a.code as libc::c_int)?;
            let setup = UinputAbsSetup {
                code: a.code,
                _pad: 0,
                value: 0,
                min: a.min,
                max: a.max,
                fuzz: 0,
                flat: 0,
                resolution: a.resolution,
            };
            ioctl(&file, UI_ABS_SETUP, &setup as *const UinputAbsSetup)?;
        }
        for prop in 0..32 {
            if d.props & (1 << prop) != 0 {
                ioctl(&file, UI_SET_PROPBIT, prop as libc::c_int)?;
            }
        }
        let mut name = [0u8; 80];
        for (dst, src) in name.iter_mut().zip(d.name.bytes().take(79)) {
            *dst = src;
        }
        let setup = UinputSetup {
            id: InputId { bustype: BUS_VIRTUAL, vendor: 0x1d50, product: 0x6200 + d.id as u16, version: 1 },
            name,
            ff_effects_max: 0,
        };
        ioctl(&file, UI_DEV_SETUP, &setup as *const UinputSetup)?;
        ioctl(&file, UI_DEV_CREATE, 0 as libc::c_int)?;
        Ok(Self { file, buf: Vec::new() })
    }

    /// Writes the events followed by a SYN_REPORT.
    pub fn emit(&mut self, events: &[Event]) -> io::Result<()> {
        self.buf.clear();
        let syn = Event { kind: EV_SYN, code: SYN_REPORT, value: 0 };
        for e in events.iter().chain([&syn]) {
            self.buf.extend_from_slice(&[0u8; 16]); // struct timeval: kernel fills in the time
            self.buf.extend_from_slice(&e.kind.to_ne_bytes());
            self.buf.extend_from_slice(&e.code.to_ne_bytes());
            self.buf.extend_from_slice(&e.value.to_ne_bytes());
        }
        self.file.write_all(&self.buf)
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        let _ = ioctl(&self.file, UI_DEV_DESTROY, 0 as libc::c_int);
    }
}
