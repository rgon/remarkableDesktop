mod epd;
mod gfx;
mod takeover;

use embedded_graphics::mono_font::{MonoTextStyle, ascii::FONT_10X20};
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, PrimitiveStyleBuilder, Rectangle};
use embedded_graphics::text::Text;
use epd::{Epd, Mode};
use gfx::{Canvas, Scaled};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use takeover::{Grabbed, Guardian};

static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    QUIT.store(true, Ordering::Relaxed);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("guardian") if args.len() == 5 => {
            let n = |i: usize| args[i].parse().expect("guardian: bad argument");
            takeover::run_guardian(n(2) as i32, n(3), n(4) as i32);
        }
        Some("restore") => takeover::manual_restore(),
        Some("probe") => {
            if let Err(e) = probe() {
                eprintln!("remvnc: {e}");
                std::process::exit(1);
            }
        }
        _ => eprintln!("usage: remvnc probe | restore"),
    }
}

/// Pumps Qt events for `ms`; false if we were asked to quit.
fn wait(epd: &Epd, ms: u64) -> bool {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        if QUIT.load(Ordering::Relaxed) {
            return false;
        }
        epd.process_events(20);
        std::thread::sleep(Duration::from_millis(10));
    }
    !QUIT.load(Ordering::Relaxed)
}

fn watchdog_ts() -> String {
    Command::new("systemctl")
        .args(["show", "xochitl", "-p", "WatchdogTimestampMonotonic", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn write_pgm(path: &str, w: i32, h: i32, gray: &[u8]) {
    let mut data = format!("P5\n{w} {h}\n255\n").into_bytes();
    data.extend_from_slice(gray);
    if let Err(e) = std::fs::write(path, data) {
        eprintln!("remvnc: write {path}: {e}");
    }
}

fn probe() -> Result<(), String> {
    let xpid = takeover::xochitl_pid().ok_or("xochitl is not running")?;
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
    let mut guardian = Guardian::spawn(xpid, 50).map_err(|e| format!("guardian: {e}"))?;
    eprintln!("remvnc: xochitl pid {xpid}, guardian pid {}", guardian.id());

    let mut inputs = vec![];
    for i in 0..3 {
        let path = format!("/dev/input/event{i}");
        match Grabbed::open(&path) {
            Ok(g) => {
                eprintln!("remvnc: grabbed {} ({})", g.path, g.name);
                inputs.push(g);
            }
            Err(e) => eprintln!("remvnc: grab {path}: {e}"),
        }
    }

    let wd_before = watchdog_ts();
    takeover::signal(xpid, libc::SIGSTOP).map_err(|e| format!("pause xochitl: {e}"))?;
    let t0 = Instant::now();
    takeover::private_tmp().map_err(|e| format!("private /tmp: {e}"))?;
    let epd = Epd::open()?;
    eprintln!("remvnc: engine up in {:?}", t0.elapsed());
    for c in &epd.candidates {
        eprintln!("remvnc: candidate {c:?}");
    }
    eprintln!("remvnc: drawing into {:?}", epd.fb);

    let dir = "/home/root/remvnc-probe";
    let _ = std::fs::create_dir_all(dir);
    let (w, h) = (epd.width(), epd.height());
    let ours = unsafe { std::slice::from_raw_parts(epd.fb.pixels, epd.fb.len()) };
    write_pgm(&format!("{dir}/ours-initial.pgm"), w, h, &epd.fb.to_gray(ours));
    // xochitl's own copy of the screen, found by its QImageData header.
    let t = Instant::now();
    let saved = takeover::learn_layout(epd.fb.pixels as usize, w as u32, h as u32, 32)
        .and_then(|layout| takeover::snapshot_images(xpid, layout, w as u32, h as u32, 32, epd.fb.len()))
        .unwrap_or_else(|e| {
            eprintln!("remvnc: snapshot: {e}");
            vec![]
        });
    for (i, (addr, buf)) in saved.iter().enumerate() {
        eprintln!("remvnc: xochitl screen image {i} at {addr:#x}");
        write_pgm(&format!("{dir}/xochitl-{i}.pgm"), w, h, &epd.fb.to_gray(buf));
    }
    eprintln!("remvnc: snapshot scan took {:?}", t.elapsed());

    let text = MonoTextStyle::new(&FONT_10X20, Gray8::BLACK);
    let panel = PrimitiveStyleBuilder::new().fill_color(Gray8::WHITE).stroke_color(Gray8::BLACK).stroke_width(6).build();
    let mut ok = true;
    let mut canvas = Canvas::new(&epd, vec![epd.fb]);

    for (i, mode) in Mode::ALL.into_iter().enumerate() {
        if !ok {
            break;
        }
        let y = 360 + i as i32 * 200;
        let _ = Rectangle::new(Point::new(100, y), Size::new(1204, 160)).into_styled(panel).draw(&mut canvas);
        let _ = Rectangle::new(Point::new(1000, y + 30), Size::new(250, 100))
            .into_styled(PrimitiveStyle::with_fill(Gray8::new(0x80)))
            .draw(&mut canvas);
        let label = format!("{i} {mode:?}");
        let _ = Text::new(&label, Point::new(15, y / 3 + 35), text).draw(&mut Scaled { target: &mut canvas, factor: 3 });
        let t = Instant::now();
        canvas.flush(mode);
        eprintln!("remvnc: swap {mode:?} returned in {:?}", t.elapsed());
        ok = wait(&epd, 1200);
    }

    if ok {
        let _ = Rectangle::new(Point::new(100, 1600), Size::new(1204, 160)).into_styled(panel).draw(&mut canvas);
        let _ = Text::new("TOUCH + PEN NOW", Point::new(40, 1600 / 4 + 25), text).draw(&mut Scaled { target: &mut canvas, factor: 4 });
        canvas.flush(Mode::Ui);
        eprintln!("remvnc: touch the screen / use the pen for 8 s");
        let end = Instant::now() + Duration::from_secs(8);
        let mut counts = vec![0usize; inputs.len()];
        while ok && Instant::now() < end {
            let mut fds: Vec<libc::pollfd> =
                inputs.iter().map(|g| libc::pollfd { fd: g.file.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 100) };
            for (i, g) in inputs.iter_mut().enumerate() {
                let mut buf = [0u8; 16 * 64];
                while let Ok(n) = g.file.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    for ev in buf[..n].chunks_exact(16) {
                        let (ty, code) = (u16::from_le_bytes([ev[8], ev[9]]), u16::from_le_bytes([ev[10], ev[11]]));
                        let value = i32::from_le_bytes([ev[12], ev[13], ev[14], ev[15]]);
                        if counts[i] < 12 && ty != 0 {
                            eprintln!("remvnc:   {} type {ty} code {code} value {value}", g.name);
                        }
                        counts[i] += 1;
                    }
                }
            }
            ok = !QUIT.load(Ordering::Relaxed);
        }
        for (g, c) in inputs.iter().zip(&counts) {
            eprintln!("remvnc: {} -> {c} events", g.name);
        }
    }

    let wd_after = watchdog_ts();
    eprintln!("remvnc: xochitl watchdog timestamp {wd_before} -> {wd_after} ({})",
        if wd_before != wd_after { "petting works" } else { "NOT updated" });

    // Put xochitl's last frame back so its idea of the panel matches reality.
    // Only restore when the match is unambiguous.
    if let [(_, buf)] = &saved[..] {
        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), epd.fb.pixels, buf.len()) };
        epd.swap(0, 0, w, h, Mode::Content, 0);
        epd.process_events(20);
        std::thread::sleep(Duration::from_millis(1500));
        epd.process_events(20);
        guardian.restored();
        eprintln!("remvnc: screen restored, xochitl resumes on exit");
    } else {
        eprintln!("remvnc: no snapshot; guardian will restart xochitl");
    }
    eprintln!("remvnc: paused xochitl for {:?}", t0.elapsed());
    drop(inputs);
    // Exit without unwinding Qt: the vendor engine has no clean shutdown path.
    std::process::exit(0);
}
