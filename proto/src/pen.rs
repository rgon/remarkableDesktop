//! The pen as a `Snapshot`: tool, buttons and contact are the numbered steps;
//! position, pressure, distance and tilt are carried by every snapshot.

use crate::sync::{Datagram, HISTORY, Rx, Snapshot, Tx};
use crate::{Event, Msg};

/// Device id of the pen (the same on both ends).
pub const PEN_DEVICE: u8 = 1;

pub type PenTx = Tx<PenState>;
pub type PenSync = Rx<PenState>;

const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_PRESSURE: u16 = 0x18;
const ABS_DISTANCE: u16 = 0x19;
const ABS_TILT_X: u16 = 0x1a;
const ABS_TILT_Y: u16 = 0x1b;

/// Button/tool/contact bits of `PenState::flags`, with their key codes.
const FLAG_KEYS: [u16; 7] = [
    0x140, // BTN_TOOL_PEN
    0x141, // BTN_TOOL_RUBBER
    0x14a, // BTN_TOUCH
    0x14b, // BTN_STYLUS
    0x14c, // BTN_STYLUS2
    0x110, // BTN_LEFT
    0x111, // BTN_RIGHT
];

/// Everything the pen reports, as of one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PenState {
    /// Bit i is the key `FLAG_KEYS[i]`.
    pub flags: u8,
    pub x: i32,
    pub y: i32,
    pub pressure: i32,
    pub distance: i16,
    pub tilt: [i16; 2],
    /// How many times `flags` changed so far (wrapping).
    pub edges: u16,
}

impl PenState {
    pub const SIZE: usize = 21;

    /// Folds evdev events (as sent in a `Frame`) into the state.
    pub fn apply(&mut self, events: &[Event]) {
        for e in events {
            match (e.kind, e.code) {
                (EV_KEY, code) => {
                    if let Some(bit) = FLAG_KEYS.iter().position(|&k| k == code) {
                        self.flags = self.flags & !(1 << bit) | ((e.value != 0) as u8) << bit;
                    }
                }
                (EV_ABS, ABS_X) => self.x = e.value,
                (EV_ABS, ABS_Y) => self.y = e.value,
                (EV_ABS, ABS_PRESSURE) => self.pressure = e.value,
                (EV_ABS, ABS_DISTANCE) => self.distance = clamp16(e.value),
                (EV_ABS, ABS_TILT_X) => self.tilt[0] = clamp16(e.value),
                (EV_ABS, ABS_TILT_Y) => self.tilt[1] = clamp16(e.value),
                _ => {}
            }
        }
    }

    /// The events that turn this state into `to`: the tool first, then the
    /// axes, then the contact and buttons, as a kernel driver orders them.
    pub fn diff(&self, to: &PenState) -> Vec<Event> {
        let key = |bit: usize| {
            let on = to.flags >> bit & 1;
            (on != self.flags >> bit & 1).then_some(Event { kind: EV_KEY, code: FLAG_KEYS[bit], value: on as i32 })
        };
        let abs = |code, old: i32, new: i32| (old != new).then_some(Event { kind: EV_ABS, code, value: new });
        [key(0), key(1)]
            .into_iter()
            .chain([
                abs(ABS_X, self.x, to.x),
                abs(ABS_Y, self.y, to.y),
                abs(ABS_PRESSURE, self.pressure, to.pressure),
                abs(ABS_DISTANCE, self.distance as i32, to.distance as i32),
                abs(ABS_TILT_X, self.tilt[0] as i32, to.tilt[0] as i32),
                abs(ABS_TILT_Y, self.tilt[1] as i32, to.tilt[1] as i32),
            ])
            .chain((2..FLAG_KEYS.len()).map(key))
            .flatten()
            .collect()
    }

    pub fn put(&self, out: &mut Vec<u8>) {
        out.push(self.flags);
        for v in [self.x, self.y, self.pressure] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [self.distance, self.tilt[0], self.tilt[1]] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.edges.to_le_bytes());
    }

    /// Parses `SIZE` bytes.
    pub fn get(b: &[u8]) -> Option<PenState> {
        let b = b.get(..Self::SIZE)?;
        let i32_at = |i: usize| i32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let i16_at = |i: usize| i16::from_le_bytes(b[i..i + 2].try_into().unwrap());
        Some(PenState { flags: b[0], x: i32_at(1), y: i32_at(5), pressure: i32_at(9), distance: i16_at(13), tilt: [i16_at(15), i16_at(17)], edges: u16::from_le_bytes([b[19], b[20]]) })
    }
}

fn clamp16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}


impl Snapshot for PenState {
    fn edges(&self) -> u16 {
        self.edges
    }
    fn set_edges(&mut self, n: u16) {
        self.edges = n;
    }
    fn apply(&mut self, events: &[Event]) {
        PenState::apply(self, events)
    }
    fn steps_to(&self, new: &Self) -> bool {
        self.flags != new.flags
    }
    fn diff(&self, to: &Self) -> Vec<Event> {
        PenState::diff(self, to)
    }
    fn merge(&self, s: &Self, step: bool) -> Self {
        PenState { flags: if step { s.flags } else { self.flags }, edges: if step { s.edges } else { self.edges }, ..*s }
    }
    fn blip(&self, step: &Self) -> [Self; 2] {
        [PenState { edges: self.edges, ..*step }, PenState { flags: step.flags, edges: self.edges, ..*self }]
    }
    fn put(&self, out: &mut Vec<u8>) {
        PenState::put(self, out)
    }
    fn get(b: &[u8]) -> Option<(Self, usize)> {
        PenState::get(b).map(|s| (s, Self::SIZE))
    }
    fn datagram(key: u32, seq: u32, time_us: u64, states: [Self; HISTORY]) -> Datagram {
        Datagram::Pen { key, seq, time_us, states }
    }
    fn edge_msg(seq: u32, time_us: u64, state: Self) -> Msg {
        Msg::PenState { seq, time_us, state }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{Packet, REPEATS_US};
    use std::collections::VecDeque;

    fn ev(kind: u16, code: u16, value: i32) -> Event {
        Event { kind, code, value }
    }
    const TOUCH: u16 = 0x14a;
    const TOOL: u16 = 0x140;

    fn at(x: i32) -> Vec<Event> {
        vec![ev(EV_ABS, ABS_X, x)]
    }

    /// What a device that received `batches` shows.
    fn show(batches: &[Vec<Event>]) -> PenState {
        let mut s = PenState::default();
        batches.iter().for_each(|b| s.apply(b));
        s
    }

    fn pen(p: &Packet) -> (u32, [PenState; HISTORY]) {
        match Datagram::decode(&p.datagram) {
            Some(Datagram::Pen { seq, states, .. }) => (seq, states),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn state_and_datagram_round_trip() {
        let mut tx = PenTx::new(7);
        let p = tx.frame(5, 0, &[ev(EV_KEY, TOOL, 1), ev(EV_ABS, ABS_X, 12345), ev(EV_ABS, ABS_TILT_X, -3000), ev(EV_ABS, ABS_PRESSURE, 4000)]);
        let d = Datagram::decode(&p.datagram).unwrap();
        assert!(matches!(d, Datagram::Pen { key: 7, seq: 1, time_us: 5, .. }));
        assert_eq!(d.encode(), p.datagram);
        let Datagram::Pen { states, .. } = d else { unreachable!() };
        assert_eq!(states[0].x, 12345);
        assert_eq!(states[0].tilt[0], -3000);
        assert_eq!(states[0].flags, 1);
        for m in [Datagram::Probe { key: 1 }, Datagram::Ack { key: 2 }] {
            assert_eq!(Datagram::decode(&m.encode()), Some(m));
        }
        assert_eq!(Datagram::decode(&p.datagram[..40]), None);
    }

    #[test]
    fn diff_reproduces_the_state() {
        let mut a = PenState::default();
        a.apply(&[ev(EV_KEY, TOOL, 1), ev(EV_ABS, ABS_X, 10), ev(EV_ABS, ABS_Y, 20), ev(EV_KEY, TOUCH, 1)]);
        let mut b = a;
        b.apply(&[ev(EV_ABS, ABS_X, 11), ev(EV_KEY, TOUCH, 0)]);
        let events = a.diff(&b);
        assert_eq!(events, vec![ev(EV_ABS, ABS_X, 11), ev(EV_KEY, TOUCH, 0)]);
        let mut c = a;
        c.apply(&events);
        assert_eq!(c, b);
        assert!(b.diff(&b).is_empty());
    }

    #[test]
    fn in_order_delivery_applies_each_snapshot_once() {
        let (mut tx, mut rx) = (PenTx::new(1), PenSync::synced());
        for x in 1..=5 {
            let (seq, states) = pen(&tx.frame(0, 0, &at(x)));
            let batches = rx.datagram(seq, &states);
            assert_eq!(batches, vec![at(x)]);
        }
        assert_eq!(rx.stats.lost + rx.stats.stale + rx.stats.recovered, 0);
    }

    #[test]
    fn a_lost_datagram_is_replayed_from_the_next_ones_history() {
        let (mut tx, mut rx) = (PenTx::new(1), PenSync::synced());
        let down = tx.frame(0, 0, &[ev(EV_KEY, TOUCH, 1)]);
        let up = tx.frame(0, 0, &[ev(EV_KEY, TOUCH, 0)]);
        let (s1, st1) = pen(&down);
        let (s3, st3) = {
            let _lost = tx.frame(0, 0, &at(1));
            pen(&tx.frame(0, 0, &at(2)))
        };
        rx.datagram(s1, &st1);
        let (s2, _) = pen(&up); // never delivered
        assert_eq!(s2, 2);
        let batches = rx.datagram(s3, &st3);
        // The release between them is not lost, and the end state is right.
        assert!(batches.iter().flatten().any(|e| *e == ev(EV_KEY, TOUCH, 0)));
        assert_eq!(show(&batches).x, 2);
        assert_eq!(rx.stats.recovered, 2);
    }

    #[test]
    fn reordered_and_duplicate_datagrams_are_ignored() {
        let (mut tx, mut rx) = (PenTx::new(1), PenSync::synced());
        let a = pen(&tx.frame(0, 0, &at(1)));
        let b = pen(&tx.frame(0, 0, &at(2)));
        rx.datagram(b.0, &b.1);
        assert!(rx.datagram(a.0, &a.1).is_empty());
        assert!(rx.datagram(b.0, &b.1).is_empty());
        assert_eq!(rx.stats.stale, 2);
    }

    #[test]
    fn sequence_wraps() {
        let (mut tx, mut rx) = (PenTx::new(1), PenSync::synced());
        tx.set_seq(u32::MAX - 1);
        for x in 1..=4 {
            let (seq, states) = pen(&tx.frame(0, 0, &at(x)));
            assert_eq!(rx.datagram(seq, &states), vec![at(x)]);
        }
    }

    #[test]
    fn an_edge_every_datagram_missed_is_replayed_from_tcp() {
        let (mut tx, mut rx) = (PenTx::new(1), PenSync::synced());
        let first = pen(&tx.frame(0, 0, &at(1)));
        rx.datagram(first.0, &first.1);
        // A tap in the middle of a long outage: only TCP carries it.
        let down = tx.frame(0, 0, &[ev(EV_KEY, TOUCH, 1)]);
        let up = tx.frame(0, 0, &[ev(EV_KEY, TOUCH, 0)]);
        for _ in 0..5 {
            tx.frame(0, 0, &at(9));
        }
        let (s, st) = pen(&tx.frame(0, 0, &at(10)));
        let batches = rx.datagram(s, &st);
        assert!(!batches.iter().flatten().any(|e| e.code == TOUCH), "history is too short to know");
        let mut all = vec![];
        for edge in [down.edge, up.edge] {
            let Some(Msg::PenState { seq, state, .. }) = edge else { panic!("no edge copy") };
            all.extend(rx.edge(seq, state));
        }
        let touches: Vec<i32> = all.iter().flatten().filter(|e| e.code == TOUCH).map(|e| e.value).collect();
        assert_eq!(touches, vec![1, 0], "the tap shows up as press and release");
        assert_eq!(rx.stats.replays, 2);
        // And the device ends up where the tablet is.
        let mut dev = PenState::default();
        dev.apply(&[ev(EV_ABS, ABS_X, 1)]);
        dev.apply(&batches.concat());
        dev.apply(&all.concat());
        assert_eq!(dev.x, 10);
        assert_eq!(dev.flags, 0);
    }

    #[test]
    fn an_edge_that_already_arrived_is_not_replayed() {
        let (mut tx, mut rx) = (PenTx::new(1), PenSync::synced());
        let p = tx.frame(0, 0, &[ev(EV_KEY, TOUCH, 1)]);
        let (s, st) = pen(&p);
        rx.datagram(s, &st);
        let q = pen(&tx.frame(0, 0, &at(3)));
        rx.datagram(q.0, &q.1);
        let Some(Msg::PenState { seq, state, .. }) = p.edge else { panic!() };
        assert!(rx.edge(seq, state).is_empty());
        assert_eq!(rx.stats.replays, 0);
    }

    #[test]
    fn an_edge_ahead_of_the_datagrams_is_applied() {
        let (mut tx, mut rx) = (PenTx::new(1), PenSync::synced());
        let a = pen(&tx.frame(0, 0, &at(1)));
        rx.datagram(a.0, &a.1);
        let down = tx.frame(0, 0, &[ev(EV_KEY, TOUCH, 1)]);
        let Some(Msg::PenState { seq, state, .. }) = down.edge else { panic!() };
        let batches = rx.edge(seq, state);
        assert_eq!(batches, vec![vec![ev(EV_KEY, TOUCH, 1)]]);
        // The datagram itself, late, is then a duplicate.
        let (s, st) = pen(&down);
        assert!(rx.datagram(s, &st).is_empty());
    }

    #[test]
    fn repeats_follow_the_last_change() {
        let mut tx = PenTx::new(1);
        tx.frame(0, 1_000_000, &at(1));
        assert_eq!(tx.next_in_ms(1_000_000), Some(8));
        assert!(tx.tick(1_005_000).is_none());
        let rep = tx.tick(1_009_000).expect("first repeat");
        let Some(Datagram::Pen { seq, time_us, states, .. }) = Datagram::decode(&rep) else { panic!() };
        assert_eq!((seq, time_us), (2, 0));
        assert_eq!(states[0], states[1]);
        // A new frame restarts the schedule; late ticks coalesce.
        tx.frame(0, 2_000_000, &at(2));
        assert!(tx.tick(6_100_000).is_some());
        assert!(tx.tick(6_100_001).is_none());
        assert_eq!(tx.next_in_ms(6_100_001), None);
    }

    #[test]
    fn tracking_other_routes_keeps_the_first_datagram_true() {
        let (mut tx, mut rx) = (PenTx::new(1), PenSync::synced());
        // Frames went over TCP so far: both ends followed them.
        let events = [ev(EV_KEY, TOOL, 1), ev(EV_ABS, ABS_X, 100)];
        tx.track(&events);
        rx.track(&events);
        let (s, st) = pen(&tx.frame(0, 0, &at(101)));
        assert_eq!(rx.datagram(s, &st), vec![at(101)]);
    }

    /// Tiny deterministic generator, so a failure can be reproduced.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn chance(&mut self, percent: u32) -> bool {
            self.next() % 100 < percent
        }
    }

    /// A pen session over a bad link: UDP loses and reorders, TCP edge copies
    /// arrive in order but late. The device must end where the tablet is and
    /// see every press and release the tablet made, in order.
    #[test]
    fn random_loss_never_loses_an_edge_or_the_final_state() {
        for (seed, loss) in (0..300u64).zip([0, 10, 30, 60, 90].into_iter().cycle()) {
            let mut rng = Rng(seed + 1);
            let (mut tx, mut rx) = (PenTx::new(1), PenSync::synced());
            let mut device = PenState::default();
            let mut emitted_touch = vec![];
            let mut truth_touch = vec![];
            let mut udp: Vec<(u32, [PenState; HISTORY])> = vec![];
            let mut tcp: VecDeque<(u32, PenState)> = VecDeque::new();
            let (mut x, mut down) = (0, false);
            let deliver = |batches: Vec<Vec<Event>>, device: &mut PenState, out: &mut Vec<i32>| {
                for b in batches {
                    out.extend(b.iter().filter(|e| e.code == TOUCH).map(|e| e.value));
                    device.apply(&b);
                }
            };
            for step in 0..400u64 {
                let mut events = vec![];
                if rng.chance(70) {
                    x += rng.next() as i32 % 7 - 3;
                    events.push(ev(EV_ABS, ABS_X, x));
                }
                if rng.chance(8) {
                    down = !down;
                    events.push(ev(EV_KEY, TOUCH, down as i32));
                    truth_touch.push(down as i32);
                }
                if events.is_empty() {
                    continue;
                }
                let p = tx.frame(step, step * 5_000, &events);
                let (seq, states) = pen(&p);
                if !rng.chance(loss) {
                    udp.push((seq, states));
                }
                if let Some(Msg::PenState { seq, state, .. }) = p.edge {
                    tcp.push_back((seq, state));
                }
                // Reordering: sometimes hold a datagram back one step.
                if udp.len() > 1 && rng.chance(15) {
                    let n = udp.len();
                    udp.swap(n - 1, n - 2);
                }
                for (seq, states) in std::mem::take(&mut udp) {
                    deliver(rx.datagram(seq, &states), &mut device, &mut emitted_touch);
                }
                // TCP is slow, not lossy.
                while tcp.front().is_some() && rng.chance(20) {
                    let (seq, state) = tcp.pop_front().unwrap();
                    deliver(rx.edge(seq, state), &mut device, &mut emitted_touch);
                }
            }
            // Quiet: whatever repeats get through, then TCP catches up.
            for after in REPEATS_US {
                if let Some(d) = tx.tick(400 * 5_000 + after + 1) {
                    if !rng.chance(loss) {
                        let Some(Datagram::Pen { seq, states, .. }) = Datagram::decode(&d) else { unreachable!() };
                        deliver(rx.datagram(seq, &states), &mut device, &mut emitted_touch);
                    }
                }
            }
            for (seq, state) in tcp {
                deliver(rx.edge(seq, state), &mut device, &mut emitted_touch);
            }
            assert_eq!(device.flags, tx.state().flags, "seed {seed} loss {loss}%: buttons");
            if loss <= 30 {
                // Position has no reliable copy, only the repeats.
                assert_eq!(device.x, tx.state().x, "seed {seed} loss {loss}%: x");
            }
            assert_eq!(emitted_touch, truth_touch, "seed {seed} loss {loss}%: edges");
        }
    }
}
