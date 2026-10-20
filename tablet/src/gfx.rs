//! embedded-graphics drawing target over the e-ink framebuffer.

use crate::epd::{Epd, ImageInfo, Mode};
use core::convert::Infallible;
use embedded_graphics::pixelcolor::{Gray8, GrayColor};
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;

/// Draws into the framebuffer and remembers the bounding box of what changed.
pub struct Canvas<'a> {
    epd: &'a Epd,
    images: Vec<ImageInfo>,
    dirty: Option<(i32, i32, i32, i32)>,
}

impl<'a> Canvas<'a> {
    /// Draws into each of `images` (same size as the screen).
    pub fn new(epd: &'a Epd, images: Vec<ImageInfo>) -> Self {
        Self { epd, images, dirty: None }
    }

    #[inline]
    fn put(&self, x: i32, y: i32, v: u8) {
        for im in &self.images {
            unsafe { im.set_gray(x, y, v) };
        }
    }

    fn mark(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.dirty = Some(match self.dirty {
            None => (x0, y0, x1, y1),
            Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
        });
    }

    /// Refreshes the changed region, if any.
    pub fn flush(&mut self, mode: Mode) {
        if let Some((x0, y0, x1, y1)) = self.dirty.take() {
            self.epd.swap(x0, y0, x1 - x0, y1 - y0, mode, 0);
        }
    }
}

impl OriginDimensions for Canvas<'_> {
    fn size(&self) -> Size {
        Size::new(self.epd.width() as u32, self.epd.height() as u32)
    }
}

impl DrawTarget for Canvas<'_> {
    type Color = Gray8;
    type Error = Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Gray8>>>(&mut self, pixels: I) -> Result<(), Infallible> {
        let (w, h) = (self.epd.width(), self.epd.height());
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 && p.x < w && p.y < h {
                self.put(p.x, p.y, c.luma());
                self.mark(p.x, p.y, p.x + 1, p.y + 1);
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Gray8) -> Result<(), Infallible> {
        let area = area.intersection(&self.bounding_box());
        let Some(br) = area.bottom_right() else { return Ok(()) };
        for im in &self.images {
            unsafe { im.fill_gray(area.top_left.x, area.top_left.y, br.x + 1, br.y + 1, color.luma()) };
        }
        self.mark(area.top_left.x, area.top_left.y, br.x + 1, br.y + 1);
        Ok(())
    }
}

/// Integer-scales everything drawn through it, so the small built-in mono
/// fonts are legible at 226 DPI.
pub struct Scaled<'a, T> {
    pub target: &'a mut T,
    pub factor: i32,
}

impl<T: OriginDimensions> OriginDimensions for Scaled<'_, T> {
    fn size(&self) -> Size {
        self.target.size() / self.factor as u32
    }
}

impl<T: DrawTarget<Color = Gray8, Error = Infallible> + OriginDimensions> DrawTarget for Scaled<'_, T> {
    type Color = Gray8;
    type Error = Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Gray8>>>(&mut self, pixels: I) -> Result<(), Infallible> {
        let f = self.factor;
        for Pixel(p, c) in pixels {
            let r = Rectangle::new(Point::new(p.x * f, p.y * f), Size::new(f as u32, f as u32));
            self.target.fill_solid(&r, c)?;
        }
        Ok(())
    }
}
