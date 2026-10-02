//! USB HID through FunctionFS, added next to the tablet's USB network
//! functions (RNDIS in config c.1, ECM in c.2), so the network stays up.
//!
//! The kernel has no HID gadget function, but FunctionFS lets this process
//! implement one: we write the interface descriptors to ep0, answer the HID
//! class requests that arrive there as setup events, and send input reports
//! on ep1.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

const GADGET: &str = "/sys/kernel/config/usb_gadget/g_ether";
const INSTANCE: &str = "remdesktop";
const MOUNT: &str = "/dev/ffs-remdesktop";
const CONFIGS: [&str; 2] = ["c.1", "c.2"];
/// Present while our function is in the gadget: tells the guardian to
/// restore the gadget if we die.
pub const MARKER: &str = "/run/remdesktop/gadget";

fn function_dir() -> String {
    format!("{GADGET}/functions/ffs.{INSTANCE}")
}

/// Shell commands that undo everything `Gadget::add` does, idempotently.
/// Also used by the guardian and by wrappers, so it must not depend on us.
pub fn restore_script() -> String {
    format!(
        "U=$(cat {GADGET}/UDC); [ -n \"$U\" ] || U=$(ls /sys/class/udc | head -n 1); \
         echo '' > {GADGET}/UDC 2>/dev/null; \
         rm -f {GADGET}/configs/c.1/ffs.{INSTANCE} {GADGET}/configs/c.2/ffs.{INSTANCE}; \
         echo $U > {GADGET}/UDC; \
         umount {MOUNT} 2>/dev/null; rmdir {MOUNT} 2>/dev/null; rmdir {fd} 2>/dev/null; rm -f {MARKER}; true",
        fd = function_dir()
    )
}

/// Undoes `Gadget::add` (see `restore_script`).
pub fn restore() {
    let _ = std::process::Command::new("sh").args(["-c", &restore_script()]).status();
}

const DESCS_MAGIC_V2: u32 = 3;
const STRINGS_MAGIC: u32 = 2;
const HAS_FS_DESC: u32 = 1;
const HAS_HS_DESC: u32 = 2;

fn hid_descriptor(report_len: usize) -> [u8; 9] {
    let [lo, hi] = (report_len as u16).to_le_bytes();
    // bLength, HID, bcdHID 1.11, country, 1 descriptor: report, length
    [9, 0x21, 0x11, 0x01, 0, 1, 0x22, lo, hi]
}

fn descriptors(report_len: usize) -> Vec<u8> {
    // One HID interface (no boot protocol) with one interrupt IN endpoint.
    let interface = [9u8, 4, 0, 0, 1, 3, 0, 0, 1];
    let hid = hid_descriptor(report_len);
    // Polled every 1 ms: full speed bInterval 1 (ms), high speed 4 (2^3 × 125 µs).
    let ep_fs = [7u8, 5, 0x81, 3, 64, 0, 1];
    let ep_hs = [7u8, 5, 0x81, 3, 64, 0, 4];
    let mut body = vec![];
    for ep in [ep_fs, ep_hs] {
        body.extend_from_slice(&interface);
        body.extend_from_slice(&hid);
        body.extend_from_slice(&ep);
    }
    let mut out = vec![];
    let len = 4 * 5 + body.len();
    for v in [DESCS_MAGIC_V2, len as u32, HAS_FS_DESC | HAS_HS_DESC, 3, 3] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&body);
    out
}

fn strings() -> Vec<u8> {
    let name = b"remdesktop tablet\0";
    let mut out = vec![];
    let len = 4 * 4 + 2 + name.len();
    for v in [STRINGS_MAGIC, len as u32, 1, 1] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&0x0409u16.to_le_bytes());
    out.extend_from_slice(name);
    out
}

/// USB-side state, as reported by FunctionFS events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Link {
    /// Function exists but the host hasn't configured it.
    Down = 0,
    /// The host configured us: reports are being polled.
    Up = 1,
    Suspended = 2,
}

pub struct Gadget {
    state: Arc<AtomicU8>,
    ep1: Arc<Mutex<Option<File>>>,
}

impl Gadget {
    /// Adds our HID function to the live gadget and re-enumerates USB (the
    /// USB network drops for a moment). Undo with `restore`.
    pub fn add(report: &'static [u8]) -> io::Result<Gadget> {
        let udc = fs::read_to_string(format!("{GADGET}/UDC"))?.trim().to_string();
        if udc.is_empty() {
            return Err(io::Error::other("gadget is not bound to a UDC"));
        }
        let fdir = function_dir();
        if !fs::exists(&fdir)? {
            fs::create_dir(&fdir)?;
        }
        fs::create_dir_all(MOUNT)?;
        let mounted = fs::read_to_string("/proc/mounts")?.lines().any(|l| l.split_whitespace().nth(1) == Some(MOUNT));
        if !mounted {
            let (src, dst, fstype) = (c"remdesktop", c"/dev/ffs-remdesktop", c"functionfs");
            if unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), fstype.as_ptr(), 0, std::ptr::null()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let mut ep0 = OpenOptions::new().read(true).write(true).open(format!("{MOUNT}/ep0"))?;
        ep0.write_all(&descriptors(report.len()))?;
        ep0.write_all(&strings())?;

        fs::create_dir_all("/run/remdesktop")?;
        fs::write(MARKER, b"")?;
        for c in CONFIGS {
            let link = format!("{GADGET}/configs/{c}/ffs.{INSTANCE}");
            if fs::symlink_metadata(&link).is_err() {
                std::os::unix::fs::symlink(&fdir, &link)?;
            }
        }
        fs::write(format!("{GADGET}/UDC"), "\n")?;
        fs::write(format!("{GADGET}/UDC"), &udc)?;

        let state = Arc::new(AtomicU8::new(Link::Down as u8));
        let ep1: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
        let (st, e1) = (state.clone(), ep1.clone());
        std::thread::spawn(move || serve_ep0(ep0, report, st, e1));
        Ok(Gadget { state, ep1 })
    }

    pub fn link(&self) -> Link {
        match self.state.load(Ordering::Relaxed) {
            1 => Link::Up,
            2 => Link::Suspended,
            _ => Link::Down,
        }
    }

    /// Sends one input report (first byte = report ID). Dropped while the
    /// host isn't polling.
    pub fn send(&self, report: &[u8]) -> bool {
        if self.link() != Link::Up {
            return false;
        }
        let mut ep1 = self.ep1.lock().unwrap();
        match ep1.as_mut().map(|f| f.write(report)) {
            Some(Ok(_)) => true,
            Some(Err(e)) => {
                log!("remdesktop: hid report: {e}");
                *ep1 = None;
                false
            }
            None => false,
        }
    }
}

const EV_BIND: u8 = 0;
const EV_UNBIND: u8 = 1;
const EV_ENABLE: u8 = 2;
const EV_DISABLE: u8 = 3;
const EV_SETUP: u8 = 4;
const EV_SUSPEND: u8 = 5;
const EV_RESUME: u8 = 6;

/// Answers control requests on ep0 for as long as the function lives.
fn serve_ep0(mut ep0: File, report: &'static [u8], state: Arc<AtomicU8>, ep1: Arc<Mutex<Option<File>>>) {
    let mut buf = [0u8; 12 * 4];
    loop {
        let n = match ep0.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                log!("remdesktop: ep0: {e}");
                return;
            }
        };
        for ev in buf[..n].chunks_exact(12) {
            match ev[8] {
                EV_ENABLE => {
                    *ep1.lock().unwrap() = OpenOptions::new().write(true).open(format!("{MOUNT}/ep1")).ok();
                    state.store(Link::Up as u8, Ordering::Relaxed);
                    log!("remdesktop: usb host configured the hid function");
                }
                EV_DISABLE | EV_UNBIND => {
                    *ep1.lock().unwrap() = None;
                    state.store(Link::Down as u8, Ordering::Relaxed);
                }
                EV_SUSPEND => state.store(Link::Suspended as u8, Ordering::Relaxed),
                EV_RESUME => state.store(Link::Up as u8, Ordering::Relaxed),
                EV_SETUP => setup(&mut ep0, &ev[..8], report),
                EV_BIND => {}
                _ => {}
            }
        }
    }
}

fn setup(ep0: &mut File, req: &[u8], report: &[u8]) {
    let (kind, request) = (req[0], req[1]);
    let value = u16::from_le_bytes([req[2], req[3]]);
    let length = u16::from_le_bytes([req[6], req[7]]) as usize;
    let reply = |ep0: &mut File, data: &[u8]| {
        let _ = ep0.write(&data[..data.len().min(length)]);
    };
    match (kind, request) {
        // GET_DESCRIPTOR to the interface: report or HID descriptor.
        (0x81, 0x06) if value >> 8 == 0x22 => reply(ep0, report),
        (0x81, 0x06) if value >> 8 == 0x21 => reply(ep0, &hid_descriptor(report.len())),
        // GET_REPORT: feature reports carry real values (contact maximum).
        (0xa1, 0x01) => match crate::hid_report::feature(value as u8).filter(|_| value >> 8 == 3) {
            Some(f) => reply(ep0, &f),
            None => {
                let mut zero = vec![0u8; length];
                if let Some(b) = zero.first_mut() {
                    *b = value as u8;
                }
                reply(ep0, &zero)
            }
        },
        (0xa1, 0x02) => reply(ep0, &[0]),               // GET_IDLE
        (0xa1, 0x03) => reply(ep0, &[1]),               // GET_PROTOCOL: report
        // SET_IDLE, SET_PROTOCOL, SET_REPORT: read (any data) to acknowledge.
        (0x21, 0x0a | 0x0b | 0x09) => {
            let mut sink = vec![0u8; length];
            let _ = ep0.read(&mut sink);
        }
        _ => {
            // Stall: transfer in the wrong direction.
            log!("remdesktop: hid: unsupported request {kind:#04x}/{request:#04x} value {value:#06x}");
            if kind & 0x80 != 0 {
                let _ = ep0.read(&mut [0u8; 0]);
            } else {
                let _ = ep0.write(&[]);
            }
        }
    }
}

/// Prototype report: relative mouse, report ID 1: buttons, dx, dy.
pub const MOUSE_REPORT: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xa1, 0x00, // mouse, pointer
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, // 3 buttons
    0x95, 0x01, 0x75, 0x05, 0x81, 0x03, // padding
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, // x, y relative
    0xc0, 0xc0,
];

/// `remdesktop hid-test`: enumerate as a HID mouse and draw circles for a while.
/// Run detached with `restore_script` after it: USB re-enumerates.
pub fn test(seconds: u64) -> Result<(), String> {
    let g = Gadget::add(MOUSE_REPORT).map_err(|e| format!("gadget: {e}"))?;
    let start = std::time::Instant::now();
    while g.link() != Link::Up && start.elapsed().as_secs() < 15 {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    log!("remdesktop: hid link {:?} after {:?}", g.link(), start.elapsed());
    let begin = std::time::Instant::now();
    let mut sent = 0u32;
    while begin.elapsed().as_secs() < seconds {
        let t = begin.elapsed().as_secs_f64() * 3.0;
        let (dx, dy) = ((t.cos() * 6.0) as i8, (t.sin() * 6.0) as i8);
        if g.send(&[1, 0, dx as u8, dy as u8]) {
            sent += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
    log!("remdesktop: sent {sent} mouse reports");
    restore();
    Ok(())
}
