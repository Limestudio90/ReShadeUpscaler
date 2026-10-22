# ReShade Upscaler

A small Windows app that runs a video through **ReShade** — in particular the DLSS 5
Neural Rendering add-ons — and exports the processed result as an MP4 with the original
audio. It replaces the old workaround of loading the clip into OBS, injecting ReShade and
screen-recording the output.

Import a video, tune the effect live in the ReShade overlay, compare against the original,
export.

## Features

- **Live preview** with synced audio, seek bar, loop.
- **Compare** — ReShade / Original / draggable Split view.
- **Output scale** 1x, 1.5x, 2x. At 2x, DLSS and Neural Rendering run on the video at its
  native resolution and the result is brought up to the output size, so the preview shows
  what the export will look like.
- **Aspect ratio** — original, 9:16, 16:9, 1:1, 4:5, 21:9 — with crop (drag to frame), fit
  (black or blurred bars) or stretch.
- **Export** with NVENC — HEVC or H.264, three quality levels — and the source audio
  (AAC 320k). It runs in a separate process so the UI stays responsive, with progress and
  cancel.
- Neural Rendering on/off indicator, with a warning before exporting with it off.

## How it works

ReShade hooks every DXGI swap chain in the process. The app draws each video frame into a
D3D11 swap chain, presents it so ReShade and its add-ons process it, and reads the processed
frame back from the back buffer. Frames are piped to `ffmpeg` (`hevc_nvenc` / `h264_nvenc`).

The UI is drawn with Direct2D on a GDI DC, with no swap chain, so ReShade leaves it alone.

The preview back buffer has the same size the effects work at during export: output size
divided by scale. That way Neural Rendering gets the same input in both places.

Two details matter for the export:

- **It runs in a worker process** (`ReShadeUpscaler.exe --worker …`). The dlss5-feed add-on
  reads `work_resolution` only when it loads, so the app writes `100 / scale` into
  `dlss5-feed.cfg` before starting the worker and restores it afterwards.
- **It waits before recording.** Neural Rendering only activates about 12 seconds after the
  first DLSS evaluation in a fresh process, and the ReShade startup banner would otherwise
  end up in the first frames. So the worker presents the first frame until the feed reports
  that DLSS is ready, waits 15 more seconds, then starts encoding.

## Requirements

- Windows 10/11 x64, an NVIDIA RTX GPU (DLSS + NVENC).
- [`ffmpeg`](https://ffmpeg.org/) and `ffprobe` on `PATH` (tested with 8.0).
- In the folder next to the exe — **not included in this repo**, get them from their authors:
  - [ReShade](https://reshade.me/) with add-on support, installed as `dxgi.dll`, plus
    `ReShade.ini`, a preset and `reshade-shaders\`;
  - the DLSS 5 add-ons, `dlss5-feed.addon64` and `renodx-dlss5.addon64`, with their
    shaders (`DLSS5_Feed.fx`, a motion-vector provider such as MartysMods Launchpad);
  - NVIDIA `nvngx_dlss.dll` / `nvngx_dlssnr.dll`.

## Build

```
cargo build --release
```

`build.rs` embeds `assets\app.ico` with `rc.exe` from the Windows SDK. If `rc.exe` is not
found, the exe is built without the icon. Copy `target\release\ReShadeUpscaler.exe` into the
folder that holds ReShade. That folder must be writable by the user, because ReShade, the
add-ons and the app save their settings there.

`assets\make-icon.ps1` regenerates the icon.

## Usage

| Key | Action |
| --- | --- |
| `Ins` | ReShade overlay |
| `Del` | Toggle Neural Rendering (saved in `ReShade.ini`, affects the export too) |
| `Space` | Play / pause |
| `C` | Cycle compare mode |
| `←` / `→` | Seek ±5 s |
| `Ctrl+O` / `Ctrl+E` | Import / export |

Drag a video onto the window to load it. Exports go next to the source video, or into the
folder chosen in the panel, named `<name> - ReShade 2x[ 9x16].mp4`.

Command-line options for testing:
`ReShadeUpscaler.exe video.mp4 [--export] [--frames N] [--scale 1|1.5|2] [--aspect 0-5] [--fit 0-2] [--compare 0-2]`

## Code

| File | What it does |
| --- | --- |
| `src/main.rs` | Windows, UI layout, playback, compare, input |
| `src/render.rs` | D3D11 swap chain, framing shader (bicubic / blurred bars), back buffer readback |
| `src/export.rs` | Export state machine: warm-up, present, readback, ffmpeg pipe |
| `src/worker.rs` | Export worker process and its line protocol |
| `src/media.rs` | ffprobe, ffmpeg video decoder, waveOut audio player |
| `src/framing.rs` | Scale, aspect and crop / fit / stretch math |
| `src/ui.rs` | Direct2D / DirectWrite drawing helpers |

Code comments are in Italian.

## License

[MIT](LICENSE) © 2026 Limestudio90. This covers only the code in this repository: ReShade,
the DLSS 5 add-ons and the NVIDIA libraries keep their own licenses.
