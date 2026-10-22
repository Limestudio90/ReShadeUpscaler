//! Disegno dell'interfaccia con Direct2D in modalita' software su un DC GDI.
//!
//! Niente swapchain: ReShade elabora solo le swapchain DXGI, quindi l'interfaccia resta
//! fuori dagli effetti. Le coordinate sono in DIP (96 dpi), il fattore di scala lo applica D2D.

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::HDC;
use windows_numerics::Vector2;

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct R {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl R {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
    pub fn inset(&self, dx: f32, dy: f32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w - 2.0 * dx, self.h - 2.0 * dy)
    }
    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.right(), bottom: self.bottom() }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

fn color(rgb: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 255) as f32 / 255.0,
        g: ((rgb >> 8) & 255) as f32 / 255.0,
        b: (rgb & 255) as f32 / 255.0,
        a,
    }
}

pub struct Fonts {
    pub title: IDWriteTextFormat,
    pub body: IDWriteTextFormat,
    pub body_b: IDWriteTextFormat,
    pub small: IDWriteTextFormat,
    pub small_b: IDWriteTextFormat,
    pub big: IDWriteTextFormat,
    pub icon: IDWriteTextFormat,
    pub icon_lg: IDWriteTextFormat,
    pub icon_sm: IDWriteTextFormat,
}

fn text_format(dw: &IDWriteFactory, family: PCWSTR, weight: DWRITE_FONT_WEIGHT, size: f32) -> Result<IDWriteTextFormat> {
    unsafe {
        let f = dw.CreateTextFormat(family, None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("it-it"))?;
        f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        let sign = dw.CreateEllipsisTrimmingSign(&f)?;
        let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
        f.SetTrimming(&trimming, &sign)?;
        Ok(f)
    }
}

pub struct Painter {
    rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
    pub fonts: Fonts,
    bitmaps: [Option<(ID2D1Bitmap, u32, u32)>; 2],
}

impl Painter {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = factory.CreateDCRenderTarget(&props)?;
            let brush = rt.CreateSolidColorBrush(&color(0xffffff, 1.0), None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let ui = w!("Segoe UI Variable Text");
            let fonts = Fonts {
                title: text_format(&dw, w!("Segoe UI Variable Display"), DWRITE_FONT_WEIGHT_SEMI_BOLD, 19.0)?,
                body: text_format(&dw, ui, DWRITE_FONT_WEIGHT_NORMAL, 13.5)?,
                body_b: text_format(&dw, ui, DWRITE_FONT_WEIGHT_SEMI_BOLD, 13.5)?,
                small: text_format(&dw, ui, DWRITE_FONT_WEIGHT_NORMAL, 12.0)?,
                small_b: text_format(&dw, ui, DWRITE_FONT_WEIGHT_SEMI_BOLD, 11.0)?,
                big: text_format(&dw, w!("Segoe UI Variable Display"), DWRITE_FONT_WEIGHT_SEMI_BOLD, 17.0)?,
                icon: text_format(&dw, w!("Segoe Fluent Icons"), DWRITE_FONT_WEIGHT_NORMAL, 15.0)?,
                icon_lg: text_format(&dw, w!("Segoe Fluent Icons"), DWRITE_FONT_WEIGHT_NORMAL, 40.0)?,
                icon_sm: text_format(&dw, w!("Segoe Fluent Icons"), DWRITE_FONT_WEIGHT_NORMAL, 9.0)?,
            };
            Ok(Self { rt, brush, fonts, bitmaps: [None, None] })
        }
    }

    /// Inizia a disegnare su `hdc` (rettangolo in pixel) con il fattore di scala dato.
    pub fn begin(&self, hdc: HDC, rc: &RECT, scale: f32) -> bool {
        unsafe {
            if self.rt.BindDC(hdc, rc).is_err() {
                return false;
            }
            self.rt.SetDpi(96.0 * scale, 96.0 * scale);
            self.rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            self.rt.BeginDraw();
        }
        true
    }

    pub fn end(&self) {
        unsafe {
            let _ = self.rt.EndDraw(None, None);
        }
    }

    fn set(&self, rgb: u32, a: f32) {
        unsafe { self.brush.SetColor(&color(rgb, a)) };
    }

    pub fn clear(&self, rgb: u32) {
        unsafe { self.rt.Clear(Some(&color(rgb, 1.0))) };
    }

    pub fn rect(&self, r: R, rgb: u32, a: f32) {
        self.set(rgb, a);
        unsafe { self.rt.FillRectangle(&r.d2d(), &self.brush) };
    }

    pub fn round(&self, r: R, radius: f32, rgb: u32, a: f32) {
        self.set(rgb, a);
        let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
        unsafe { self.rt.FillRoundedRectangle(&rr, &self.brush) };
    }

    pub fn round_stroke(&self, r: R, radius: f32, rgb: u32, a: f32, width: f32) {
        self.set(rgb, a);
        let r = r.inset(width / 2.0, width / 2.0);
        let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
        unsafe { self.rt.DrawRoundedRectangle(&rr, &self.brush, width, None) };
    }

    pub fn circle(&self, cx: f32, cy: f32, radius: f32, rgb: u32, a: f32) {
        self.set(rgb, a);
        let e = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: radius, radiusY: radius };
        unsafe { self.rt.FillEllipse(&e, &self.brush) };
    }

    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, rgb: u32, a: f32, width: f32) {
        self.set(rgb, a);
        unsafe { self.rt.DrawLine(Vector2 { X: x0, Y: y0 }, Vector2 { X: x1, Y: y1 }, &self.brush, width, None) };
    }

    pub fn text(&self, s: &str, font: &IDWriteTextFormat, r: R, rgb: u32, a: f32, align: Align) {
        let wide: Vec<u16> = s.encode_utf16().collect();
        self.set(rgb, a);
        unsafe {
            let _ = font.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            self.rt.DrawText(&wide, font, &r.d2d(), &self.brush, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
        }
    }

    /// Disegna un fotogramma BGRA (righe compatte) nella zona `dst`, ritagliando `src` (pixel).
    pub fn image(&mut self, slot: usize, pixels: &[u8], w: u32, h: u32, dst: R, src: R) {
        if w == 0 || h == 0 || pixels.len() < (w * h * 4) as usize {
            return;
        }
        unsafe {
            let reuse = matches!(&self.bitmaps[slot], Some((_, bw, bh)) if *bw == w && *bh == h);
            if !reuse {
                let props = D2D1_BITMAP_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                    dpiX: 96.0,
                    dpiY: 96.0,
                };
                self.bitmaps[slot] = self
                    .rt
                    .CreateBitmap(D2D_SIZE_U { width: w, height: h }, None, 0, &props)
                    .ok()
                    .map(|b| (b, w, h));
            }
            let Some((bitmap, _, _)) = &self.bitmaps[slot] else { return };
            let _ = bitmap.CopyFromMemory(None, pixels.as_ptr() as _, w * 4);
            self.rt.DrawBitmap(bitmap, Some(&dst.d2d()), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, Some(&src.d2d()));
        }
    }
}
