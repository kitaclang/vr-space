# vr-space

A low-latency macOS app that turns 3DoF AR glasses into a head-tracked virtual monitor.

`headset-visualizer` reads head orientation from the glasses, captures your Mac's screen with ScreenCaptureKit, and renders it in a 3D scene with wgpu (Metal), so the screen stays fixed in space as you turn your head.

> Status: experimental. Developed and tested only on macOS (Apple Silicon).

## What's in this repo

| Crate | What it does |
|---|---|
| [`apps/headset-visualizer`](apps/headset-visualizer) | The main app: head-tracked virtual screen with a small HUD and a calibrate button |
| [`capture-check`](capture-check) | A small tool that checks ScreenCaptureKit works and records raw BGRA frames (optionally to MP4 via ffmpeg) |

## Supported glasses

Head tracking comes from [`ar-drivers`](https://github.com/3rl-io/headset-utils), which supports Rokid Air/Max, XREAL (Nreal) Light/Air/Air 2/Air 2 Pro, Mad Gaze and Grawoow glasses. Which models work on macOS depends on that library.

## Requirements

- macOS 12.3 or later (ScreenCaptureKit)
- Rust (stable), installed with [rustup](https://rustup.rs)
- Supported AR glasses connected over USB
- **Screen Recording permission** for your terminal app: System Settings → Privacy & Security → Screen Recording
- Optional, for `capture-check --mp4-out`: `ffmpeg` on your `PATH`

## Usage

Run the visualizer (release build recommended for lowest latency):

```bash
cargo run --release -p headset-visualizer
```

Put the glasses on, look straight ahead and click **Calibrate** in the HUD (or press `C`) to set the forward direction. If the glasses aren't connected yet, plug them in: the app keeps looking for them every second.

Options:

| Flag | Default | Meaning |
|---|---|---|
| `--vsync` | off | Enable vsync (off gives the lowest latency) |
| `--max-fps <n>` | 0 (unlimited) | Cap the frame rate when vsync is off |
| `--poll-ms <ms>` | 0 (as fast as possible) | How often to read head tracking |
| `--invert-x` / `--invert-y` / `--invert-z` | off | Flip roll / pitch / yaw |
| `--no-hud` | off | Hide the text overlay |
| `--hud-size <px>` | 16 | HUD font size |

Check screen capture on its own:

```bash
cargo run --release -p capture-check -- --seconds 5 --out capture.bgra
```

`capture.bgra` starts with a 32-byte header (magic `BGR4`, version, width, height, stride, bytes per pixel, frame count), followed by raw BGRA frames. Add `--mp4-out capture.mp4` to also encode an H.264 video with ffmpeg. Run with `--help` for all options.

## Acknowledgements

- [3rl-io/spidgets-3dof](https://github.com/3rl-io/spidgets-3dof) by Kiran Kota and the 3rl.io team, the browser-based 3DoF AR project that inspired this one. vr-space is a native rewrite of the idea and shares no code with it.
- [3rl-io/headset-utils](https://github.com/3rl-io/headset-utils), a fork of [badicsalex/ar-drivers-rs](https://github.com/badicsalex/ar-drivers-rs) by Alex Badics, Peng Cheng and the 3rl.io team (MIT). The head-tracking loop follows its `euler_60` example.
- [doom-fish/screencapturekit-rs](https://github.com/doom-fish/screencapturekit-rs) by Per Johansson and contributors (MIT OR Apache-2.0).

## License

vr-space is released under the [MIT License](LICENSE).

Third-party components keep their own licenses:

- **Noto Sans** (`apps/headset-visualizer/assets/NotoSans-Regular.ttf`, embedded in the app binary): SIL Open Font License 1.1, see [`OFL.txt`](apps/headset-visualizer/assets/OFL.txt).
- **Rust dependencies** are fetched by Cargo and are almost all MIT, Apache-2.0, BSD, ISC or Zlib licensed. `serialport` is MPL-2.0.
- **libusb** (LGPL-2.1) is compiled statically into the binary through `rusb`/`libusb1-sys`. This repository ships source only. If you distribute compiled binaries, you need to meet the LGPL's terms for libusb (for example, by providing its source and allowing it to be relinked).

Rokid, XREAL, Nreal and other product names are trademarks of their respective owners and are used only to describe compatibility. This project is not affiliated with or endorsed by them.
