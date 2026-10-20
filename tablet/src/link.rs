//! Connection to `remdesktop-host`. The instance listens on a TCP port with
//! Nagle off (dropbear's SSH channel adds ~40 ms stalls) and writes the port
//! and a one-time token to a root-only file. The host reads that file over
//! SSH, which is what authenticates it, then connects and sends the token.
//! Traffic on the socket itself is unencrypted.

use crate::display::{Decoder, Rect};
use crate::epd::ImageInfo;
use remdesktop_proto::Datagram;
use remdesktop_proto::Msg;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::time::Duration;

/// Where `remdesktop session` (run by the host over SSH) finds "port token".
pub const SESSION_FILE: &str = "/run/remdesktop/session";

pub struct Server {
    listener: TcpListener,
    token: [u8; 16],
    /// Pen datagrams to hosts on Wi-Fi (see `remdesktop_proto::pen`).
    udp: UdpSocket,
}

impl Server {
    pub fn listen() -> io::Result<Server> {
        let listener = TcpListener::bind(("0.0.0.0", 0))?;
        listener.set_nonblocking(true)?;
        let mut token = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut token)?;
        let udp = UdpSocket::bind(("0.0.0.0", 0))?;
        udp.set_nonblocking(true)?;
        remdesktop_proto::prioritize(udp.as_raw_fd());
        Ok(Server { listener, token, udp })
    }

    pub fn udp_fd(&self) -> RawFd {
        self.udp.as_raw_fd()
    }

    pub fn udp_port(&self) -> u16 {
        self.udp.local_addr().map_or(0, |a| a.port())
    }

    /// Best effort: a lost datagram is covered by the protocol.
    pub fn udp_send(&self, to: SocketAddr, datagram: &[u8]) {
        let _ = self.udp.send_to(datagram, to);
    }

    /// Everything waiting on the UDP socket, as (sender, datagram).
    pub fn udp_recv(&self) -> Vec<(SocketAddr, Datagram)> {
        let mut out = vec![];
        let mut buf = [0u8; 256];
        while let Ok((n, from)) = self.udp.recv_from(&mut buf) {
            out.extend(Datagram::decode(&buf[..n]).map(|d| (from, d)));
        }
        out
    }

    /// Publishes port and token for the host. Done once the USB setup has
    /// settled, so the host doesn't connect just before re-enumeration.
    pub fn publish(&self) -> io::Result<()> {
        let port = self.listener.local_addr()?.port();
        let hex: String = self.token.iter().map(|b| format!("{b:02x}")).collect();
        std::fs::create_dir_all("/run/remdesktop")?;
        std::fs::write(SESSION_FILE, format!("{port} {hex}\n"))?;
        std::fs::set_permissions(SESSION_FILE, std::fs::Permissions::from_mode(0o600))
    }

    pub fn fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// A newly connected host that presented the token, if any.
    pub fn accept(&self) -> Option<Link> {
        let (tcp, peer) = self.listener.accept().ok()?;
        let check = || -> io::Result<bool> {
            tcp.set_nonblocking(false)?;
            tcp.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut got = [0u8; 16];
            (&tcp).read_exact(&mut got)?;
            tcp.set_read_timeout(None)?;
            // A dead link (cable pulled) must never stall the input loop.
            tcp.set_write_timeout(Some(Duration::from_millis(250)))?;
            tcp.set_nodelay(true)?;
            remdesktop_proto::tune_socket(tcp.as_raw_fd());
            Ok(got == self.token)
        };
        match check() {
            Ok(true) => {
                log!("remdesktop: host connected from {peer}");
                let out = tcp.try_clone().ok()?;
                Some(Link { out: BufWriter::new(Box::new(out)), tcp, peer: peer.ip() })
            }
            // Hosts test reachability by connecting and hanging up: not worth a log line.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => None,
            _ => {
                log!("remdesktop: rejected connection from {peer}");
                None
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(SESSION_FILE);
    }
}

pub struct Link {
    pub out: BufWriter<Box<dyn Write>>,
    tcp: TcpStream,
    pub peer: std::net::IpAddr,
}

impl Link {
    /// Moves receiving off the (real-time) input thread. Screen updates are
    /// decoded straight into the framebuffer on the reader thread; everything
    /// arrives on the returned channel, and the returned fd becomes readable
    /// whenever something is waiting.
    pub fn spawn_reader(&mut self, fb: ImageInfo) -> io::Result<(mpsc::Receiver<Incoming>, File)> {
        let source = self.tcp.try_clone()?;
        let (mut wake_tx, wake_rx) = pipe()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Threads inherit the spawner's SCHED_FIFO; decoding must not
            // compete with input forwarding.
            let normal = libc::sched_param { sched_priority: 0 };
            unsafe { libc::sched_setscheduler(0, libc::SCHED_OTHER, &normal) };
            let mut source = io::BufReader::with_capacity(1 << 16, source);
            let mut decoder = Decoder::new();
            loop {
                let item = match Msg::read_from(&mut source) {
                    Ok(Some(Msg::Rect { id, x, y, w, h, flags, data })) => {
                        let r = Rect { x: x as i32, y: y as i32, w: w as i32, h: h as i32 };
                        Incoming::Decoded { id, flags, rect: decoder.write(&fb, r, flags, &data) }
                    }
                    Ok(Some(m)) => Incoming::Msg(m),
                    Ok(None) | Err(_) => Incoming::Closed,
                };
                let closed = matches!(item, Incoming::Closed);
                if tx.send(item).is_err() || wake_tx.write_all(&[0]).is_err() || closed {
                    return;
                }
            }
        });
        Ok((rx, wake_rx))
    }

    /// Ends the connection (the reader thread then reports `Closed`).
    pub fn close(&self) {
        let _ = self.tcp.shutdown(std::net::Shutdown::Both);
    }
}

/// A random key for one host's UDP datagrams.
pub fn random_key() -> u32 {
    let mut b = [0u8; 4];
    let _ = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    u32::from_le_bytes(b)
}

/// Wi-Fi power save, switched off while a host is connected over Wi-Fi.
/// Remembers the interface's previous setting and puts it back on `restore`
/// (also when dropped, as a fallback for early returns).
#[derive(Default)]
pub struct PowerSave {
    /// Interface and whether power save was on before we touched it.
    saved: Option<(String, bool)>,
}

impl PowerSave {
    pub fn disable(&mut self) {
        if self.saved.is_some() {
            return;
        }
        let Some(iface) = wireless_interface() else { return };
        let on = match iw(&["dev", &iface, "get", "power_save"]) {
            Some(out) => out.contains(": on"),
            None => return,
        };
        if on {
            if iw(&["dev", &iface, "set", "power_save", "off"]).is_none() {
                return;
            }
            log!("remdesktop: wifi power save off on {iface}");
        }
        self.saved = Some((iface, on));
    }

    pub fn restore(&mut self) {
        if let Some((iface, on)) = self.saved.take() {
            if on && iw(&["dev", &iface, "set", "power_save", "on"]).is_some() {
                log!("remdesktop: wifi power save back on for {iface}");
            }
        }
    }
}

impl Drop for PowerSave {
    fn drop(&mut self) {
        self.restore();
    }
}

fn wireless_interface() -> Option<String> {
    std::fs::read_dir("/sys/class/net").ok()?.flatten().find(|e| e.path().join("wireless").is_dir()).map(|e| e.file_name().to_string_lossy().into_owned())
}

/// Output of `iw args`, None if it is missing or failed.
fn iw(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("iw").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether an address belongs to the USB network (the tablet is 10.11.99.1).
pub fn is_usb(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.octets()[..3] == [10, 11, 99],
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(|v4| v4.octets()[..3] == [10, 11, 99]),
    }
}

/// Battery charge in percent, from the first battery in sysfs.
pub fn battery_percent() -> Option<u8> {
    for entry in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let dir = entry.path();
        if std::fs::read_to_string(dir.join("type")).is_ok_and(|t| t.trim() == "Battery") {
            if let Some(p) = std::fs::read_to_string(dir.join("capacity")).ok().and_then(|c| c.trim().parse::<u8>().ok()) {
                return Some(p.min(100));
            }
        }
    }
    None
}

/// IPv4 addresses on interfaces other than loopback and USB (e.g. Wi-Fi).
pub fn network_addresses() -> Vec<String> {
    let mut out = vec![];
    unsafe {
        let mut ifs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifs) != 0 {
            return out;
        }
        let mut p = ifs;
        while !p.is_null() {
            let i = &*p;
            p = i.ifa_next;
            if i.ifa_addr.is_null() || (*i.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(i.ifa_name).to_string_lossy();
            if name == "lo" || name.starts_with("usb") {
                continue;
            }
            let sin = &*(i.ifa_addr as *const libc::sockaddr_in);
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if !is_usb(ip.into()) {
                out.push(ip.to_string());
            }
        }
        libc::freeifaddrs(ifs);
    }
    out
}

/// What the reader thread hands to the input thread.
pub enum Incoming {
    Msg(Msg),
    /// A screen update already in the framebuffer, ready to refresh.
    Decoded { id: u32, flags: u8, rect: Result<Rect, String> },
    Closed,
}

fn pipe() -> io::Result<(File, File)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { Ok((File::from_raw_fd(fds[1]), File::from_raw_fd(fds[0]))) }
}
