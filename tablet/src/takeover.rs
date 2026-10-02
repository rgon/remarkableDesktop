//! Pausing xochitl while we own the panel, and always getting it back.
//!
//! Layers of recovery, from normal to worst case:
//! 1. Clean exit: we restore xochitl's screen, exit, and the guardian resumes it.
//! 2. We crash or get killed: the guardian sees EOF without "restored" and
//!    resumes + restarts xochitl so it repaints.
//! 3. Everything dies: nobody pets xochitl's systemd watchdog (60 s), so
//!    systemd restarts it.
//! 4. Manual: `remvnc restore`.

use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
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

/// Handle to the guardian process. Dropping it without `restored()` makes the
/// guardian restart xochitl.
pub struct Guardian {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Guardian {
    /// Spawns `remvnc guardian`, detached into its own session so SSH
    /// hang-ups and Ctrl-C don't reach it. Call before loading Qt.
    pub fn spawn(xochitl: i32, deadline_secs: u64) -> io::Result<Self> {
        let mut child = unsafe {
            Command::new(std::env::current_exe()?)
                .args(["guardian", &xochitl.to_string(), &deadline_secs.to_string(), &std::process::id().to_string()])
                .stdin(Stdio::piped())
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()?
        };
        let stdin = child.stdin.take();
        Ok(Self { child, stdin })
    }

    /// Tells the guardian the screen is restored: it only needs to resume xochitl.
    pub fn restored(&mut self) {
        use std::io::Write;
        if let Some(s) = self.stdin.as_mut() {
            let _ = s.write_all(b"restored\n");
        }
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

/// `remvnc guardian <xochitl pid> <deadline secs, 0 = none> <parent pid>`.
/// Pets xochitl's watchdog while the parent lives; resumes xochitl when the
/// parent's end of the pipe closes.
pub fn run_guardian(xochitl: i32, deadline_secs: u64, parent: i32) {
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let start = Instant::now();
    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    let mut reader = BufReader::new(stdin.lock());
    let mut restored = false;
    loop {
        let _ = notify_as(xochitl, "WATCHDOG=1");
        if deadline_secs > 0 && start.elapsed() > Duration::from_secs(deadline_secs) {
            eprintln!("remvnc guardian: deadline reached, killing {parent}");
            let _ = signal(parent, libc::SIGKILL);
            break;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 15_000) } <= 0 {
            continue;
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) if line.trim() == "restored" => restored = true,
            Ok(_) => {}
        }
    }
    // Drain in case the deadline path left the parent writing.
    let _ = reader.read_to_end(&mut Vec::new());
    let _ = signal(xochitl, libc::SIGCONT);
    if !restored {
        eprintln!("remvnc guardian: unclean exit, restarting xochitl");
        restart_xochitl();
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

/// Copies of xochitl's buffers that are laid out like ours: same mapping size,
/// permissions and backing, read at the same offset into the mapping.
pub fn snapshot_like(xochitl: i32, ours: *const u8, len: usize) -> io::Result<Vec<(Mapping, Vec<u8>)>> {
    let addr = ours as usize;
    let mine = mappings("self")
        .into_iter()
        .find(|m| m.start <= addr && addr < m.end)
        .ok_or_else(|| io::Error::other("our framebuffer is not in any mapping"))?;
    let offset = addr - mine.start;
    let mem = fs::File::open(format!("/proc/{xochitl}/mem"))?;
    let mut out = vec![];
    for m in mappings(&xochitl.to_string()) {
        if m.end - m.start == mine.end - mine.start && m.perms == mine.perms && m.path == mine.path {
            let mut buf = vec![0u8; len];
            if mem.read_exact_at(&mut buf, (m.start + offset) as u64).is_ok() {
                out.push((m, buf));
            }
        }
    }
    eprintln!("remvnc: our fb mapping {mine:?} offset {offset:#x}; {} xochitl candidates", out.len());
    Ok(out)
}

fn readable(maps: &[Mapping], addr: usize, len: usize) -> bool {
    maps.iter().any(|m| m.perms.starts_with('r') && m.start <= addr && addr + len <= m.end)
}

/// Finds a chain of offsets from `from` to a word equal to `target`:
/// `[o1, o2, .., k]` means `*( *(*(from + o1) + o2) .. + k) == target`.
/// Only follows pointers into readable mappings of this process.
fn find_path(maps: &[Mapping], from: usize, target: usize, depth: u32, span: usize) -> Option<Vec<usize>> {
    for off in (0..span).step_by(4) {
        if !readable(maps, from + off, 4) {
            return None;
        }
        let v = unsafe { ((from + off) as *const usize).read() };
        if v == target {
            return Some(vec![off]);
        }
    }
    if depth == 0 {
        return None;
    }
    for off in (0..span).step_by(4) {
        let v = unsafe { ((from + off) as *const usize).read() };
        if v % 4 == 0 && readable(maps, v, span) {
            if let Some(mut rest) = find_path(maps, v, target, depth - 1, 256) {
                rest.insert(0, off);
                return Some(rest);
            }
        }
    }
    None
}

fn read_word(mem: &fs::File, addr: usize) -> io::Result<usize> {
    let mut b = [0u8; 4];
    mem.read_exact_at(&mut b, addr as u64)?;
    Ok(u32::from_le_bytes(b) as usize)
}

/// Copies xochitl's engine buffers, located by replaying in xochitl the
/// pointer path that leads from the engine singleton to each of our buffers.
/// The singleton is found through its static slot in libqsgepaper.so.
pub fn snapshot_via_engine(xochitl: i32, instance: usize, buffers: &[(usize, usize)]) -> io::Result<Vec<Vec<u8>>> {
    const LIB: &str = "libqsgepaper.so";
    let maps = mappings("self");
    let lib: Vec<&Mapping> = maps.iter().filter(|m| m.path.ends_with(LIB)).collect();
    let base = lib.iter().map(|m| m.start).min().ok_or_else(|| io::Error::other("libqsgepaper not mapped"))?;
    // Writable data plus the anonymous .bss right after the library's last mapping.
    let last_end = lib.iter().map(|m| m.end).max().unwrap();
    let mut regions: Vec<&Mapping> = lib.iter().copied().filter(|m| m.perms.starts_with("rw")).collect();
    regions.extend(maps.iter().filter(|m| m.start == last_end && m.path.is_empty()));
    let slot = regions
        .iter()
        .flat_map(|m| (m.start..m.end).step_by(4))
        .find(|&a| unsafe { (a as *const usize).read() } == instance)
        .ok_or_else(|| io::Error::other("engine singleton slot not found"))?;

    let xmaps = mappings(&xochitl.to_string());
    let xbase = xmaps.iter().filter(|m| m.path.ends_with(LIB)).map(|m| m.start).min()
        .ok_or_else(|| io::Error::other("libqsgepaper not mapped in xochitl"))?;
    let mem = fs::File::open(format!("/proc/{xochitl}/mem"))?;
    let xinstance = read_word(&mem, xbase + (slot - base))?;
    eprintln!("remvnc: singleton slot lib+{:#x}; ours {instance:#x}, xochitl's {xinstance:#x}", slot - base);

    let mut out = vec![];
    for &(pixels, len) in buffers {
        let path = find_path(&maps, instance, pixels, 2, 1024)
            .ok_or_else(|| io::Error::other(format!("no pointer path to {pixels:#x}")))?;
        let mut addr = xinstance;
        for off in &path {
            addr = read_word(&mem, addr + off)?;
        }
        eprintln!("remvnc: path {path:x?} -> xochitl buffer {addr:#x}");
        let mut buf = vec![0u8; len];
        mem.read_exact_at(&mut buf, addr as u64)?;
        out.push(buf);
    }
    Ok(out)
}
