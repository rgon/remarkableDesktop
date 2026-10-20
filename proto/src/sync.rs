//! Pen and touch over Wi-Fi: state snapshots in UDP datagrams instead of
//! evdev deltas in the TCP stream, so a lost packet delays nothing and a
//! late one blocks nothing behind it (TCP would stall every later sample for
//! at least its 200 ms retransmit timeout).
//!
//! A snapshot is a device's whole state, so any later packet repairs the
//! host's picture of positions, pressure and the like. Some changes are
//! different: a press and release both lost would never have happened. Those
//! (`Snapshot::steps_to`: buttons, tool and contact for the pen, which
//! contacts are down for touch) are numbered (`Snapshot::edges`), and the
//! host applies them strictly in order and once each:
//! - each datagram repeats the previous `HISTORY - 1` snapshots, so short
//!   gaps are replayed in order, including the steps in them;
//! - every step also goes over TCP as a `Msg`, which always arrives. A step
//!   the datagrams never delivered is replayed from it as a blip (at the
//!   position it happened) before the present is restored;
//! - after the last change the latest state is sent again a few times, so a
//!   lost last sample doesn't leave the pointer short of where the pen is.
//!
//! `Tx` is the tablet's half of a device, `Rx` the host's.

use crate::{Event, Msg};
use std::collections::VecDeque;

/// Snapshots per datagram: the latest and the ones before it.
pub const HISTORY: usize = 3;

/// When the latest state is sent again after the last change (µs).
pub const REPEATS_US: [u64; 7] = [8_000, 25_000, 80_000, 250_000, 1_000_000, 2_000_000, 4_000_000];

/// What a device reports, as of one instant.
pub trait Snapshot: Copy + Default + PartialEq + std::fmt::Debug {
    /// How many steps happened up to this state (wrapping).
    fn edges(&self) -> u16;
    fn set_edges(&mut self, n: u16);
    /// Folds evdev events (as sent in a `Frame`) into the state.
    fn apply(&mut self, events: &[Event]);
    /// Whether `new` differs from `self` by a step.
    fn steps_to(&self, new: &Self) -> bool;
    /// The events that turn `self` into `to`.
    fn diff(&self, to: &Self) -> Vec<Event>;
    /// What to show after `self` (the present) when `s` is the next snapshot
    /// of a history: all of `s` if its step is due (`step`), else only what
    /// follows from the present's own step (positions of the same things).
    fn merge(&self, s: &Self, step: bool) -> Self;
    /// A step that arrived late: the state showing it at the place it
    /// happened, then the state to go back to (the present's positions).
    fn blip(&self, step: &Self) -> [Self; 2];
    fn put(&self, out: &mut Vec<u8>);
    /// Parses one, returning it and the bytes used.
    fn get(b: &[u8]) -> Option<(Self, usize)>;
    fn datagram(key: u32, seq: u32, time_us: u64, states: [Self; HISTORY]) -> Datagram;
    /// The TCP copy of a step.
    fn edge_msg(seq: u32, time_us: u64, state: Self) -> Msg;
}

/// What travels in a UDP datagram. `key` is the one the tablet offered over
/// TCP, so only the authenticated host's packets are accepted.
#[derive(Debug, Clone, PartialEq)]
pub enum Datagram {
    /// Host → tablet: "send input to where this came from".
    Probe { key: u32 },
    /// Tablet → host: the probe arrived.
    Ack { key: u32 },
    /// Tablet → host: `states[k]` is the state at sequence `seq - k`.
    /// `time_us` is the kernel time of the latest event (0 for a repeat).
    Pen { key: u32, seq: u32, time_us: u64, states: [crate::PenState; HISTORY] },
    Touch { key: u32, seq: u32, time_us: u64, states: [crate::TouchState; HISTORY] },
}

const PROBE: u8 = 1;
const ACK: u8 = 2;
const PEN: u8 = 3;
const TOUCH: u8 = 4;

impl Datagram {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        match self {
            Datagram::Probe { key } => {
                out.push(PROBE);
                out.extend_from_slice(&key.to_le_bytes());
            }
            Datagram::Ack { key } => {
                out.push(ACK);
                out.extend_from_slice(&key.to_le_bytes());
            }
            Datagram::Pen { key, seq, time_us, states } => put_snapshots(&mut out, PEN, *key, *seq, *time_us, states),
            Datagram::Touch { key, seq, time_us, states } => put_snapshots(&mut out, TOUCH, *key, *seq, *time_us, states),
        }
        out
    }

    pub fn decode(b: &[u8]) -> Option<Datagram> {
        let key = u32::from_le_bytes(b.get(1..5)?.try_into().ok()?);
        match *b.first()? {
            PROBE => Some(Datagram::Probe { key }),
            ACK => Some(Datagram::Ack { key }),
            PEN => get_snapshots(b).map(|(seq, time_us, states)| Datagram::Pen { key, seq, time_us, states }),
            TOUCH => get_snapshots(b).map(|(seq, time_us, states)| Datagram::Touch { key, seq, time_us, states }),
            _ => None,
        }
    }
}

fn put_snapshots<S: Snapshot>(out: &mut Vec<u8>, kind: u8, key: u32, seq: u32, time_us: u64, states: &[S; HISTORY]) {
    out.push(kind);
    out.extend_from_slice(&key.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&time_us.to_le_bytes());
    states.iter().for_each(|s| s.put(out));
}

fn get_snapshots<S: Snapshot>(b: &[u8]) -> Option<(u32, u64, [S; HISTORY])> {
    let seq = u32::from_le_bytes(b.get(5..9)?.try_into().ok()?);
    let time_us = u64::from_le_bytes(b.get(9..17)?.try_into().ok()?);
    let mut states = [S::default(); HISTORY];
    let mut at = 17;
    for s in &mut states {
        let (parsed, used) = S::get(b.get(at..)?)?;
        *s = parsed;
        at += used;
    }
    Some((seq, time_us, states))
}

/// The tablet's side of one device: turns its frames into datagrams.
pub struct Tx<S: Snapshot> {
    key: u32,
    seq: u32,
    /// State at `seq`, then the ones before it.
    states: [S; HISTORY],
    /// Times (µs, `monotonic_us`) at which the latest state is repeated.
    repeats: VecDeque<u64>,
}

/// One frame, ready to send.
pub struct Packet {
    pub datagram: Vec<u8>,
    /// For a step: the same snapshot for the reliable channel.
    pub edge: Option<Msg>,
}

impl<S: Snapshot> Tx<S> {
    pub fn new(key: u32) -> Self {
        Tx { key, seq: 0, states: [S::default(); HISTORY], repeats: VecDeque::new() }
    }

    pub fn key(&self) -> u32 {
        self.key
    }

    #[cfg(test)]
    pub(crate) fn set_seq(&mut self, seq: u32) {
        self.seq = seq;
    }

    /// The newest state.
    pub fn state(&self) -> &S {
        &self.states[0]
    }

    /// Follows frames that went another way (TCP, USB HID), so the first
    /// datagram carries the true state.
    pub fn track(&mut self, events: &[Event]) {
        self.states[0] = self.next(events).0;
    }

    /// The state after `events`, and whether a step happened in it.
    fn next(&self, events: &[Event]) -> (S, bool) {
        let mut next = self.states[0];
        next.apply(events);
        let step = self.states[0].steps_to(&next);
        next.set_edges(next.edges().wrapping_add(step as u16));
        (next, step)
    }

    /// A frame: the next datagram, repeats rescheduled.
    pub fn frame(&mut self, time_us: u64, now_us: u64, events: &[Event]) -> Packet {
        let (next, step) = self.next(events);
        let packet = self.advance(next, time_us, step);
        self.repeats = REPEATS_US.iter().map(|d| now_us + d).collect();
        packet
    }

    /// The datagram for a due repeat, if any.
    pub fn tick(&mut self, now_us: u64) -> Option<Vec<u8>> {
        if *self.repeats.front()? > now_us {
            return None;
        }
        // Several overdue repeats are one.
        while self.repeats.front().is_some_and(|&t| t <= now_us) {
            self.repeats.pop_front();
        }
        Some(self.advance(self.states[0], 0, false).datagram)
    }

    /// Milliseconds until the next repeat (rounded up), if one is pending.
    pub fn next_in_ms(&self, now_us: u64) -> Option<i32> {
        self.repeats.front().map(|&t| t.saturating_sub(now_us).div_ceil(1000).min(i32::MAX as u64) as i32)
    }

    fn advance(&mut self, next: S, time_us: u64, step: bool) -> Packet {
        self.seq = self.seq.wrapping_add(1);
        self.states.rotate_right(1);
        self.states[0] = next;
        Packet { datagram: S::datagram(self.key, self.seq, time_us, self.states).encode(), edge: step.then(|| S::edge_msg(self.seq, time_us, next)) }
    }
}

/// Counters for `--stats`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncStats {
    pub datagrams: u32,
    /// Sequence numbers no snapshot of ever arrived (lost beyond the history).
    pub lost: u32,
    /// Datagrams that were duplicates or older than one already applied.
    pub stale: u32,
    /// Sequence numbers recovered from a datagram's history.
    pub recovered: u32,
    /// Steps the datagrams never delivered, replayed from TCP.
    pub replays: u32,
}

/// The host's side of one device: works out the events that bring the
/// virtual device to the state the tablet reports.
pub struct Rx<S: Snapshot> {
    /// What the virtual device shows now.
    state: S,
    /// Newest sequence number whose positions were applied.
    last: Option<u32>,
    /// The newest snapshot heard, for positions to catch up once the steps
    /// they wait for have been shown.
    newest: Option<S>,
    /// `edges` of the last step shown (steps are shown in order, once).
    /// Counted from the tablet's start of the session, like the tablet does;
    /// unknown (`None`) for a device created later, which adopts the first
    /// count it hears.
    shown: Option<u16>,
    pub stats: SyncStats,
}

impl<S: Snapshot> Default for Rx<S> {
    fn default() -> Self {
        Rx { state: S::default(), last: None, newest: None, shown: None, stats: SyncStats::default() }
    }
}

impl<S: Snapshot> Rx<S> {
    /// For the device declared when the session starts, when the tablet's
    /// count starts too.
    pub fn synced() -> Self {
        Rx { shown: Some(0), ..Default::default() }
    }

    /// Follows events the host applied itself (frames over TCP).
    pub fn track(&mut self, events: &[Event]) {
        let before = self.state;
        self.state.apply(events);
        if let Some(n) = &mut self.shown {
            *n = n.wrapping_add(before.steps_to(&self.state) as u16);
        }
    }

    /// A datagram: batches of events to emit in order, each followed by a
    /// SYN_REPORT. `states[k]` is the state at `seq - k`.
    pub fn datagram(&mut self, seq: u32, states: &[S]) -> Vec<Vec<Event>> {
        self.stats.datagrams += 1;
        let ahead = self.last.map_or(1, |last| seq.wrapping_sub(last) as i32);
        if ahead <= 0 {
            self.stats.stale += 1;
            return vec![];
        }
        self.newest = Some(states[0]);
        let mut batches = vec![];
        let Some(mut shown) = self.shown else {
            // Nothing to compare with: take it as it is.
            self.shown = Some(states[0].edges());
            self.last = Some(seq);
            self.show(states[0], &mut batches);
            return batches;
        };
        let n = (ahead as usize).min(states.len());
        if self.last.is_some() {
            self.stats.lost += ahead as u32 - n as u32;
            self.stats.recovered += n as u32 - 1;
        }
        self.last = Some(seq);
        for s in states[..n].iter().rev() {
            // A step more than one ahead has a step in between that only TCP
            // has: the step waits for it, the positions move on.
            let step = s.edges().wrapping_sub(shown) as i16 == 1;
            if step {
                shown = s.edges();
            }
            let to = self.state.merge(s, step);
            self.show(to, &mut batches);
        }
        self.shown = Some(shown);
        batches
    }

    /// The copy of a step over TCP.
    pub fn edge(&mut self, seq: u32, state: S) -> Vec<Vec<Event>> {
        let mut batches = vec![];
        let Some(shown) = self.shown else {
            self.shown = Some(state.edges());
            self.last = Some(seq);
            self.newest = Some(state);
            self.show(state, &mut batches);
            return batches;
        };
        if (state.edges().wrapping_sub(shown) as i16) <= 0 {
            return batches; // the datagrams delivered it
        }
        self.shown = Some(state.edges());
        if self.last.is_none_or(|last| (seq.wrapping_sub(last) as i32) > 0) {
            self.last = Some(seq);
            self.newest = Some(state);
            self.show(state, &mut batches);
            return batches;
        }
        // The datagrams skipped it and have moved on: show the step where it
        // happened, then put the present's positions back.
        self.stats.replays += 1;
        for to in self.state.blip(&state) {
            self.show(to, &mut batches);
        }
        if let Some(newest) = self.newest {
            let to = self.state.merge(&newest, false);
            self.show(to, &mut batches);
        }
        batches
    }

    fn show(&mut self, to: S, batches: &mut Vec<Vec<Event>>) {
        let events = self.state.diff(&to);
        self.state = to;
        if !events.is_empty() {
            batches.push(events);
        }
    }
}
