//! Impostazioni di uscita (scala, proporzioni, adattamento) e come si traducono in
//! dimensioni del file e in coordinate di campionamento per lo shader.

pub const SCALES: [f64; 3] = [1.0, 1.5, 2.0];
pub const SCALE_LABELS: [&str; 3] = ["1x", "1.5x", "2x"];

pub struct Aspect {
    pub label: &'static str,
    pub ratio: f64, // larghezza / altezza; 0 = come il video
    pub file: &'static str,
}

pub const ASPECTS: [Aspect; 6] = [
    Aspect { label: "Originale", ratio: 0.0, file: "" },
    Aspect { label: "9:16", ratio: 9.0 / 16.0, file: "9x16" },
    Aspect { label: "16:9", ratio: 16.0 / 9.0, file: "16x9" },
    Aspect { label: "1:1", ratio: 1.0, file: "1x1" },
    Aspect { label: "4:5", ratio: 4.0 / 5.0, file: "4x5" },
    Aspect { label: "21:9", ratio: 21.0 / 9.0, file: "21x9" },
];

pub const FIT_CROP: usize = 0;
pub const FIT_FIT: usize = 1;
pub const FIT_STRETCH: usize = 2;

/// Qualita' NVENC (CQ): piu' basso = migliore e piu' pesante.
pub const QUALITY_CQ: [u32; 3] = [21, 17, 13];

#[derive(Clone)]
pub struct Settings {
    pub scale: usize,
    pub aspect: usize,
    pub fit: usize,
    pub bars_blur: bool,
    pub crop_cx: f64,
    pub crop_cy: f64,
    pub hevc: bool,
    pub quality: usize,
    pub out_dir: String, // vuoto = cartella del video
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            scale: 2,
            aspect: 0,
            fit: FIT_CROP,
            bars_blur: true,
            crop_cx: 0.5,
            crop_cy: 0.5,
            hevc: true,
            quality: 1,
            out_dir: String::new(),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Framing {
    pub out_w: u32,
    pub out_h: u32,
    /// uv_sorgente = uv_uscita * map.xy + map.zw
    pub map: [f32; 4],
    /// Per le bande sfocate: stessa forma di `map`, riempie tutta l'uscita.
    pub cover: [f32; 4],
    /// -1 nessuna banda, 0 nere, 1 sfocate
    pub bars: i32,
}

impl Framing {
    pub fn aspect(&self) -> f64 {
        self.out_w as f64 / self.out_h.max(1) as f64
    }
}

fn even(v: f64) -> u32 {
    ((v.round() as u32) & !1).max(2)
}

pub fn compute(w: u32, h: u32, s: &Settings) -> Framing {
    let (w, h) = (w as f64, h as f64);
    let scale = SCALES[s.scale];
    let src_aspect = w / h;
    let target = if ASPECTS[s.aspect].ratio > 0.0 { ASPECTS[s.aspect].ratio } else { src_aspect };
    // Regione piu' grande con le proporzioni scelte che sta dentro il video
    let (cw, ch) = if target < src_aspect { (h * target, h) } else { (w, w / target) };
    let (ux, uy) = (cw / w, ch / h);
    let cover = [ux as f32, uy as f32, ((1.0 - ux) / 2.0) as f32, ((1.0 - uy) / 2.0) as f32];
    let mode = if s.aspect == 0 { FIT_CROP } else { s.fit };

    match mode {
        FIT_FIT => {
            // Tela che contiene tutto il video
            let (tw, th) = if target > src_aspect { (h * target, h) } else { (w, w / target) };
            let (sx, sy) = (w / tw, h / th);
            let (mx, my) = ((1.0 - sx) / 2.0, (1.0 - sy) / 2.0);
            Framing {
                out_w: even(tw * scale),
                out_h: even(th * scale),
                map: [(1.0 / sx) as f32, (1.0 / sy) as f32, (-mx / sx) as f32, (-my / sy) as f32],
                cover,
                bars: if s.bars_blur { 1 } else { 0 },
            }
        }
        FIT_STRETCH => Framing {
            out_w: even(cw * scale),
            out_h: even(ch * scale),
            map: [1.0, 1.0, 0.0, 0.0],
            cover,
            bars: -1,
        },
        _ => {
            let ox = (s.crop_cx - ux / 2.0).clamp(0.0, 1.0 - ux);
            let oy = (s.crop_cy - uy / 2.0).clamp(0.0, 1.0 - uy);
            Framing {
                out_w: even(cw * scale),
                out_h: even(ch * scale),
                map: [ux as f32, uy as f32, ox as f32, oy as f32],
                cover,
                bars: -1,
            }
        }
    }
}
