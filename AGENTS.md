# AGENTS.md

Guidance for AI coding agents working in this repository. See [README.md](README.md) for the user-facing overview.

## Project

vr-space is a macOS app (Rust) that turns 3DoF AR glasses into a head-tracked virtual monitor: it reads head orientation from the glasses, captures the Mac's screen with ScreenCaptureKit, and draws it in a 3D scene with wgpu on Metal. Latency is the main design goal.

Platform: macOS only (developed on Apple Silicon). Screen capture code is behind `#[cfg(target_os = "macos")]`.

## Layout

Cargo workspace with two crates:

- `apps/headset-visualizer/`: the main app.
  - `src/main.rs`: CLI (`clap`), head-tracking thread, wgpu setup, scene, HUD and event loop, all in one file.
  - `src/macos_capture.rs`: ScreenCaptureKit stream (`start_stream`, `take_latest_bgra`, `stop_stream`). Captures at the display's native pixel size and keeps only the newest frame. The capture callback and renderer hand frames over by swapping buffers (no per-frame copy or allocation), and a sequence counter lets the renderer skip the upload when nothing new arrived.
  - `src/shader.wgsl`: shader for the grid scene. The textured screen quad's shader is inline in `main.rs`.
  - `assets/NotoSans-Regular.ttf`: HUD font, embedded with `include_bytes!`. Licensed SIL OFL 1.1 (`assets/OFL.txt`).
- `capture-check/`: standalone tool that checks ScreenCaptureKit works and records raw BGRA frames (32-byte `BGR4` header, see `write_header`), with optional MP4 output through `ffmpeg`.

Libraries come from Cargo, not from this repo:

- `ar-drivers` (head tracking): git dependency on `github.com/3rl-io/headset-utils`, pinned to a commit `rev`. Entry point: `ar_drivers::any_fusion()`, then `fusion.update()` and `fusion.attitude_frd_rad()`.
- `screencapturekit` 0.3.5 from crates.io.

`headset-utils/` and `screencapturekit-rs/` may exist on disk as local clones of those upstream repos. They are git-ignored and not workspace members: don't edit them or reference them by path.

## How the visualizer works

1. A reader thread calls `any_fusion()` (retrying every second until glasses are found), polls the fusion as fast as `--poll-ms` allows and publishes the raw roll/pitch/yaw into `PoseAtomic` (`f32` stored as `u32` bits in atomics), so the render loop never waits on a lock.
2. The winit event loop reads the latest pose, turns it into a head rotation (`head_rotation_from_pose`), applies the calibration and builds the view matrix (`camera_view`). When a new captured frame is available it uploads it with `queue.write_texture`, recreating the screen texture and resizing the quad if the frame size changed. Then it draws the grid, screen quad and HUD.
3. Calibration lives on the render thread: clicking the HUD's Calibrate button (hit-tested in physical pixels) or pressing `C` sets `calibrate_pending`, and the next frame stores the inverse of the current head rotation as a quaternion (`calib_inv`).
4. Present mode prefers Mailbox, then Immediate, then Fifo, unless `--vsync` is passed.

When changing the hot path (pose reading, frame upload, rendering), don't add locks, allocations or blocking calls per frame.

## Commands

```bash
cargo build --workspace
```

```bash
cargo run --release -p headset-visualizer -- --help
```

```bash
cargo run --release -p capture-check -- --seconds 5 --out capture.bgra
```

There are no automated tests yet. To verify a change, at minimum build the workspace and run each binary with `--help`. Real behaviour needs AR glasses over USB and Screen Recording permission for the terminal, which an agent usually can't provide. Say so rather than claiming a change works on hardware.

The build currently shows 3 warnings (an unused `stop_stream`, an unread `has_device` field and an unused `render_bundle` initial value). Don't introduce new warnings.

## Dependency constraints

- `wgpu` 0.17, `wgpu_glyph` `=0.21.0` and `winit` 0.28 are pinned together on purpose: `wgpu_glyph` 0.21 needs the wgpu 0.17 API and winit 0.28 matches its `raw-window-handle` 0.5. Upgrading one means upgrading all three.
- To update `ar-drivers`, change the `rev` in `apps/headset-visualizer/Cargo.toml` to a specific commit. Don't switch to a local `path` dependency.

## Licensing

The project is MIT (`LICENSE`). When adding anything:

- New dependencies must be permissively licensed (MIT, Apache-2.0, BSD, ISC, Zlib or similar). Flag anything GPL, LGPL or MPL before adding it.
- Don't copy code from other projects without keeping its license and crediting it in the README.
- New assets (fonts, images) need a license that allows redistribution, with the license file next to them.
- libusb (LGPL-2.1) is already linked statically through `rusb`. Keep the README's note about it accurate.

## Conventions

- Keep platform-specific code behind `cfg(target_os = "macos")`.
- Never hard-code paths from a developer's machine (for example `/Users/...`). Use `env!("CARGO_MANIFEST_DIR")` or embed the file.
- Keep CLI flags documented in the README's options table when you add or change one.
- Don't commit build output, `.DS_Store`, or capture files (`*.bgra`, `*.mp4`). `.gitignore` covers them.
