//! The v1 screen: "remote controlling" and a hold-to-close X button.

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
    fn phys(&self, p: Point) -> Point {
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

const BUTTON: u32 = 180;
const MARGIN: i32 = 48;
const GAP: i32 = 40;
pub const HOLD_STEPS: u32 = 12;

/// The hold-to-activate buttons, left to right along the top edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// Cycles which monitor absolute input maps to.
    Screen = 0,
    /// Switches the pen between drawing tablet and absolute mouse.
    Pen = 1,
    /// Switches touch between touchpad and touchscreen.
    Mode = 2,
    Close = 3,
}

impl Button {
    pub const ALL: [Button; 4] = [Button::Screen, Button::Pen, Button::Mode, Button::Close];

    /// How long the button must be held.
    pub fn hold(self) -> std::time::Duration {
        std::time::Duration::from_millis(if self == Button::Close { 1500 } else { 700 })
    }
}

struct Slot {
    rect: Rectangle,
    /// Text shown inside; the close button draws an X instead.
    label: String,
    drawn: Option<u32>,
}

pub struct Ui<'a> {
    canvas: Canvas<'a>,
    rotation: u16,
    pub size: [i32; 2],
    buttons: Vec<Slot>,
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
                Slot { rect: Rectangle::new(Point::new(x, MARGIN), Size::new(BUTTON, BUTTON)), label: String::new(), drawn: None }
            })
            .collect();
        Self { canvas, rotation, size, buttons }
    }

    fn target(&mut self) -> Rotated<'_, Canvas<'a>> {
        Rotated { target: &mut self.canvas, rotation: self.rotation }
    }

    /// The button under a logical pixel position (with a little slack).
    pub fn button_at(&self, p: [i32; 2]) -> Option<Button> {
        let slack = GAP / 2;
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
        let big = MonoTextStyle::new(&FONT_10X20, Gray8::BLACK);
        let _ = Text::with_alignment("remote controlling", Point::new(w / 12, h / 12), big, Alignment::Center)
            .draw(&mut Scaled { target: &mut t, factor: 6 });
        let _ = Text::with_alignment("hold a button to use it", Point::new(w / 8, h / 8 + 40), big, Alignment::Center)
            .draw(&mut Scaled { target: &mut t, factor: 4 });
        drop(t);
        for b in Button::ALL {
            self.buttons[b as usize].drawn = None;
            self.draw_button(b, 0);
        }
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
        let (r, label) = (slot.rect, slot.label.clone());
        let frame = PrimitiveStyleBuilder::new().fill_color(Gray8::WHITE).stroke_color(Gray8::BLACK).stroke_width(8).build();
        let fill_h = BUTTON * steps.min(HOLD_STEPS) / HOLD_STEPS;
        // White ink over the fill once the button is mostly covered.
        let ink = if steps * 2 > HOLD_STEPS { Gray8::WHITE } else { Gray8::BLACK };
        let mut t = self.target();
        let _ = r.into_styled(frame).draw(&mut t);
        if fill_h > 0 {
            let fill = Rectangle::new(r.top_left + Point::new(0, (BUTTON - fill_h) as i32), Size::new(BUTTON, fill_h));
            let _ = fill.into_styled(PrimitiveStyle::with_fill(Gray8::BLACK)).draw(&mut t);
        }
        if b == Button::Close {
            let pad = 48;
            let (tl, br) = (r.top_left + Point::new(pad, pad), r.top_left + Point::new(BUTTON as i32 - pad, BUTTON as i32 - pad));
            let cross = PrimitiveStyle::with_stroke(ink, 14);
            let _ = Line::new(tl, br).into_styled(cross).draw(&mut t);
            let _ = Line::new(Point::new(br.x, tl.y), Point::new(tl.x, br.y)).into_styled(cross).draw(&mut t);
        } else {
            // Up to 4 characters fit at 4x; longer labels drop to 3x.
            let factor = if label.len() <= 4 { 4 } else { 3 };
            let style = MonoTextStyle::new(&FONT_10X20, ink);
            let c = r.center();
            let _ = Text::with_alignment(&label, Point::new(c.x / factor, c.y / factor + 6), style, Alignment::Center)
                .draw(&mut Scaled { target: &mut t, factor });
        }
    }
}
