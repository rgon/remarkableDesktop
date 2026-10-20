//! Screen capture: Mutter's ScreenCast D-Bus API provides a PipeWire stream
//! (of a real monitor, or of a virtual monitor it creates for us), and a
//! GStreamer pipeline copies its frames, as they are (BGRx, at the monitor's
//! own size), into a pipe. Everything that is computed from them happens on
//! the GPU (`gpu.rs`).

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

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
    /// The capture's own size: frames are this wide and high, 4 bytes a pixel.
    pub src_size: (usize, usize),
}

/// The size in a stream's `Parameters`, written the way a GVariant prints:
/// `{"position": (0, 0), "size": (1920, 1080), ...}`.
fn parse_size(parameters: &str) -> Option<(usize, usize)> {
    let tail = &parameters[parameters.find("size")?..];
    let numbers: Vec<usize> = tail.split(|c: char| !c.is_ascii_digit()).filter(|n| !n.is_empty()).take(2).filter_map(|n| n.parse().ok()).collect();
    match numbers[..] {
        [w, h] if w > 0 && h > 0 => Some((w, h)),
        _ => None,
    }
}

impl Capture {
    /// `logical` is the tablet's screen size as the user sees it (e.g.
    /// 1872x1404 when rotated 90°), which a virtual monitor takes as its size.
    pub fn start(source: &Source, logical: (u32, u32)) -> Result<Capture, String> {
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
        // The frames' size: a virtual monitor has the one we ask for, a real
        // one says in its `Parameters`.
        let src_size = match source {
            Source::Virtual => (logical.0 as usize, logical.1 as usize),
            Source::Monitor(_) => {
                let parameters: OwnedValue = stream.get_property("Parameters").map_err(err("stream parameters"))?;
                parse_size(&parameters.to_string()).ok_or("the monitor's size is not in the stream parameters")?
            }
        };
        let mut added = stream.receive_signal("PipeWireStreamAdded").map_err(err("subscribe"))?;
        sess.call_method("Start", &()).map_err(err("Start"))?;
        let node: u32 = added
            .next()
            .ok_or("stream ended before PipeWire node appeared")?
            .body()
            .deserialize()
            .map_err(|e| format!("PipeWireStreamAdded: {e}"))?;

        let (lw, lh) = logical;
        // A virtual monitor takes its size from the format we ask for.
        let source_caps = match source {
            Source::Virtual => format!("video/x-raw,width={lw},height={lh} ! "),
            Source::Monitor(_) => String::new(),
        };
        // Nothing to compute here: the frame goes to the pipe as it is
        // (`videoconvert` only passes it on when it is BGRx already). Leaky
        // single-buffer queues: when we are busy, stale frames are dropped
        // instead of queued (latency over completeness).
        let pipeline = format!(
            "pipewiresrc path={node} always-copy=true ! {source_caps}queue leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 ! \
             videoconvert ! video/x-raw,format=BGRx ! \
             queue leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 ! fdsink fd=1 sync=false"
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
        Ok(Capture { virtual_name, _conn: conn, session, gst, out, src_size })
    }

    /// The size of one frame in the pipe.
    pub fn frame_bytes(&self) -> usize {
        self.src_size.0 * self.src_size.1 * 4
    }

    /// Waits up to `timeout` for a frame to start arriving, then reads all
    /// of it. Ok(false) if none came (Mutter only sends frames on change).
    pub fn next_frame(&mut self, buf: &mut Vec<u8>, timeout: std::time::Duration) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut pfd = libc::pollfd { fd: self.out.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) } <= 0 {
            return Ok(false);
        }
        buf.resize(self.frame_bytes(), 0);
        self.out.read_exact(buf)?;
        // While we were busy, GStreamer sat blocked writing its next frame
        // into the pipe, so that frame is as old as our busy time. Whatever is
        // already waiting is newer than what we hold: take it instead (a few
        // at most, as frames keep coming while the screen changes).
        for _ in 0..4 {
            let mut more = libc::pollfd { fd: self.out.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // A few ms, not 0: GStreamer may not have handed its next frame to the pipe yet.
            if unsafe { libc::poll(&mut more, 1, 3) } <= 0 {
                break;
            }
            self.out.read_exact(buf)?;
        }
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

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn reads_the_size_from_printed_parameters() {
        assert_eq!(parse_size(r#"{"position": (1440, 1731), "size": (1920, 1080), "output-name": "eDP-1"}"#), Some((1920, 1080)));
        assert_eq!(parse_size(r#"[{"size": <(3440, 1440)>, "position": <(1440, 291)>}]"#), Some((3440, 1440)));
        assert_eq!(parse_size(r#"{"position": (0, 0)}"#), None);
        assert_eq!(parse_size(r#"{"size": (0, 0)}"#), None);
    }
}
