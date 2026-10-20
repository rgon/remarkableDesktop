//! Touch as a `Snapshot`: which contacts are down (a step whenever that set
//! changes, so a tap is never lost) and where they are, how hard they press
//! and how big they are (carried by every snapshot).
//!
//! The legacy single-touch axes and `BTN_TOUCH`/`BTN_TOOL_*` that touchpad
//! drivers rely on follow from the contacts, so they are not part of the
//! state; `diff` produces them.

use crate::sync::{Datagram, HISTORY, Rx, Snapshot, Tx};
use crate::{Event, Msg};

/// Device id of the touch screen (the same on both ends).
pub const TOUCH_DEVICE: u8 = 2;

/// Multitouch slots.
pub const SLOTS: usize = 16;

pub type TouchTx = Tx<TouchState>;
pub type TouchSync = Rx<TouchState>;

const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_TOUCH_MINOR: u16 = 0x31;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_PRESSURE: u16 = 0x3a;
const BTN_TOUCH: u16 = 0x14a;
/// One finger to five, as the touchpad stack counts them.
const TOOLS: [u16; 5] = [0x145, 0x14d, 0x14e, 0x14f, 0x148];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Contact {
    pub id: i32,
    pub x: i32,
    pub y: i32,
    pub pressure: i16,
    pub major: i16,
    pub minor: i16,
}

impl Contact {
    const SIZE: usize = 18;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TouchState {
    pub slots: [Option<Contact>; SLOTS],
    /// How many times the set of contacts changed so far (wrapping).
    pub edges: u16,
}

fn clamp16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

impl TouchState {
    fn count(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    fn first(&self) -> Option<&Contact> {
        self.slots.iter().flatten().next()
    }

    fn ids(&self) -> impl Iterator<Item = Option<i32>> + '_ {
        self.slots.iter().map(|s| s.map(|c| c.id))
    }
}

impl Snapshot for TouchState {
    fn edges(&self) -> u16 {
        self.edges
    }

    fn set_edges(&mut self, n: u16) {
        self.edges = n;
    }

    /// Multitouch protocol B: each block starts by selecting its slot.
    fn apply(&mut self, events: &[Event]) {
        let mut cur = 0;
        for e in events.iter().filter(|e| e.kind == EV_ABS) {
            if e.code == ABS_MT_SLOT {
                cur = (e.value.max(0) as usize).min(SLOTS - 1);
                continue;
            }
            let slot = &mut self.slots[cur];
            match e.code {
                ABS_MT_TRACKING_ID => {
                    *slot = match (e.value, *slot) {
                        (v, _) if v < 0 => None,
                        (v, Some(c)) if c.id == v => Some(c),
                        (v, _) => Some(Contact { id: v, ..Default::default() }),
                    }
                }
                ABS_MT_POSITION_X => slot.iter_mut().for_each(|c| c.x = e.value),
                ABS_MT_POSITION_Y => slot.iter_mut().for_each(|c| c.y = e.value),
                ABS_MT_PRESSURE => slot.iter_mut().for_each(|c| c.pressure = clamp16(e.value)),
                ABS_MT_TOUCH_MAJOR => slot.iter_mut().for_each(|c| c.major = clamp16(e.value)),
                ABS_MT_TOUCH_MINOR => slot.iter_mut().for_each(|c| c.minor = clamp16(e.value)),
                _ => {}
            }
        }
    }

    fn steps_to(&self, new: &Self) -> bool {
        self.ids().ne(new.ids())
    }

    fn diff(&self, to: &Self) -> Vec<Event> {
        let abs = |code, value| Event { kind: EV_ABS, code, value };
        let key = |code, on: bool| Event { kind: EV_KEY, code, value: on as i32 };
        let mut ev = vec![];
        for (i, (from, to)) in self.slots.iter().zip(&to.slots).enumerate() {
            match (from, to) {
                (None, None) => {}
                (Some(_), None) => ev.extend([abs(ABS_MT_SLOT, i as i32), abs(ABS_MT_TRACKING_ID, -1)]),
                (from, Some(c)) => {
                    let new = from.is_none_or(|f| f.id != c.id);
                    if !new && *from == Some(*c) {
                        continue;
                    }
                    ev.push(abs(ABS_MT_SLOT, i as i32));
                    if new {
                        ev.push(abs(ABS_MT_TRACKING_ID, c.id));
                    }
                    let f = from.filter(|_| !new);
                    let field = |code, old: Option<i32>, v: i32| (old != Some(v)).then(|| abs(code, v));
                    ev.extend(
                        [
                            field(ABS_MT_POSITION_X, f.map(|f| f.x), c.x),
                            field(ABS_MT_POSITION_Y, f.map(|f| f.y), c.y),
                            field(ABS_MT_PRESSURE, f.map(|f| f.pressure as i32), c.pressure as i32),
                            field(ABS_MT_TOUCH_MAJOR, f.map(|f| f.major as i32), c.major as i32),
                            field(ABS_MT_TOUCH_MINOR, f.map(|f| f.minor as i32), c.minor as i32),
                        ]
                        .into_iter()
                        .flatten(),
                    );
                }
            }
        }
        // Legacy single touch: the first contact, and how many there are.
        if let Some(t) = to.first() {
            if self.first().map(|f| (f.x, f.y)) != Some((t.x, t.y)) {
                ev.extend([abs(ABS_X, t.x), abs(ABS_Y, t.y)]);
            }
        }
        let (before, after) = (self.count(), to.count());
        if (before > 0) != (after > 0) {
            ev.push(key(BTN_TOUCH, after > 0));
        }
        for (n, tool) in TOOLS.into_iter().enumerate() {
            let (was, is) = (before.min(5) == n + 1, after.min(5) == n + 1);
            if was != is {
                ev.push(key(tool, is));
            }
        }
        ev
    }

    fn merge(&self, s: &Self, step: bool) -> Self {
        if step {
            return *s;
        }
        let mut out = *self;
        for (mine, theirs) in out.slots.iter_mut().zip(&s.slots) {
            // Same contact: its newer position. Anything else waits for its step.
            if let (Some(m), Some(t)) = (*mine, theirs) {
                if m.id == t.id {
                    *mine = Some(*t);
                }
            }
        }
        out
    }

    fn blip(&self, step: &Self) -> [Self; 2] {
        // Only what the step changed, leaving the other contacts where they are now.
        let mut shown = *self;
        for ((mine, theirs), (a, b)) in shown.slots.iter_mut().zip(&step.slots).zip(self.ids().zip(step.ids())) {
            if a != b {
                *mine = *theirs;
            }
        }
        [shown, shown]
    }

    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.edges.to_le_bytes());
        let mask = self.slots.iter().enumerate().fold(0u16, |m, (i, s)| m | (s.is_some() as u16) << i);
        out.extend_from_slice(&mask.to_le_bytes());
        for c in self.slots.iter().flatten() {
            for v in [c.id, c.x, c.y] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            for v in [c.pressure, c.major, c.minor] {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }

    fn get(b: &[u8]) -> Option<(Self, usize)> {
        let edges = u16::from_le_bytes(b.get(..2)?.try_into().ok()?);
        let mask = u16::from_le_bytes(b.get(2..4)?.try_into().ok()?);
        let mut at = 4;
        let mut state = TouchState { edges, ..Default::default() };
        for (i, slot) in state.slots.iter_mut().enumerate() {
            if mask >> i & 1 == 0 {
                continue;
            }
            let c = b.get(at..at + Contact::SIZE)?;
            let i32_at = |o: usize| i32::from_le_bytes(c[o..o + 4].try_into().unwrap());
            let i16_at = |o: usize| i16::from_le_bytes(c[o..o + 2].try_into().unwrap());
            *slot = Some(Contact { id: i32_at(0), x: i32_at(4), y: i32_at(8), pressure: i16_at(12), major: i16_at(14), minor: i16_at(16) });
            at += Contact::SIZE;
        }
        Some((state, at))
    }

    fn datagram(key: u32, seq: u32, time_us: u64, states: [Self; HISTORY]) -> Datagram {
        Datagram::Touch { key, seq, time_us, states }
    }

    fn edge_msg(seq: u32, time_us: u64, state: Self) -> Msg {
        Msg::TouchState { seq, time_us, state }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::REPEATS_US;

    fn ev(code: u16, value: i32) -> Event {
        Event { kind: EV_ABS, code, value }
    }

    /// A finger lands in `slot` with tracking id `id`.
    fn land(slot: i32, id: i32, x: i32, y: i32) -> Vec<Event> {
        vec![ev(ABS_MT_SLOT, slot), ev(ABS_MT_TRACKING_ID, id), ev(ABS_MT_POSITION_X, x), ev(ABS_MT_POSITION_Y, y), ev(ABS_MT_PRESSURE, 30), ev(ABS_MT_TOUCH_MAJOR, 9), ev(ABS_MT_TOUCH_MINOR, 7)]
    }

    fn lift(slot: i32) -> Vec<Event> {
        vec![ev(ABS_MT_SLOT, slot), ev(ABS_MT_TRACKING_ID, -1)]
    }

    fn slide(slot: i32, x: i32, y: i32) -> Vec<Event> {
        vec![ev(ABS_MT_SLOT, slot), ev(ABS_MT_POSITION_X, x), ev(ABS_MT_POSITION_Y, y)]
    }

    fn state(frames: &[Vec<Event>]) -> TouchState {
        let mut s = TouchState::default();
        frames.iter().for_each(|f| s.apply(f));
        s
    }

    fn touch(p: &crate::sync::Packet) -> (u32, [TouchState; HISTORY]) {
        match Datagram::decode(&p.datagram) {
            Some(Datagram::Touch { seq, states, .. }) => (seq, states),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn snapshots_round_trip_with_any_number_of_contacts() {
        let mut tx = TouchTx::new(5);
        let p = tx.frame(9, 0, &[land(0, 1, 100, 200), land(7, 2, -5, 300), land(15, 3, 1, 2)].concat());
        let d = Datagram::decode(&p.datagram).unwrap();
        assert_eq!(d.encode(), p.datagram);
        let Datagram::Touch { key: 5, seq: 1, time_us: 9, states } = d else { panic!() };
        assert_eq!(states[0].count(), 3);
        assert_eq!(states[0].slots[7], Some(Contact { id: 2, x: -5, y: 300, pressure: 30, major: 9, minor: 7 }));
        assert_eq!(states[1], TouchState::default());
        assert!(matches!(p.edge, Some(Msg::TouchState { seq: 1, .. })));
        assert_eq!(Datagram::decode(&p.datagram[..p.datagram.len() - 1]), None);
        // The worst case fits one Wi-Fi packet.
        let all: Vec<Event> = (0..16).flat_map(|i| land(i, i + 1, 1, 1)).collect();
        assert!(TouchTx::new(1).frame(0, 0, &all).datagram.len() < 1200);
    }

    #[test]
    fn diff_produces_the_protocol_b_stream_and_the_legacy_axes() {
        let none = TouchState::default();
        let one = state(&[land(0, 4, 10, 20)]);
        let events = none.diff(&one);
        assert_eq!(
            events,
            [
                ev(ABS_MT_SLOT, 0),
                ev(ABS_MT_TRACKING_ID, 4),
                ev(ABS_MT_POSITION_X, 10),
                ev(ABS_MT_POSITION_Y, 20),
                ev(ABS_MT_PRESSURE, 30),
                ev(ABS_MT_TOUCH_MAJOR, 9),
                ev(ABS_MT_TOUCH_MINOR, 7),
                ev(ABS_X, 10),
                ev(ABS_Y, 20),
                Event { kind: EV_KEY, code: BTN_TOUCH, value: 1 },
                Event { kind: EV_KEY, code: TOOLS[0], value: 1 },
            ]
        );
        let two = state(&[land(0, 4, 10, 20), land(1, 5, 50, 60)]);
        let events = one.diff(&two);
        assert!(events.contains(&Event { kind: EV_KEY, code: TOOLS[0], value: 0 }));
        assert!(events.contains(&Event { kind: EV_KEY, code: TOOLS[1], value: 1 }));
        // Moving only sends what moved.
        let moved = state(&[land(0, 4, 10, 20), slide(0, 11, 20)]);
        assert_eq!(one.diff(&moved), [ev(ABS_MT_SLOT, 0), ev(ABS_MT_POSITION_X, 11), ev(ABS_X, 11), ev(ABS_Y, 20)]);
        assert_eq!(two.diff(&one), [ev(ABS_MT_SLOT, 1), ev(ABS_MT_TRACKING_ID, -1), Event { kind: EV_KEY, code: TOOLS[0], value: 1 }, Event { kind: EV_KEY, code: TOOLS[1], value: 0 }]);
        assert!(two.diff(&two).is_empty());
    }

    #[test]
    fn a_tap_whose_down_and_up_both_missed_shows_up_from_tcp() {
        let (mut tx, mut rx) = (TouchTx::new(1), TouchSync::synced());
        let mut shown = TouchState::default();
        let mut see = |batches: Vec<Vec<Event>>| {
            let flat: Vec<Event> = batches.into_iter().flatten().collect();
            shown.apply(&flat);
            flat
        };
        let first = tx.frame(0, 0, &land(0, 1, 5, 5));
        let (s, st) = touch(&first);
        see(rx.datagram(s, &st));
        let up = tx.frame(0, 0, &lift(0));
        // A tap in a gap longer than the history, found only by TCP.
        let down2 = tx.frame(0, 0, &land(1, 2, 70, 80));
        let up2 = tx.frame(0, 0, &lift(1));
        for x in 0..5 {
            tx.frame(0, 0, &slide(0, x, x));
        }
        let (s, st) = touch(&tx.frame(0, 0, &slide(0, 9, 9)));
        let from_udp = see(rx.datagram(s, &st));
        assert!(!from_udp.iter().any(|e| e.code == ABS_MT_TRACKING_ID), "the history is too short to know");
        let mut ids = vec![];
        for edge in [up.edge, down2.edge, up2.edge] {
            let Some(Msg::TouchState { seq, state, .. }) = edge else { panic!("no edge copy") };
            ids.extend(see(rx.edge(seq, state)).iter().filter(|e| e.code == ABS_MT_TRACKING_ID).map(|e| e.value));
        }
        assert_eq!(ids, vec![-1, 2, -1], "the first finger lifts, the second taps");
        assert_eq!(rx.stats.replays, 3);
        assert_eq!(shown.count(), tx.state().count());
    }

    #[test]
    fn a_lift_replayed_late_leaves_other_contacts_where_they_are() {
        let (mut tx, mut rx) = (TouchTx::new(1), TouchSync::synced());
        let a = tx.frame(0, 0, &[land(0, 1, 0, 0), land(1, 2, 500, 500)].concat());
        let (s, st) = touch(&a);
        rx.datagram(s, &st);
        let lift_second = tx.frame(0, 0, &lift(1));
        // Two more steps, so that the datagrams can't account for the lift.
        tx.frame(0, 0, &land(2, 3, 9, 9));
        tx.frame(0, 0, &lift(2));
        for i in 0..6 {
            tx.frame(0, 0, &slide(0, 10 + i, 10 + i));
        }
        let (s, st) = touch(&tx.frame(0, 0, &slide(0, 99, 99)));
        let batches = rx.datagram(s, &st);
        assert!(batches.iter().flatten().all(|e| e.code != ABS_MT_TRACKING_ID));
        let Some(Msg::TouchState { seq, state, .. }) = lift_second.edge else { panic!() };
        let replay: Vec<Event> = rx.edge(seq, state).into_iter().flatten().collect();
        assert_eq!(replay.iter().filter(|e| e.code == ABS_MT_SLOT).map(|e| e.value).collect::<Vec<_>>(), vec![1], "only slot 1 is touched");
        assert!(replay.contains(&ev(ABS_MT_TRACKING_ID, -1)));
    }

    #[test]
    fn repeats_resend_the_same_contacts() {
        let mut tx = TouchTx::new(1);
        tx.frame(0, 0, &land(0, 1, 5, 5));
        let rep = tx.tick(REPEATS_US[0] + 1).unwrap();
        let Some(Datagram::Touch { time_us: 0, states, .. }) = Datagram::decode(&rep) else { panic!() };
        assert_eq!(states[0], states[1]);
    }

    /// Fingers landing, sliding and lifting over a bad link. The device ends
    /// up with the tablet's contacts, and has seen every contact land and
    /// lift, in order.
    #[test]
    fn random_loss_never_loses_a_contact() {
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
        for (seed, loss) in (0..100u64).zip([0, 10, 30, 60, 90].into_iter().cycle()) {
            let mut rng = Rng(seed + 1);
            let (mut tx, mut rx) = (TouchTx::new(1), TouchSync::synced());
            let mut device = TouchState::default();
            let (mut seen_ids, mut truth_ids) = (vec![], vec![]);
            let deliver = |batches: Vec<Vec<Event>>, device: &mut TouchState, seen: &mut Vec<i32>| {
                for b in batches {
                    seen.extend(b.iter().filter(|e| e.code == ABS_MT_TRACKING_ID).map(|e| e.value));
                    device.apply(&b);
                }
            };
            let mut tcp = std::collections::VecDeque::new();
            let mut next_id = 1;
            for step in 0..400u64 {
                let free: Vec<i32> = (0..SLOTS as i32).filter(|&i| tx.state().slots[i as usize].is_none()).collect();
                let busy: Vec<i32> = (0..SLOTS as i32).filter(|&i| tx.state().slots[i as usize].is_some()).collect();
                let events = match rng.next() % 10 {
                    0 | 1 if !free.is_empty() => {
                        let slot = free[rng.next() as usize % free.len()];
                        truth_ids.push(next_id);
                        next_id += 1;
                        land(slot, next_id - 1, step as i32, 0)
                    }
                    2 | 3 if !busy.is_empty() => {
                        truth_ids.push(-1);
                        lift(busy[rng.next() as usize % busy.len()])
                    }
                    _ if !busy.is_empty() => slide(busy[rng.next() as usize % busy.len()], step as i32, 7),
                    _ => continue,
                };
                let p = tx.frame(step, step * 5_000, &events);
                if !rng.chance(loss) {
                    let (seq, states) = touch(&p);
                    deliver(rx.datagram(seq, &states), &mut device, &mut seen_ids);
                }
                if let Some(Msg::TouchState { seq, state, .. }) = p.edge {
                    tcp.push_back((seq, state));
                }
                while tcp.front().is_some() && rng.chance(20) {
                    let (seq, state) = tcp.pop_front().unwrap();
                    deliver(rx.edge(seq, state), &mut device, &mut seen_ids);
                }
            }
            for after in REPEATS_US {
                if let Some(d) = tx.tick(400 * 5_000 + after + 1) {
                    if !rng.chance(loss) {
                        let Some(Datagram::Touch { seq, states, .. }) = Datagram::decode(&d) else { unreachable!() };
                        deliver(rx.datagram(seq, &states), &mut device, &mut seen_ids);
                    }
                }
            }
            for (seq, state) in tcp {
                deliver(rx.edge(seq, state), &mut device, &mut seen_ids);
            }
            assert_eq!(device.ids().collect::<Vec<_>>(), tx.state().ids().collect::<Vec<_>>(), "seed {seed} loss {loss}%: contacts");
            assert_eq!(seen_ids, truth_ids, "seed {seed} loss {loss}%: every landing and lift, in order");
            if loss <= 30 {
                assert_eq!(device.slots, tx.state().slots, "seed {seed} loss {loss}%: positions");
            }
        }
    }
}
