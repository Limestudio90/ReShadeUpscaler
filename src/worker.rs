//! Export in un processo separato.
//!
//! Il feed DLSS legge `work_resolution` da dlss5-feed.cfg solo quando si carica, e per un
//! upscale vero serve un valore diverso da quello dell'anteprima: a 2x DLSS e Neural Rendering
//! devono lavorare alla risoluzione nativa del video (50%), non sull'immagine gia' ingrandita.
//! Quindi l'app scrive il valore giusto nel cfg, avvia se stessa con `--worker`, e rimette il
//! cfg com'era quando il worker ha finito. Il worker comunica a righe su stdout:
//!   READY                       ReShade caricato
//!   P <prep> <done> <total> <fps>
//!   OK <percorso>  |  ERR <messaggio>
//! e si ferma se riceve "cancel" (o se si chiude il suo stdin).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::export::{ExportJob, Step};
use crate::framing::{self, Framing, Settings, SCALES};
use crate::media::{exe_dir, probe, VideoInfo};
use crate::render::Renderer;

fn feed_cfg() -> PathBuf {
    exe_dir().join("dlss5-feed.cfg")
}

/// Scrive `work_resolution=pct` nel cfg del feed e restituisce il contenuto precedente.
fn set_work_resolution(pct: u32) -> Option<String> {
    let old = std::fs::read_to_string(feed_cfg()).ok()?;
    let mut found = false;
    let mut text: String = old
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("work_resolution=") {
                found = true;
                format!("work_resolution={pct}\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    if !found {
        text += &format!("work_resolution={pct}\n");
    }
    std::fs::write(feed_cfg(), text).ok()?;
    Some(old)
}

pub struct RemoteExport {
    pub framing: Framing,
    pub total: i64,
    pub done: i64,
    pub fps: f64,
    preparing: bool,
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
    cfg_backup: Option<String>,
}

impl RemoteExport {
    pub fn start(src: &Path, info: &VideoInfo, set: &Settings, frames: Option<i64>) -> Result<Self, String> {
        let framing = framing::compute(info.width, info.height, set);
        // DLSS e NR lavorano alla risoluzione nativa del video; il feed riporta il risultato
        // alla dimensione d'uscita
        let pct = std::env::var("RSU_WORK_RES") // solo per i test
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| (100.0 / SCALES[set.scale]).round() as u32)
            .clamp(10, 100);
        let cfg_backup = set_work_resolution(pct);

        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(exe);
        cmd.current_dir(exe_dir())
            .arg("--worker")
            .arg(src)
            .args([
                "--set",
                &format!(
                    "{},{},{},{},{},{},{},{}",
                    set.scale, set.aspect, set.fit, set.bars_blur as u8, set.crop_cx, set.crop_cy, set.hevc as u8, set.quality
                ),
            ]);
        if !set.out_dir.is_empty() {
            cmd.args(["--outdir", &set.out_dir]);
        }
        if let Some(n) = frames {
            cmd.args(["--frames", &n.to_string()]);
        }
        let mut child = match cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
            Ok(c) => c,
            Err(e) => {
                if let Some(old) = &cfg_backup {
                    let _ = std::fs::write(feed_cfg(), old);
                }
                return Err(format!("Non riesco ad avviare l'esportazione: {e}"));
            }
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let total = frames.map_or(info.frames, |n| n.min(info.frames));
        Ok(Self { framing, total, done: 0, fps: 0.0, preparing: true, child, stdin, rx, cfg_backup })
    }

    pub fn preparing(&self) -> bool {
        self.preparing
    }

    fn restore_cfg(&mut self) {
        if let Some(old) = self.cfg_backup.take() {
            let _ = std::fs::write(feed_cfg(), old);
        }
    }

    /// Legge i messaggi del worker senza bloccare.
    pub fn step(&mut self) -> Step {
        while let Ok(line) = self.rx.try_recv() {
            let (kind, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            match kind {
                // il feed rilegge il cfg anche quando crea il suo device D3D12, quindi il cfg
                // si ripristina solo a fine export (finish / cancel / drop)
                "READY" => {}
                "P" => {
                    let v: Vec<&str> = rest.split(' ').collect();
                    if v.len() == 4 {
                        self.preparing = v[0] == "1";
                        self.done = v[1].parse().unwrap_or(self.done);
                        self.total = v[2].parse().unwrap_or(self.total);
                        self.fps = v[3].parse().unwrap_or(self.fps);
                    }
                }
                "OK" => {
                    self.finish();
                    return Step::Done(Ok(PathBuf::from(rest)));
                }
                "ERR" => {
                    self.finish();
                    return Step::Done(Err(rest.to_string()));
                }
                _ => {}
            }
        }
        if let Ok(Some(_)) = self.child.try_wait() {
            // uscito senza dire niente (crash): leggi le ultime righe al prossimo giro, poi errore
            if let Ok(line) = self.rx.try_recv() {
                if let Some(path) = line.strip_prefix("OK ") {
                    self.finish();
                    return Step::Done(Ok(PathBuf::from(path)));
                }
            }
            self.finish();
            return Step::Done(Err("L'esportazione si e' interrotta (vedi ReShade.log).".into()));
        }
        Step::Working
    }

    fn finish(&mut self) {
        self.restore_cfg();
        self.stdin = None;
        let _ = self.child.wait();
    }

    pub fn cancel(mut self) {
        if let Some(mut s) = self.stdin.take() {
            let _ = writeln!(s, "cancel");
        }
        // il worker chiude ffmpeg e cancella il file; se non risponde lo chiudiamo noi
        for _ in 0..50 {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = self.child.kill();
        self.finish();
    }
}

impl Drop for RemoteExport {
    fn drop(&mut self) {
        self.restore_cfg();
    }
}

// --- lato worker -------------------------------------------------------------------------------

extern "system" fn worker_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn parse_settings(s: &str, out_dir: String) -> Option<Settings> {
    let v: Vec<&str> = s.split(',').collect();
    if v.len() != 8 {
        return None;
    }
    Some(Settings {
        scale: v[0].parse::<usize>().ok()?.min(2),
        aspect: v[1].parse::<usize>().ok()?.min(5),
        fit: v[2].parse::<usize>().ok()?.min(2),
        bars_blur: v[3] == "1",
        crop_cx: v[4].parse().ok()?,
        crop_cy: v[5].parse().ok()?,
        hevc: v[6] == "1",
        quality: v[7].parse::<usize>().ok()?.min(2),
        out_dir,
    })
}

/// `ReShadeUpscaler.exe --worker <video> --set ... [--outdir d] [--frames n]`
pub fn run(mut args: impl Iterator<Item = String>) -> i32 {
    let mut src = None;
    let (mut set_arg, mut out_dir, mut frames) = (String::new(), String::new(), None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--set" => set_arg = args.next().unwrap_or_default(),
            "--outdir" => out_dir = args.next().unwrap_or_default(),
            "--frames" => frames = args.next().and_then(|v| v.parse().ok()),
            _ => src = Some(PathBuf::from(a)),
        }
    }
    let (Some(src), Some(set)) = (src, parse_settings(&set_arg, out_dir)) else {
        say("ERR Parametri dell'esportazione non validi.");
        return 2;
    };
    let Some(info) = probe(&src) else {
        say("ERR Non riesco a leggere il video.");
        return 2;
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    std::thread::spawn(move || {
        let mut line = String::new();
        // "cancel" oppure stdin chiuso (l'app e' stata chiusa)
        let _ = std::io::stdin().read_line(&mut line);
        flag.store(true, Ordering::Relaxed);
    });

    // Finestra nascosta: ReShade elabora i Present anche se non e' visibile
    let hwnd = unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        let wc = WNDCLASSW { lpfnWndProc: Some(worker_proc), hInstance: hinst, lpszClassName: w!("RSUPreview"), ..Default::default() };
        RegisterClassW(&wc);
        match CreateWindowExW(WS_EX_TOOLWINDOW, w!("RSUPreview"), w!("ReShade Upscaler export"), WS_POPUP, 0, 0, 256, 256, None, None, Some(hinst), None) {
            Ok(h) => h,
            Err(_) => {
                say("ERR Non riesco a creare la finestra di esportazione.");
                return 2;
            }
        }
    };
    let mut r = match Renderer::new(hwnd) {
        Ok(r) => r,
        Err(e) => {
            say(&format!("ERR Inizializzazione D3D11 fallita: {e}"));
            return 2;
        }
    };
    say("READY");
    if let Err(e) = r.set_video(info.width, info.height) {
        say(&format!("ERR Errore della GPU: {e}"));
        return 2;
    }

    let start = std::time::Instant::now();
    let now = || start.elapsed().as_secs_f64();
    let mut job = match ExportJob::start(&mut r, &src, &info, &set, frames, now()) {
        Ok(j) => j,
        Err(e) => {
            say(&format!("ERR {e}"));
            return 2;
        }
    };
    let mut last = 0.0;
    loop {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
        }
        if cancel.load(Ordering::Relaxed) {
            job.cancel();
            say("ERR Esportazione annullata.");
            return 1;
        }
        match job.step(&mut r, now()) {
            Step::Working => {
                if now() - last > 0.2 {
                    last = now();
                    say(&format!("P {} {} {} {:.2}", job.preparing() as u8, job.done, job.total, job.fps));
                }
            }
            Step::Done(Ok(p)) => {
                say(&format!("OK {}", p.display()));
                return 0;
            }
            Step::Done(Err(e)) => {
                say(&format!("ERR {e}"));
                return 2;
            }
        }
    }
}
