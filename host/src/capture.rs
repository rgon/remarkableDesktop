//! Screen capture: Mutter's ScreenCast D-Bus API provides a PipeWire stream
//! (of a real monitor, or of a virtual monitor it creates for us), and a
//! GStreamer pipeline turns it into gray frames laid out like the panel.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, Value};

const DEST: &str = "org.gnome.Mutter.ScreenCast";
/// Embed the pointer in the frames so it shows on the tablet.
const CURSOR_EMBEDDED: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// A new monitor of the tablet's logical size, extending the desktop.
    Virtual,
    /// Mirror of an existing monitor, by connector name (e.g. "DP-6").
    Monitor(String),
}

pub struct Capture {
    /// Connector of the virtual monitor we created, if any.
    pub virtual_name: Option<String>,
    _conn: Connection,
    session: OwnedObjectPath,
    gst: Child,
    out: ChildStdout,
    /// Output frame size (panel portrait), bytes = width * height.
    pub width: usize,
    pub height: usize,
}

/// GStreamer `videoflip` method turning the tablet's logical (rotated)
/// picture into the panel's portrait layout.
fn flip(rotation: u16) -> &'static str {
    match rotation {
        90 => "counterclockwise",
        180 => "rotate-180",
        270 => "clockwise",
        _ => "none",
    }
}

impl Capture {
    /// `logical` is the tablet's screen size as the user sees it (e.g.
    /// 1872x1404 when rotated 90°); frames come out in panel portrait.
    pub fn start(source: &Source, logical: (u32, u32), rotation: u16) -> Result<Capture, String> {
        let err = |what: &'static str| move |e: zbus::Error| format!("{what}: {e}");
        let conn = Connection::session().map_err(err("session bus"))?;
        let screencast = Proxy::new(&conn, DEST, "/org/gnome/Mutter/ScreenCast", "org.gnome.Mutter.ScreenCast").map_err(err("ScreenCast"))?;
        let none: HashMap<&str, Value> = HashMap::new();
        let session: OwnedObjectPath = screencast.call("CreateSession", &(none,)).map_err(err("CreateSession"))?;
        let sess = Proxy::new(&conn, DEST, session.clone(), "org.gnome.Mutter.ScreenCast.Session").map_err(err("session"))?;

        let layout = match source {
            Source::Virtual => Some(crate::displayconfig::snapshot(&conn)?),
            Source::Monitor(_) => None,
        };
        let mut props: HashMap<&str, Value> = HashMap::new();
        props.insert("cursor-mode", Value::U32(CURSOR_EMBEDDED));
        let stream: OwnedObjectPath = match source {
            Source::Virtual => sess.call("RecordVirtual", &(props,)).map_err(err("RecordVirtual"))?,
            Source::Monitor(c) => sess.call("RecordMonitor", &(c.as_str(), props)).map_err(err("RecordMonitor"))?,
        };
        let stream = Proxy::new(&conn, DEST, stream.as_str(), "org.gnome.Mutter.ScreenCast.Stream").map_err(err("stream"))?;
        let mut added = stream.receive_signal("PipeWireStreamAdded").map_err(err("subscribe"))?;
        sess.call_method("Start", &()).map_err(err("Start"))?;
        let node: u32 = added
            .next()
            .ok_or("stream ended before PipeWire node appeared")?
            .body()
            .deserialize()
            .map_err(|e| format!("PipeWireStreamAdded: {e}"))?;

        let (lw, lh) = logical;
        let (pw, ph) = if rotation % 180 == 90 { (lh, lw) } else { (lw, lh) };
        // A virtual monitor takes its size from the format we ask for.
        let source_caps = match source {
            Source::Virtual => format!("video/x-raw,width={lw},height={lh} ! "),
            Source::Monitor(_) => String::new(),
        };
        let pipeline = format!(
            // Leaky single-buffer queues: when we are busy, stale frames are
            // dropped instead of queued (latency over completeness).
            "pipewiresrc path={node} always-copy=true ! {source_caps}queue leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 ! \
             videoconvert ! videoscale add-borders=true ! \
             video/x-raw,format=GRAY8,width={lw},height={lh},pixel-aspect-ratio=1/1 ! videoflip method={} ! \
             video/x-raw,format=GRAY8,width={pw},height={ph} ! \
             queue leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 ! fdsink fd=1 sync=false",
            flip(rotation)
        );
        let mut gst = Command::new("gst-launch-1.0")
            .arg("-q")
            .args(pipeline.split_whitespace())
            .stdout(Stdio::piped())
            .stdin(Stdio::null())
            // Its only output is noise about the stream closing when we stop.
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("gst-launch-1.0: {e}"))?;
        let out = gst.stdout.take().unwrap();
        // The virtual monitor exists once the stream is negotiated.
        let virtual_name = match &layout {
            Some(l) => match crate::displayconfig::keep_layout(&conn, l, std::time::Duration::from_secs(5)) {
                Ok(name) => Some(name),
                Err(e) => {
                    eprintln!("remdesktop-host: could not keep the monitor layout: {e}");
                    None
                }
            },
            None => None,
        };
        Ok(Capture { virtual_name, _conn: conn, session, gst, out, width: pw as usize, height: ph as usize })
    }

    /// Waits up to `timeout` for a frame to start arriving, then reads all
    /// of it. Ok(false) if none came (Mutter only sends frames on change).
    pub fn next_frame(&mut self, buf: &mut Vec<u8>, timeout: std::time::Duration) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut pfd = libc::pollfd { fd: self.out.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) } <= 0 {
            return Ok(false);
        }
        buf.resize(self.width * self.height, 0);
        self.out.read_exact(buf)?;
        Ok(true)
    }

}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.gst.kill();
        let _ = self.gst.wait();
        if let Ok(sess) = Proxy::new(&self._conn, DEST, self.session.as_str(), "org.gnome.Mutter.ScreenCast.Session") {
            let _ = sess.call_method("Stop", &());
        }
    }
}
