//! Pausing xochitl while we own the panel, and always getting it back.
//!
//! Layers of recovery, from normal to worst case:
//! 1. Clean exit: we restore xochitl's screen, exit, and the guardian resumes it.
//! 2. We crash or get killed: the guardian sees us exit without the restored
//!    marker and resumes + restarts xochitl so it repaints.
//! 3. Everything dies: nobody pets xochitl's systemd watchdog (60 s), so
//!    systemd restarts it.
//! 4. Manual: `remvnc restore`.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn xochitl_pid() -> Option<i32> {
    pids_named("xochitl").into_iter().next()
}

pub fn pids_named(name: &str) -> Vec<i32> {
    let Ok(dir) = fs::read_dir("/proc") else { return vec![] };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == name))
        .collect()
}

pub fn signal(pid: i32, sig: i32) -> io::Result<()> {
    if unsafe { libc::kill(pid, sig) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

pub fn restart_xochitl() {
    let _ = Command::new("systemctl").args(["restart", "xochitl"]).status();
}

/// Sends a sd_notify message to systemd on behalf of `pid` (root may forge
/// SCM_CREDENTIALS), e.g. "WATCHDOG=1" for the paused xochitl.
pub fn notify_as(pid: i32, msg: &str) -> io::Result<()> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as _;
        for (d, s) in addr.sun_path.iter_mut().zip(b"/run/systemd/notify") {
            *d = *s as _;
        }
        let mut iov = libc::iovec { iov_base: msg.as_ptr() as *mut _, iov_len: msg.len() };
        let cred = libc::ucred { pid, uid: 0, gid: 0 };
        let space = libc::CMSG_SPACE(size_of::<libc::ucred>() as u32) as usize;
        let mut cbuf = vec![0u8; space];
        let mut hdr: libc::msghdr = std::mem::zeroed();
        hdr.msg_name = (&raw mut addr).cast();
        hdr.msg_namelen = size_of::<libc::sockaddr_un>() as u32;
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = cbuf.as_mut_ptr().cast();
        hdr.msg_controllen = space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&hdr);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_CREDENTIALS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<libc::ucred>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast(), cred);
        let r = libc::sendmsg(fd, &hdr, 0);
        let err = io::Error::last_os_error();
        libc::close(fd);
        if r < 0 { Err(err) } else { Ok(()) }
    }
}

/// Gives this process its own /tmp, so the engine's lock files don't collide
/// with the ones the paused xochitl holds.
pub fn private_tmp() -> io::Result<()> {
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::mount(c"tmpfs".as_ptr(), c"/tmp".as_ptr(), c"tmpfs".as_ptr(), 0, c"size=4m".as_ptr().cast()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Handle to the guardian. Unless `restored()` was called before this
/// process exits, the guardian restarts xochitl so it repaints.
pub struct Guardian {
    marker: String,
}

fn marker_path(parent: i32) -> String {
    format!("/run/remvnc/restored-{parent}")
}

impl Guardian {
    /// Starts `remvnc guardian` as a transient systemd unit, so it outlives
    /// the SSH session and logs to the journal. Falls back to a detached
    /// child. Call before loading Qt.
    pub fn spawn(xochitl: i32, deadline_secs: u64) -> io::Result<Self> {
        let me = std::process::id();
        let exe = std::env::current_exe()?;
        let args = ["guardian".to_string(), xochitl.to_string(), deadline_secs.to_string(), me.to_string()];
        let _ = fs::remove_file(marker_path(me as i32));
        let unit = format!("--unit=remvnc-guardian-{me}");
        let systemd = Command::new("systemd-run")
            .args(["--quiet", "--collect", &unit])
            .arg(&exe)
            .args(&args)
            .stdout(Stdio::null())
            .status();
        if !matches!(systemd, Ok(s) if s.success()) {
            log!("remvnc: systemd-run unavailable, guardian runs as a child");
            unsafe {
                Command::new(&exe)
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .pre_exec(|| {
                        libc::setsid();
                        Ok(())
                    })
                    .spawn()?;
            }
        }
        Ok(Self { marker: marker_path(me as i32) })
    }

    /// Tells the guardian the screen is restored: it only needs to resume xochitl.
    pub fn restored(&mut self) {
        let _ = fs::create_dir_all("/run/remvnc");
        let _ = fs::write(&self.marker, b"");
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// `remvnc guardian <xochitl pid> <deadline secs, 0 = none> <parent pid>`.
/// Pets xochitl's watchdog while the parent lives; once the parent is gone
/// (and its e-ink engine with it), resumes xochitl, restarting it unless the
/// parent restored the screen.
pub fn run_guardian(xochitl: i32, deadline_secs: u64, parent: i32) {
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let start = Instant::now();
    let mut last_pet: Option<Instant> = None;
    while alive(parent) {
        if last_pet.is_none_or(|t| t.elapsed() >= Duration::from_secs(15)) {
            let _ = notify_as(xochitl, "WATCHDOG=1");
            last_pet = Some(Instant::now());
        }
        if deadline_secs > 0 && start.elapsed() > Duration::from_secs(deadline_secs) {
            log!("remvnc guardian: deadline reached, killing {parent}");
            let _ = signal(parent, libc::SIGKILL);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let marker = marker_path(parent);
    let restored = fs::remove_file(&marker).is_ok();
    let _ = signal(xochitl, libc::SIGCONT);
    if restored {
        log!("remvnc guardian: screen restored, resumed xochitl");
    } else {
        restart_xochitl();
        log!("remvnc guardian: no restored screen, restarted xochitl");
    }
}

/// `remvnc restore`: kill stray instances, resume and restart xochitl.
pub fn manual_restore() {
    let me = std::process::id() as i32;
    for pid in pids_named("remvnc").into_iter().filter(|&p| p != me) {
        let _ = signal(pid, libc::SIGKILL);
    }
    if let Some(pid) = xochitl_pid() {
        let _ = signal(pid, libc::SIGCONT);
    }
    restart_xochitl();
}

/// Exclusive access to an input device; released when dropped (or on exit).
pub struct Grabbed {
    pub file: fs::File,
    pub path: String,
    pub name: String,
}

const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
const fn eviocgname(len: u32) -> libc::c_ulong {
    (0x8000_0000 | (len << 16) | (0x45 << 8) | 0x06) as libc::c_ulong
}

impl Grabbed {
    pub fn open(path: &str) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path)?;
        let mut buf = [0u8; 128];
        unsafe { libc::ioctl(file.as_raw_fd(), eviocgname(buf.len() as u32) as _, buf.as_mut_ptr()) };
        let name = CString::from_vec_with_nul(buf.split_inclusive(|&b| b == 0).next().unwrap_or(&[0]).to_vec())
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        if unsafe { libc::ioctl(file.as_raw_fd(), EVIOCGRAB as _, 1 as libc::c_int) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file, path: path.into(), name })
    }
}

impl Drop for Grabbed {
    fn drop(&mut self) {
        unsafe { libc::ioctl(self.file.as_raw_fd(), EVIOCGRAB as _, 0 as libc::c_int) };
    }
}

/// One mapping line from /proc/<pid>/maps.
#[derive(Debug, Clone)]
pub struct Mapping {
    pub start: usize,
    pub end: usize,
    pub perms: String,
    pub path: String,
}

pub fn mappings(pid: &str) -> Vec<Mapping> {
    let Ok(text) = fs::read_to_string(format!("/proc/{pid}/maps")) else { return vec![] };
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (a, b) = it.next()?.split_once('-')?;
            let perms = it.next()?.to_string();
            let path = it.nth(3).unwrap_or("").to_string();
            Some(Mapping { start: usize::from_str_radix(a, 16).ok()?, end: usize::from_str_radix(b, 16).ok()?, perms, path })
        })
        .collect()
}


/// Where `QImageData` keeps the fields we match on, relative to `width`.
#[derive(Debug, Clone, Copy)]
pub struct ImageDataLayout {
    /// Offset of the `data` pointer from the `width` field.
    data_from_width: usize,
}

fn words(buf: &[u8]) -> impl Iterator<Item = (usize, u32)> + '_ {
    buf.chunks_exact(4).enumerate().map(|(i, c)| (i * 4, u32::from_le_bytes([c[0], c[1], c[2], c[3]])))
}

/// Whether `buf[at..]` starts with `width, height, depth` of a screen-sized image.
fn header_at(buf: &[u8], at: usize, w: u32, h: u32, depth: u32) -> bool {
    let word = |o: usize| buf.get(at + o..at + o + 4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    word(0) == Some(w) && word(4) == Some(h) && word(8) == Some(depth)
}

fn heap_mappings(pid: &str) -> Vec<Mapping> {
    mappings(pid).into_iter().filter(|m| m.perms.starts_with("rw") && (m.path.is_empty() || m.path == "[heap]")).collect()
}

/// Our own image's QImageData header words from `width` on, for comparing
/// against xochitl's candidates.
pub fn own_header(pixels: usize, w: u32, h: u32, depth: u32) -> Vec<u32> {
    let Ok(mem) = fs::File::open("/proc/self/mem") else { return vec![] };
    for m in heap_mappings("self") {
        let mut buf = vec![0u8; m.end - m.start];
        if mem.read_exact_at(&mut buf, m.start as u64).is_err() {
            continue;
        }
        for (off, v) in words(&buf) {
            if v == w && header_at(&buf, off, w, h, depth) && words(&buf[off..(off + 64).min(buf.len())]).any(|(_, p)| p as usize == pixels) {
                return words(&buf[off..(off + 96).min(buf.len())]).map(|(_, v)| v).collect();
            }
        }
    }
    vec![]
}

/// Learns the `QImageData` layout from one of our own images.
pub fn learn_layout(pixels: usize, w: u32, h: u32, depth: u32) -> io::Result<ImageDataLayout> {
    let mem = fs::File::open("/proc/self/mem")?;
    for m in heap_mappings("self") {
        let mut buf = vec![0u8; m.end - m.start];
        if mem.read_exact_at(&mut buf, m.start as u64).is_err() {
            continue;
        }
        for (off, v) in words(&buf) {
            if v == w && header_at(&buf, off, w, h, depth) {
                if let Some((d, _)) = words(&buf[off..(off + 64).min(buf.len())]).find(|&(_, p)| p as usize == pixels) {
                    return Ok(ImageDataLayout { data_from_width: d });
                }
            }
        }
    }
    Err(io::Error::other("our own QImageData not found"))
}

/// Finds xochitl's screen-sized images of `depth` bits in QImage::Format
/// `format_wanted` and copies their pixels.
/// Returns (data address, QImageData header words from `width` on, pixels)
/// for each match.
pub fn snapshot_images(
    xochitl: i32,
    layout: ImageDataLayout,
    w: u32,
    h: u32,
    depth: u32,
    format_wanted: u32,
    len: usize,
) -> io::Result<Vec<(usize, Vec<u32>, Vec<u8>)>> {
    let mem = fs::File::open(format!("/proc/{xochitl}/mem"))?;
    let mut out = vec![];
    let mut chunk = vec![0u8; 1 << 20];
    for m in heap_mappings(&xochitl.to_string()) {
        let mut at = m.start;
        while at < m.end {
            let n = (m.end - at).min(chunk.len());
            let buf = &mut chunk[..n];
            if mem.read_exact_at(buf, at as u64).is_err() {
                break;
            }
            // Headers straddling a chunk boundary are missed; images are rare enough not to care.
            for (off, v) in words(buf) {
                if v == w && header_at(buf, off, w, h, depth) && off + layout.data_from_width + 4 <= n {
                    let data = u32::from_le_bytes(buf[off + layout.data_from_width..][..4].try_into().unwrap()) as usize;
                    let mut pixels = vec![0u8; len];
                    // QImage::Format sits right after the data pointer; other
                    // screen-sized images (e.g. ARGB32_Premultiplied) aren't the engine's.
                    let fmt_at = off + layout.data_from_width + 4;
                    let format = buf.get(fmt_at..fmt_at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
                    if format != Some(format_wanted) {
                        continue;
                    }
                    if data != 0 && mem.read_exact_at(&mut pixels, data as u64).is_ok() {
                        let header = words(&buf[off..(off + 96).min(n)]).map(|(_, v)| v).collect();
                        out.push((data, header, pixels));
                    }
                }
            }
            at += n;
        }
    }
    Ok(out)
}
