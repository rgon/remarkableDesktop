//! Taking the panel and inputs from xochitl, and handing them back.

use crate::epd::{Epd, Mode};
use crate::takeover::{self, Grabbed, Guardian};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    QUIT.store(true, Ordering::Relaxed);
}

fn catch_signals() {
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            libc::signal(sig, on_signal as *const () as libc::sighandler_t);
        }
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

pub fn quitting() -> bool {
    QUIT.load(Ordering::Relaxed)
}

pub struct Inputs {
    pub pen: Option<Grabbed>,
    pub touch: Option<Grabbed>,
    pub power: Option<Grabbed>,
}

pub struct Session {
    guardian: Guardian,
    pub inputs: Inputs,
    /// xochitl's last frame, when it could be located unambiguously.
    saved: Option<Vec<u8>>,
    started: Instant,
}

impl Session {
    /// Grabs inputs, pauses xochitl and starts the e-ink engine.
    /// `deadline_secs` (0 = none) bounds how long xochitl may stay paused.
    pub fn begin(deadline_secs: u64) -> Result<(Session, Epd), String> {
        let xochitl = takeover::xochitl_pid().ok_or("xochitl is not running")?;
        // No core dumps: this process holds a copy of xochitl's screen and the
        // session token, and cores here go to reMarkable's crash reporter.
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
        catch_signals();
        // Before Qt starts any threads.
        let guardian = Guardian::spawn(xochitl, deadline_secs).map_err(|e| format!("guardian: {e}"))?;

        let mut inputs = Inputs { pen: None, touch: None, power: None };
        for i in 0..3 {
            let path = format!("/dev/input/event{i}");
            let grabbed = Grabbed::open(&path).inspect(|g| {
                if let Err(e) = crate::input::use_monotonic_clock(&g.file) {
                    log!("remvnc: {path}: monotonic timestamps: {e}");
                }
            });
            match grabbed {
                Ok(g) if g.name.contains("Wacom") => inputs.pen = Some(g),
                Ok(g) if g.name.contains("pt_mt") => inputs.touch = Some(g),
                Ok(g) if g.name.contains("powerkey") => inputs.power = Some(g),
                Ok(g) => log!("remvnc: ignoring {} ({})", g.path, g.name),
                Err(e) => log!("remvnc: grab {path}: {e}"),
            }
        }

        takeover::signal(xochitl, libc::SIGSTOP).map_err(|e| format!("pause xochitl: {e}"))?;
        let started = Instant::now();
        takeover::private_tmp().map_err(|e| format!("private /tmp: {e}"))?;
        let epd = Epd::open()?;
        // The engine installs its own handlers and, on SIGTERM, tears itself
        // down at once, while we may still be restoring the screen through it.
        // Take the signals back: we quit through our loop, and the engine
        // shuts down at process exit.
        catch_signals();

        let (w, h) = (epd.width() as u32, epd.height() as u32);
        let saved = match takeover::learn_layout(epd.fb.pixels as usize, w, h, 32)
            .and_then(|layout| takeover::snapshot_images(xochitl, layout, w, h, 32, epd.fb.format as u32, epd.fb.len()))
        {
            Ok(mut found) if found.len() == 1 => Some(found.remove(0).2),
            // Copies of the same frame (xochitl keeps more than one) are all fine.
            Ok(mut found) if found.len() > 1 && found.iter().all(|f| f.2 == found[0].2) => {
                log!("remvnc: {} identical xochitl frames", found.len());
                Some(found.remove(0).2)
            }
            Ok(found) => {
                log!("remvnc: {} differing xochitl frames; will restart it on exit", found.len());
                // Diagnostics for telling the engine's frame apart next time.
                log!("remvnc:   ours   {:08x?}", takeover::own_header(epd.fb.pixels as usize, w, h, 32));
                for (addr, header, _) in &found {
                    log!("remvnc:   {addr:08x} {header:08x?}");
                }
                None
            }
            Err(e) => {
                log!("remvnc: snapshot: {e}; will restart xochitl on exit");
                None
            }
        };
        log!("remvnc: paused xochitl ({xochitl}), engine up in {:?}, {} buffers", started.elapsed(), epd.candidates.len());
        Ok((Session { guardian, inputs, saved, started }, epd))
    }

    /// Puts xochitl's frame back and exits; the guardian resumes xochitl
    /// once this process (and its engine) is gone.
    pub fn end(mut self, epd: &Epd) -> ! {
        if let Some(buf) = &self.saved {
            log!("remvnc: putting xochitl's frame back");
            unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), epd.fb.pixels, buf.len()) };
            epd.swap(0, 0, epd.width(), epd.height(), Mode::Content, 0);
            let end = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < end {
                epd.process_events(20);
                std::thread::sleep(Duration::from_millis(20));
            }
            self.guardian.restored();
            log!("remvnc: screen restored");
        }
        log!("remvnc: xochitl was paused for {:?}", self.started.elapsed());
        drop(self.inputs);
        // The vendor engine has no clean shutdown; let process exit stop it.
        std::process::exit(0);
    }
}
