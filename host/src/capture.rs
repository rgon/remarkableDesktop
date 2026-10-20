//! Screen capture: Mutter's ScreenCast D-Bus API provides a PipeWire stream
//! (of a real monitor, or of a virtual monitor it creates for us), and an
//! in-process PipeWire consumer (its own thread, the `pipewire` crate) takes
//! its frames as they are: BGRx or a variant, at the monitor's own size. The
//! frames' shared memory is mapped, not copied: `Capture::frame` is a slice
//! of the PipeWire buffer, which the GPU upload copies once. Everything that
//! is computed from the frames happens on the GPU (`gpu.rs`).
//!
//! Only the newest frame is kept: a frame that arrives while the previous
//! one is still waiting replaces it (its buffer goes back to PipeWire), and
//! the capture thread is woken the moment one arrives.

use pipewire as pw;
use pw::spa;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{Object, Pod, Property, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as DbusValue};

const DEST: &str = "org.gnome.Mutter.ScreenCast";
/// Embed the pointer in the frames so it shows on the tablet.
const CURSOR_EMBEDDED: u32 = 1;
/// How long the stream has to settle on a format.
const NEGOTIATION: Duration = Duration::from_secs(5);

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
    hub: Arc<Hub<Frame>>,
    to_loop: pw::channel::Sender<Msg>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The frame `frame` shows: its buffer is ours until the next one is taken.
    held: Option<Frame>,
    /// The capture's own size: frames are this wide and high, 4 bytes a pixel.
    pub src_size: (usize, usize),
    /// The frames' bytes are R, G, B, x (not B, G, R, x).
    pub swap_rb: bool,
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

/// The pixel formats taken, with whether their bytes come R first. Four
/// bytes a pixel in all of them, the fourth ignored.
const FORMATS: [(VideoFormat, bool); 4] = [(VideoFormat::BGRx, false), (VideoFormat::BGRA, false), (VideoFormat::RGBx, true), (VideoFormat::RGBA, true)];

fn swap_rb_of(format: VideoFormat) -> Option<bool> {
    FORMATS.iter().find(|(f, _)| *f == format).map(|&(_, swap)| swap)
}

/// What the stream settled on.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Negotiated {
    size: (usize, usize),
    swap_rb: bool,
}

/// Where the PipeWire thread leaves what the capture thread picks up: the
/// newest frame (one slot, no queue), the negotiated format and the error
/// that ended the stream. Every change wakes the waiting reader.
struct Hub<T> {
    state: Mutex<HubState<T>>,
    wake: Condvar,
}

struct HubState<T> {
    frame: Option<T>,
    format: Option<Negotiated>,
    error: Option<String>,
}

impl<T> Hub<T> {
    fn new() -> Hub<T> {
        Hub { state: Mutex::new(HubState { frame: None, format: None, error: None }), wake: Condvar::new() }
    }

    /// Makes `frame` the newest, giving back the one it replaces.
    fn put(&self, frame: T) -> Option<T> {
        let old = self.state.lock().unwrap().frame.replace(frame);
        self.wake.notify_all();
        old
    }

    /// Drops the waiting frame if `gone` says so (its buffer is going away).
    fn discard_if(&self, gone: impl FnOnce(&T) -> bool) {
        let mut s = self.state.lock().unwrap();
        if s.frame.as_ref().is_some_and(gone) {
            s.frame = None;
        }
    }

    /// The first error stays: what follows is its consequence.
    fn fail(&self, error: String) {
        self.state.lock().unwrap().error.get_or_insert(error);
        self.wake.notify_all();
    }

    /// A format different from the one negotiated before ends the stream: the
    /// capture starts over with the new size.
    fn set_format(&self, format: Negotiated) {
        let mut s = self.state.lock().unwrap();
        match s.format {
            Some(old) if old != format => {
                s.error.get_or_insert(format!("the stream's format changed ({}x{} to {}x{})", old.size.0, old.size.1, format.size.0, format.size.1));
            }
            _ => s.format = Some(format),
        }
        drop(s);
        self.wake.notify_all();
    }

    fn format(&self) -> Option<Negotiated> {
        self.state.lock().unwrap().format
    }

    fn wait_format(&self, timeout: Duration) -> Result<Negotiated, String> {
        let s = self.state.lock().unwrap();
        let (s, _) = self.wake.wait_timeout_while(s, timeout, |s| s.format.is_none() && s.error.is_none()).unwrap();
        match (&s.error, s.format) {
            (Some(e), _) => Err(e.clone()),
            (None, Some(f)) => Ok(f),
            (None, None) => Err("the stream did not settle on a format".into()),
        }
    }

    /// Waits up to `timeout` for a frame and takes it. Ok(None) if none came;
    /// Err once the stream has failed, even if a frame is still waiting.
    fn take(&self, timeout: Duration) -> Result<Option<T>, String> {
        let s = self.state.lock().unwrap();
        let (mut s, _) = self.wake.wait_timeout_while(s, timeout, |s| s.frame.is_none() && s.error.is_none()).unwrap();
        match &s.error {
            Some(e) => Err(e.clone()),
            None => Ok(s.frame.take()),
        }
    }
}

/// A buffer's shared memory, mapped for as long as anyone has a frame in it
/// (it may outlive the buffer, when the stream renegotiates under a frame).
struct Mapping {
    base: *mut libc::c_void,
    total: usize,
    data: *const u8,
    len: usize,
}

// Read-only memory that only goes away with the mapping.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn new(fd: i32, offset: usize, len: usize) -> Result<Mapping, String> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let delta = offset % page;
        let total = len + delta;
        let base = unsafe { libc::mmap(std::ptr::null_mut(), total, libc::PROT_READ, libc::MAP_SHARED, fd, (offset - delta) as libc::off_t) };
        if base == libc::MAP_FAILED {
            return Err(format!("mapping a buffer: {}", std::io::Error::last_os_error()));
        }
        Ok(Mapping { base, total, data: unsafe { (base as *const u8).add(delta) }, len })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base, self.total) };
    }
}

/// One frame: a PipeWire buffer that is ours until it is released.
struct Frame {
    buffer: usize,
    mapping: Arc<Mapping>,
    offset: usize,
    len: usize,
}

impl Frame {
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.mapping.data.add(self.offset), self.len) }
    }
}

/// What the capture thread tells the PipeWire thread.
enum Msg {
    /// A frame is no longer looked at: its buffer goes back to PipeWire.
    Release(usize, Arc<Mapping>),
    Quit,
}

fn pod_bytes(object: Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object)).expect("a pod").0.into_inner()
}

/// The formats we take. A virtual monitor takes its size from the one we ask
/// for (`fixed`); a real monitor has the one it has (`default`), whatever
/// the range says.
fn format_pod(fixed: bool, default: (u32, u32)) -> Vec<u8> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::pod::ChoiceValue;
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle};
    let rect = |(width, height)| Rectangle { width, height };
    let size = if fixed {
        Value::Rectangle(rect(default))
    } else {
        Value::Choice(ChoiceValue::Rectangle(Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: rect(default), min: rect((1, 1)), max: rect((16384, 16384)) })))
    };
    let formats = FORMATS.iter().map(|(f, _)| Id(f.as_raw())).collect::<Vec<_>>();
    let framerate = Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: Fraction { num: 60, denom: 1 }, min: Fraction { num: 0, denom: 1 }, max: Fraction { num: 1000, denom: 1 } });
    pod_bytes(Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(FormatProperties::MediaType.as_raw(), Value::Id(Id(MediaType::Video.as_raw()))),
            Property::new(FormatProperties::MediaSubtype.as_raw(), Value::Id(Id(MediaSubtype::Raw.as_raw()))),
            Property::new(FormatProperties::VideoFormat.as_raw(), Value::Choice(ChoiceValue::Id(Choice(ChoiceFlags::empty(), ChoiceEnum::Enum { default: formats[0], alternatives: formats })))),
            Property::new(FormatProperties::VideoSize.as_raw(), size),
            Property::new(FormatProperties::VideoFramerate.as_raw(), Value::Choice(ChoiceValue::Fraction(framerate))),
        ],
    })
}

/// The buffers we take: shared memory (memfd), which we map ourselves, in
/// enough numbers that one waiting and one being copied leave Mutter some.
fn buffers_pod() -> Vec<u8> {
    use spa::pod::ChoiceValue;
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    let count = Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: 6, min: 4, max: 16 });
    pod_bytes(Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_BUFFERS_buffers, Value::Choice(ChoiceValue::Int(count))),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_dataType, Value::Int(1 << spa::sys::SPA_DATA_MemFd)),
        ],
    })
}

/// The frame in a PipeWire buffer, if it holds one: a buffer whose chunk is
/// empty or flagged corrupt carries no new picture.
///
/// # Safety
/// `buffer` is a live buffer of the stream.
unsafe fn frame_of(buffer: *mut pw::sys::pw_buffer, mapping: &Arc<Mapping>, size: (usize, usize)) -> Option<Frame> {
    let spa_buffer = unsafe { &*(*buffer).buffer };
    if spa_buffer.n_datas == 0 {
        return None;
    }
    let chunk = unsafe { &*(*spa_buffer.datas).chunk };
    if chunk.size == 0 || chunk.flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
        return None;
    }
    let stride = (chunk.stride.max(0) as usize).max(size.0 * 4);
    let (offset, len) = (chunk.offset as usize, stride * size.1);
    (offset + len <= mapping.len).then(|| Frame { buffer: buffer as usize, mapping: mapping.clone(), offset, len })
}

/// The PipeWire thread: connects to the node, keeps the newest frame in the
/// hub and runs until told to quit. Whatever ends it is left in the hub.
fn pipewire_thread(node: u32, fixed: bool, size: (usize, usize), hub: Arc<Hub<Frame>>, from_capture: pw::channel::Receiver<Msg>) {
    if let Err(e) = pipewire_run(node, fixed, size, &hub, from_capture) {
        hub.fail(e);
    }
    hub.fail("the PipeWire loop ended".into());
}

fn pipewire_run(node: u32, fixed: bool, size: (usize, usize), hub: &Arc<Hub<Frame>>, from_capture: pw::channel::Receiver<Msg>) -> Result<(), String> {
    use pw::stream::{StreamFlags, StreamState};
    let e = |what: &'static str| move |e: pw::Error| format!("PipeWire {what}: {e}");
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(e("loop"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(e("context"))?;
    let core = context.connect_rc(None).map_err(e("connect"))?;
    let _core_listener = {
        let (hub, mainloop) = (hub.clone(), mainloop.clone());
        core.add_listener_local()
            .error(move |_, _, _, message| {
                hub.fail(format!("PipeWire: {message}"));
                mainloop.quit();
            })
            .register()
    };
    let mut props = pw::properties::PropertiesBox::new();
    props.insert(*pw::keys::MEDIA_TYPE, "Video");
    props.insert(*pw::keys::MEDIA_CATEGORY, "Capture");
    props.insert(*pw::keys::MEDIA_ROLE, "Screen");
    // If the monitor's node goes away, the capture ends; it is not for
    // PipeWire to link us to whatever else produces video.
    props.insert(*pw::keys::NODE_DONT_RECONNECT, "true");
    let stream = pw::stream::StreamRc::new(core.clone(), "remdesktop", props).map_err(e("stream"))?;
    let buffers: Rc<RefCell<HashMap<usize, Arc<Mapping>>>> = Rc::default();

    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let (hub, mainloop) = (hub.clone(), mainloop.clone());
            move |_, _, old, new| match new {
                StreamState::Error(message) => {
                    hub.fail(format!("stream error: {message}"));
                    mainloop.quit();
                }
                StreamState::Unconnected if old != StreamState::Unconnected => {
                    hub.fail("the stream closed".into());
                    mainloop.quit();
                }
                _ => {}
            }
        })
        .param_changed({
            let hub = hub.clone();
            move |stream, _, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                match spa::param::format_utils::parse_format(param) {
                    Ok((spa::param::format::MediaType::Video, spa::param::format::MediaSubtype::Raw)) => {}
                    _ => return,
                }
                let mut info = VideoInfoRaw::new();
                if info.parse(param).is_err() {
                    hub.fail("the stream's format cannot be read".into());
                    return;
                }
                let Some(swap_rb) = swap_rb_of(info.format()) else {
                    hub.fail(format!("the stream's pixel format ({:?}) is not one we take", info.format()));
                    return;
                };
                let size = (info.size().width as usize, info.size().height as usize);
                hub.set_format(Negotiated { size, swap_rb });
                let bytes = buffers_pod();
                if let Some(pod) = Pod::from_bytes(&bytes) {
                    let _ = stream.update_params(&mut [pod]);
                }
            }
        })
        .add_buffer({
            let (hub, buffers) = (hub.clone(), buffers.clone());
            move |_, _, buffer| {
                let spa_buffer = unsafe { &*(*buffer).buffer };
                let data = (spa_buffer.n_datas > 0).then(|| unsafe { &*spa_buffer.datas });
                match data {
                    Some(d) if d.type_ == spa::sys::SPA_DATA_MemFd => match Mapping::new(d.fd as i32, d.mapoffset as usize, d.maxsize as usize) {
                        Ok(m) => {
                            buffers.borrow_mut().insert(buffer as usize, Arc::new(m));
                        }
                        Err(e) => hub.fail(e),
                    },
                    _ => hub.fail("the stream's buffers are not shared memory".into()),
                }
            }
        })
        .remove_buffer({
            let (hub, buffers) = (hub.clone(), buffers.clone());
            move |_, _, buffer| {
                buffers.borrow_mut().remove(&(buffer as usize));
                hub.discard_if(|f| f.buffer == buffer as usize);
            }
        })
        .process({
            let (hub, buffers) = (hub.clone(), buffers.clone());
            move |stream, _| {
                // Whatever is queued, only the newest is worth a look.
                let mut newest: *mut pw::sys::pw_buffer = std::ptr::null_mut();
                loop {
                    let b = unsafe { stream.dequeue_raw_buffer() };
                    if b.is_null() {
                        break;
                    }
                    if !newest.is_null() {
                        unsafe { stream.queue_raw_buffer(newest) };
                    }
                    newest = b;
                }
                if newest.is_null() {
                    return;
                }
                let mapping = buffers.borrow().get(&(newest as usize)).cloned();
                let frame = hub.format().zip(mapping).and_then(|(f, m)| unsafe { frame_of(newest, &m, f.size) });
                match frame.map(|f| hub.put(f)) {
                    Some(Some(old)) => unsafe { stream.queue_raw_buffer(old.buffer as *mut _) },
                    Some(None) => {}
                    None => unsafe { stream.queue_raw_buffer(newest) },
                }
            }
        })
        .register()
        .map_err(e("listener"))?;

    let bytes = format_pod(fixed, (size.0 as u32, size.1 as u32));
    let mut params = [Pod::from_bytes(&bytes).ok_or("format pod")?];
    stream.connect(spa::utils::Direction::Input, Some(node), StreamFlags::AUTOCONNECT, &mut params).map_err(e("connect stream"))?;

    let _messages = {
        let (stream, buffers, quit) = (stream.clone(), buffers.clone(), mainloop.clone());
        from_capture.attach(mainloop.loop_(), move |msg| match msg {
            Msg::Release(buffer, mapping) => {
                // Not if the buffer went away meanwhile (a new format).
                if buffers.borrow().get(&buffer).is_some_and(|m| Arc::ptr_eq(m, &mapping)) {
                    unsafe { stream.queue_raw_buffer(buffer as *mut _) };
                }
            }
            Msg::Quit => quit.quit(),
        })
    };
    mainloop.run();
    Ok(())
}

impl Capture {
    /// `logical` is the tablet's screen size as the user sees it (e.g.
    /// 1872x1404 when rotated 90°), which a virtual monitor takes as its size.
    pub fn start(source: &Source, logical: (u32, u32)) -> Result<Capture, String> {
        let err = |what: &'static str| move |e: zbus::Error| format!("{what}: {e}");
        let conn = Connection::session().map_err(err("session bus"))?;
        let screencast = Proxy::new(&conn, DEST, "/org/gnome/Mutter/ScreenCast", "org.gnome.Mutter.ScreenCast").map_err(err("ScreenCast"))?;
        let none: HashMap<&str, DbusValue> = HashMap::new();
        let session: OwnedObjectPath = screencast.call("CreateSession", &(none,)).map_err(err("CreateSession"))?;
        let sess = Proxy::new(&conn, DEST, session.clone(), "org.gnome.Mutter.ScreenCast.Session").map_err(err("session"))?;

        let layout = match source {
            Source::Virtual => Some(crate::displayconfig::snapshot(&conn)?),
            Source::Monitor(_) => None,
        };
        let mut props: HashMap<&str, DbusValue> = HashMap::new();
        props.insert("cursor-mode", DbusValue::U32(CURSOR_EMBEDDED));
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

        let hub = Arc::new(Hub::new());
        let (to_loop, from_capture) = pw::channel::channel();
        let fixed = matches!(source, Source::Virtual);
        let thread = {
            let hub = hub.clone();
            std::thread::Builder::new()
                .name("pipewire".into())
                .spawn(move || pipewire_thread(node, fixed, src_size, hub, from_capture))
                .map_err(|e| format!("PipeWire thread: {e}"))?
        };
        // From here on a failure drops the capture, which stops the thread and the session.
        let mut cap = Capture { virtual_name: None, _conn: conn, session, hub, to_loop, thread: Some(thread), held: None, src_size, swap_rb: false };
        // The virtual monitor exists once the stream is negotiated.
        let negotiated = cap.hub.wait_format(NEGOTIATION)?;
        cap.src_size = negotiated.size;
        cap.swap_rb = negotiated.swap_rb;
        if let Some(l) = &layout {
            match crate::displayconfig::keep_layout(&cap._conn, l, Duration::from_secs(5)) {
                Ok(name) => cap.virtual_name = Some(name),
                Err(e) => eprintln!("remdesktop-host: could not keep the monitor layout: {e}"),
            }
        }
        Ok(cap)
    }

    /// Waits up to `timeout` for a frame newer than the one held and makes it
    /// the held one: `frame` shows it, and the old one goes back to PipeWire.
    /// Woken the moment a frame arrives. Ok(false) if none came (Mutter only
    /// sends frames on change), Err when the stream has ended or changed
    /// format, after which the capture has to be started again.
    pub fn next_frame(&mut self, timeout: Duration) -> std::io::Result<bool> {
        match self.hub.take(timeout) {
            Ok(Some(frame)) => {
                self.release_held();
                self.held = Some(frame);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(std::io::Error::other(e)),
        }
    }

    /// The held frame as the shared memory holds it: rows of at least
    /// `src_size.0 * 4` bytes, `src_size.1` of them, valid until the next
    /// `next_frame`. Empty before the first.
    pub fn frame(&self) -> &[u8] {
        self.held.as_ref().map_or(&[], Frame::bytes)
    }

    fn release_held(&mut self) {
        if let Some(f) = self.held.take() {
            let _ = self.to_loop.send(Msg::Release(f.buffer, f.mapping));
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.to_loop.send(Msg::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Ok(sess) = Proxy::new(&self._conn, DEST, self.session.as_str(), "org.gnome.Mutter.ScreenCast.Session") {
            let _ = sess.call_method("Stop", &());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_size_from_printed_parameters() {
        assert_eq!(parse_size(r#"{"position": (1440, 1731), "size": (1920, 1080), "output-name": "eDP-1"}"#), Some((1920, 1080)));
        assert_eq!(parse_size(r#"[{"size": <(3440, 1440)>, "position": <(1440, 291)>}]"#), Some((3440, 1440)));
        assert_eq!(parse_size(r#"{"position": (0, 0)}"#), None);
        assert_eq!(parse_size(r#"{"size": (0, 0)}"#), None);
    }

    #[test]
    fn knows_the_byte_order_of_the_formats_it_takes() {
        assert_eq!(swap_rb_of(VideoFormat::BGRx), Some(false));
        assert_eq!(swap_rb_of(VideoFormat::BGRA), Some(false));
        assert_eq!(swap_rb_of(VideoFormat::RGBx), Some(true));
        assert_eq!(swap_rb_of(VideoFormat::RGBA), Some(true));
        assert_eq!(swap_rb_of(VideoFormat::I420), None);
        assert_eq!(swap_rb_of(VideoFormat::RGB), None);
    }

    #[test]
    fn the_format_and_buffer_pods_serialize() {
        assert!(Pod::from_bytes(&format_pod(true, (1872, 1404))).is_some());
        assert!(Pod::from_bytes(&format_pod(false, (1920, 1080))).is_some());
        assert!(Pod::from_bytes(&buffers_pod()).is_some());
    }

    #[test]
    fn the_hub_keeps_only_the_newest_frame() {
        let hub = Hub::new();
        assert_eq!(hub.put(1), None);
        assert_eq!(hub.put(2), Some(1));
        assert_eq!(hub.put(3), Some(2));
        assert_eq!(hub.take(Duration::ZERO), Ok(Some(3)));
        assert_eq!(hub.take(Duration::ZERO), Ok(None));
    }

    #[test]
    fn the_hub_wakes_the_reader_when_a_frame_arrives() {
        let hub = Arc::new(Hub::new());
        let writer = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                hub.put(7);
            })
        };
        let started = std::time::Instant::now();
        assert_eq!(hub.take(Duration::from_secs(5)), Ok(Some(7)));
        assert!(started.elapsed() < Duration::from_secs(1));
        writer.join().unwrap();
    }

    #[test]
    fn a_failure_ends_the_reading_and_the_first_error_stays() {
        let hub = Hub::new();
        hub.put(1);
        hub.fail("closed".into());
        hub.fail("later".into());
        assert_eq!(hub.take(Duration::ZERO), Err("closed".to_string()));
        assert_eq!(hub.wait_format(Duration::ZERO), Err("closed".to_string()));
    }

    #[test]
    fn a_new_format_after_the_first_is_an_error() {
        let hub: Hub<u32> = Hub::new();
        let a = Negotiated { size: (1920, 1080), swap_rb: false };
        hub.set_format(a);
        hub.set_format(a);
        assert_eq!(hub.wait_format(Duration::ZERO), Ok(a));
        hub.set_format(Negotiated { size: (1280, 720), swap_rb: false });
        assert!(hub.take(Duration::ZERO).is_err());
    }

    #[test]
    fn a_buffer_going_away_takes_its_waiting_frame_with_it() {
        let hub = Hub::new();
        hub.put(5);
        hub.discard_if(|&f| f == 6);
        assert_eq!(hub.take(Duration::ZERO), Ok(Some(5)));
        hub.put(5);
        hub.discard_if(|&f| f == 5);
        assert_eq!(hub.take(Duration::ZERO), Ok(None));
    }
}
