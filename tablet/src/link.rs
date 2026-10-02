//! Session transport. Starts on SSH stdio (authenticated, encrypted) and,
//! unless disabled, moves to a direct TCP socket with Nagle off: dropbear
//! leaves Nagle on for non-PTY sessions, which adds ~40 ms stalls.
//!
//! The direct socket listens only on the address the SSH client reached,
//! accepts only the SSH client's IP, and requires a one-time token sent over
//! SSH. Input events then travel unencrypted on that link.

use remvnc_proto::Msg;
use std::io::{self, BufWriter, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

pub struct Link {
    pub out: BufWriter<Box<dyn Write>>,
    tcp: Option<TcpStream>,
    buf: Vec<u8>,
}

impl Link {
    pub fn open(ssh_only: bool) -> Link {
        if !ssh_only {
            match direct() {
                Ok(tcp) => {
                    let out = tcp.try_clone().expect("clone tcp socket");
                    return Link { out: BufWriter::new(Box::new(out)), tcp: Some(tcp), buf: vec![] };
                }
                Err(e) => eprintln!("remvnc: direct link unavailable ({e}), using ssh"),
            }
        }
        Link { out: BufWriter::new(Box::new(io::stdout())), tcp: None, buf: vec![] }
    }

    /// Where host messages arrive.
    pub fn input_fd(&self) -> RawFd {
        self.tcp.as_ref().map_or(0, |t| t.as_raw_fd())
    }

    pub fn is_direct(&self) -> bool {
        self.tcp.is_some()
    }

    /// Reads what is available from the host. `Ok(None)` once it closed.
    pub fn receive(&mut self) -> io::Result<Option<Vec<Msg>>> {
        let mut chunk = [0u8; 4096];
        let n = match &mut self.tcp {
            Some(t) => match t.read(&mut chunk) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(Some(vec![])),
                r => r?,
            },
            None => {
                let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
                if n < 0 {
                    return Err(io::Error::last_os_error());
                }
                n as usize
            }
        };
        if n == 0 {
            return Ok(None);
        }
        self.buf.extend_from_slice(&chunk[..n]);
        let mut msgs = vec![];
        while let Some((m, used)) = Msg::parse(&self.buf)? {
            self.buf.drain(..used);
            msgs.push(m);
        }
        Ok(Some(msgs))
    }
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
                    eprintln!("remvnc: rejected direct connection from {peer}: bad token");
                    continue;
                }
                tcp.set_nodelay(true).map_err(|e| e.to_string())?;
                tcp.set_read_timeout(None).map_err(|e| e.to_string())?;
                tcp.set_nonblocking(true).map_err(|e| e.to_string())?;
                return Ok(tcp);
            }
            Ok((_, peer)) => eprintln!("remvnc: rejected direct connection from {peer}"),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("host did not connect".into())
}
