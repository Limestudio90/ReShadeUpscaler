//! ReShade Upscaler: anteprima ed esportazione di un video attraverso ReShade + DLSS 5.
//!
//! Finestre: la principale disegna l'interfaccia (Direct2D software, fuori da ReShade); dentro
//! ci sono due figlie sovrapposte nella zona video: `preview` con la swapchain D3D11 che
//! ReShade elabora, e `compare` (Direct2D) che mostra originale o split durante il confronto.

#![windows_subsystem = "windows"]

mod export;
mod framing;
mod media;
mod render;
mod ui;
mod worker;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use export::Step;
use framing::*;
use media::*;
use render::Renderer;
use ui::{Align, Painter, R};
use worker::RemoteExport;

// --- aspetto -----------------------------------------------------------------------------------

const SIDE_W: f32 = 348.0;
const TRANSPORT_H: f32 = 78.0;
const MARGIN: f32 = 18.0;

const BG: u32 = 0x0F0F13;
const PANEL: u32 = 0x16161B;
const CARD: u32 = 0x1D1D24;
const FIELD: u32 = 0x1A1A20;
const BORDER: u32 = 0x2A2A34;
const TEXT: u32 = 0xECECF1;
const MUTED: u32 = 0x8C8C9A;
const FAINT: u32 = 0x5E5E6B;
const ACCENT: u32 = 0x7C6CFF;
const ACCENT_HOVER: u32 = 0x8E80FF;
const SUCCESS: u32 = 0x3DD68C;
const DANGER: u32 = 0xFF6B78;

// --- controlli ---------------------------------------------------------------------------------

const ID_NONE: u32 = 0;
const ID_IMPORT: u32 = 1;
const ID_PLAY: u32 = 2;
const ID_SEEK: u32 = 3;
const ID_EXPORT: u32 = 4;
const ID_CANCEL: u32 = 5;
const ID_OPEN: u32 = 6;
const ID_FOLDER: u32 = 7;
const ID_FOLDER_RESET: u32 = 8;
const ID_CENTER: u32 = 9;
const ID_CMP: u32 = 20; // +0..2
const ID_SCALE: u32 = 30; // +0..2
const ID_ASPECT: u32 = 40; // +0..5
const ID_FIT: u32 = 50; // +0..2
const ID_BARS: u32 = 60; // +0..1
const ID_CODEC: u32 = 70; // +0..1
const ID_QUAL: u32 = 80; // +0..2

const WM_MOUSELEAVE_: u32 = 0x02A3; // non esportato dalla feature WindowsAndMessaging

const CMP_RESHADE: usize = 0;
const CMP_SPLIT: usize = 2;

enum Request {
    Import,
    PickFolder,
    Load(PathBuf),
    Message(String),
    Quit(i32),
    ConfirmExport,
}

struct Hit {
    id: u32,
    r: R,
    enabled: bool,
}

#[derive(Default)]
struct TestOptions {
    export_on_load: bool,
    auto_export_at: Option<f64>,
    frames: Option<i64>,
}

struct App {
    hwnd: HWND,
    preview: HWND,
    compare: HWND,
    painter: Painter,
    cmp_painter: Painter,
    r: Renderer,
    vdec: VideoDecoder,
    audio: AudioPlayer,
    path: Option<PathBuf>,
    info: Option<VideoInfo>,
    set: Settings,

    // riproduzione: orologio dell'app, video e audio ripartono insieme a ogni seek
    playing: bool,
    started: bool,
    pos_base: f64,
    clock_start: f64,
    seek_time: f64,
    next_index: i64,
    need_frame: bool,

    // confronto
    cmp_mode: usize,
    split: f32,
    orig: Vec<u8>,
    processed: Vec<u8>,
    cmp_size: (u32, u32),
    split_drag: bool,
    crop_drag: Option<(i32, i32)>,

    preview_rect: RECT,
    view: [u32; 4],
    resize_pending: Option<f64>,

    hits: Vec<Hit>,
    hot: u32,
    active: u32,
    tracking: bool,
    scrub: Option<f64>,
    scrub_dirty: bool,
    last_scrub: f64,
    scrub_resume: bool,

    export: Option<RemoteExport>,
    resume: (f64, bool),
    last_result: Option<Result<PathBuf, String>>,
    requests: VecDeque<Request>,
    last_ui: f64,
    /// NeuralUplift in ReShade.ini (il tasto Canc lo spegne e ReShade lo salva: vale anche per l'export)
    nr_on: Option<bool>,
    nr_checked: f64,
    test: TestOptions,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static NEED_PAINT: Cell<bool> = const { Cell::new(false) };
}

/// Accesso allo stato dall'interno delle window procedure. Se lo stato e' gia' in uso (un
/// messaggio inviato in modo sincrono mentre l'app lavora) restituisce None.
fn with_app<T>(f: impl FnOnce(&mut App) -> T) -> Option<T> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn now() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn fmt_time(t: f64) -> String {
    let s = t.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn fmt_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.01 { format!("{}", fps.round()) } else { format!("{fps:.2}") }
}

fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32)
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    (rc.right - rc.left, rc.bottom - rc.top)
}

fn dpi_scale(hwnd: HWND) -> f32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

// --- layout ------------------------------------------------------------------------------------

struct Layout {
    side: R,
    transport: R,
    preview: R,
    video: R,
}

fn layout(cw: f32, ch: f32, aspect: f64) -> Layout {
    let side = R::new(cw - SIDE_W, 0.0, SIDE_W, ch);
    let transport = R::new(0.0, ch - TRANSPORT_H, cw - SIDE_W, TRANSPORT_H);
    let preview = R::new(MARGIN, MARGIN, (cw - SIDE_W - 2.0 * MARGIN).max(1.0), (ch - TRANSPORT_H - MARGIN).max(1.0));
    let area = preview.inset(12.0, 12.0);
    let a = aspect as f32;
    let (mut w, mut h) = (area.w, area.w / a);
    if h > area.h {
        h = area.h;
        w = h * a;
    }
    let video = R::new(area.cx() - w / 2.0, area.cy() - h / 2.0, w, h);
    Layout { side, transport, preview, video }
}

// --- widget ------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

struct Ui<'a> {
    p: &'a Painter,
    hits: Vec<Hit>,
    hot: u32,
    active: u32,
}

impl Ui<'_> {
    fn hit(&mut self, id: u32, r: R, enabled: bool) {
        self.hits.push(Hit { id, r, enabled });
    }

    fn button(&mut self, id: u32, r: R, icon: &str, label: &str, kind: Kind, enabled: bool) {
        let p = self.p;
        let hover = enabled && self.hot == id;
        let pressed = hover && self.active == id;
        let alpha = if enabled { 1.0 } else { 0.4 };
        let fg = match kind {
            Kind::Primary => {
                p.round(r, 10.0, if pressed { 0x6A5AE8 } else if hover { ACCENT_HOVER } else { ACCENT }, alpha);
                0xFFFFFF
            }
            Kind::Secondary => {
                p.round(r, 8.0, if hover { 0x2A2A34 } else { 0x22222A }, alpha);
                p.round_stroke(r, 8.0, BORDER, alpha, 1.0);
                TEXT
            }
            Kind::Ghost => {
                if hover {
                    p.round(r, 7.0, 0x26262F, 1.0);
                }
                if hover { TEXT } else { MUTED }
            }
            Kind::Danger => {
                p.round(r, 8.0, if hover { 0x3A1F26 } else { 0x2A1A1F }, alpha);
                p.round_stroke(r, 8.0, DANGER, 0.55 * alpha, 1.0);
                0xFF9AA4
            }
        };
        let font = if kind == Kind::Ghost { &p.fonts.small } else { &p.fonts.body_b };
        if icon.is_empty() {
            p.text(label, font, r, fg, alpha, Align::Center);
        } else if label.is_empty() {
            p.text(icon, &p.fonts.icon, r, fg, alpha, Align::Center);
        } else {
            let est = label.chars().count() as f32 * 7.2 + 24.0;
            let x = r.cx() - est / 2.0;
            p.text(icon, &p.fonts.icon, R::new(x, r.y, 18.0, r.h), fg, alpha, Align::Left);
            p.text(label, font, R::new(x + 24.0, r.y, r.right() - x - 24.0, r.h), fg, alpha, Align::Left);
        }
        self.hit(id, r, enabled);
    }

    fn segmented(&mut self, base: u32, r: R, labels: &[&str], cols: usize, selected: usize, enabled: bool) {
        let p = self.p;
        let alpha = if enabled { 1.0 } else { 0.45 };
        p.round(r, 9.0, FIELD, 1.0);
        p.round_stroke(r, 9.0, BORDER, 1.0, 1.0);
        let rows = labels.len().div_ceil(cols);
        let (pad, gap) = (3.0, 3.0);
        let cw = (r.w - 2.0 * pad - (cols as f32 - 1.0) * gap) / cols as f32;
        let ch = (r.h - 2.0 * pad - (rows as f32 - 1.0) * gap) / rows as f32;
        let font = if cols >= 3 { &p.fonts.small } else { &p.fonts.body };
        for (i, label) in labels.iter().enumerate() {
            let (col, row) = ((i % cols) as f32, (i / cols) as f32);
            let cr = R::new(r.x + pad + col * (cw + gap), r.y + pad + row * (ch + gap), cw, ch);
            let id = base + i as u32;
            let hover = enabled && self.hot == id;
            let fg = if i == selected {
                p.round(cr, 7.0, 0x2C2852, alpha);
                p.round_stroke(cr, 7.0, ACCENT, 0.85 * alpha, 1.0);
                0xFFFFFF
            } else {
                if hover {
                    p.round(cr, 7.0, 0x25252E, 1.0);
                }
                if hover { TEXT } else { MUTED }
            };
            p.text(label, font, cr, fg, alpha, Align::Center);
            self.hit(id, cr, enabled);
        }
    }
}

fn section(p: &Painter, title: &str, x: f32, y: f32, w: f32) {
    p.text(title, &p.fonts.small_b, R::new(x, y, w, 16.0), FAINT, 1.0, Align::Left);
    let tx = x + title.chars().count() as f32 * 7.4 + 10.0;
    p.line(tx, y + 8.5, x + w, y + 8.5, BORDER, 1.0, 1.0);
}

// --- app ---------------------------------------------------------------------------------------

fn settings_path() -> PathBuf {
    exe_dir().join("ReShadeUpscaler.ini")
}

fn load_settings() -> Settings {
    let mut s = Settings::default();
    let text = std::fs::read_to_string(settings_path()).unwrap_or_default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "scale" => s.scale = v.parse::<usize>().unwrap_or(2).min(2),
            "hevc" => s.hevc = v != "0",
            "quality" => s.quality = v.parse::<usize>().unwrap_or(1).min(2),
            "bars_blur" => s.bars_blur = v != "0",
            "out_dir" => s.out_dir = v.to_string(),
            _ => {}
        }
    }
    s
}

impl App {
    fn scale(&self) -> f32 {
        dpi_scale(self.hwnd)
    }

    fn save_settings(&self) {
        let s = &self.set;
        let text = format!(
            "scale={}\nhevc={}\nquality={}\nbars_blur={}\nout_dir={}\n",
            s.scale, s.hevc as u8, s.quality, s.bars_blur as u8, s.out_dir
        );
        let _ = std::fs::write(settings_path(), text);
    }

    fn invalidate_main(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn invalidate_compare(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.compare), None, false);
        }
    }

    fn framing(&self) -> Framing {
        self.info.as_ref().map(|i| framing::compute(i.width, i.height, &self.set)).unwrap_or_default()
    }

    fn output_aspect(&self) -> f64 {
        if self.info.is_some() { self.framing().aspect() } else { 16.0 / 9.0 }
    }

    fn current_time(&self, now: f64) -> f64 {
        if self.playing && self.started { self.pos_base + (now - self.clock_start) } else { self.pos_base }
    }

    /// Posiziona le finestre del video (ReShade e confronto) sul riquadro del video.
    fn apply_layout(&mut self) {
        let s = self.scale();
        let (cw, ch) = client_size(self.hwnd);
        let v = layout(cw as f32 / s, ch as f32 / s, self.output_aspect()).video;
        let rc = RECT {
            left: (v.x * s).round() as i32,
            top: (v.y * s).round() as i32,
            right: (v.right() * s).round() as i32,
            bottom: (v.bottom() * s).round() as i32,
        };
        let (w, h) = ((rc.right - rc.left).max(16), (rc.bottom - rc.top).max(16));
        if rc != self.preview_rect {
            self.preview_rect = rc;
            unsafe {
                let _ = MoveWindow(self.preview, rc.left, rc.top, w, h, false);
                let _ = MoveWindow(self.compare, rc.left, rc.top, w, h, true);
            }
        }
        self.view = [0, 0, w as u32, h as u32];
        self.invalidate_main();
    }

    /// Dimensioni del back buffer dell'anteprima: le stesse su cui DLSS e Neural Rendering
    /// lavorano in export (uscita / scala, cioe' il video alla sua risoluzione), cosi'
    /// l'anteprima mostra lo stesso effetto; la swapchain la rimpicciolisce sulla finestra.
    /// Sempre pari: con dimensioni dispari la creazione della feature DLSS e' andata in crash.
    fn preview_target(&self) -> (u32, u32) {
        let f = self.framing();
        let s = SCALES[self.set.scale];
        let even = |v: u32| ((v as f64 / s).round() as u32).max(16) & !1;
        (even(f.out_w), even(f.out_h))
    }
    fn set_compare(&mut self, mode: usize) {
        if self.info.is_none() {
            return;
        }
        self.cmp_mode = mode;
        unsafe {
            if mode == CMP_RESHADE {
                let _ = ShowWindow(self.compare, SW_HIDE);
                let _ = ShowWindow(self.preview, SW_SHOWNA);
            } else {
                let _ = ShowWindow(self.preview, SW_HIDE);
                let _ = ShowWindow(self.compare, SW_SHOWNA);
            }
        }
        self.invalidate_main();
    }

    fn load(&mut self, path: PathBuf) {
        let Some(info) = probe(&path) else {
            self.requests.push_back(Request::Message(format!("Non riesco a leggere il video:\n{}", path.display())));
            return;
        };
        self.cancel_export();
        self.vdec.stop();
        self.audio.stop();
        if let Err(e) = self.r.set_video(info.width, info.height) {
            self.requests.push_back(Request::Message(format!("Errore della GPU: {e}")));
            return;
        }
        self.set.aspect = 0;
        self.set.crop_cx = 0.5;
        self.set.crop_cy = 0.5;
        self.last_result = None;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        unsafe {
            let _ = SetWindowTextW(self.hwnd, &HSTRING::from(format!("ReShade Upscaler — {name}")));
        }
        self.info = Some(info);
        self.path = Some(path);
        self.apply_layout();
        self.set_compare(self.cmp_mode);
        self.seek(0.0, true);
        if self.test.export_on_load {
            self.test.auto_export_at = Some(now() + 6.0);
        }
    }

    fn seek(&mut self, t: f64, play: bool) {
        let (Some(path), Some(info)) = (self.path.clone(), self.info.clone()) else { return };
        let t = t.clamp(0.0, (info.duration - 1.0 / info.fps).max(0.0));
        let start = (t * info.fps).round() as i64;
        let t = start as f64 / info.fps;
        self.vdec.start(&path, &info, t);
        self.audio.stop();
        if play && info.has_audio {
            self.audio.load(&path, t);
        }
        self.pos_base = t;
        self.next_index = start;
        self.need_frame = true;
        self.playing = play;
        self.started = false;
        self.seek_time = now();
        self.invalidate_main();
    }

    fn pause(&mut self) {
        self.pos_base = self.current_time(now());
        self.playing = false;
        self.started = false;
        self.need_frame = false;
        self.audio.stop();
    }

    fn toggle_play(&mut self) {
        if self.info.is_none() {
            return;
        }
        if self.playing {
            self.pause();
        } else {
            self.seek(self.pos_base, true);
        }
        self.invalidate_main();
    }

    /// Esporta, chiedendo conferma se il Neural Rendering e' spento.
    fn request_export(&mut self) {
        self.nr_on = nr_enabled();
        if self.info.is_some() && self.export.is_none() && self.nr_on == Some(false) {
            self.requests.push_back(Request::ConfirmExport);
        } else {
            self.start_export();
        }
    }

    fn start_export(&mut self) {
        if self.export.is_some() {
            return;
        }
        let (Some(path), Some(info)) = (self.path.clone(), self.info.clone()) else { return };
        self.resume = (self.current_time(now()), self.playing);
        self.vdec.stop();
        self.audio.stop();
        self.playing = false;
        self.started = false;
        self.set_compare(CMP_RESHADE);
        self.last_result = None;
        match RemoteExport::start(&path, &info, &self.set, self.test.frames) {
            Ok(job) => self.export = Some(job),
            Err(e) => {
                self.last_result = Some(Err(e));
                let (t, play) = self.resume;
                self.seek(t, play);
            }
        }
        self.invalidate_main();
    }

    fn cancel_export(&mut self) {
        if let Some(job) = self.export.take() {
            job.cancel();
            self.after_export(None);
        }
    }

    fn after_export(&mut self, result: Option<Result<PathBuf, String>>) {
        let (w, h) = self.preview_target();
        self.r.resize(w, h);
        if let (Some(res), Some(_)) = (&result, self.test.frames) {
            self.requests.push_back(Request::Quit(if res.is_ok() { 0 } else { 2 }));
        }
        if result.is_some() {
            self.last_result = result;
        }
        let (t, play) = self.resume;
        self.seek(t, play);
        unsafe {
            let _ = FlashWindow(self.hwnd, true);
        }
    }

    /// Un giro del ciclo principale. Falso se non c'era niente da fare.
    fn tick(&mut self) -> bool {
        let now = now();

        if let Some(job) = &mut self.export {
            // l'export gira nel processo worker: qui si legge solo l'avanzamento
            let step = job.step();
            if now - self.last_ui > 0.1 {
                self.last_ui = now;
                self.invalidate_main();
            }
            if let Step::Done(result) = step {
                self.export = None;
                self.after_export(Some(result));
                return true;
            }
            return false;
        }

        if now - self.nr_checked > 1.0 {
            self.nr_checked = now;
            let on = nr_enabled();
            if on != self.nr_on {
                self.nr_on = on;
                self.invalidate_main();
            }
        }
        let Some(info) = self.info.clone() else { return false };
        if self.test.auto_export_at.is_some_and(|at| now >= at) {
            self.test.auto_export_at = None;
            self.start_export();
            return true;
        }
        // Il back buffer segue proporzioni/adattamento dopo un attimo di calma, perche' ogni
        // ResizeBuffers fa ricaricare gli effetti a ReShade
        let target = self.preview_target();
        if target == (self.r.back_w, self.r.back_h) {
            self.resize_pending = None;
        } else if self.resize_pending.is_none() {
            self.resize_pending = Some(now);
        } else if self.resize_pending.is_some_and(|since| now - since > 0.3) {
            self.r.resize(target.0, target.1);
            self.resize_pending = None;
        }
        if let Some(t) = self.scrub {
            if self.scrub_dirty && now - self.last_scrub > 0.1 {
                self.seek(t, false);
                self.scrub_dirty = false;
                self.last_scrub = now;
            }
        }

        // Parte quando video e audio hanno qualcosa in coda (o dopo 1.5 s comunque)
        if self.playing && !self.started && ((self.vdec.has_frame() && self.audio.ready()) || now - self.seek_time > 1.5) {
            self.started = true;
            self.clock_start = now;
            self.audio.play();
        }
        let t = self.current_time(now);
        if self.playing && self.started && t >= info.duration {
            self.seek(0.0, true); // loop
            return true;
        }

        let mut newest = None;
        if self.playing && self.started {
            let target = (t * info.fps).floor() as i64;
            while self.next_index <= target {
                match self.vdec.pop() {
                    Some(f) => {
                        newest = Some(f);
                        self.next_index += 1;
                    }
                    None => break,
                }
            }
        } else if self.need_frame {
            if let Some(f) = self.vdec.pop() {
                newest = Some(f);
                self.next_index += 1;
                self.need_frame = false;
            }
        }
        if let Some(f) = newest {
            self.r.upload(&f);
        }

        self.r.view = None;

        let f = self.framing();
        let _ = self.r.present(&f, true);
        if self.cmp_mode != CMP_RESHADE {
            let ok = self.r.read_original(&f, &mut self.orig) && self.r.read_back(&mut self.processed);
            if ok {
                self.cmp_size = self.r.view_size();
                self.invalidate_compare();
            }
        }
        if now - self.last_ui > 0.1 {
            self.last_ui = now;
            self.invalidate_main();
        }
        true
    }

    // --- input ---------------------------------------------------------------------------------

    fn hit_at(&self, x: f32, y: f32) -> Option<&Hit> {
        self.hits.iter().rev().find(|h| h.r.contains(x, y))
    }

    fn set_hot(&mut self, id: u32) {
        if id != self.hot {
            self.hot = id;
            self.invalidate_main();
        }
    }

    fn on_mouse_move(&mut self, px: i32, py: i32) {
        let s = self.scale();
        let (x, y) = (px as f32 / s, py as f32 / s);
        if !self.tracking {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tme);
            }
            self.tracking = true;
        }
        if self.active == ID_SEEK {
            self.scrub_to(x);
            return;
        }
        let hot = self.hit_at(x, y).filter(|h| h.enabled).map_or(ID_NONE, |h| h.id);
        self.set_hot(hot);
    }

    fn on_mouse_down(&mut self, px: i32, py: i32) {
        let s = self.scale();
        let (x, y) = (px as f32 / s, py as f32 / s);
        let Some(id) = self.hit_at(x, y).filter(|h| h.enabled).map(|h| h.id) else { return };
        self.active = id;
        unsafe { SetCapture(self.hwnd) };
        if id == ID_SEEK {
            self.scrub_resume = self.playing;
            if self.playing {
                self.pause();
            }
            self.scrub_to(x);
        }
        self.invalidate_main();
    }

    fn on_mouse_up(&mut self, px: i32, py: i32) {
        if self.active == ID_NONE {
            return;
        }
        let s = self.scale();
        let (x, y) = (px as f32 / s, py as f32 / s);
        unsafe {
            let _ = ReleaseCapture();
        }
        let id = std::mem::replace(&mut self.active, ID_NONE);
        if id == ID_SEEK {
            if let Some(t) = self.scrub.take() {
                self.seek(t, self.scrub_resume);
            }
        } else if self.hit_at(x, y).map(|h| h.id) == Some(id) {
            self.action(id);
        }
        self.invalidate_main();
    }

    fn scrub_to(&mut self, x: f32) {
        let Some(info) = &self.info else { return };
        if let Some(h) = self.hits.iter().find(|h| h.id == ID_SEEK) {
            let frac = ((x - h.r.x) / h.r.w).clamp(0.0, 1.0) as f64;
            self.scrub = Some(frac * info.duration);
            self.scrub_dirty = true;
            self.invalidate_main();
        }
    }

    fn on_key(&mut self, vk: u16) {
        if self.export.is_some() {
            if vk == VK_ESCAPE.0 {
                self.cancel_export();
            }
            return;
        }
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        let t = self.current_time(now());
        match vk {
            v if v == VK_SPACE.0 => self.toggle_play(),
            v if v == VK_LEFT.0 => self.seek(t - 5.0, self.playing),
            v if v == VK_RIGHT.0 => self.seek(t + 5.0, self.playing),
            v if v == b'C' as u16 => self.set_compare((self.cmp_mode + 1) % 3),
            v if v == b'O' as u16 && ctrl => self.requests.push_back(Request::Import),
            v if v == b'E' as u16 && ctrl => self.request_export(),
            _ => {}
        }
    }

    fn action(&mut self, id: u32) {
        let busy = self.export.is_some();
        match id {
            ID_IMPORT if !busy => self.requests.push_back(Request::Import),
            ID_PLAY => self.toggle_play(),
            ID_EXPORT => self.request_export(),
            ID_CANCEL => self.cancel_export(),
            ID_OPEN => {
                if let Some(Ok(p)) = &self.last_result {
                    let args = HSTRING::from(format!("/select,\"{}\"", p.display()));
                    unsafe {
                        ShellExecuteW(None, w!("open"), w!("explorer.exe"), &args, PCWSTR::null(), SW_SHOWNORMAL);
                    }
                }
            }
            ID_FOLDER if !busy => self.requests.push_back(Request::PickFolder),
            ID_FOLDER_RESET if !busy => {
                self.set.out_dir.clear();
                self.save_settings();
            }
            ID_CENTER => {
                self.set.crop_cx = 0.5;
                self.set.crop_cy = 0.5;
            }
            _ if busy => {}
            _ if (ID_CMP..ID_CMP + 3).contains(&id) => self.set_compare((id - ID_CMP) as usize),
            _ if (ID_SCALE..ID_SCALE + 3).contains(&id) => {
                self.set.scale = (id - ID_SCALE) as usize;
                self.save_settings();
            }
            _ if (ID_ASPECT..ID_ASPECT + 6).contains(&id) => {
                self.set.aspect = (id - ID_ASPECT) as usize;
                self.set.crop_cx = 0.5;
                self.set.crop_cy = 0.5;
                self.apply_layout();
            }
            _ if (ID_FIT..ID_FIT + 3).contains(&id) => {
                self.set.fit = (id - ID_FIT) as usize;
                self.apply_layout();
            }
            _ if (ID_BARS..ID_BARS + 2).contains(&id) => {
                self.set.bars_blur = id == ID_BARS + 1;
                self.save_settings();
            }
            _ if (ID_CODEC..ID_CODEC + 2).contains(&id) => {
                self.set.hevc = id == ID_CODEC;
                self.save_settings();
            }
            _ if (ID_QUAL..ID_QUAL + 3).contains(&id) => {
                self.set.quality = (id - ID_QUAL) as usize;
                self.save_settings();
            }
            _ => {}
        }
        self.invalidate_main();
    }

    fn crop_enabled(&self) -> bool {
        self.info.is_some() && self.set.aspect != 0 && self.set.fit == FIT_CROP && self.export.is_none()
    }

    /// Il punto (coordinate della finestra di anteprima) cade sul video?
    fn in_view(&self, x: i32, y: i32) -> bool {
        let [vx, vy, vw, vh] = self.view.map(|v| v as i32);
        x >= vx && x < vx + vw && y >= vy && y < vy + vh
    }

    fn crop_move(&mut self, x: i32, y: i32) {
        let Some((lx, ly)) = self.crop_drag else { return };
        let f = self.framing();
        let (cw, ch) = (self.view[2] as i32, self.view[3] as i32);
        let (ux, uy) = (f.map[0] as f64, f.map[1] as f64);
        self.set.crop_cx = (self.set.crop_cx - (x - lx) as f64 / cw.max(1) as f64 * ux).clamp(ux / 2.0, 1.0 - ux / 2.0);
        self.set.crop_cy = (self.set.crop_cy - (y - ly) as f64 / ch.max(1) as f64 * uy).clamp(uy / 2.0, 1.0 - uy / 2.0);
        self.crop_drag = Some((x, y));
    }

    fn split_to(&mut self, x: i32) {
        let (w, _) = client_size(self.compare);
        self.split = (x as f32 / w.max(1) as f32).clamp(0.02, 0.98);
        self.invalidate_compare();
    }

    // --- disegno -------------------------------------------------------------------------------

    fn paint_main(&mut self, hdc: HDC) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let s = self.scale();
        if !self.painter.begin(hdc, &rc, s) {
            return;
        }
        let mut ui = Ui { p: &self.painter, hits: Vec::new(), hot: self.hot, active: self.active };
        self.draw_main(&mut ui, rc.right as f32 / s, rc.bottom as f32 / s);
        let hits = ui.hits;
        self.painter.end();
        self.hits = hits;
    }

    fn draw_main(&self, ui: &mut Ui, cw: f32, ch: f32) {
        let p = ui.p;
        p.clear(BG);
        let lay = layout(cw, ch, self.output_aspect());
        if self.info.is_some() {
            p.round(lay.preview, 14.0, 0x0A0A0D, 1.0);
        } else {
            let hover = ui.hot == ID_IMPORT;
            p.round(lay.preview, 14.0, if hover { 0x17171D } else { 0x131318 }, 1.0);
            p.round_stroke(lay.preview, 14.0, if hover { ACCENT } else { BORDER }, if hover { 0.7 } else { 1.0 }, 1.5);
            let c = lay.preview.cy();
            p.text("\u{E8E5}", &p.fonts.icon_lg, R::new(lay.preview.x, c - 70.0, lay.preview.w, 56.0), if hover { ACCENT } else { MUTED }, 1.0, Align::Center);
            p.text("Trascina qui un video", &p.fonts.big, R::new(lay.preview.x, c - 4.0, lay.preview.w, 26.0), TEXT, 1.0, Align::Center);
            p.text("oppure clicca per sceglierlo  ·  MP4, MOV, MKV, WEBM", &p.fonts.small, R::new(lay.preview.x, c + 24.0, lay.preview.w, 20.0), MUTED, 1.0, Align::Center);
            ui.hit(ID_IMPORT, lay.preview, true);
        }
        self.draw_transport(ui, lay.transport);
        self.draw_sidebar(ui, lay.side);
    }

    fn draw_transport(&self, ui: &mut Ui, t: R) {
        let p = ui.p;
        let has = self.info.is_some() && self.export.is_none();
        let cy = t.cy() - 4.0;
        let duration = self.info.as_ref().map_or(0.0, |i| i.duration);
        let cur = self.scrub.unwrap_or_else(|| self.current_time(now()));

        let play = R::new(t.x + MARGIN + 6.0, cy - 21.0, 42.0, 42.0);
        let hover = has && ui.hot == ID_PLAY;
        p.circle(play.cx(), play.cy(), 21.0, if hover { ACCENT_HOVER } else { ACCENT }, if has { 1.0 } else { 0.35 });
        let glyph = if self.playing { "\u{E769}" } else { "\u{E768}" };
        p.text(glyph, &p.fonts.icon, play, 0xFFFFFF, 1.0, Align::Center);
        ui.hit(ID_PLAY, play, has);

        let time = R::new(play.right() + 14.0, cy - 12.0, 104.0, 24.0);
        let alpha = if has { 1.0 } else { 0.4 };
        p.text(&format!("{} / {}", fmt_time(cur), fmt_time(duration)), &p.fonts.body, time, TEXT, alpha, Align::Left);

        let cmp = R::new(t.right() - 12.0 - 264.0, cy - 17.0, 264.0, 34.0);
        ui.segmented(ID_CMP, cmp, &["ReShade", "Originale", "Split"], 3, self.cmp_mode, has);

        let seek = R::new(time.right() + 6.0, cy - 14.0, (cmp.x - 26.0 - time.right() - 6.0).max(20.0), 28.0);
        let frac = if duration > 0.0 { (cur / duration).clamp(0.0, 1.0) as f32 } else { 0.0 };
        p.round(R::new(seek.x, cy - 2.0, seek.w, 4.0), 2.0, 0x2A2A34, 1.0);
        if has {
            p.round(R::new(seek.x, cy - 2.0, seek.w * frac, 4.0), 2.0, ACCENT, 1.0);
            let big = ui.hot == ID_SEEK || ui.active == ID_SEEK;
            p.circle(seek.x + seek.w * frac, cy, if big { 7.5 } else { 6.0 }, 0xFFFFFF, 1.0);
        }
        ui.hit(ID_SEEK, seek, has);
    }

    fn draw_sidebar(&self, ui: &mut Ui, side: R) {
        let p = ui.p;
        p.rect(side, PANEL, 1.0);
        p.line(side.x + 0.5, 0.0, side.x + 0.5, side.h, BORDER, 1.0, 1.0);
        let busy = self.export.is_some();
        let has = self.info.is_some();
        let (x, w) = (side.x + 22.0, side.w - 44.0);
        let (lw, row) = (96.0, 34.0);
        let (cx, cwid) = (x + lw, w - lw);
        let label = |text: &str, y: f32| p.text(text, &p.fonts.small, R::new(x, y, lw, row), MUTED, 1.0, Align::Left);
        let mut y = 22.0;

        p.text("ReShade Upscaler", &p.fonts.title, R::new(x, y, w, 26.0), TEXT, 1.0, Align::Left);
        y += 26.0;
        p.text("DLSS 5 Neural Rendering  ·  export NVENC", &p.fonts.small, R::new(x, y, w, 18.0), MUTED, 1.0, Align::Left);
        y += 36.0;

        // Video
        section(p, "VIDEO", x, y, w);
        y += 26.0;
        let card = R::new(x, y, w, 60.0);
        p.round(card, 10.0, CARD, 1.0);
        p.round_stroke(card, 10.0, BORDER, 1.0, 1.0);
        match (&self.path, &self.info) {
            (Some(path), Some(info)) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                p.text(&name, &p.fonts.body_b, R::new(x + 14.0, y + 10.0, w - 28.0, 20.0), TEXT, 1.0, Align::Left);
                let detail = format!(
                    "{} × {}  ·  {} fps  ·  {}  ·  {}",
                    info.width,
                    info.height,
                    fmt_fps(info.fps),
                    fmt_time(info.duration),
                    if info.has_audio { "audio" } else { "senza audio" }
                );
                p.text(&detail, &p.fonts.small, R::new(x + 14.0, y + 32.0, w - 28.0, 18.0), MUTED, 1.0, Align::Left);
            }
            _ => {
                p.text("Nessun video caricato", &p.fonts.body, R::new(x + 14.0, y + 10.0, w - 28.0, 20.0), MUTED, 1.0, Align::Left);
                p.text("Trascinalo nella finestra o usa Importa", &p.fonts.small, R::new(x + 14.0, y + 32.0, w - 28.0, 18.0), FAINT, 1.0, Align::Left);
            }
        }
        y += 70.0;
        ui.button(ID_IMPORT, R::new(x, y, w, 36.0), "\u{E8E5}", "Importa video…", Kind::Secondary, !busy);
        y += 36.0 + 26.0;

        // Output
        section(p, "OUTPUT", x, y, w);
        y += 26.0;
        label("Scala", y);
        ui.segmented(ID_SCALE, R::new(cx, y, cwid, row), &SCALE_LABELS, 3, self.set.scale, !busy);
        y += row + 10.0;
        label("Proporzioni", y);
        let aspects: Vec<&str> = ASPECTS.iter().map(|a| a.label).collect();
        ui.segmented(ID_ASPECT, R::new(cx, y, cwid, 66.0), &aspects, 3, self.set.aspect, !busy && has);
        y += 66.0 + 10.0;
        if self.set.aspect != 0 {
            label("Adattamento", y);
            ui.segmented(ID_FIT, R::new(cx, y, cwid, row), &["Ritaglia", "Adatta", "Stira"], 3, self.set.fit, !busy);
            y += row + 10.0;
            if self.set.fit == FIT_FIT {
                label("Bande", y);
                ui.segmented(ID_BARS, R::new(cx, y, cwid, row), &["Nere", "Sfocate"], 2, self.set.bars_blur as usize, !busy);
                y += row + 10.0;
            } else if self.set.fit == FIT_CROP {
                p.text("Trascina il video per inquadrare", &p.fonts.small, R::new(x, y, w - 78.0, 30.0), MUTED, 1.0, Align::Left);
                ui.button(ID_CENTER, R::new(x + w - 72.0, y, 72.0, 30.0), "", "Centra", Kind::Ghost, !busy);
                y += 30.0 + 10.0;
            }
        }
        let res = R::new(x, y, w, 46.0);
        p.round(res, 10.0, 0x1B1930, 1.0);
        p.round_stroke(res, 10.0, ACCENT, 0.35, 1.0);
        p.text("Risultato", &p.fonts.small, R::new(x + 14.0, y, 100.0, 46.0), MUTED, 1.0, Align::Left);
        let f = self.framing();
        let dims = if has { format!("{} × {}", f.out_w, f.out_h) } else { "—".into() };
        p.text(&dims, &p.fonts.big, R::new(x, y, w - 14.0, 46.0), TEXT, 1.0, Align::Right);
        y += 46.0 + 10.0;
        // Stato del Neural Rendering (Canc lo accende/spegne e vale anche per l'export)
        if let Some(on) = self.nr_on {
            let (color, text) = if on {
                (SUCCESS, "Neural Rendering attivo")
            } else {
                (DANGER, "Neural Rendering spento  ·  premi Canc sul video")
            };
            p.circle(x + 5.0, y + 11.0, 4.0, color, 1.0);
            p.text(text, &p.fonts.small, R::new(x + 16.0, y, w - 16.0, 22.0), color, 1.0, Align::Left);
            y += 22.0;
        }
        y += 16.0;

        // Esportazione
        section(p, "ESPORTAZIONE", x, y, w);
        y += 26.0;
        label("Codec", y);
        ui.segmented(ID_CODEC, R::new(cx, y, cwid, row), &["HEVC", "H.264"], 2, if self.set.hevc { 0 } else { 1 }, !busy);
        y += row + 10.0;
        label("Qualità", y);
        ui.segmented(ID_QUAL, R::new(cx, y, cwid, row), &["Alta", "Molto alta", "Massima"], 3, self.set.quality, !busy);
        y += row + 10.0;
        label("Cartella", y);
        let field = R::new(cx, y, cwid - 40.0, row);
        p.round(field, 8.0, FIELD, 1.0);
        p.round_stroke(field, 8.0, BORDER, 1.0, 1.0);
        let folder = if self.set.out_dir.is_empty() { "Come il video".to_string() } else { self.set.out_dir.clone() };
        p.text(&folder, &p.fonts.small, field.inset(10.0, 0.0), if self.set.out_dir.is_empty() { MUTED } else { TEXT }, 1.0, Align::Left);
        ui.button(ID_FOLDER, R::new(x + w - 34.0, y, 34.0, row), "\u{E8B7}", "", Kind::Secondary, !busy);
        y += row + 4.0;
        if !self.set.out_dir.is_empty() {
            ui.button(ID_FOLDER_RESET, R::new(cx, y, cwid, 24.0), "", "Usa la cartella del video", Kind::Ghost, !busy);
        }

        // In fondo: esporta / avanzamento, scorciatoie
        let hints_y = side.h - 20.0 - 36.0;
        p.text("Ins  menu ReShade      Canc  Neural Rendering", &p.fonts.small, R::new(x, hints_y, w, 18.0), FAINT, 1.0, Align::Left);
        p.text("Spazio  play/pausa      C  confronto      ←/→  ±5 s", &p.fonts.small, R::new(x, hints_y + 18.0, w, 18.0), FAINT, 1.0, Align::Left);
        let by = hints_y - 16.0 - 48.0;
        if let Some(job) = &self.export {
            let top = by - 62.0;
            let (title, frac) = if job.preparing() {
                ("Preparazione DLSS…".to_string(), 0.0)
            } else {
                (format!("Esportazione  {} / {}", job.done, job.total), job.done as f32 / job.total.max(1) as f32)
            };
            p.text(&title, &p.fonts.body_b, R::new(x, top, w, 20.0), TEXT, 1.0, Align::Left);
            p.text(&format!("{:.0}%", frac * 100.0), &p.fonts.body_b, R::new(x, top, w, 20.0), ACCENT, 1.0, Align::Right);
            p.round(R::new(x, top + 28.0, w, 6.0), 3.0, 0x2A2A34, 1.0);
            p.round(R::new(x, top + 28.0, (w * frac).max(6.0), 6.0), 3.0, ACCENT, 1.0);
            let stats = if job.preparing() {
                format!("ReShade ricarica gli effetti a {} × {}", job.framing.out_w, job.framing.out_h)
            } else if job.fps > 0.0 {
                let left = (job.total - job.done) as f64 / job.fps;
                format!("{:.1} fps  ·  circa {} rimanenti", job.fps, fmt_time(left))
            } else {
                String::new()
            };
            p.text(&stats, &p.fonts.small, R::new(x, top + 40.0, w, 18.0), MUTED, 1.0, Align::Left);
            ui.button(ID_CANCEL, R::new(x, by + 4.0, w, 42.0), "\u{E711}", "Annulla", Kind::Danger, true);
        } else {
            match &self.last_result {
                Some(Ok(path)) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    p.text("\u{E73E}", &p.fonts.icon, R::new(x, by - 38.0, 20.0, 28.0), SUCCESS, 1.0, Align::Left);
                    p.text(&name, &p.fonts.small, R::new(x + 24.0, by - 38.0, w - 24.0 - 104.0, 28.0), TEXT, 1.0, Align::Left);
                    ui.button(ID_OPEN, R::new(x + w - 100.0, by - 38.0, 100.0, 28.0), "", "Apri cartella", Kind::Ghost, true);
                }
                Some(Err(e)) => {
                    p.text("\u{E783}", &p.fonts.icon, R::new(x, by - 38.0, 20.0, 28.0), DANGER, 1.0, Align::Left);
                    p.text(e, &p.fonts.small, R::new(x + 24.0, by - 38.0, w - 24.0, 28.0), DANGER, 1.0, Align::Left);
                }
                None => {}
            }
            ui.button(ID_EXPORT, R::new(x, by, w, 48.0), "\u{E74E}", "Esporta", Kind::Primary, has);
        }
    }

    fn paint_compare(&mut self, hdc: HDC) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.compare, &mut rc);
        }
        let s = dpi_scale(self.compare);
        if !self.cmp_painter.begin(hdc, &rc, s) {
            return;
        }
        let (cw, ch) = (rc.right as f32 / s, rc.bottom as f32 / s);
        let p = &mut self.cmp_painter;
        p.clear(0x000000);
        let (bw, bh) = self.cmp_size;
        let (fw, fh) = (bw as f32, bh as f32);
        if bw > 0 && self.cmp_mode == CMP_SPLIT {
            let sx = cw * self.split;
            let px = fw * self.split;
            p.image(0, &self.orig, bw, bh, R::new(0.0, 0.0, sx, ch), R::new(0.0, 0.0, px, fh));
            p.image(1, &self.processed, bw, bh, R::new(sx, 0.0, cw - sx, ch), R::new(px, 0.0, fw - px, fh));
            p.line(sx, 0.0, sx, ch, 0xFFFFFF, 0.9, 2.0);
            p.circle(sx, ch / 2.0, 16.0, 0xFFFFFF, 0.95);
            p.text("\u{E76B}", &p.fonts.icon_sm, R::new(sx - 13.0, ch / 2.0 - 10.0, 12.0, 20.0), 0x111114, 1.0, Align::Center);
            p.text("\u{E76C}", &p.fonts.icon_sm, R::new(sx + 1.0, ch / 2.0 - 10.0, 12.0, 20.0), 0x111114, 1.0, Align::Center);
            pill(p, "ORIGINALE", 12.0, 12.0, false, cw);
            pill(p, "RESHADE", cw - 12.0, 12.0, true, cw);
        } else if bw > 0 {
            p.image(0, &self.orig, bw, bh, R::new(0.0, 0.0, cw, ch), R::new(0.0, 0.0, fw, fh));
            pill(p, "ORIGINALE", 12.0, 12.0, false, cw);
        }
        p.end();
    }
}

/// Etichetta in sovrimpressione; `right` = ancorata a destra in x.
fn pill(p: &Painter, text: &str, x: f32, y: f32, right: bool, _cw: f32) {
    let w = text.chars().count() as f32 * 7.4 + 22.0;
    let r = if right { R::new(x - w, y, w, 24.0) } else { R::new(x, y, w, 24.0) };
    p.round(r, 12.0, 0x000000, 0.6);
    if right {
        p.round_stroke(r, 12.0, ACCENT, 0.9, 1.0);
    }
    p.text(text, &p.fonts.small_b, r, 0xFFFFFF, 1.0, Align::Center);
}

// --- finestre ----------------------------------------------------------------------------------

fn dropped_file(hdrop: HDROP) -> Option<PathBuf> {
    unsafe {
        let len = DragQueryFileW(hdrop, 0, None) as usize;
        let mut buf = vec![0u16; len + 1];
        DragQueryFileW(hdrop, 0, Some(&mut buf));
        DragFinish(hdrop);
        (len > 0).then(|| PathBuf::from(OsString::from_wide(&buf[..len])))
    }
}

fn on_drop(wp: WPARAM) {
    if let Some(p) = dropped_file(HDROP(wp.0 as _)) {
        with_app(|a| {
            if a.export.is_none() {
                a.requests.push_back(Request::Load(p));
            }
        });
    }
}

extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                if with_app(|a| a.paint_main(hdc)).is_none() {
                    NEED_PAINT.with(|c| c.set(true));
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                with_app(|a| a.apply_layout());
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let s = dpi_scale(hwnd);
                let mmi = lp.0 as *mut MINMAXINFO;
                (*mmi).ptMinTrackSize = POINT { x: (980.0 * s) as i32, y: (700.0 * s) as i32 };
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let rc = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top, SWP_NOZORDER | SWP_NOACTIVATE);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| a.on_mouse_move(x, y));
                LRESULT(0)
            }
            WM_MOUSELEAVE_ => {
                with_app(|a| {
                    a.tracking = false;
                    a.set_hot(ID_NONE);
                });
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| a.on_mouse_down(x, y));
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| a.on_mouse_up(x, y));
                LRESULT(0)
            }
            WM_SETCURSOR => {
                let over_control = (lp.0 & 0xffff) as u32 == HTCLIENT && with_app(|a| a.hot != ID_NONE).unwrap_or(false);
                if over_control {
                    SetCursor(LoadCursorW(None, IDC_HAND).ok());
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_KEYDOWN => {
                with_app(|a| a.on_key(wp.0 as u16));
                LRESULT(0)
            }
            WM_DROPFILES => {
                on_drop(wp);
                LRESULT(0)
            }
            WM_CLOSE => {
                with_app(|a| a.cancel_export());
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

extern "system" fn preview_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                if with_app(|a| {
                    let on = a.crop_enabled() && a.in_view(x, y);
                    if on {
                        a.crop_drag = Some((x, y));
                    }
                    on
                }) == Some(true)
                {
                    SetCapture(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                with_app(|a| a.crop_move(x, y));
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                with_app(|a| a.crop_drag = None);
                let _ = ReleaseCapture();
                LRESULT(0)
            }
            WM_SETCURSOR
                if with_app(|a| {
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let _ = ScreenToClient(hwnd, &mut pt);
                    a.crop_enabled() && a.in_view(pt.x, pt.y)
                }) == Some(true) =>
            {
                SetCursor(LoadCursorW(None, IDC_SIZEALL).ok());
                LRESULT(1)
            }
            WM_KEYDOWN => {
                with_app(|a| a.on_key(wp.0 as u16));
                LRESULT(0)
            }
            WM_DROPFILES => {
                on_drop(wp);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

extern "system" fn compare_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                if with_app(|a| a.paint_compare(hdc)).is_none() {
                    NEED_PAINT.with(|c| c.set(true));
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, _) = lparam_xy(lp);
                if with_app(|a| {
                    let on = a.cmp_mode == CMP_SPLIT;
                    if on {
                        a.split_drag = true;
                        a.split_to(x);
                    }
                    on
                }) == Some(true)
                {
                    SetCapture(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, _) = lparam_xy(lp);
                with_app(|a| {
                    if a.split_drag {
                        a.split_to(x);
                    }
                });
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                with_app(|a| a.split_drag = false);
                let _ = ReleaseCapture();
                LRESULT(0)
            }
            WM_SETCURSOR if with_app(|a| a.cmp_mode == CMP_SPLIT) == Some(true) => {
                SetCursor(LoadCursorW(None, IDC_SIZEWE).ok());
                LRESULT(1)
            }
            WM_KEYDOWN => {
                with_app(|a| a.on_key(wp.0 as u16));
                LRESULT(0)
            }
            WM_DROPFILES => {
                on_drop(wp);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// --- dialoghi ----------------------------------------------------------------------------------

fn dialog_path(dlg: &IFileOpenDialog) -> Option<String> {
    unsafe {
        let item = dlg.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as _));
        s
    }
}

fn pick_video(owner: HWND) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let specs = [
            COMDLG_FILTERSPEC { pszName: w!("Video"), pszSpec: w!("*.mp4;*.mov;*.mkv;*.avi;*.webm;*.m4v;*.mxf") },
            COMDLG_FILTERSPEC { pszName: w!("Tutti i file"), pszSpec: w!("*.*") },
        ];
        dlg.SetFileTypes(&specs).ok()?;
        dlg.SetTitle(w!("Scegli il video")).ok()?;
        dlg.Show(Some(owner)).ok()?;
        dialog_path(&dlg).map(PathBuf::from)
    }
}

fn pick_folder(owner: HWND) -> Option<String> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        dlg.SetTitle(w!("Cartella di esportazione")).ok()?;
        dlg.Show(Some(owner)).ok()?;
        dialog_path(&dlg)
    }
}

/// Esegue le richieste che aprono finestre modali, fuori da qualsiasi accesso allo stato.
fn handle_request(req: Request) -> Option<i32> {
    let owner = with_app(|a| a.hwnd)?;
    match req {
        Request::Import => {
            if let Some(p) = pick_video(owner) {
                with_app(|a| a.load(p));
            }
        }
        Request::PickFolder => {
            if let Some(d) = pick_folder(owner) {
                with_app(|a| {
                    a.set.out_dir = d;
                    a.save_settings();
                    a.invalidate_main();
                });
            }
        }
        Request::Load(p) => {
            with_app(|a| a.load(p));
        }
        Request::Message(m) => unsafe {
            MessageBoxW(Some(owner), &HSTRING::from(m), w!("ReShade Upscaler"), MB_ICONWARNING);
        },
        Request::Quit(code) => return Some(code),
        Request::ConfirmExport => {
            let answer = unsafe {
                MessageBoxW(
                    Some(owner),
                    w!("Il Neural Rendering è spento (tasto Canc), quindi l'export avrà solo l'upscale DLSS, senza l'effetto.\n\nEsportare comunque?\n\nPer riaccenderlo: clicca sul video e premi Canc."),
                    w!("ReShade Upscaler"),
                    MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                )
            };
            if answer == IDYES {
                with_app(|a| a.start_export());
            }
        }
    }
    None
}

fn run_loop() -> i32 {
    loop {
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return msg.wParam.0 as i32;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if let Some(Some(req)) = with_app(|a| a.requests.pop_front()) {
            if let Some(code) = handle_request(req) {
                return code;
            }
            continue;
        }
        let worked = with_app(|a| a.tick()).unwrap_or(false);
        if NEED_PAINT.with(|c| c.replace(false)) {
            with_app(|a| {
                a.invalidate_main();
                a.invalidate_compare();
            });
        }
        if !worked {
            unsafe {
                MsgWaitForMultipleObjects(None, false, 15, QS_ALLINPUT);
            }
        }
    }
}

fn register_class(name: PCWSTR, proc_: WNDPROC, hinst: HINSTANCE) {
    let wc = WNDCLASSW {
        lpfnWndProc: proc_,
        hInstance: hinst,
        lpszClassName: name,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        // icona incorporata da build.rs (risorsa 1)
        hIcon: unsafe { LoadIconW(Some(hinst), PCWSTR(1 as _)).unwrap_or_default() },
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };
}

fn main() {
    let mut argv = std::env::args().skip(1).peekable();
    if argv.peek().map(String::as_str) == Some("--worker") {
        argv.next();
        std::process::exit(worker::run(argv));
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    // Uso: ReShadeUpscaler.exe [video] [--export] [--frames N] [--scale 1|1.5|2]
    //      [--aspect 0-5] [--fit 0-2] [--compare 0-2]   (le opzioni servono per i test)
    let mut set = load_settings();
    let mut test = TestOptions::default();
    let mut path = None;
    let mut compare = CMP_RESHADE;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--export" => test.export_on_load = true,
            "--frames" => test.frames = args.next().and_then(|v| v.parse().ok()),
            "--scale" => {
                let v: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(2.0);
                set.scale = SCALES.iter().position(|s| (s - v).abs() < 0.01).unwrap_or(2);
            }
            "--aspect" => set.aspect = args.next().and_then(|v| v.parse().ok()).unwrap_or(0usize).min(5),
            "--fit" => set.fit = args.next().and_then(|v| v.parse().ok()).unwrap_or(0usize).min(2),
            "--compare" => compare = args.next().and_then(|v| v.parse().ok()).unwrap_or(0usize).min(2),
            _ => path = Some(PathBuf::from(a)),
        }
    }
    let (aspect, fit) = (set.aspect, set.fit);

    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        register_class(w!("RSUMain"), Some(main_proc), hinst);
        register_class(w!("RSUPreview"), Some(preview_proc), hinst);
        register_class(w!("RSUCompare"), Some(compare_proc), hinst);

        let s = GetDpiForSystem() as f32 / 96.0;
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let ww = ((1440.0 * s) as i32).min(work.right - work.left);
        let wh = ((940.0 * s) as i32).min(work.bottom - work.top);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("RSUMain"),
            w!("ReShade Upscaler"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            work.left + (work.right - work.left - ww) / 2,
            work.top + (work.bottom - work.top - wh) / 2,
            ww,
            wh,
            None,
            None,
            Some(hinst),
            None,
        ) else {
            return;
        };
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        let child_style = WS_CHILD | WS_CLIPSIBLINGS;
        let preview = CreateWindowExW(WINDOW_EX_STYLE(0), w!("RSUPreview"), PCWSTR::null(), child_style, 0, 0, 64, 64, Some(hwnd), None, Some(hinst), None).unwrap_or_default();
        let compare_wnd = CreateWindowExW(WINDOW_EX_STYLE(0), w!("RSUCompare"), PCWSTR::null(), child_style, 0, 0, 64, 64, Some(hwnd), None, Some(hinst), None).unwrap_or_default();
        DragAcceptFiles(hwnd, true);
        DragAcceptFiles(preview, true);
        DragAcceptFiles(compare_wnd, true);

        let renderer = match Renderer::new(preview) {
            Ok(r) => r,
            Err(e) => {
                MessageBoxW(Some(hwnd), &HSTRING::from(format!("Inizializzazione D3D11 fallita: {e}")), w!("ReShade Upscaler"), MB_ICONERROR);
                return;
            }
        };
        let (Ok(painter), Ok(cmp_painter)) = (Painter::new(), Painter::new()) else {
            MessageBoxW(Some(hwnd), w!("Inizializzazione Direct2D fallita."), w!("ReShade Upscaler"), MB_ICONERROR);
            return;
        };

        let app = App {
            hwnd,
            preview,
            compare: compare_wnd,
            painter,
            cmp_painter,
            r: renderer,
            vdec: VideoDecoder::default(),
            audio: AudioPlayer::default(),
            path: None,
            info: None,
            set,
            playing: false,
            started: false,
            pos_base: 0.0,
            clock_start: 0.0,
            seek_time: 0.0,
            next_index: 0,
            need_frame: false,
            cmp_mode: CMP_RESHADE,
            split: 0.5,
            orig: Vec::new(),
            processed: Vec::new(),
            cmp_size: (0, 0),
            split_drag: false,
            crop_drag: None,
            preview_rect: RECT::default(),
            view: [0, 0, 16, 16],
            resize_pending: None,
            hits: Vec::new(),
            hot: ID_NONE,
            active: ID_NONE,
            tracking: false,
            scrub: None,
            scrub_dirty: false,
            last_scrub: 0.0,
            scrub_resume: false,
            export: None,
            resume: (0.0, true),
            last_result: None,
            requests: VecDeque::new(),
            last_ui: 0.0,
            nr_on: nr_enabled(),
            nr_checked: 0.0,
            test,
        };
        APP.with(|a| *a.borrow_mut() = Some(app));
        if let Some(p) = path {
            with_app(|a| {
                a.load(p);
                // le opzioni da riga di comando vincono sul reset di load()
                a.set.aspect = aspect;
                a.set.fit = fit;
                a.apply_layout();
                a.set_compare(compare);
            });
        }
        with_app(|a| a.apply_layout());
        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    let code = run_loop();
    APP.with(|a| a.borrow_mut().take());
    std::process::exit(code);
}

/// NeuralUplift dalla sezione [RenoDX.DLSS5] di ReShade.ini; None se non si legge.
fn nr_enabled() -> Option<bool> {
    let text = std::fs::read_to_string(exe_dir().join("ReShade.ini")).ok()?;
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[RenoDX.DLSS5]");
        } else if in_section {
            if let Some(v) = line.strip_prefix("NeuralUplift=") {
                return Some(v.trim() != "0");
            }
        }
    }
    None
}