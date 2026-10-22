//! Esportazione: ogni frame passa per il Present (cosi' ReShade lo elabora), poi viene
//! riletto dal back buffer e mandato a ffmpeg, che codifica con NVENC e rimette l'audio.
//! E' una macchina a stati: l'app chiama `step` a ogni giro del ciclo principale, cosi'
//! l'interfaccia resta viva durante l'esportazione.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};

use crate::framing::{self, Framing, Settings, ASPECTS, QUALITY_CQ, SCALE_LABELS};
use crate::media::{exe_dir, ffmpeg, log_file, VideoDecoder, VideoInfo};
use crate::render::Renderer;

/// Attesa dopo che il feed ha costruito DLSS, prima di registrare (vedi Phase::Preparing).
pub const PREPARE_AFTER_FEED: f64 = 15.0;

pub enum Step {
    Working,
    Done(Result<PathBuf, String>),
}

enum Phase {
    /// ReShade ricarica gli effetti e il feed ricostruisce la feature DLSS alla nuova
    /// risoluzione: ripresentiamo il primo frame finche' il log del feed dice che e' pronta,
    /// poi aspettiamo il Neural Rendering, che parte circa 12 s dopo il primo DLSS (i log di
    /// ReShade non sono affidabili per vederlo: con due istanze aperte si alternano i file).
    Preparing { since: f64, presents: u32, feed_at: Option<f64> },
    Encoding { index: i64 },
}

pub struct ExportJob {
    pub out_path: PathBuf,
    pub framing: Framing,
    pub total: i64,
    pub done: i64,
    pub fps: f64,
    phase: Phase,
    decoder: VideoDecoder,
    encoder: Child,
    stdin: Option<ChildStdin>,
    feed_log: PathBuf,
    feed_log_from: u64,
    encode_start: f64,
    processed: Vec<u8>,
}

/// "cartella\nome - ReShade 2x 1x1.mp4", con (2), (3)... se esiste gia'.
pub fn output_path(src: &Path, set: &Settings) -> PathBuf {
    let dir = if set.out_dir.is_empty() {
        src.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        PathBuf::from(&set.out_dir)
    };
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut base = format!("{stem} - ReShade {}", SCALE_LABELS[set.scale]);
    if set.aspect != 0 {
        base += &format!(" {}", ASPECTS[set.aspect].file);
    }
    let mut path = dir.join(format!("{base}.mp4"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base} ({n}).mp4"));
        n += 1;
    }
    path
}

fn feed_ready(log: &Path, from: u64, w: u32, h: u32) -> bool {
    let Ok(bytes) = std::fs::read(log) else { return false };
    let rest = String::from_utf8_lossy(bytes.get(from as usize..).unwrap_or_default());
    match rest.find(&format!("-> {w}x{h} backbuffer")) {
        Some(at) => rest[at..].contains("feature ready:"),
        None => false,
    }
}

impl ExportJob {
    pub fn start(
        r: &mut Renderer,
        src: &Path,
        info: &VideoInfo,
        set: &Settings,
        limit: Option<i64>,
        now: f64,
    ) -> Result<Self, String> {
        let f = framing::compute(info.width, info.height, set);
        if !set.hevc && (f.out_w > 4096 || f.out_h > 4096) {
            return Err("H.264 con NVENC arriva al massimo a 4096 pixel: scegli HEVC o una scala minore.".into());
        }
        let total = limit.map_or(info.frames, |l| l.min(info.frames));
        let out_path = output_path(src, set);

        let mut decoder = VideoDecoder::default();
        if !decoder.start(src, info, 0.0) {
            return Err("Non riesco ad avviare ffmpeg (e' nel PATH?).".into());
        }

        let mut cmd = ffmpeg("ffmpeg");
        cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "bgra"])
            .args(["-video_size", &format!("{}x{}", f.out_w, f.out_h)])
            .args(["-framerate", &info.fps_rational, "-i", "-", "-i"])
            .arg(src)
            .args(["-map", "0:v:0", "-map", "1:a:0?"]);
        if limit.is_some() {
            cmd.args(["-frames:v", &total.to_string()]);
        }
        cmd.args(["-vf", "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p"]);
        if set.hevc {
            cmd.args(["-c:v", "hevc_nvenc", "-tag:v", "hvc1"]);
        } else {
            cmd.args(["-c:v", "h264_nvenc", "-profile:v", "high"]);
        }
        cmd.args(["-preset", "p7", "-tune", "hq", "-rc", "vbr", "-cq", &QUALITY_CQ[set.quality].to_string(), "-b:v", "0"])
            .args(["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"])
            .args(["-c:a", "aac", "-b:a", "320k", "-shortest", "-movflags", "+faststart"])
            .arg(&out_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log_file("export-ffmpeg.log"));
        let mut encoder = cmd.spawn().map_err(|e| format!("Non riesco ad avviare ffmpeg: {e}"))?;
        let stdin = encoder.stdin.take();

        let Some(first) = decoder.pop_wait() else {
            let _ = encoder.kill();
            return Err("Il video non ha frame leggibili.".into());
        };
        let feed_log = exe_dir().join("dlss5-feed.log");
        let feed_log_from = std::fs::metadata(&feed_log).map(|m| m.len()).unwrap_or(0);
        r.view = None; // in export il video occupa tutto il back buffer
        r.resize(f.out_w, f.out_h);
        r.upload(&first);

        Ok(Self {
            out_path,
            framing: f,
            total,
            done: 0,
            fps: 0.0,
            phase: Phase::Preparing { since: now, presents: 0, feed_at: None },
            decoder,
            encoder,
            stdin,
            feed_log,
            feed_log_from,
            encode_start: now,
            processed: Vec::new(),
        })
    }

    pub fn preparing(&self) -> bool {
        matches!(self.phase, Phase::Preparing { .. })
    }

    pub fn step(&mut self, r: &mut Renderer, now: f64) -> Step {
        match &mut self.phase {
            Phase::Preparing { since, presents, feed_at } => {
                let _ = r.present(&self.framing, true);
                *presents += 1;
                if feed_at.is_none()
                    && *presents % 10 == 0
                    && feed_ready(&self.feed_log, self.feed_log_from, self.framing.out_w, self.framing.out_h)
                {
                    *feed_at = Some(now);
                }
                // 15 s dopo il feed: il NR e' attivo, la storia temporale stabile e il banner di
                // ReShade sparito. Senza feed (addon spento) si parte comunque dopo 60 secondi.
                if feed_at.is_some_and(|t| now - t > PREPARE_AFTER_FEED) || now - *since > 60.0 {
                    self.phase = Phase::Encoding { index: 0 };
                    self.encode_start = now;
                }
                Step::Working
            }
            Phase::Encoding { index } => {
                // Dopo il Present del frame i il back buffer contiene il frame i-1 elaborato
                let i = *index;
                *index = i + 1;
                if i >= self.total {
                    return self.finish(r);
                }
                if i > 0 {
                    match self.decoder.pop_wait() {
                        Some(frame) => r.upload(&frame),
                        None => return self.finish(r),
                    }
                }
                let _ = r.present(&self.framing, false);
                if i > 0 {
                    if let Err(e) = self.write_processed(r) {
                        return self.fail(e);
                    }
                }
                let secs = now - self.encode_start;
                if secs > 0.5 {
                    self.fps = self.done as f64 / secs;
                }
                Step::Working
            }
        }
    }

    fn write_processed(&mut self, r: &mut Renderer) -> Result<(), String> {
        let expected = self.framing.out_w as usize * self.framing.out_h as usize * 4;
        if !r.read_back(&mut self.processed) || self.processed.len() != expected {
            return Err("Lettura del frame dalla GPU fallita.".into());
        }
        let stdin = self.stdin.as_mut().ok_or("ffmpeg non accetta piu' dati.")?;
        stdin
            .write_all(&self.processed)
            .map_err(|_| "ffmpeg ha interrotto la codifica (vedi export-ffmpeg.log).".to_string())?;
        self.done += 1;
        Ok(())
    }

    /// Ultimo frame: un Present in piu' per farlo arrivare nel back buffer, poi chiude.
    fn finish(&mut self, r: &mut Renderer) -> Step {
        let _ = r.present(&self.framing, false);
        if let Err(e) = self.write_processed(r) {
            return self.fail(e);
        }
        self.decoder.stop();
        self.stdin = None; // chiude la pipe: ffmpeg finisce di scrivere il file
        match self.encoder.wait() {
            Ok(status) if status.success() => Step::Done(Ok(self.out_path.clone())),
            _ => self.fail("ffmpeg ha chiuso con un errore (vedi export-ffmpeg.log).".into()),
        }
    }

    fn fail(&mut self, msg: String) -> Step {
        self.abort();
        Step::Done(Err(msg))
    }

    fn abort(&mut self) {
        self.stdin = None;
        let _ = self.encoder.kill();
        let _ = self.encoder.wait();
        self.decoder.stop();
        let _ = std::fs::remove_file(&self.out_path);
    }

    pub fn cancel(mut self) {
        self.abort();
    }
}
