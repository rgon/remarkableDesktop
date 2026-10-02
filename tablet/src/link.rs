//! Session transport. Starts on SSH stdio (authenticated, encrypted) and,
//! unless disabled, moves to a direct TCP socket with Nagle off: dropbear
//! leaves Nagle on for non-PTY sessions, which adds ~40 ms stalls.
//!
//! The direct socket listens only on the address the SSH client reached,
//! accepts only the SSH client's IP, and requires a one-time token sent over
//! SSH. Input events then travel unencrypted on that link.

use crate::display::{Decoder, Rect};
use crate::epd::ImageInfo;
use remvnc_proto::Msg;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::os::fd::FromRawFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub struct Link {
    pub out: BufWriter<Box<dyn Write>>,
    tcp: Option<TcpStream>,
}

impl Link {
    pub fn open(ssh_only: bool) -> Link {
        if !ssh_only {
            match direct() {
                Ok(tcp) => {
                    let out = tcp.try_clone().expect("clone tcp socket");
                    return Link { out: BufWriter::new(Box::new(out)), tcp: Some(tcp) };
                }
                Err(e) => log!("remvnc: direct link unavailable ({e}), using ssh"),
            }
        }
        Link { out: BufWriter::new(Box::new(io::stdout())), tcp: None }
    }

    pub fn is_direct(&self) -> bool {
        self.tcp.is_some()
    }

    /// Moves receiving off the (real-time) input thread. Screen updates are
    /// decoded straight into the framebuffer on the reader thread; everything
    /// arrives on the returned channel, and the returned fd becomes readable
    /// whenever something is waiting.
    pub fn spawn_reader(&mut self, fb: ImageInfo) -> io::Result<(mpsc::Receiver<Incoming>, File)> {
        let source: Box<dyn Read + Send> = match &self.tcp {
            Some(t) => {
                // Shared with the writer half: both block from now on.
                t.set_nonblocking(false)?;
                Box::new(t.try_clone()?)
            }
            None => Box::new(io::stdin()),
        };
        let (mut wake_tx, wake_rx) = pipe()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Threads inherit the spawner's SCHED_FIFO; decoding must not
            // compete with input forwarding.
            let normal = libc::sched_param { sched_priority: 0 };
            unsafe { libc::sched_setscheduler(0, libc::SCHED_OTHER, &normal) };
            let mut source = io::BufReader::with_capacity(1 << 16, source);
            let decoder = Decoder::new();
            loop {
                let item = match Msg::read_from(&mut source) {
                    Ok(Some(Msg::Rect { id, x, y, w, h, data })) => {
                        let r = Rect { x: x as i32, y: y as i32, w: w as i32, h: h as i32 };
                        Incoming::Decoded { id, rect: decoder.write(&fb, r, &data) }
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
}

/// What the reader thread hands to the input thread.
pub enum Incoming {
    Msg(Msg),
    /// A screen update already in the framebuffer, ready to refresh.
    Decoded { id: u32, rect: Result<Rect, String> },
    Closed,
}

fn pipe() -> io::Result<(File, File)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { Ok((File::from_raw_fd(fds[1]), File::from_raw_fd(fds[0]))) }
}

fn unmap(ip: &str) -> Result<IpAddr, String> {
    ip.trim_start_matches("::ffff:").parse().map_err(|e| format!("bad address {ip}: {e}"))
}

fn direct() -> Result<TcpStream, String> {
    let conn = std::env::var("SSH_CONNECTION").map_err(|_| "not started over ssh")?;
    let f: Vec<&str> = conn.split_whitespace().collect();
    let [client, _, server, _] = f[..] else { return Err(format!("odd SSH_CONNECTION {conn:?}")) };
    let (client, server) = (unmap(client)?, unmap(server)?);

    let listener = TcpListener::bind((server, 0)).map_err(|e| format!("bind {server}: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let mut token = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut token)).map_err(|e| e.to_string())?;

    let mut stdout = io::stdout().lock();
    Msg::Direct { addr: server.to_string(), port, token }
        .write_to(&mut stdout)
        .and_then(|_| stdout.flush())
        .map_err(|e| format!("announce: {e}"))?;

    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((tcp, peer)) if peer.ip().to_canonical() == client => {
                tcp.set_nonblocking(false).map_err(|e| e.to_string())?;
                tcp.set_read_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
                let mut got = [0u8; 16];
                if (&tcp).read_exact(&mut got).is_err() || got != token {
                    log!("remvnc: rejected direct connection from {peer}: bad token");
                    continue;
                }
                tcp.set_nodelay(true).map_err(|e| e.to_string())?;
                tcp.set_read_timeout(None).map_err(|e| e.to_string())?;
                tcp.set_nonblocking(true).map_err(|e| e.to_string())?;
                return Ok(tcp);
            }
            Ok((_, peer)) => log!("remvnc: rejected direct connection from {peer}"),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("host did not connect".into())
}
