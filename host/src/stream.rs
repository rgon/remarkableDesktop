//! Streams a captured screen to the tablet: only the changed parts of the
//! newest frame (as a few rectangles found on the GPU, smallest first, down
//! to 8x8 cells; the 32x32 tile grid when the GPU has no answer). Every captured frame
//! is rendered at once, so the newest picture is always ready, and it goes
//! out the moment the tablet can take it: one update may be in flight and
//! one more sent behind it while the tablet is quick to acknowledge, and
//! never more, so no queue (and no latency) builds up on the slow e-ink
//! side. Tiles that then stay still are sent again for the tablet's quality
//! refresh, which clears the fast mode's ghosting; the host decides which
//! tiles and when, as it knows what changed. A refresh like that never goes
//! in front of a changed frame.

use crate::capture::{Capture, Source};
use crate::ghost::{self, Budget, Drive, Kind};
use crate::gpu::{Convert, DARK, Frame, Gpu};
use crate::tiles::{self, Grid};
use remdesktop_proto::{DisplayMode, Msg, RECT_BITS1, RECT_LAST, RECT_SETTLE, bits1, gray4};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Updates that may be sent and not yet acknowledged.
const MAX_IN_FLIGHT: usize = 2;
/// A second update goes out behind one in flight only if the tablet's
/// last acknowledgement came this quickly (the panel keeps up), or if the
/// one in flight is a quality refresh, which can hold the tablet for a long
/// time: a slow panel gets the newest frame once it is free instead of an
/// update that would wait, stale, behind the one being refreshed.
const PIPELINE_ACK_MAX: Duration = Duration::from_millis(40);
/// A quality refresh that is due and has waited this much longer goes out
/// even though a changed frame is waiting (while scrolling one always is).
const SETTLE_GRACE: Duration = Duration::from_millis(300);
/// Whether a quality refresh covers the boxes the GPU draws around the cells
/// with ghosting error (a thin trail is refreshed by a thin box) instead of
/// whole 32x32 tiles. The tiles still decide which tiles, when and how many
/// (error sums, quiet time, budget); the boxes only tighten them
/// (`Grid::rects_within`). False: whole tiles, as before.
const SETTLE_BY_BOXES: bool = true;
/// How often the sender looks again while the ghosting model has not yet
/// taken in the updates sent (it normally notifies as soon as it has).
const MODEL_WAIT: Duration = Duration::from_millis(30);

/// An update sent and not yet acknowledged.
struct Flight {
    /// Id of its last rectangle, the one that is acked.
    id: u32,
    sent: Instant,
    settle: bool,
    /// For the latency stats (None for a settle): when its frame was
    /// captured, how old the frame was when sent, and its size in bytes.
    report: Option<(Instant, Duration, usize)>,
}

#[derive(Default)]
struct Shared {
    /// What to capture; `generation` bumps on every change.
    source: Option<Source>,
    /// Where the picture sits on the tablet (fractions of the logical
    /// screen); the rest is letterbox, painted white.
    shown: [f64; 4],
    generation: u64,
    /// What the tablet shows. Capture keeps running while it is `None`
    /// (stopping it would remove the virtual monitor); nothing is sent.
    mode: DisplayMode,
    /// Bumps whenever the tablet asks for a mode, even the same one: the next frame is a whole one.
    look: u64,
    /// Newest frame as gray levels, the epoch it belongs to (it changes with
    /// the source and the mode, and the next update is then a whole frame),
    /// when it was read from the capture pipeline, and the mode it was
    /// rendered for.
    frame: Option<Published>,
    /// Sequence number of the frame the sender took last.
    sent_seq: u64,
    /// `frame` is newer than what the sender took.
    fresh: bool,
    /// The updates sent and not yet acked, oldest first.
    in_flight: VecDeque<Flight>,
    /// How long the tablet took to ack the update acked last.
    last_ack: Duration,
    /// Set when a capture (re)started; carries the virtual monitor's name.
    started: Option<Option<String>>,
    stop: bool,
    stats: Option<DisplayStats>,
    /// The ghosting model (ghost.rs): the updates sent that it has yet to
    /// take in (`pushed` counts those the sender made, `applied` those the
    /// GPU thread took in), and its error per tile once `applied` has caught
    /// up with `pushed`, as of the newest frame.
    drives: VecDeque<Sent>,
    pushed: u64,
    applied: u64,
    ghost_err: Option<Vec<u32>>,
    /// Boxes around the cells with ghosting error, from the same pass.
    ghost_boxes: Option<Vec<tiles::Rect>>,
}

/// An update that went to the tablet, for the ghosting model: what it did
/// to which 8x8 cells, and the picture it carried (None: the last one given).
struct Sent {
    drive: Drive,
    picture: Option<Picture>,
    mask: Vec<bool>,
}

impl Shared {
    /// Whether another update may go out now.
    fn may_send(&self) -> bool {
        match self.in_flight.len() {
            0 => true,
            n if n < MAX_IN_FLIGHT => self.last_ack <= PIPELINE_ACK_MAX || self.in_flight.iter().any(|f| f.settle),
            _ => false,
        }
    }

    /// The tablet has the update whose last rectangle is `id` (and so all
    /// before it). An id that is not in flight (a late ack, after the
    /// source or mode changed) is ignored.
    fn acked(&mut self, id: u32) {
        let Some(at) = self.in_flight.iter().position(|f| f.id == id) else { return };
        for f in self.in_flight.drain(..=at) {
            self.last_ack = f.sent.elapsed();
            if let (Some(st), Some((captured, age, bytes))) = (&mut self.stats, f.report) {
                st.record(captured.elapsed(), bytes, age, f.sent.elapsed());
            }
        }
    }
}

/// What the GPU makes of a frame: the picture, whether it came from the GPU,
/// the tiles and the boxes (8x8 cells) that changed, and the ghosting
/// model's error per tile and its boxes.
type Rendered = (Picture, bool, Option<Vec<bool>>, Option<Vec<tiles::Rect>>, Option<Vec<u32>>, Option<Vec<tiles::Rect>>);

/// A rendered frame as the GPU packs it: 4-bit gray levels, two pixels a
/// byte (`gray4`'s layout), or, for the edge view, one bit per pixel
/// (`bits1`'s layout). Rows are padded to whole bytes.
#[derive(Clone)]
enum Picture {
    Gray4(Arc<Vec<u8>>),
    Bits(Arc<Vec<u8>>),
}

impl Picture {
    fn kind(&self) -> Kind {
        match self {
            Picture::Gray4(_) => Kind::Gray4,
            Picture::Bits(_) => Kind::Bits,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Picture::Gray4(v) | Picture::Bits(v) => v,
        }
    }

    fn same(&self, other: &Picture) -> bool {
        match (self, other) {
            (Picture::Gray4(a), Picture::Gray4(b)) | (Picture::Bits(a), Picture::Bits(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Whether it has the size of a `w`×`h` frame.
    fn fits(&self, w: usize, h: usize) -> bool {
        match self {
            Picture::Gray4(v) => v.len() == gray4::row_bytes(w) * h,
            Picture::Bits(v) => v.len() == bits1::row_bytes(w) * h,
        }
    }

    /// The tiles that differ from `prev` (all of them if it is another kind).
    fn dirty(&self, prev: &Picture, grid: &Grid) -> Vec<bool> {
        match (self, prev) {
            (Picture::Gray4(a), Picture::Gray4(b)) => grid.dirty_packed(b, a, 2),
            (Picture::Bits(a), Picture::Bits(b)) => grid.dirty_packed(b, a, 8),
            _ => vec![true; grid.tiles()],
        }
    }

    /// The `rw`×`rh` window at (`x`, `y`) of a `w`-wide frame as it goes on
    /// the wire, and whether that is 1-bit (`RECT_BITS1`). Rectangles start
    /// on cell boundaries (a multiple of 8 pixels), so a window starts on a
    /// byte of the packed rows; its width may be anything.
    /// It is written into `out`, which is reused.
    fn pack(&self, w: usize, x: usize, y: usize, rw: usize, rh: usize, out: &mut Vec<u8>) -> bool {
        let (bits, frame, per_byte) = match self {
            Picture::Gray4(v) => (false, v, 2),
            Picture::Bits(v) => (true, v, 8),
        };
        debug_assert!(x % per_byte == 0);
        let (rb, wb) = (w.div_ceil(per_byte), rw.div_ceil(per_byte));
        out.clear();
        for r in 0..rh {
            out.extend_from_slice(&frame[(y + r) * rb + x / per_byte..][..wb]);
        }
        bits
    }
}

/// A frame as the capture thread hands it over.
#[derive(Clone)]
struct Published {
    epoch: u64,
    picture: Picture,
    captured: Instant,
    /// Counts the frames made.
    seq: u64,
    /// The tiles that differ from frame number `.0`, as the GPU found out
    /// (known only when frames came out of it one after the other).
    dirty: Option<(u64, Vec<bool>)>,
    /// Boxes that cover what differs from frame number `.0`, found on the
    /// GPU in 8x8 cells (see `Gpu::gray_frame`); the same frames as `dirty`
    /// would be, and good for any frame sent at or after that one.
    boxes: Option<(u64, Vec<tiles::Rect>)>,
}

/// Capture → tablet-ack latency of screen updates, reported every 2 s.
#[derive(Default)]
struct DisplayStats {
    since: Option<Instant>,
    latencies_ms: Vec<f64>,
    /// Capture → sent, and sent → ack: where the latency goes.
    ages_ms: Vec<f64>,
    acks_ms: Vec<f64>,
    bytes: usize,
    pixels: usize,
}

impl DisplayStats {
    fn record(&mut self, latency: Duration, bytes: usize, age: Duration, ack: Duration) {
        let since = *self.since.get_or_insert_with(Instant::now);
        self.latencies_ms.push(latency.as_secs_f64() * 1000.0);
        self.ages_ms.push(age.as_secs_f64() * 1000.0);
        self.acks_ms.push(ack.as_secs_f64() * 1000.0);
        self.bytes += bytes;
        if since.elapsed() >= Duration::from_secs(2) {
            let n = self.latencies_ms.len();
            let pct = |v: &mut Vec<f64>, p: f64| {
                v.sort_by(|a, b| a.total_cmp(b));
                v[((n - 1) as f64 * p) as usize]
            };
            eprintln!(
                "remdesktop-host: display {:.1}/s, {:.0} KB/update ({:.0} kpx), capture→ack p50 {:.1} p99 {:.1} max {:.1} ms (capture→sent p50 {:.1}, sent→ack p50 {:.1} p99 {:.1})",
                n as f64 / since.elapsed().as_secs_f64(),
                self.bytes as f64 / n as f64 / 1024.0,
                self.pixels as f64 / n as f64 / 1000.0,
                pct(&mut self.latencies_ms, 0.5),
                pct(&mut self.latencies_ms, 0.99),
                pct(&mut self.latencies_ms, 1.0),
                pct(&mut self.ages_ms, 0.5),
                pct(&mut self.acks_ms, 0.5),
                pct(&mut self.acks_ms, 0.99),
            );
            *self = DisplayStats::default();
        }
    }
}

/// When each tile first changed since its last quality refresh, and when it
/// changed last. Without the ghosting model a tile is due `settle_after`
/// from that first change, whether or not it keeps changing: content that
/// moves all the time (scrolling) still gets the refresh that clears the fast
/// mode's ghosting, at most once per `settle_after` per tile, instead of
/// only after it has been still.
struct Unsettled {
    first: Vec<Option<Instant>>,
    last: Vec<Option<Instant>>,
}

impl Unsettled {
    fn new(tiles: usize) -> Self {
        Unsettled { first: vec![None; tiles], last: vec![None; tiles] }
    }

    /// The marked tiles changed (and sent) at `now`.
    fn touch(&mut self, changed: &[bool], now: Instant) {
        for (t, _) in changed.iter().enumerate().filter(|(_, c)| **c) {
            self.first[t].get_or_insert(now);
            self.last[t] = Some(now);
        }
    }

    /// When the first tile is due.
    fn next_due(&self, settle_after: Duration) -> Option<Instant> {
        self.first.iter().flatten().min().map(|t| *t + settle_after)
    }

    /// The tiles due at `now`.
    fn due(&self, now: Instant, settle_after: Duration) -> Vec<bool> {
        self.first.iter().map(|t| t.is_some_and(|t| now >= t + settle_after)).collect()
    }

    /// The marked tiles got their refresh; a later change starts the wait anew.
    fn done(&mut self, due: &[bool]) {
        for (t, _) in due.iter().enumerate().filter(|(_, d)| **d) {
            self.first[t] = None;
            self.last[t] = None;
        }
    }

    fn any(&self) -> bool {
        self.first.iter().any(|t| t.is_some())
    }

    fn clear(&mut self) {
        self.first.fill(None);
        self.last.fill(None);
    }
}

/// What the sender does about quality refreshes next.
#[derive(Debug, PartialEq)]
enum Plan {
    /// Refresh these tiles now.
    Send(Vec<bool>),
    /// Nothing yet; look again then (None: when something changes).
    Wait(Option<Instant>),
}

/// The timer alone: the tiles due, or when the first will be.
fn plan_timer(u: &Unsettled, now: Instant, settle_after: Duration) -> Plan {
    let due = u.due(now, settle_after);
    if due.iter().any(|&d| d) { Plan::Send(due) } else { Plan::Wait(u.next_due(settle_after)) }
}

/// With the ghosting model: `err` is its error per tile. A tile that has
/// changed since its last refresh is refreshed when its error reaches
/// `strong` and it has been still for `ghost::QUIET`; the timer is the upper
/// bound: a tile due by the timer is refreshed if any visible error is
/// left (`TILE_ERR_WEAK`), whether or not it is still, and dropped if not.
/// The tiles with the largest error go first, as many as `budget` allows.
fn plan_ghost(u: &mut Unsettled, budget: &mut Budget, err: &[u32], strong: u32, now: Instant, settle_after: Duration) -> Plan {
    let mut candidates = vec![false; u.first.len()];
    let mut wake: Option<Instant> = None;
    let mut note = |t: Instant| wake = Some(wake.map_or(t, |w| w.min(t)));
    for t in 0..candidates.len() {
        let Some(first) = u.first[t] else { continue };
        let timer_due = now >= first + settle_after;
        if timer_due && err[t] < ghost::TILE_ERR_WEAK {
            u.first[t] = None;
            u.last[t] = None;
            continue;
        }
        if !timer_due {
            note(first + settle_after);
            if err[t] < strong {
                continue;
            }
            let still_at = u.last[t].unwrap_or(first) + ghost::QUIET;
            if now < still_at {
                note(still_at);
                continue;
            }
        }
        candidates[t] = true;
    }
    if !candidates.iter().any(|&c| c) {
        return Plan::Wait(wake);
    }
    let room = budget.available(now);
    if room == 0 {
        note(now + budget.until_one());
        return Plan::Wait(wake);
    }
    let chosen = ghost::pick(err, &candidates, room);
    budget.spend(chosen.iter().filter(|&&c| c).count());
    Plan::Send(chosen)
}

/// The sender thread: takes the newest frame whenever the tablet can take
/// another update, and sends the tiles that changed since the frame the
/// tablet got last; when nothing newer is waiting, it sends the quality
/// refresh of tiles that have been still for `settle_after`.
fn run_sender(shared: Arc<(Mutex<Shared>, Condvar)>, logical: (u32, u32), rotation: u16, settle_after: Duration, ghost: bool, mut send: Send) {
    let (lock, cv) = &*shared;
    let (pw, ph) = if rotation % 180 == 90 { (logical.1, logical.0) } else { logical };
    let (w, h) = (pw as usize, ph as usize);
    let grid = Grid::new(w, h);
    // The epoch, the content and the sequence number of the frame sent last.
    let mut last: Option<(u64, Picture, u64)> = None;
    let mut unsettled = Unsettled::new(grid.tiles());
    let mut budget = Budget::new(Instant::now());
    // Tells the ghosting model what an update did, once it has been sent.
    // Updates that belong together (a reset and the frame after it) go in at once.
    let push = |sent: Vec<Sent>| {
        if ghost {
            let mut s = lock.lock().unwrap();
            s.pushed += sent.len() as u64;
            s.drives.extend(sent);
            cv.notify_all();
        }
    };
    let mut id = 0u32;
    // A rectangle's pixels, and its compressed form, reused between rectangles.
    let (mut packed, mut compressed) = (vec![], vec![]);
    // Sends the rectangles of one update, smallest first (so a pen or
    // cursor reaches the panel before bulk changes); the last one is
    // acked. `captured` is when the frame was read, for the latency
    // stats (None: not reported). False if the link broke.
    let mut send_rects = |rects: &[tiles::Rect], frame: &Picture, flags: u8, captured: Option<Instant>| -> bool {
        let (mut pixels, mut bytes) = (0, 0);
        let msgs: Vec<Msg> = rects
            .iter()
            .enumerate()
            .map(|(i, &(x, y, rw, rh))| {
                let bits = frame.pack(w, x, y, rw, rh, &mut packed);
                let flags = if bits { flags | RECT_BITS1 } else { flags };
                let flags = if i + 1 == rects.len() { flags | RECT_LAST } else { flags };
                // lz4_flex's size-prepended format, compressed into a reused buffer.
                compressed.resize(4 + lz4_flex::block::get_maximum_output_size(packed.len()), 0);
                compressed[..4].copy_from_slice(&(packed.len() as u32).to_le_bytes());
                let n = lz4_flex::block::compress_into(&packed, &mut compressed[4..]).expect("the buffer holds the worst case");
                let data = compressed[..4 + n].to_vec();
                id = id.wrapping_add(1);
                pixels += rw * rh;
                bytes += data.len();
                Msg::Rect { id, x: x as u16, y: y as u16, w: rw as u16, h: rh as u16, flags, data }
            })
            .collect();
        {
            let mut s = lock.lock().unwrap();
            // Before the first byte goes out, so a quick ack cannot beat it.
            s.in_flight.push_back(Flight { id, sent: Instant::now(), settle: flags & RECT_SETTLE != 0, report: captured.map(|c| (c, c.elapsed(), bytes)) });
            if let (Some(st), Some(_)) = (&mut s.stats, captured) {
                st.pixels += pixels;
            }
        }
        msgs.iter().all(|m| send(m).is_ok())
    };
    loop {
        enum Work {
            Frame(Published),
            Settle(Vec<bool>),
        }
        let work = {
            let mut s = lock.lock().unwrap();
            loop {
                if s.stop {
                    return;
                }
                // A refresh that is overdue goes first, or a constantly
                // changing screen (scrolling) would never get one.
                let overdue = last.is_some()
                    && s.mode != DisplayMode::None
                    && unsettled.next_due(settle_after).is_some_and(|t| Instant::now() >= t + SETTLE_GRACE);
                if overdue && s.may_send() {
                    break Work::Settle(unsettled.due(Instant::now(), settle_after));
                }
                if s.fresh && s.may_send() {
                    s.fresh = false;
                    if s.mode == DisplayMode::None {
                        // Nothing is sent; whatever comes after starts from a whole frame.
                        last = None;
                        unsettled.clear();
                        continue;
                    }
                    let frame = s.frame.clone().unwrap();
                    s.sent_seq = frame.seq;
                    break Work::Frame(frame);
                }
                // A quality refresh waits for the tablet to be idle and
                // for any changed frame to go first: it never delays one.
                let idle = s.in_flight.is_empty() && !s.fresh;
                let now = Instant::now();
                if !idle {
                    s = cv.wait(s).unwrap();
                    continue;
                }
                let plan = match &s.ghost_err {
                    None if !ghost => plan_timer(&unsettled, now, settle_after),
                    // The model has the newest frame and every update sent so far.
                    Some(err) if ghost && s.pushed == s.applied && err.len() == grid.tiles() => {
                        let strong = last.as_ref().map_or(ghost::TILE_ERR, |l| l.1.kind().tile_err());
                        plan_ghost(&mut unsettled, &mut budget, err, strong, now, settle_after)
                    }
                    // It is catching up: ask again shortly.
                    _ if ghost => Plan::Wait(unsettled.any().then(|| now + MODEL_WAIT)),
                    _ => plan_timer(&unsettled, now, settle_after),
                };
                match plan {
                    Plan::Send(tiles) => break Work::Settle(tiles),
                    Plan::Wait(Some(t)) => s = cv.wait_timeout(s, t.saturating_duration_since(now)).unwrap().0,
                    Plan::Wait(None) => s = cv.wait(s).unwrap(),
                }
            }
        };
        match work {
            Work::Frame(Published { epoch: generation, picture: frame, captured, seq, dirty: gpu_dirty, boxes: gpu_boxes }) => {
                if !frame.fits(w, h) {
                    continue;
                }
                // A new source starts from a full frame. The GPU's boxes
                // are good when against a frame no newer than the one we
                // sent last (older means more cells, never fewer).
                let boxes = match (&last, gpu_boxes) {
                    (Some((g, _, sent)), Some((base, b))) if *g == generation && *sent >= base => Some(b),
                    _ => None,
                };
                // Else the GPU's tiles when against the frame sent last, else a comparison here.
                let (rects, dirty) = match boxes {
                    Some(b) => {
                        let rects = tiles::finish(b);
                        let dirty = grid.covered(&rects);
                        (rects, dirty)
                    }
                    None => {
                        let dirty = match (&last, gpu_dirty) {
                            (Some((g, _, sent)), Some((base, d))) if *g == generation && *sent == base && d.len() == grid.tiles() => d,
                            (Some((g, prev, _)), _) if *g == generation => frame.dirty(prev, &grid),
                            _ => vec![true; grid.tiles()],
                        };
                        let rects = if dirty.iter().all(|&d| d) { vec![(0, 0, w, h)] } else { grid.rects(&dirty) };
                        (rects, dirty)
                    }
                };
                let fresh_start = !matches!(&last, Some((g, ..)) if *g == generation);
                if rects.is_empty() {
                    // Same picture as the one sent last: nothing to pack, compress or send.
                    if let Some(l) = &mut last {
                        l.2 = seq;
                    }
                    continue;
                }
                if !send_rects(&rects, &frame, 0, Some(captured)) {
                    return;
                }
                unsettled.touch(&dirty, Instant::now());
                // The whole frame went out (a new source or mode): the panel's
                // state before it is not known.
                let mut told = vec![];
                if fresh_start {
                    told.push(Sent { drive: Drive::Reset, picture: None, mask: vec![true; grid.cells()] });
                }
                told.push(Sent { drive: Drive::Fast, picture: Some(frame.clone()), mask: grid.covered_cells(&rects) });
                push(told);
                last = Some((generation, frame, seq));
            }
            Work::Settle(due) => {
                let Some((_, frame, _)) = &last else {
                    unsettled.clear();
                    continue;
                };
                let (mode, boxes) = {
                    let s = lock.lock().unwrap();
                    (s.mode, s.ghost_boxes.clone())
                };
                if mode == DisplayMode::None {
                    unsettled.clear();
                    continue;
                }
                let rects = match boxes {
                    Some(b) if SETTLE_BY_BOXES && ghost => grid.rects_within(&due, &b),
                    _ => grid.rects(&due),
                };
                if !send_rects(&rects, frame, RECT_SETTLE, None) {
                    return;
                }
                push(vec![Sent { drive: Drive::Settle, picture: Some(frame.clone()), mask: grid.covered_cells(&rects) }]);
                unsettled.done(&due);
            }
        }
    }
}

pub struct Streamer {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

pub type Send = Box<dyn FnMut(&Msg) -> std::io::Result<()> + std::marker::Send>;

impl Streamer {
    /// `logical` is the tablet's screen as the user sees it; frames are
    /// produced in panel portrait for `rotation`.
    pub fn start(logical: (u32, u32), rotation: u16, stats: bool, settle_after: Duration, ghost: bool, send: Send) -> Streamer {
        let stats = stats.then(DisplayStats::default);
        let shared = Arc::new((Mutex::new(Shared { stats, ..Default::default() }), Condvar::new()));

        let cap_shared = shared.clone();
        let capture = std::thread::spawn(move || {
            let (lock, cv) = &*cap_shared;
            let mut current: Option<(u64, Capture)> = None;
            // The panel's frame size (portrait for a quarter turn).
            let (pw, ph) = if rotation % 180 == 90 { (logical.1 as usize, logical.0 as usize) } else { (logical.0 as usize, logical.1 as usize) };
            // Where the picture is made, set up with each capture; rebuilt, with the capture, whenever it fails.
            let mut gpu: Option<Gpu> = None;
            // `raw` holds a frame of this generation that is not yet rendered.
            let mut raw_new = false;
            let mut raw_for: Option<u64> = None;
            let mut rendered_as: Option<(u64, DisplayMode, u64)> = None;
            let mut epoch = 0u64;
            let mut captured_at = Instant::now();
            // Frames made, and what the GPU says changed since the one the
            // sender last took: the changes of every frame since, joined.
            let mut seq = 0u64;
            let mut gpu_seq: Option<u64> = None;
            let mut acc: Vec<bool> = vec![];
            let mut acc_base: Option<u64> = None;
            // The ghosting model: the picture last given to the GPU (so the
            // same one is not copied again), and the updates it has taken in
            // that are not yet reported to the sender.
            let mut uploaded: Option<Picture> = None;
            let mut applied_pending = 0u64;
            let mut model_moved = false;
            loop {
                let (wanted, generation, shown, mode, look) = {
                    let s = lock.lock().unwrap();
                    if s.stop {
                        return;
                    }
                    (s.source.clone(), s.generation, s.shown, s.mode, s.look)
                };
                if current.as_ref().map(|c| c.0) != Some(generation) {
                    current = None; // stops the old stream (and removes a virtual monitor)
                    if let Some(src) = &wanted {
                        // The old GPU setup goes first: one device at a time.
                        gpu = None;
                        let started = Capture::start(src, logical).and_then(|c| {
                            let geometry = Convert { src: c.src_size, logical: (logical.0 as usize, logical.1 as usize), rotation, swap_rb: c.swap_rb };
                            if ghost { Gpu::with_ghost(pw, ph, DARK, geometry, ghost::Params::default()) } else { Gpu::new(pw, ph, DARK, geometry) }.map(|g| (c, g))
                        });
                        match started {
                            Ok((c, g)) => {
                                eprintln!("remdesktop-host: the picture is made on the GPU ({})", g.name());
                                gpu = Some(g);
                                let mut s = lock.lock().unwrap();
                                // A new GPU starts with the panel assumed white: what was sent before no longer counts.
                                s.drives.clear();
                                s.applied = s.pushed;
                                s.ghost_err = None;
                                applied_pending = 0;
                                s.started = Some(c.virtual_name.clone());
                                current = Some((generation, c));
                            }
                            Err(e) => {
                                eprintln!("remdesktop-host: capture {src:?}: {e}; trying again");
                                std::thread::sleep(Duration::from_secs(1));
                                continue;
                            }
                        }
                    } else {
                        let s = lock.lock().unwrap();
                        let _ = cv.wait_timeout(s, Duration::from_millis(200));
                        continue;
                    }
                }
                let (_, cap) = current.as_mut().unwrap();
                match cap.next_frame(Duration::from_millis(100)) {
                    Ok(true) => {
                        captured_at = Instant::now();
                        raw_for = Some(generation);
                        raw_new = true;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("remdesktop-host: capture ended: {e}");
                        current = None;
                        raw_for = None;
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
                // Render a new frame, or the last one again after a mode change
                // (Mutter sends frames only when the screen changes). Every
                // frame is rendered as soon as it has been read, so the newest
                // picture is ready when the sender is; rendering is one at a
                // time, so the GPU the compositor shares is never queued up
                // with work, and the capture pipe drops what comes too fast.
                let wanted_look = (generation, mode, look);
                let render_new = raw_new;
                let will_render = raw_for == Some(generation) && (render_new || rendered_as != Some(wanted_look));
                // The updates sent since go into the model first, so a frame
                // is measured against what the panel was given.
                if let Some(g) = gpu.as_mut().filter(|_| ghost) {
                    let batch: Vec<Sent> = lock.lock().unwrap().drives.drain(..).collect();
                    for d in &batch {
                        let kind = d.picture.as_ref().map_or(Kind::Gray4, |p| p.kind());
                        let fresh = d.picture.as_ref().filter(|p| !uploaded.as_ref().is_some_and(|u| u.same(p)));
                        if let Err(e) = g.ghost_drive(fresh.map(|p| p.bytes()), kind, d.drive, &d.mask) {
                            eprintln!("remdesktop-host: ghosting model: {e}");
                        }
                        if d.picture.is_some() {
                            uploaded = d.picture.clone();
                        }
                    }
                    applied_pending += batch.len() as u64;
                    model_moved |= !batch.is_empty();
                    if model_moved && !will_render {
                        // No frame to measure it with: measure it against the newest one.
                        match g.ghost_tick() {
                            Ok(Some(err)) => {
                                let mut s = lock.lock().unwrap();
                                s.ghost_err = Some(err);
                                s.ghost_boxes = g.ghost_boxes();
                                s.applied += std::mem::take(&mut applied_pending);
                                model_moved = false;
                                cv.notify_all();
                            }
                            Ok(None) => {}
                            Err(e) => eprintln!("remdesktop-host: ghosting model: {e}"),
                        }
                    }
                }
                if will_render {
                    if rendered_as != Some(wanted_look) {
                        rendered_as = Some(wanted_look);
                        epoch += 1;
                    }
                    raw_new = false;
                    let raw = current.as_ref().map_or(&[][..], |(_, c)| c.frame());
                    // The boxes of what changed are asked against the frame the
                    // sender has now; one it takes meanwhile only makes them a few cells too many.
                    let at = Frame { seq: seq + 1, base: Some(lock.lock().unwrap().sent_seq) };
                    let rendered: Result<Rendered, String> = match (&mut gpu, mode) {
                        // Nothing is sent; the frame is never looked at.
                        (_, DisplayMode::None) => Ok((Picture::Gray4(Arc::new(vec![])), false, None, None, None, None)),
                        (Some(g), DisplayMode::Edge) => g.outline_frame(raw, shown, Some(at)).map(|o| (Picture::Bits(Arc::new(o.bits)), true, o.dirty, o.boxes, o.ghost, o.ghost_boxes)),
                        (Some(g), _) => g.gray_frame(raw, shown, Some(at)).map(|o| (Picture::Gray4(Arc::new(o.bytes)), true, o.dirty, o.boxes, o.ghost, o.ghost_boxes)),
                        (None, _) => Err("no GPU set up".to_string()),
                    };
                    let (picture, on_gpu, tiles_changed, boxes_changed, ghost_err, ghost_boxes) = match rendered {
                        Ok(r) => r,
                        Err(e) => {
                            // Start over: a new capture and a new GPU setup.
                            eprintln!("remdesktop-host: the GPU failed ({e}); setting it up again");
                            gpu = None;
                            current = None;
                            raw_for = None;
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                    };
                    seq += 1;
                    let mut s = lock.lock().unwrap();
                    let mut dirty = None;
                    match tiles_changed {
                        // Only against the frame right before, which the GPU also made.
                        Some(d) if gpu_seq == Some(seq - 1) => {
                            if acc_base != Some(s.sent_seq) {
                                // The sender took a frame since: count from it, if it was the one before.
                                if s.sent_seq == seq - 1 {
                                    acc.clear();
                                    acc.resize(d.len(), false);
                                    acc_base = Some(s.sent_seq);
                                } else {
                                    acc_base = None;
                                }
                            }
                            if let Some(base) = acc_base {
                                acc.iter_mut().zip(&d).for_each(|(a, &b)| *a |= b);
                                dirty = Some((base, acc.clone()));
                            }
                        }
                        _ => acc_base = None,
                    }
                    gpu_seq = on_gpu.then_some(seq);
                    if let Some(err) = ghost_err {
                        s.ghost_err = Some(err);
                        s.ghost_boxes = ghost_boxes;
                        s.applied += std::mem::take(&mut applied_pending);
                        model_moved = false;
                    }
                    let boxes = boxes_changed.zip(at.base);
                    s.frame = Some(Published { epoch, picture, captured: captured_at, seq, dirty, boxes: boxes.map(|(b, base)| (base, b)) });
                    s.fresh = true;
                    cv.notify_all();
                }
            }
        });

        let send_shared = shared.clone();
        let sender = std::thread::spawn(move || run_sender(send_shared, logical, rotation, settle_after, ghost, send));
        Streamer { shared, threads: vec![capture, sender] }
    }

    /// Switches what is captured (None pauses capture); `shown` is where the
    /// picture lands on the tablet (see `mapping::letterbox`).
    pub fn set_source(&self, source: Option<Source>, shown: [f64; 4]) {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap();
        s.source = source;
        s.shown = shown;
        s.generation += 1;
        s.in_flight.clear();
        cv.notify_all();
    }

    /// Switches between the screen, its edges, and nothing at all.
    pub fn set_mode(&self, mode: DisplayMode) {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap();
        s.mode = mode;
        // Asking again for the same mode sends the whole frame again (the
        // tablet's CLR button does, after it flashed the panel).
        s.look += 1;
        s.in_flight.clear();
        cv.notify_all();
    }

    pub fn ack(&self, id: u32) {
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().unwrap();
        s.acked(id);
        cv.notify_all();
    }

    /// Once per capture start: `Some(virtual monitor name)`.
    pub fn take_started(&self) -> Option<Option<String>> {
        self.shared.0.lock().unwrap().started.take()
    }
}

impl Drop for Streamer {
    fn drop(&mut self) {
        {
            let (lock, cv) = &*self.shared;
            lock.lock().unwrap().stop = true;
            cv.notify_all();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const SIZE: usize = 64;

    /// A sender thread on a simulated link: what it sends arrives on the channel.
    struct Rig {
        shared: Arc<(Mutex<Shared>, Condvar)>,
        sent: mpsc::Receiver<Msg>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Rig {
        fn new(settle_after: Duration) -> Rig {
            Rig::with_model(settle_after, false)
        }

        fn with_model(settle_after: Duration, ghost: bool) -> Rig {
            let shared = Arc::new((Mutex::new(Shared { mode: DisplayMode::Normal, ..Default::default() }), Condvar::new()));
            let (tx, sent) = mpsc::channel();
            let s = shared.clone();
            let send: Send = Box::new(move |m| {
                tx.send(m.clone()).unwrap();
                Ok(())
            });
            let thread = Some(std::thread::spawn(move || run_sender(s, (SIZE as u32, SIZE as u32), 0, settle_after, ghost, send)));
            Rig { shared, sent, thread }
        }

        /// Publishes frame `seq`, the content being `level` everywhere except
        /// the first tile, which is `tile`; `dirty` is the GPU's answer.
        fn publish(&self, seq: u64, level: u8, tile: u8, dirty: Option<(u64, Vec<bool>)>) {
            self.publish_with(seq, level, tile, dirty, None);
        }

        /// As `publish`, with the GPU's boxes as the answer.
        fn publish_boxes(&self, seq: u64, level: u8, tile: u8, boxes: (u64, Vec<tiles::Rect>)) {
            self.publish_with(seq, level, tile, None, Some(boxes));
        }

        fn publish_with(&self, seq: u64, level: u8, tile: u8, dirty: Option<(u64, Vec<bool>)>, boxes: Option<(u64, Vec<tiles::Rect>)>) {
            let mut v = vec![level << 4 | level; gray4::row_bytes(SIZE) * SIZE];
            for y in 0..tiles::TILE {
                v[y * gray4::row_bytes(SIZE)..][..tiles::TILE / 2].fill(tile << 4 | tile);
            }
            let (lock, cv) = &*self.shared;
            let mut s = lock.lock().unwrap();
            s.frame = Some(Published { epoch: 1, picture: Picture::Gray4(Arc::new(v)), captured: Instant::now(), seq, dirty, boxes });
            s.fresh = true;
            cv.notify_all();
        }

        fn ack(&self, id: u32) {
            let (lock, cv) = &*self.shared;
            lock.lock().unwrap().acked(id);
            cv.notify_all();
        }

        /// Plays the GPU thread: takes in the updates sent so far and reports
        /// the model's error per tile (the same everywhere but the first tile).
        fn model(&self, first: u32, rest: u32) -> Vec<Sent> {
            let (lock, cv) = &*self.shared;
            let mut s = lock.lock().unwrap();
            // The sender tells the model after it has sent: wait for that.
            let deadline = Instant::now() + SOON;
            while s.drives.is_empty() && Instant::now() < deadline {
                s = cv.wait_timeout(s, Duration::from_millis(10)).unwrap().0;
            }
            let sent: Vec<Sent> = s.drives.drain(..).collect();
            s.applied += sent.len() as u64;
            let tiles = Grid::new(SIZE, SIZE).tiles();
            s.ghost_err = Some((0..tiles).map(|t| if t == 0 { first } else { rest }).collect());
            cv.notify_all();
            sent
        }

        /// The model's error changes (nothing new was sent).
        fn error(&self, first: u32, rest: u32) {
            let (lock, cv) = &*self.shared;
            let tiles = Grid::new(SIZE, SIZE).tiles();
            lock.lock().unwrap().ghost_err = Some((0..tiles).map(|t| if t == 0 { first } else { rest }).collect());
            cv.notify_all();
        }

        /// The next update: its rectangles' ids and flags, or None if nothing came in time.
        fn update(&self, wait: Duration) -> Option<Vec<(u32, u8, (u16, u16, u16, u16))>> {
            let mut rects = vec![];
            loop {
                match self.sent.recv_timeout(wait) {
                    Ok(Msg::Rect { id, x, y, w, h, flags, .. }) => {
                        rects.push((id, flags, (x, y, w, h)));
                        if flags & RECT_LAST != 0 {
                            return Some(rects);
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let (lock, cv) = &*self.shared;
            lock.lock().unwrap().stop = true;
            cv.notify_all();
            self.thread.take().unwrap().join().unwrap();
        }
    }

    const SOON: Duration = Duration::from_millis(500);
    const NOT: Duration = Duration::from_millis(150);

    fn first_tile() -> Vec<bool> {
        let tiles = Grid::new(SIZE, SIZE).tiles();
        (0..tiles).map(|t| t == 0).collect()
    }

    #[test]
    fn at_most_one_update_waits_behind_the_one_in_flight() {
        let rig = Rig::new(Duration::from_secs(3600));
        rig.publish(1, 15, 15, None);
        let first = rig.update(SOON).unwrap();
        assert_eq!(first, vec![(1, RECT_LAST, (0, 0, SIZE as u16, SIZE as u16))]);
        // The tablet acks quickly, so the next update goes out without waiting for the ack.
        rig.publish(2, 15, 3, Some((1, first_tile())));
        let second = rig.update(SOON).unwrap();
        assert_eq!(second[0].2, (0, 0, 32, 32));
        // Two in flight: the newest frame is held, only the newest of them.
        rig.publish(3, 15, 5, Some((2, first_tile())));
        rig.publish(4, 15, 7, Some((2, first_tile())));
        assert!(rig.update(NOT).is_none());
        // The test waited long, so make the ack look quick, as it is on the device.
        rig.ack(first[0].0);
        rig.shared.0.lock().unwrap().last_ack = Duration::ZERO;
        rig.shared.1.notify_all();
        let third = rig.update(SOON).unwrap();
        assert_eq!(third[0].2, (0, 0, 32, 32));
        assert!(rig.update(NOT).is_none());
        assert_eq!(rig.shared.0.lock().unwrap().in_flight.len(), 2);
    }

    #[test]
    fn a_screen_that_never_stops_changing_still_gets_its_settle() {
        let rig = Rig::new(Duration::from_millis(200));
        rig.publish(1, 15, 15, None);
        let (start, mut seq, mut settled) = (Instant::now(), 1u64, false);
        // A frame is always waiting, as while scrolling; the tablet acks at once.
        while start.elapsed() < Duration::from_secs(3) && !settled {
            if let Some(update) = rig.update(Duration::from_millis(30)) {
                settled = update.iter().any(|r| r.1 & RECT_SETTLE != 0);
                rig.ack(update.last().unwrap().0);
            }
            seq += 1;
            rig.publish(seq, 15, (seq % 14) as u8, None);
        }
        assert!(settled, "no settle in {:?} of constant change", start.elapsed());
    }

    #[test]
    fn a_slow_panel_gets_one_update_at_a_time() {
        let rig = Rig::new(Duration::from_secs(3600));
        rig.shared.0.lock().unwrap().last_ack = Duration::from_millis(200);
        rig.publish(1, 15, 15, None);
        let first = rig.update(SOON).unwrap();
        rig.publish(2, 15, 3, Some((1, first_tile())));
        assert!(rig.update(NOT).is_none());
        rig.ack(first[0].0);
        assert!(rig.update(SOON).is_some());
    }

    #[test]
    fn a_changed_frame_goes_ahead_of_a_settle_and_the_tile_settles_again_later() {
        let rig = Rig::new(Duration::from_millis(100));
        rig.publish(1, 15, 15, None);
        let first = rig.update(SOON).unwrap();
        rig.ack(first[0].0);
        // The whole frame is still: it settles, and that is in flight until acked.
        let settle = rig.update(SOON).unwrap();
        assert_eq!(settle[0].1 & RECT_SETTLE, RECT_SETTLE);
        // A change arrives meanwhile: it is sent at once, behind the settle, unacked as it is.
        rig.publish(2, 15, 3, Some((1, first_tile())));
        let change = rig.update(SOON).unwrap();
        assert_eq!(change[0].1 & RECT_SETTLE, 0);
        assert_eq!(change[0].2, (0, 0, 32, 32));
        rig.ack(settle.last().unwrap().0);
        rig.ack(change.last().unwrap().0);
        // The touched tile gets its settle afterwards.
        let again = rig.update(SOON).unwrap();
        assert_eq!(again, vec![(again[0].0, RECT_SETTLE | RECT_LAST, (0, 0, 32, 32))]);
    }

    #[test]
    fn the_model_is_told_the_updates_sent_and_decides_which_tiles_settle() {
        let rig = Rig::with_model(Duration::from_millis(500), true);
        rig.publish(1, 15, 15, None);
        let first = rig.update(SOON).unwrap();
        rig.ack(first[0].0);
        // A new source: the panel's state is reset, then the whole frame driven fast.
        let sent = rig.model(0, 0);
        assert_eq!(sent.iter().map(|d| d.drive).collect::<Vec<_>>(), [Drive::Reset, Drive::Fast]);
        assert!(sent[1].mask.iter().all(|&m| m) && sent[1].picture.is_some());
        // No error anywhere: the timer drops the tiles instead of settling them.
        assert!(rig.update(Duration::from_millis(900)).is_none());
        // One tile changes and ghosts; it settles once it has been still, long before the timer.
        rig.publish(2, 15, 3, Some((1, first_tile())));
        let change = rig.update(SOON).unwrap();
        rig.ack(change[0].0);
        // The 32x32 tile is 16 cells.
        assert_eq!(rig.model(0, 0).iter().map(|d| (d.drive, d.mask.iter().filter(|&&m| m).count())).collect::<Vec<_>>(), [(Drive::Fast, 16)]);
        let t = Instant::now();
        rig.error(ghost::TILE_ERR, 0);
        let settle = rig.update(SOON).unwrap();
        assert_eq!(settle, vec![(settle[0].0, RECT_SETTLE | RECT_LAST, (0, 0, 32, 32))]);
        assert!(t.elapsed() < Duration::from_millis(400), "{:?}", t.elapsed());
        // The settle is a drive for the model too, and the model has to take it in before the next decision.
        rig.ack(settle[0].0);
        assert_eq!(rig.model(0, 0).iter().map(|d| d.drive).collect::<Vec<_>>(), [Drive::Settle]);
        assert!(rig.update(NOT).is_none());
    }

    #[test]
    fn a_small_error_waits_for_the_timer_and_a_busy_tile_for_stillness() {
        let sa = Duration::from_millis(1000);
        let t0 = Instant::now();
        let mut u = Unsettled::new(3);
        let mut b = Budget::new(t0);
        u.touch(&[true, true, true], t0);
        u.touch(&[true, false, false], t0 + Duration::from_millis(500));
        let err = [ghost::TILE_ERR, ghost::TILE_ERR_WEAK, 10];
        // Tile 0 is strong but changed 50 ms ago: it waits for stillness; the others for the timer.
        let now = t0 + Duration::from_millis(550);
        assert_eq!(plan_ghost(&mut u, &mut b, &err, ghost::TILE_ERR, now, sa), Plan::Wait(Some(t0 + Duration::from_millis(500) + ghost::QUIET)));
        // Once still for QUIET, tile 0 goes alone, before the timer.
        let now = t0 + Duration::from_millis(500) + ghost::QUIET;
        assert_eq!(plan_ghost(&mut u, &mut b, &err, ghost::TILE_ERR, now, sa), Plan::Send(vec![true, false, false]));
        u.done(&[true, false, false]);
        // At the timer: tile 1 (a visible error) goes, tile 2 (none) is dropped.
        let now = t0 + sa;
        assert_eq!(plan_ghost(&mut u, &mut b, &err, ghost::TILE_ERR, now, sa), Plan::Send(vec![false, true, false]));
        u.done(&[false, true, false]);
        assert!(!u.any());
    }

    #[test]
    fn settles_are_rate_limited_and_the_worst_tiles_go_first() {
        let sa = Duration::from_millis(1000);
        let t0 = Instant::now();
        let n = ghost::BURST_TILES as usize + 50;
        let mut u = Unsettled::new(n);
        let mut b = Budget::new(t0);
        u.touch(&vec![true; n], t0);
        let err: Vec<u32> = (0..n as u32).map(|t| ghost::TILE_ERR + t).collect();
        let now = t0 + Duration::from_millis(500);
        let Plan::Send(first) = plan_ghost(&mut u, &mut b, &err, ghost::TILE_ERR, now, sa) else { panic!("nothing sent") };
        assert_eq!(first.iter().filter(|&&c| c).count(), ghost::BURST_TILES as usize);
        assert!(first[n - 1] && !first[0], "the largest errors first");
        u.done(&first);
        // The rest has to wait for the budget: a wake-up is set, no send.
        let Plan::Wait(Some(at)) = plan_ghost(&mut u, &mut b, &err, ghost::TILE_ERR, now, sa) else { panic!("should wait") };
        assert!(at > now);
        // And goes once tokens are back.
        let later = now + Duration::from_millis(200);
        assert!(matches!(plan_ghost(&mut u, &mut b, &err, ghost::TILE_ERR, later, sa), Plan::Send(_)));
    }

    #[test]
    fn an_unchanged_frame_sends_nothing() {
        let rig = Rig::new(Duration::from_secs(3600));
        rig.publish(1, 15, 15, None);
        let first = rig.update(SOON).unwrap();
        rig.ack(first[0].0);
        rig.publish(2, 15, 15, Some((1, vec![false; Grid::new(SIZE, SIZE).tiles()])));
        assert!(rig.update(NOT).is_none());
        // And the one after it is compared with what the tablet has.
        rig.publish(3, 15, 4, Some((2, first_tile())));
        assert_eq!(rig.update(SOON).unwrap()[0].2, (0, 0, 32, 32));
    }

    #[test]
    fn the_gpus_boxes_set_the_rectangles_when_they_are_against_a_frame_not_newer_than_the_one_sent() {
        let rig = Rig::new(Duration::from_secs(3600));
        rig.publish(1, 15, 15, None);
        let first = rig.update(SOON).unwrap();
        rig.ack(first[0].0);
        // A small box (a cursor): the update is that box, not its 32x32 tile.
        rig.publish_boxes(2, 15, 3, (1, vec![(8, 8, 16, 24)]));
        let second = rig.update(SOON).unwrap();
        assert_eq!(second.iter().map(|r| r.2).collect::<Vec<_>>(), [(8, 8, 16, 24)]);
        rig.ack(second[0].0);
        // Boxes against an older frame than the one sent are a superset: good.
        rig.publish_boxes(3, 15, 5, (1, vec![(8, 8, 16, 24), (40, 40, 8, 8)]));
        let third = rig.update(SOON).unwrap();
        assert_eq!(third.iter().map(|r| r.2).collect::<Vec<_>>(), [(40, 40, 8, 8), (8, 8, 16, 24)]);
        rig.ack(third.last().unwrap().0);
        // Boxes against a frame newer than the one sent could miss changes: the tiles decide.
        rig.publish_with(5, 15, 7, Some((3, first_tile())), Some((4, vec![(40, 40, 8, 8)])));
        assert_eq!(rig.update(SOON).unwrap()[0].2, (0, 0, 32, 32));
        // No boxes at all (an unchanged frame): nothing is sent.
        rig.publish_boxes(6, 15, 7, (5, vec![]));
        assert!(rig.update(NOT).is_none());
    }

    #[test]
    fn a_tile_that_keeps_changing_still_settles_on_time() {
        let sa = Duration::from_millis(1000);
        let t0 = Instant::now();
        let mut u = Unsettled::new(3);
        u.touch(&[true, true, false], t0);
        // Tile 0 keeps changing; that does not push its refresh back.
        u.touch(&[true, false, false], t0 + Duration::from_millis(600));
        assert_eq!(u.next_due(sa), Some(t0 + sa));
        assert_eq!(u.due(t0 + Duration::from_millis(999), sa), [false, false, false]);
        let due = u.due(t0 + sa, sa);
        assert_eq!(due, [true, true, false]);
        u.done(&due);
        assert_eq!(u.next_due(sa), None);
        // A change after the refresh starts a new wait.
        let t1 = t0 + Duration::from_millis(1200);
        u.touch(&[true, false, false], t1);
        assert_eq!(u.next_due(sa), Some(t1 + sa));
    }

    #[test]
    fn acks_match_updates_in_order_and_late_ones_are_ignored() {
        let mut s = Shared::default();
        let flight = |id| Flight { id, sent: Instant::now(), settle: false, report: None };
        s.in_flight.extend([flight(3), flight(5)]);
        s.acked(4);
        assert_eq!(s.in_flight.len(), 2);
        s.acked(3);
        assert_eq!(s.in_flight.iter().map(|f| f.id).collect::<Vec<_>>(), [5]);
        s.acked(5);
        assert!(s.in_flight.is_empty() && s.may_send());
    }
}
