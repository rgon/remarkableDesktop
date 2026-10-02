//! Confining absolute pen/touch input to one monitor.
//!
//! Compositors map an unassigned absolute device across the whole desktop
//! (the bounding box of all outputs). To target one monitor we squeeze the
//! device's full range into that monitor's share of the bounding box.

use crate::outputs::Output;
use remvnc_proto::{Device, Event};

const EV_ABS: u16 = 3;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const INPUT_PROP_DIRECT: u32 = 1;

#[derive(Debug, Clone, Copy)]
struct Axis {
    min: i32,
    max: i32,
    /// Part of the tablet showing the target, as fractions of the device
    /// range (less than 0..1 when the picture is letterboxed).
    shown: (f64, f64),
    /// Target span as fractions of the desktop bounding box.
    from: f64,
    to: f64,
}

impl Axis {
    fn map(&self, v: i32) -> i32 {
        let range = (self.max - self.min) as f64;
        let f = (v - self.min) as f64 / range;
        let f = ((f - self.shown.0) / (self.shown.1 - self.shown.0)).clamp(0.0, 1.0);
        (self.min as f64 + (self.from + f * (self.to - self.from)) * range).round() as i32
    }
}

/// Where a target monitor appears on the tablet when its picture is fitted
/// into the tablet's `logical` size: (x0, x1, y0, y1) as fractions.
pub fn letterbox(logical: (u32, u32), target: &Output) -> [f64; 4] {
    let (lw, lh) = (logical.0 as f64, logical.1 as f64);
    let s = (lw / target.w as f64).min(lh / target.h as f64);
    let (cw, ch) = (target.w as f64 * s / lw, target.h as f64 * s / lh);
    [(1.0 - cw) / 2.0, (1.0 + cw) / 2.0, (1.0 - ch) / 2.0, (1.0 + ch) / 2.0]
}

/// The target's share of the desktop bounding box, (x0, x1, y0, y1) as
/// fractions: where an absolute pointer spanning the desktop must land.
pub fn desktop_span(outputs: &[Output], target: Option<&Output>) -> [f64; 4] {
    let Some(t) = target.filter(|_| !outputs.is_empty()) else { return FULL };
    let x0 = outputs.iter().map(|o| o.x).min().unwrap();
    let y0 = outputs.iter().map(|o| o.y).min().unwrap();
    let bw = (outputs.iter().map(|o| o.x + o.w).max().unwrap() - x0) as f64;
    let bh = (outputs.iter().map(|o| o.y + o.h).max().unwrap() - y0) as f64;
    [(t.x - x0) as f64 / bw, (t.x + t.w - x0) as f64 / bw, (t.y - y0) as f64 / bh, (t.y + t.h - y0) as f64 / bh]
}

/// The whole tablet shows the target.
pub const FULL: [f64; 4] = [0.0, 1.0, 0.0, 1.0];

/// Per-device remapping of X/Y axes.
#[derive(Default)]
pub struct Mapper {
    axes: Option<[Axis; 2]>,
}

impl Mapper {
    /// `target` = None maps across the whole desktop (no change). `shown`
    /// is the part of the tablet that shows it (see `letterbox`).
    pub fn new(device: &Device, outputs: &[Output], target: Option<&Output>, shown: [f64; 4]) -> Self {
        let Some(t) = target else { return Self::default() };
        // Touchpads move a relative pointer; only absolute devices are confined.
        let absolute = device.name.contains("pen") || device.props & (1 << INPUT_PROP_DIRECT) != 0;
        if !absolute || outputs.is_empty() {
            return Self::default();
        }
        let x0 = outputs.iter().map(|o| o.x).min().unwrap();
        let y0 = outputs.iter().map(|o| o.y).min().unwrap();
        let x1 = outputs.iter().map(|o| o.x + o.w).max().unwrap();
        let y1 = outputs.iter().map(|o| o.y + o.h).max().unwrap();
        let range = |code| device.abs.iter().find(|a| a.code == code).map(|a| (a.min, a.max)).unwrap_or((0, 1));
        let (xmin, xmax) = range(ABS_X);
        let (ymin, ymax) = range(ABS_Y);
        let (bw, bh) = ((x1 - x0) as f64, (y1 - y0) as f64);
        Self {
            axes: Some([
                Axis { min: xmin, max: xmax, shown: (shown[0], shown[1]), from: (t.x - x0) as f64 / bw, to: (t.x + t.w - x0) as f64 / bw },
                Axis { min: ymin, max: ymax, shown: (shown[2], shown[3]), from: (t.y - y0) as f64 / bh, to: (t.y + t.h - y0) as f64 / bh },
            ]),
        }
    }

    /// For devices the compositor already maps onto the target monitor:
    /// only stretch the part of the tablet that shows it (`shown`) to the
    /// device's full range, so letterboxed areas don't distort the aspect.
    pub fn letterboxed(device: &Device, shown: [f64; 4]) -> Self {
        if shown == FULL {
            return Self::default();
        }
        let range = |code| device.abs.iter().find(|a| a.code == code).map(|a| (a.min, a.max)).unwrap_or((0, 1));
        let ((xmin, xmax), (ymin, ymax)) = (range(ABS_X), range(ABS_Y));
        Self {
            axes: Some([
                Axis { min: xmin, max: xmax, shown: (shown[0], shown[1]), from: 0.0, to: 1.0 },
                Axis { min: ymin, max: ymax, shown: (shown[2], shown[3]), from: 0.0, to: 1.0 },
            ]),
        }
    }

    pub fn apply(&self, events: &mut [Event]) {
        let Some(axes) = &self.axes else { return };
        for e in events.iter_mut().filter(|e| e.kind == EV_ABS) {
            match e.code {
                ABS_X | ABS_MT_POSITION_X => e.value = axes[0].map(e.value),
                ABS_Y | ABS_MT_POSITION_Y => e.value = axes[1].map(e.value),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remvnc_proto::Abs;

    #[test]
    fn squeezes_into_right_monitor() {
        let outs = vec![
            Output { name: "A".into(), x: 0, y: 0, w: 1000, h: 1000 },
            Output { name: "B".into(), x: 1000, y: 0, w: 1000, h: 1000 },
        ];
        let dev = Device {
            id: 1,
            name: "remvnc pen".into(),
            props: 0,
            keys: vec![],
            abs: vec![Abs { code: ABS_X, min: 0, max: 100, resolution: 0 }, Abs { code: ABS_Y, min: 0, max: 100, resolution: 0 }],
        };
        let m = Mapper::new(&dev, &outs, Some(&outs[1]), FULL);
        let mut ev = [Event { kind: EV_ABS, code: ABS_X, value: 0 }, Event { kind: EV_ABS, code: ABS_X, value: 100 }];
        m.apply(&mut ev);
        assert_eq!((ev[0].value, ev[1].value), (50, 100));
        // 16:9 monitor on a 4:3 tablet: bars above and below.
        let lb = letterbox((1600, 1200), &Output { name: "W".into(), x: 0, y: 0, w: 1600, h: 900 });
        assert_eq!(lb, [0.0, 1.0, 0.125, 0.875]);
    }
}
