//! E-ink output through the vendor engine in `libqsgepaper.so` (rM2, OS 3.28).
//!
//! Everything is resolved with `dlopen`/`dlsym` at runtime, so the binary only
//! links against glibc. The engine's framebuffer accessor is not exported, so
//! we interpose `QImage`'s external-buffer constructor and record the images
//! the engine creates while `EPFramebuffer::instance()` runs.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Qt's `QRect` layout: inclusive corners.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct QRect {
    x1: c_int,
    y1: c_int,
    x2: c_int,
    y2: c_int,
}

impl QRect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x1: x, y1: y, x2: x + w - 1, y2: y + h - 1 }
    }
}

/// `EPScreenMode`. Names come from the library's `EPScreenModeItem::Mode`
/// metadata; the numeric order is assumed to match its declaration order.
#[repr(i32)]
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Pen = 0,
    Mono = 1,
    Animation = 2,
    Ui = 3,
    Content = 4,
    Sleep = 5,
}

impl Mode {
    pub const ALL: [Mode; 6] = [Mode::Pen, Mode::Mono, Mode::Animation, Mode::Ui, Mode::Content, Mode::Sleep];
}

// QImage::Format values we know how to address.
pub const FORMAT_RGB32: i32 = 4;
pub const FORMAT_RGB16: i32 = 7;
pub const FORMAT_GRAYSCALE8: i32 = 24;

/// A pixel buffer handed to a `QImage` constructor.
#[derive(Clone, Copy, Debug)]
pub struct ImageInfo {
    pub pixels: *mut u8,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: i32,
}

unsafe impl Send for ImageInfo {}

impl ImageInfo {
    pub fn len(&self) -> usize {
        self.stride as usize * self.height as usize
    }

    /// Writes one gray level (0 = black, 255 = white). Caller bounds-checks.
    #[inline]
    pub unsafe fn set_gray(&self, x: i32, y: i32, v: u8) {
        let row = unsafe { self.pixels.add(y as usize * self.stride as usize) };
        match self.format {
            FORMAT_GRAYSCALE8 => unsafe { *row.add(x as usize) = v },
            FORMAT_RGB16 => {
                let px = ((v as u16 >> 3) << 11) | ((v as u16 >> 2) << 5) | (v as u16 >> 3);
                unsafe { (row.add(x as usize * 2) as *mut u16).write_unaligned(px) }
            }
            _ => {
                let px = 0xff00_0000 | (v as u32) << 16 | (v as u32) << 8 | v as u32;
                unsafe { (row.add(x as usize * 4) as *mut u32).write_unaligned(px) }
            }
        }
    }

}

static CAPTURE: AtomicBool = AtomicBool::new(false);
static CAPTURED: Mutex<Vec<ImageInfo>> = Mutex::new(Vec::new());

type Cleanup = Option<unsafe extern "C" fn(*mut c_void)>;
type ImageCtor =
    unsafe extern "C" fn(*mut c_void, *mut u8, c_int, c_int, c_int, c_int, Cleanup, *mut c_void);

unsafe fn forward_ctor(
    name: &CStr,
    slot: &OnceLock<usize>,
    this: *mut c_void,
    data: *mut u8,
    w: c_int,
    h: c_int,
    stride: c_int,
    format: c_int,
    cleanup: Cleanup,
    info: *mut c_void,
) {
    let real = *slot.get_or_init(|| {
        // From the executable, RTLD_NEXT lands on libQt6Gui's definition.
        let sym = unsafe { libc::dlvsym(libc::RTLD_NEXT, name.as_ptr(), c"Qt_6".as_ptr()) };
        if sym.is_null() {
            eprintln!("remvnc: cannot resolve {name:?}");
            std::process::abort();
        }
        sym as usize
    });
    let real: ImageCtor = unsafe { std::mem::transmute(real) };
    unsafe { real(this, data, w, h, stride, format, cleanup, info) };
    if CAPTURE.load(Ordering::Acquire) {
        let image = ImageInfo { pixels: data, width: w, height: h, stride, format };
        CAPTURED.lock().unwrap().push(image);
    }
}

// QImage(uchar*, int w, int h, qsizetype bytesPerLine, Format, QImageCleanupFunction, void*)
// on 32-bit ARM, where qsizetype is int. Both the complete- and base-object
// constructor aliases are provided, since callers may bind either.
#[unsafe(export_name = "_ZN6QImageC1EPhiiiNS_6FormatEPFvPvES2_")]
pub unsafe extern "C" fn qimage_ctor_c1(
    this: *mut c_void, data: *mut u8, w: c_int, h: c_int, stride: c_int, format: c_int,
    cleanup: Cleanup, info: *mut c_void,
) {
    static REAL: OnceLock<usize> = OnceLock::new();
    unsafe {
        forward_ctor(c"_ZN6QImageC1EPhiiiNS_6FormatEPFvPvES2_", &REAL, this, data, w, h, stride, format, cleanup, info)
    }
}

#[unsafe(export_name = "_ZN6QImageC2EPhiiiNS_6FormatEPFvPvES2_")]
pub unsafe extern "C" fn qimage_ctor_c2(
    this: *mut c_void, data: *mut u8, w: c_int, h: c_int, stride: c_int, format: c_int,
    cleanup: Cleanup, info: *mut c_void,
) {
    static REAL: OnceLock<usize> = OnceLock::new();
    unsafe {
        forward_ctor(c"_ZN6QImageC2EPhiiiNS_6FormatEPFvPvES2_", &REAL, this, data, w, h, stride, format, cleanup, info)
    }
}

type SwapFn = unsafe extern "C" fn(*mut c_void, QRect, c_int, c_int);
type ProcessEventsFn = unsafe extern "C" fn(c_int, c_int);
type AppCtor = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut *mut c_char, c_int);

const QT_VERSION: c_int = 0x060a03;

fn dlopen(path: &CStr) -> Result<*mut c_void, String> {
    let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if h.is_null() { Err(format!("dlopen {path:?}: {}", dlerror())) } else { Ok(h) }
}

fn dlsym(handle: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
    let s = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if s.is_null() { Err(format!("missing symbol {name:?} (firmware changed?)")) } else { Ok(s) }
}

fn dlerror() -> String {
    let e = unsafe { libc::dlerror() };
    if e.is_null() { "unknown".into() } else { unsafe { CStr::from_ptr(e) }.to_string_lossy().into() }
}

pub struct Epd {
    pub instance: *mut c_void,
    swap_fn: SwapFn,
    process_events_fn: ProcessEventsFn,
    /// Every external-buffer image the engine created during start-up.
    pub candidates: Vec<ImageInfo>,
    /// The image we draw into.
    pub fb: ImageInfo,
}

impl Epd {
    /// Starts a bare `QCoreApplication` and the vendor e-ink engine.
    /// Must be called from the main thread, once.
    pub fn open() -> Result<Self, String> {
        let core = dlopen(c"libQt6Core.so.6")?;
        dlopen(c"libQt6Gui.so.6")?;

        let app_ctor: AppCtor = unsafe { std::mem::transmute(dlsym(core, c"_ZN16QCoreApplicationC1ERiPPci")?) };
        let process_events_fn: ProcessEventsFn = unsafe {
            std::mem::transmute(dlsym(core, c"_ZN16QCoreApplication13processEventsE6QFlagsIN10QEventLoop17ProcessEventsFlagEEi")?)
        };
        // Qt keeps references to argc/argv and the object for the process lifetime.
        let argc: &'static mut c_int = Box::leak(Box::new(1));
        let argv: &'static mut [*mut c_char; 2] = Box::leak(Box::new([c"remvnc".as_ptr() as *mut c_char, std::ptr::null_mut()]));
        let app: &'static mut [u64; 32] = Box::leak(Box::new([0; 32]));
        unsafe { app_ctor(app.as_mut_ptr().cast(), argc, argv.as_mut_ptr(), QT_VERSION) };

        let epaper = dlopen(c"/usr/lib/plugins/scenegraph/libqsgepaper.so")?;
        let instance_fn: unsafe extern "C" fn() -> *mut c_void =
            unsafe { std::mem::transmute(dlsym(epaper, c"_ZN13EPFramebuffer8instanceEv")?) };
        let swap_fn: SwapFn = unsafe {
            std::mem::transmute(dlsym(epaper, c"_ZN13EPFramebuffer11swapBuffersE5QRect12EPScreenMode6QFlagsINS_10UpdateFlagEE")?)
        };

        CAPTURE.store(true, Ordering::Release);
        let instance = unsafe { instance_fn() };
        for _ in 0..10 {
            unsafe { process_events_fn(0, 10) };
        }
        CAPTURE.store(false, Ordering::Release);
        if instance.is_null() {
            return Err("EPFramebuffer::instance() returned null".into());
        }

        let candidates = std::mem::take(&mut *CAPTURED.lock().unwrap());
        // The engine converts the RGB32 image into its Grayscale8 one on every
        // swap, so RGB32 is the one to draw into.
        let fb = candidates
            .iter()
            .find(|c| !c.pixels.is_null() && c.format == FORMAT_RGB32 && c.width >= 1000 && c.height >= 1000)
            .copied()
            .ok_or_else(|| format!("no screen-sized RGB32 framebuffer among {candidates:?}"))?;

        Ok(Self { instance, swap_fn, process_events_fn, candidates, fb })
    }

    pub fn width(&self) -> i32 {
        self.fb.width
    }

    pub fn height(&self) -> i32 {
        self.fb.height
    }

    /// Pushes a region of the framebuffer to the panel.
    pub fn swap(&self, x: i32, y: i32, w: i32, h: i32, mode: Mode, flags: i32) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.fb.width);
        let y1 = (y + h).min(self.fb.height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        unsafe { (self.swap_fn)(self.instance, QRect::new(x0, y0, x1 - x0, y1 - y0), mode as c_int, flags) };
    }

    pub fn process_events(&self, max_ms: i32) {
        unsafe { (self.process_events_fn)(0, max_ms) };
    }

}
