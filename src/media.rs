//! Lettura del video con ffmpeg: probe, decoder video (frame BGRA) e audio della preview.

use std::fs::File;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::Media::Audio::*;

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn ffmpeg(tool: &str) -> Command {
    let mut c = Command::new(tool);
    c.creation_flags(CREATE_NO_WINDOW).stdin(Stdio::null()).stderr(Stdio::null());
    c
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn log_file(name: &str) -> Stdio {
    File::create(exe_dir().join(name)).map(Stdio::from).unwrap_or_else(|_| Stdio::null())
}

#[derive(Clone, Default)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub fps_rational: String, // passato tale e quale a ffmpeg (es. 30000/1001)
    pub frames: i64,
    pub duration: f64, // durata della traccia video, in secondi
    pub has_audio: bool,
}

fn run_capture(args: &[&str], path: &Path) -> String {
    ffmpeg("ffprobe")
        .args(args)
        .arg(path)
        .stdout(Stdio::piped())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Dimensioni, fps, numero di frame, rotazione e presenza dell'audio. ffmpeg applica la
/// rotazione in decodifica, quindi con 90 gradi larghezza e altezza vanno scambiate.
pub fn probe(path: &Path) -> Option<VideoInfo> {
    let out = run_capture(
        &[
            "-v", "error", "-select_streams", "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate,nb_frames:stream_side_data=rotation:format=duration",
            "-of", "default=nw=1",
        ],
        path,
    );
    let mut info = VideoInfo { fps: 25.0, fps_rational: "25".into(), ..Default::default() };
    let mut rotation = 0i32;
    let mut duration = 0f64;
    for line in out.lines() {
        let Some((key, value)) = line.trim().split_once('=') else { continue };
        match key {
            "width" => info.width = value.parse().unwrap_or(0),
            "height" => info.height = value.parse().unwrap_or(0),
            "rotation" => rotation = value.parse().unwrap_or(0),
            "nb_frames" => info.frames = value.parse().unwrap_or(0),
            "duration" => duration = value.parse().unwrap_or(0.0),
            "r_frame_rate" => {
                if let Some((n, d)) = value.split_once('/') {
                    let (n, d): (f64, f64) = (n.parse().unwrap_or(0.0), d.parse().unwrap_or(0.0));
                    if n > 0.0 && d > 0.0 {
                        info.fps = n / d;
                        info.fps_rational = value.to_string();
                    }
                }
            }
            _ => {}
        }
    }
    if rotation.abs() == 90 || rotation.abs() == 270 {
        std::mem::swap(&mut info.width, &mut info.height);
    }
    if info.frames <= 0 && duration > 0.0 {
        info.frames = (duration * info.fps).round() as i64;
    }
    info.duration = info.frames as f64 / info.fps;

    let audio = run_capture(
        &["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=index", "-of", "csv=p=0"],
        path,
    );
    info.has_audio = !audio.trim().is_empty();
    (info.width > 0 && info.height > 0 && info.frames > 0).then_some(info)
}

// -------------------------------------------------------------------------------------------
// Video
// -------------------------------------------------------------------------------------------

/// ffmpeg -> coda (max 4) di frame BGRA alla risoluzione del video.
#[derive(Default)]
pub struct VideoDecoder {
    child: Option<Child>,
    rx: Option<Receiver<Vec<u8>>>,
    thread: Option<JoinHandle<()>>,
    peeked: Option<Vec<u8>>,
}

impl VideoDecoder {
    pub fn start(&mut self, path: &Path, info: &VideoInfo, start_seconds: f64) -> bool {
        self.stop();
        let mut cmd = ffmpeg("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error"]);
        if start_seconds > 0.0 {
            cmd.args(["-ss", &format!("{start_seconds:.6}")]);
        }
        cmd.arg("-i").arg(path).args([
            "-vf", &format!("scale={}:{}", info.width, info.height),
            "-f", "rawvideo", "-pix_fmt", "bgra", "-",
        ]);
        let Ok(mut child) = cmd.stdout(Stdio::piped()).spawn() else { return false };
        let mut stdout = child.stdout.take().unwrap();
        let frame_size = info.width as usize * info.height as usize * 4;
        let (tx, rx) = sync_channel::<Vec<u8>>(3);
        self.thread = Some(std::thread::spawn(move || loop {
            let mut frame = vec![0u8; frame_size];
            if stdout.read_exact(&mut frame).is_err() || tx.send(frame).is_err() {
                return;
            }
        }));
        self.child = Some(child);
        self.rx = Some(rx);
        true
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.rx = None; // sblocca un send in attesa
        self.peeked = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    pub fn has_frame(&mut self) -> bool {
        if self.peeked.is_none() {
            if let Some(rx) = &self.rx {
                self.peeked = rx.try_recv().ok();
            }
        }
        self.peeked.is_some()
    }

    /// Il frame successivo, se pronto. Non blocca.
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        if let Some(f) = self.peeked.take() {
            return Some(f);
        }
        match self.rx.as_ref()?.try_recv() {
            Ok(f) => Some(f),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Aspetta il frame successivo. None a fine video.
    pub fn pop_wait(&mut self) -> Option<Vec<u8>> {
        if let Some(f) = self.peeked.take() {
            return Some(f);
        }
        self.rx.as_ref()?.recv().ok()
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        self.stop();
    }
}

// -------------------------------------------------------------------------------------------
// Audio (preview)
// -------------------------------------------------------------------------------------------

const RATE: u32 = 48000;
const CHUNK: usize = RATE as usize * 4 / 10; // 100 ms di PCM s16 stereo
const RING: usize = 8;

#[derive(Clone, Copy)]
struct WaveOut(HWAVEOUT);
unsafe impl Send for WaveOut {}

struct AudioStream {
    out: WaveOut,
    child: Child,
    thread: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
    finished: Arc<AtomicBool>,
}

/// Audio della preview: ffmpeg decodifica PCM, waveOut lo suona. Il tempo lo tiene l'app;
/// a ogni seek, pausa o loop audio e video ripartono insieme.
#[derive(Default)]
pub struct AudioPlayer {
    stream: Option<AudioStream>,
}

impl AudioPlayer {
    /// Prepara l'audio da `start_seconds`, in pausa.
    pub fn load(&mut self, path: &Path, start_seconds: f64) {
        self.stop();
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut h = HWAVEOUT::default();
        if unsafe { waveOutOpen(Some(&mut h), WAVE_MAPPER, &format, None, None, CALLBACK_NULL) } != 0 {
            return;
        }
        unsafe { waveOutPause(h) };

        let mut cmd = ffmpeg("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-ss", &format!("{start_seconds:.6}"), "-i"])
            .arg(path)
            .args(["-vn", "-f", "s16le", "-ac", "2", "-ar", &RATE.to_string(), "-"]);
        let Ok(mut child) = cmd.stdout(Stdio::piped()).spawn() else {
            unsafe { waveOutClose(h) };
            return;
        };
        let stdout = child.stdout.take().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let queued = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let out = WaveOut(h);
        let thread = {
            let (stop, queued, finished) = (stop.clone(), queued.clone(), finished.clone());
            std::thread::spawn(move || audio_thread(out, stdout, stop, queued, finished))
        };
        self.stream = Some(AudioStream { out, child, thread, stop, queued, finished });
    }

    /// Abbastanza audio in coda per partire senza buchi.
    pub fn ready(&self) -> bool {
        match &self.stream {
            Some(s) => s.queued.load(Ordering::Relaxed) >= 2 || s.finished.load(Ordering::Relaxed),
            None => true,
        }
    }

    pub fn play(&self) {
        if let Some(s) = &self.stream {
            unsafe { waveOutRestart(s.out.0) };
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut s) = self.stream.take() {
            s.stop.store(true, Ordering::Relaxed);
            let _ = s.child.kill();
            let _ = s.child.wait();
            let _ = s.thread.join();
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn audio_thread(
    out: WaveOut,
    mut stdout: ChildStdout,
    stop: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
    finished: Arc<AtomicBool>,
) {
    let h = out.0;
    let hdr_size = std::mem::size_of::<WAVEHDR>() as u32;
    let mut buffers: Vec<Vec<u8>> = (0..RING).map(|_| vec![0u8; CHUNK]).collect();
    let mut headers: Box<[WAVEHDR; RING]> = Box::new(unsafe { std::mem::zeroed() });
    let mut in_use = [false; RING];
    let mut chunk = vec![0u8; CHUNK];

    'feed: while !stop.load(Ordering::Relaxed) {
        // Leggi 100 ms (o quello che resta)
        let mut got = 0;
        while got < CHUNK {
            match stdout.read(&mut chunk[got..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
        }
        got -= got % 4;
        if got == 0 {
            break;
        }
        // Aspetta un buffer libero (gia' suonato)
        let slot = loop {
            if stop.load(Ordering::Relaxed) {
                break 'feed;
            }
            let free = (0..RING).find(|&i| {
                !in_use[i] || unsafe { std::ptr::read_volatile(std::ptr::addr_of!(headers[i].dwFlags)) } & WHDR_DONE != 0
            });
            if let Some(i) = free {
                break i;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        unsafe {
            if in_use[slot] {
                waveOutUnprepareHeader(h, &mut headers[slot], hdr_size);
            }
            buffers[slot][..got].copy_from_slice(&chunk[..got]);
            headers[slot] = std::mem::zeroed();
            headers[slot].lpData = windows::core::PSTR(buffers[slot].as_mut_ptr());
            headers[slot].dwBufferLength = got as u32;
            waveOutPrepareHeader(h, &mut headers[slot], hdr_size);
            waveOutWrite(h, &mut headers[slot], hdr_size);
        }
        in_use[slot] = true;
        queued.fetch_add(1, Ordering::Relaxed);
        if got < CHUNK {
            break;
        }
    }
    finished.store(true, Ordering::Relaxed);
    // I buffer devono restare vivi finche' il dispositivo li usa
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    unsafe {
        waveOutReset(h);
        for i in 0..RING {
            if in_use[i] {
                waveOutUnprepareHeader(h, &mut headers[i], hdr_size);
            }
        }
        waveOutClose(h);
    }
}
