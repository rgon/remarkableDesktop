//! The v1 screen: a blank page with the buttons and the status box.

use crate::epd::{Epd, Mode};
use crate::gfx::{Canvas, Scaled};
use core::convert::Infallible;
use embedded_graphics::mono_font::{MonoTextStyle, ascii::FONT_10X20};
use embedded_graphics::pixelcolor::Gray8;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle};
use embedded_graphics::text::{Alignment, Text};

/// Draws in logical coordinates of a tablet rotated clockwise by `rotation`
/// onto the portrait framebuffer.
pub struct Rotated<'a, T> {
    pub target: &'a mut T,
    pub rotation: u16,
}

impl<T: OriginDimensions> Rotated<'_, T> {
    pub fn phys(&self, p: Point) -> Point {
        let s = self.target.size();
        let (w, h) = (s.width as i32, s.height as i32);
        match self.rotation {
            90 => Point::new(p.y, h - 1 - p.x),
            180 => Point::new(w - 1 - p.x, h - 1 - p.y),
            270 => Point::new(w - 1 - p.y, p.x),
            _ => p,
        }
    }
}

impl<T: OriginDimensions> OriginDimensions for Rotated<'_, T> {
    fn size(&self) -> Size {
        let s = self.target.size();
        if self.rotation % 180 == 90 { Size::new(s.height, s.width) } else { s }
    }
}

impl<T: DrawTarget<Color = Gray8, Error = Infallible> + OriginDimensions> DrawTarget for Rotated<'_, T> {
    type Color = Gray8;
    type Error = Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Gray8>>>(&mut self, pixels: I) -> Result<(), Infallible> {
        let mapped: Vec<Pixel<Gray8>> = pixels.into_iter().map(|Pixel(p, c)| Pixel(self.phys(p), c)).collect();
        self.target.draw_iter(mapped)
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Gray8) -> Result<(), Infallible> {
        let Some(br) = area.bottom_right() else { return Ok(()) };
        let (a, b) = (self.phys(area.top_left), self.phys(br));
        let tl = Point::new(a.x.min(b.x), a.y.min(b.y));
        let size = Size::new((a.x - b.x).unsigned_abs() + 1, (a.y - b.y).unsigned_abs() + 1);
        self.target.fill_solid(&Rectangle::new(tl, size), color)
    }
}

const BUTTON: u32 = 104;
const MARGIN: i32 = 20;
const GAP: i32 = 16;
pub const HOLD_STEPS: u32 = 12;

/// The hold-to-activate buttons, left to right along the top edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// Ctrl+Z. Acts on touch, no hold.
    Undo = 0,
    /// Ctrl+Y. Acts on touch, no hold.
    Redo = 1,
    /// Clicks the pen's side button where the pen is (or last was). Acts
    /// on touch, no hold.
    Side = 2,
    /// Moves the host connection off USB (to Wi-Fi), once the host has
    /// verified it can reach us there; then USB can be unplugged.
    Net = 3,
    /// Cycles which monitor absolute input maps to.
    Screen = 4,
    /// Switches the pen between drawing tablet and absolute mouse.
    Pen = 5,
    /// Switches touch between touchpad and touchscreen.
    Mode = 6,
    /// Cycles what the panel shows: the screen, nothing, or just its edges.
    Display = 7,
    /// Toggles the pen ink preview. Acts on touch, no hold.
    Ink = 8,
    /// Wipes the ink preview. Acts on touch, no hold.
    Clear = 9,
    Close = 10,
}

impl Button {
    pub const ALL: [Button; 11] = [
        Button::Undo,
        Button::Redo,
        Button::Side,
        Button::Net,
        Button::Screen,
        Button::Pen,
        Button::Mode,
        Button::Display,
        Button::Ink,
        Button::Clear,
        Button::Close,
    ];

    /// How long the button must be held (zero: acts on touch).
    pub fn hold(self) -> std::time::Duration {
        std::time::Duration::from_millis(match self {
            Button::Undo | Button::Redo | Button::Side | Button::Ink | Button::Clear => 0,
            Button::Close => 1500,
            _ => 700,
        })
    }
}

struct Slot {
    rect: Rectangle,
    /// Text shown inside; the close button draws an X instead.
    label: String,
    drawn: Option<u32>,
    /// Disabled buttons are drawn grey and ignore holds.
    enabled: bool,
}

/// Lines of the info box in the bottom-left corner.
const STATUS_LINES: usize = 3;
const STATUS_W: u32 = 800;
const STATUS_LINE_H: i32 = 44;

pub struct Ui<'a> {
    canvas: Canvas<'a>,
    rotation: u16,
    pub size: [i32; 2],
    buttons: Vec<Slot>,
    status: [String; STATUS_LINES],
    /// Battery charge in percent, shown right of the version line.
    battery: Option<u8>,
    /// The tablet's Wi-Fi address (or a placeholder), right of the HOST line.
    wifi: String,
}

impl<'a> Ui<'a> {
    pub fn new(epd: &'a Epd, rotation: u16) -> Self {
        let canvas = Canvas::new(epd, vec![epd.fb]);
        let (w, h) = (epd.width(), epd.height());
        let size = if rotation % 180 == 90 { [h, w] } else { [w, h] };
        let buttons = Button::ALL
            .iter()
            .map(|&b| {
                let from_right = (Button::ALL.len() - 1 - b as usize) as i32;
                let x = size[0] - MARGIN - BUTTON as i32 - from_right * (BUTTON as i32 + GAP);
                Slot { rect: Rectangle::new(Point::new(x, MARGIN), Size::new(BUTTON, BUTTON)), label: String::new(), drawn: None, enabled: true }
            })
            .collect();
        let status = [concat!("remdesktop v", env!("CARGO_PKG_VERSION")).to_string(), String::new(), String::new()];
        Self { canvas, rotation, size, buttons, status, battery: None, wifi: String::new() }
    }

    fn target(&mut self) -> Rotated<'_, Canvas<'a>> {
        Rotated { target: &mut self.canvas, rotation: self.rotation }
    }

    /// The panel (portrait) area covered by the buttons, so updates under it
    /// know to redraw them.
    fn status_rect(&self) -> Rectangle {
        let h = STATUS_LINE_H * STATUS_LINES as i32 + 16;
        Rectangle::new(Point::new(MARGIN, self.size[1] - MARGIN - h), Size::new(STATUS_W, h as u32))
    }

    /// Panel (portrait) pixel of a logical pixel position.
    pub fn to_panel(&self, p: [i32; 2]) -> [i32; 2] {
        let [w, h] = if self.rotation % 180 == 90 { [self.size[1], self.size[0]] } else { self.size };
        match self.rotation {
            90 => [p[1], h - 1 - p[0]],
            180 => [w - 1 - p[0], h - 1 - p[1]],
            270 => [w - 1 - p[1], p[0]],
            _ => p,
        }
    }

    fn rect_phys(&mut self, r: Rectangle) -> crate::display::Rect {
        let br = r.top_left + Point::new(r.size.width as i32 - 1, r.size.height as i32 - 1);
        let t = self.target();
        let (a, b) = (t.phys(r.top_left), t.phys(br));
        let (x, y) = (a.x.min(b.x), a.y.min(b.y));
        crate::display::Rect { x, y, w: (a.x - b.x).abs() + 1, h: (a.y - b.y).abs() + 1 }
    }

    /// Redraws the buttons and info box that panel area `r` covered (after
    /// screen content was drawn over them), and refreshes just those.
    pub fn redraw_overlays_in(&mut self, r: crate::display::Rect) {
        for b in Button::ALL {
            let rect = self.buttons[b as usize].rect;
            if self.rect_phys(rect).intersects(r) {
                let steps = self.buttons[b as usize].drawn.unwrap_or(0);
                self.draw_button(b, steps);
            }
        }
        let status = self.status_rect();
        if self.rect_phys(status).intersects(r) {
            self.draw_status();
        }
        self.canvas.flush(Mode::Ui);
    }

    /// Sets the info box's status lines (below the version), redrawing if changed.
    pub fn set_status(&mut self, usb: &str, host: &str, wifi: &str) {
        if self.status[1] == usb && self.status[2] == host && self.wifi == wifi {
            return;
        }
        self.status[1] = usb.into();
        self.status[2] = host.into();
        self.wifi = wifi.into();
        self.draw_status();
        self.canvas.flush(Mode::Ui);
    }

    /// Sets the battery percentage shown on the version line, redrawing if changed.
    pub fn set_battery(&mut self, percent: Option<u8>) {
        if self.battery == percent {
            return;
        }
        self.battery = percent;
        self.draw_status();
        self.canvas.flush(Mode::Ui);
    }

    fn draw_status(&mut self) {
        let r = self.status_rect();
        let lines = self.status.clone();
        let (battery, wifi) = (self.battery, self.wifi.clone());
        let mut t = self.target();
        let frame = PrimitiveStyleBuilder::new().fill_color(Gray8::WHITE).stroke_color(Gray8::new(0x80)).stroke_width(3).build();
        let _ = r.into_styled(frame).draw(&mut t);
        let style = MonoTextStyle::new(&FONT_10X20, Gray8::BLACK);
        for (i, line) in lines.iter().enumerate() {
            let y = r.top_left.y + 8 + STATUS_LINE_H * i as i32 + 30;
            let _ = Text::new(line, Point::new((r.top_left.x + 16) / 2, y / 2), style).draw(&mut Scaled { target: &mut t, factor: 2 });
        }
        // Right-aligned: the battery on the version line, the Wi-Fi address on the HOST line.
        let right = [(battery.map(|p| format!("{p}%")), 0), (Some(wifi), 2)];
        for (text, row) in right {
            if let Some(text) = text {
                let x = (r.top_left.x + r.size.width as i32 - 16) / 2 - 10 * text.len() as i32;
                let y = r.top_left.y + 8 + STATUS_LINE_H * row + 30;
                let _ = Text::new(&text, Point::new(x, y / 2), style).draw(&mut Scaled { target: &mut t, factor: 2 });
            }
        }
    }

    pub fn enabled(&self, b: Button) -> bool {
        self.buttons[b as usize].enabled
    }

    /// Greys a button out (or back in) and redraws it.
    pub fn set_enabled(&mut self, b: Button, enabled: bool) {
        if self.buttons[b as usize].enabled == enabled {
            return;
        }
        self.buttons[b as usize].enabled = enabled;
        let drawn = self.buttons[b as usize].drawn.is_some();
        self.draw_button(b, 0);
        if drawn {
            self.canvas.flush(Mode::Ui);
        }
    }

    /// Classifies a pen position (logical pixels): on a button, within the
    /// grace margin around the buttons, or outside. Everything but
    /// `Outside` is kept away from the computer.
    pub fn pen_zone(&self, p: [i32; 2]) -> crate::pen_gate::Zone {
        use crate::pen_gate::Zone;
        const MARGIN: i32 = 12;
        let inside = |b: Button, m: i32| {
            let tl = self.buttons[b as usize].rect.top_left;
            p[0] >= tl.x - m && p[0] < tl.x + BUTTON as i32 + m && p[1] >= tl.y - m && p[1] < tl.y + BUTTON as i32 + m
        };
        if let Some(b) = Button::ALL.into_iter().find(|&b| inside(b, 0)) {
            return Zone::On(b);
        }
        // Margins around buttons, which also covers the gaps between them.
        let first = self.buttons[0].rect.top_left;
        let last = self.buttons[self.buttons.len() - 1].rect.top_left;
        let in_bar = p[0] >= first.x - MARGIN && p[0] < last.x + BUTTON as i32 + MARGIN && p[1] >= first.y - MARGIN && p[1] < first.y + BUTTON as i32 + MARGIN;
        if in_bar { Zone::Margin } else { Zone::Outside }
    }

    /// The button under a logical pixel position (with a little slack).
    pub fn button_at(&self, p: [i32; 2]) -> Option<Button> {
        let slack = 14;
        Button::ALL.into_iter().find(|&b| {
            let r = self.buttons[b as usize].rect;
            let tl = r.top_left;
            p[0] >= tl.x - slack && p[0] < tl.x + BUTTON as i32 + slack && p[1] >= tl.y - slack && p[1] < tl.y + BUTTON as i32 + slack
        })
    }

    pub fn draw_home(&mut self) {
        let [w, h] = self.size;
        let mut t = self.target();
        let _ = t.fill_solid(&Rectangle::new(Point::zero(), Size::new(w as u32, h as u32)), Gray8::WHITE);
        drop(t);
        for b in Button::ALL {
            self.buttons[b as usize].drawn = None;
            self.draw_button(b, 0);
        }
        self.draw_status();
        self.canvas.flush(Mode::Content);
    }

    /// Changes a button's text and redraws it at once.
    pub fn set_label(&mut self, b: Button, label: &str) {
        let slot = &mut self.buttons[b as usize];
        if slot.label == label {
            return;
        }
        slot.label = label.into();
        let refresh = slot.drawn.is_some();
        slot.drawn = None;
        self.draw_button(b, 0);
        if refresh {
            self.canvas.flush(Mode::Ui);
        }
    }

    /// Fills a button from the bottom up, `steps` out of `HOLD_STEPS`, with
    /// a fast black/white refresh.
    pub fn set_progress(&mut self, b: Button, steps: u32) {
        if self.buttons[b as usize].drawn == Some(steps) {
            return;
        }
        self.draw_button(b, steps);
        self.canvas.flush(Mode::Pen);
    }

    fn draw_button(&mut self, b: Button, steps: u32) {
        let slot = &mut self.buttons[b as usize];
        slot.drawn = Some(steps);
        let (r, label, enabled) = (slot.rect, slot.label.clone(), slot.enabled);
        // Disabled: light grey outline and text, never filled.
        let line = if enabled { Gray8::BLACK } else { Gray8::new(0xb0) };
        let frame = PrimitiveStyleBuilder::new().fill_color(Gray8::WHITE).stroke_color(line).stroke_width(5).build();
        let steps = if enabled { steps } else { 0 };
        let fill_h = BUTTON * steps.min(HOLD_STEPS) / HOLD_STEPS;
        // White ink over the fill once the button is mostly covered.
        let ink = if steps * 2 > HOLD_STEPS { Gray8::WHITE } else { line };
        let mut t = self.target();
        let _ = r.into_styled(frame).draw(&mut t);
        if fill_h > 0 {
            let fill = Rectangle::new(r.top_left + Point::new(0, (BUTTON - fill_h) as i32), Size::new(BUTTON, fill_h));
            let _ = fill.into_styled(PrimitiveStyle::with_fill(Gray8::BLACK)).draw(&mut t);
        }
        if b == Button::Close {
            let pad = BUTTON as i32 / 4;
            let (tl, br) = (r.top_left + Point::new(pad, pad), r.top_left + Point::new(BUTTON as i32 - pad, BUTTON as i32 - pad));
            let cross = PrimitiveStyle::with_stroke(ink, 9);
            let _ = Line::new(tl, br).into_styled(cross).draw(&mut t);
            let _ = Line::new(Point::new(br.x, tl.y), Point::new(tl.x, br.y)).into_styled(cross).draw(&mut t);
        } else {
            // 10 px glyphs: 3 characters fit at 3x, up to 5 at 2x.
            let factor = if label.len() <= 3 { 3 } else { 2 };
            let style = MonoTextStyle::new(&FONT_10X20, ink);
            let c = r.center();
            let _ = Text::with_alignment(&label, Point::new(c.x / factor, c.y / factor + 6), style, Alignment::Center)
                .draw(&mut Scaled { target: &mut t, factor });
        }
    }
}
