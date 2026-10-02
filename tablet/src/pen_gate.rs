//! Keeps the pen off the computer while it is over our buttons: entering
//! the button area makes the pen leave proximity there (so the cursor stays
//! put and nothing clicks or drags), and taps inside it press our buttons
//! instead. The pen comes back once it hovers outside again.

use crate::ui::Button;
use remdesktop_proto::Event;

const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOOL_RUBBER: u16 = 0x141;
const BTN_TOUCH: u16 = 0x14a;

/// Where a position falls relative to our buttons.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zone {
    Outside,
    /// Within the grace margin around (or between) buttons.
    Margin,
    On(Button),
}

#[derive(Default)]
pub struct PenGate {
    /// Pen position in device units (logical orientation), as last reported.
    pos: [i32; 2],
    in_range: bool,
    rubber: bool,
    /// Tip down (mouse mode: a button down).
    tip: bool,
    /// Currently hidden from the computer.
    blocked: bool,
    /// The computer currently sees the pen in range (tablet mode).
    host_in_range: bool,
    /// Last position the computer saw: where the side button clicks.
    pub anchor: [i32; 2],
}

pub struct Gated {
    /// Events to forward, if any (possibly synthesized leave/enter).
    pub events: Option<Vec<Event>>,
    /// The button under the tip, while the tip is down on one.
    pub pressing: Option<Button>,
}

impl PenGate {
    /// Whether the computer sees the pen hovering (for side clicks).
    pub fn host_in_range(&self) -> bool {
        self.host_in_range
    }

    /// Filters one pen frame. `zone` classifies a device position.
    pub fn filter(&mut self, events: Vec<Event>, mouse: bool, zone: impl Fn([i32; 2]) -> Zone) -> Gated {
        for e in &events {
            match (e.kind, e.code) {
                (EV_ABS, ABS_X) => self.pos[0] = e.value,
                (EV_ABS, ABS_Y) => self.pos[1] = e.value,
                (EV_KEY, BTN_TOOL_PEN) => self.in_range = e.value != 0 || self.rubber,
                (EV_KEY, BTN_TOOL_RUBBER) => {
                    self.rubber = e.value != 0;
                    self.in_range = e.value != 0;
                }
                (EV_KEY, BTN_TOUCH) if !mouse => self.tip = e.value != 0,
                (EV_KEY, BTN_LEFT | BTN_RIGHT) if mouse => self.tip = e.value != 0,
                _ => {}
            }
        }
        let key = |code, value| Event { kind: EV_KEY, code, value };
        let at = |p: [i32; 2]| [Event { kind: EV_ABS, code: ABS_X, value: p[0] }, Event { kind: EV_ABS, code: ABS_Y, value: p[1] }];
        let z = if self.in_range || self.tip { zone(self.pos) } else { Zone::Outside };
        if z != Zone::Outside {
            let pressing = match z {
                Zone::On(b) if self.tip => Some(b),
                _ => None,
            };
            if self.blocked {
                return Gated { events: None, pressing };
            }
            self.blocked = true;
            // Leave proximity on the computer: release everything.
            let leave = if mouse {
                vec![key(BTN_LEFT, 0), key(BTN_RIGHT, 0)]
            } else {
                self.host_in_range = false;
                vec![key(BTN_TOUCH, 0), key(BTN_TOOL_PEN, 0), key(BTN_TOOL_RUBBER, 0)]
            };
            return Gated { events: Some(leave), pressing };
        }
        if self.blocked {
            // Back outside: resume only once hovering, so leaving the button
            // area with the tip down doesn't start a stroke midway.
            if self.tip || !self.in_range {
                if !self.in_range {
                    self.blocked = false;
                }
                return Gated { events: None, pressing: None };
            }
            self.blocked = false;
            let mut enter = vec![];
            if !mouse {
                enter.push(key(if self.rubber { BTN_TOOL_RUBBER } else { BTN_TOOL_PEN }, 1));
                self.host_in_range = true;
            }
            enter.extend(at(self.pos));
            self.anchor = self.pos;
            return Gated { events: Some(enter), pressing: None };
        }
        if !mouse {
            for e in &events {
                if e.kind == EV_KEY && matches!(e.code, BTN_TOOL_PEN | BTN_TOOL_RUBBER) {
                    self.host_in_range = e.value != 0;
                }
            }
        }
        self.anchor = self.pos;
        Gated { events: Some(events), pressing: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: u16, code: u16, value: i32) -> Event {
        Event { kind, code, value }
    }

    #[test]
    fn hides_the_pen_over_buttons_and_resumes_on_hover() {
        // Buttons live at x >= 100.
        let zone = |p: [i32; 2]| if p[0] >= 100 { Zone::On(Button::Undo) } else { Zone::Outside };
        let mut g = PenGate::default();
        let r = g.filter(vec![ev(EV_KEY, BTN_TOOL_PEN, 1), ev(EV_ABS, ABS_X, 10)], false, zone);
        assert_eq!(r.events.unwrap().len(), 2);
        // Into the button area: a leave-proximity frame, then nothing.
        let r = g.filter(vec![ev(EV_ABS, ABS_X, 150)], false, zone);
        assert!(r.events.unwrap().contains(&ev(EV_KEY, BTN_TOOL_PEN, 0)));
        assert!(g.filter(vec![ev(EV_ABS, ABS_X, 160)], false, zone).events.is_none());
        // Tip down on the button presses it, without forwarding anything.
        let r = g.filter(vec![ev(EV_KEY, BTN_TOUCH, 1)], false, zone);
        assert_eq!((r.events.is_none(), r.pressing), (true, Some(Button::Undo)));
        // Dragged out with the tip still down: still hidden.
        assert!(g.filter(vec![ev(EV_ABS, ABS_X, 50)], false, zone).events.is_none());
        // Tip lifted outside: the pen re-enters at its position.
        let r = g.filter(vec![ev(EV_KEY, BTN_TOUCH, 0)], false, zone).events.unwrap();
        assert!(r.contains(&ev(EV_KEY, BTN_TOOL_PEN, 1)) && r.contains(&ev(EV_ABS, ABS_X, 50)));
        assert_eq!(g.anchor[0], 50);
    }
}
