# AGENTS.md

Guidance for AI coding agents working in this repository. See [README.md](README.md) for the user-facing overview.

## Project

vr-space is a macOS app (Rust) that turns 3DoF AR glasses into a head-tracked virtual monitor: it reads head orientation from the glasses, captures the Mac's screen with ScreenCaptureKit, and draws it in a 3D scene with wgpu on Metal. Latency is the main design goal.

Platform: macOS only (developed on Apple Silicon). Screen capture code is behind `#[cfg(target_os = "macos")]`.

## Layout

Cargo workspace with two crates:

- `apps/headset-visualizer/`: the main app.
  - `src/main.rs`: CLI (`clap`), head-tracking thread, wgpu setup (surface, device), HUD and event loop.
  - `src/scene.rs`: the 3D scene, `Scene`: the floor grid and the textured screen quad, with the screen quad's shader inline. It needs only a device, queue and target format, not a window: `set_view_proj`, `upload_frame` (recreates the texture when the frame size changes, rejects frames that don't fit) and `encode` (one render pass, grid then screen). Its tests render it offscreen on the real GPU and check pixels.
  - `src/menu_bar.rs`: menu bar icon and menu (`tray-icon`) and shortcuts that work from any app (`global-hotkey`). Both send a `Command` to the winit event loop through an `EventLoopProxy`; the window's own keys produce the same `Command`s, and one `match` in the event loop carries them out. The icon is drawn in code (`icon::glasses`).
  - The app runs without a Dock icon (`ActivationPolicy::Accessory`). Because of that, and because macOS native fullscreen is unreliable on a secondary display, fullscreen on macOS is done by hand in `apply_fullscreen`: no border, the window covers the display and sits above the menu bar (`set_above_menu_bar`, via `objc2`). Don't switch it back to winit's `Fullscreen`. It's applied at `StartCause::Init`, and `Event::LoopDestroyed` stops the capture for every way of quitting.
  - `src/macos_capture.rs`: ScreenCaptureKit stream (`start_stream`, `take_latest_bgra`, `stop_stream`, and `switch_source` to capture another display or window). A `CaptureSource` is a `Display` or a `Window` (desktop-independent window filter). Captures at the native pixel size (for a window, its size in points times the scale of the display it's on) and keeps only the newest frame. `start_window_watcher` runs a background thread that lists windows every 2 s for the menu bar (`windows_if_changed`) and reports when the captured window is resized or closed (`take_window_event`), which the render loop checks on its 2 s timer and handles by restarting the stream or going back to the last display. Window owners are checked with `has_owner` before calling `owning_application`, which panics on windows without one. The capture callback and renderer hand frames over by swapping buffers (no per-frame copy or allocation), and a sequence counter lets the renderer skip the upload when nothing new arrived. It also checks Screen Recording permission (`has_screen_recording_permission`, `open_screen_recording_settings`) and exposes `capture_status()`, which the HUD reads every frame (atomics only).
  - `src/macos_app.rs`: what changes when running as `vr-space.app` (`in_bundle()`): the log goes to `~/Library/Logs/vr-space.log` when there's no terminal, permission messages name vr-space instead of the terminal (`permission_holder()`), Finder's `-psn_` argument is dropped before `clap` parses, and the menu bar's Open at Login uses `SMAppService` (macOS 13+, via `objc2`; `None` when unavailable).
  - `src/icon.rs`: the icons, drawn with signed distance functions so there's no image asset to license: `glasses` (shared with the menu bar icon) and `app_icon_rgba`, the app icon on Apple's 1024 grid. The hidden `--write-iconset <DIR>` flag writes it at every size as PNGs (`png` crate) for the bundle script.
  - `src/settings.rs`: settings saved between runs as JSON in `~/Library/Application Support/vr-space/settings.json` (`Settings`, every field optional; `SettingsStore` loads at startup and `save` writes atomically only when something changed). In `main.rs`, `ExplicitFlags` (from `clap`'s `value_source`) and `StartupPlan` decide startup values: flags given on the command line win, saved settings fill in the rest. Settings are updated only in the `Command` handler, after a user change, so a fallback (a saved window that isn't open, glasses not yet connected) never overwrites them. A saved fullscreen display that isn't connected is kept in `pending_fullscreen` and applied on the 2 s timer once it appears.
  - `macos/Info.plist`: template for the app bundle (`LSUIElement`, bundle ID `io.github.kitaclang.vr-space`); `@VERSION@` is filled in from `Cargo.toml`.
  - `src/shader.wgsl`: shader for the grid. The screen quad's shader is inline in `scene.rs`.
  - `assets/NotoSans-Regular.ttf`: HUD font, embedded with `include_bytes!`. Licensed SIL OFL 1.1 (`assets/OFL.txt`).
- `scripts/bundle-macos.sh`: builds the release binary and wraps it in `target/release/vr-space.app` (executable `Contents/MacOS/vr-space`), adds `Resources/AppIcon.icns` (from `--write-iconset` and `iconutil`), signs it (ad-hoc unless `VR_SPACE_SIGN_IDENTITY` is set) and with `--install` copies it to `~/Applications`.
- `capture-check/`: standalone tool that checks ScreenCaptureKit works and records raw BGRA frames (32-byte `BGR4` header, see `write_header`), with optional MP4 output through `ffmpeg`.

Libraries come from Cargo, not from this repo:

- `ar-drivers` (head tracking): git dependency on `github.com/3rl-io/headset-utils`, pinned to a commit `rev`. Entry point: `ar_drivers::any_fusion()`, then `fusion.update()` and `fusion.attitude_frd_rad()`. Careful: `any_fusion()` calls `std::process::exit(0)` when no glasses are found, so always probe with `ar_drivers::any_glasses()` first (it returns an error instead). A USB read error inside `update()` also exits the process; that's upstream behaviour.
- `screencapturekit` 0.3.5 from crates.io.
- `tray-icon` 0.26 (with `muda` for menus) and `global-hotkey` 0.9 from crates.io, both MIT OR Apache-2.0. On macOS the tray icon must be created after the event loop starts (`StartCause::Init`).

`headset-utils/` and `screencapturekit-rs/` may exist on disk as local clones of those upstream repos. They are git-ignored and not workspace members: don't edit them or reference them by path.

## How the visualizer works

1. A reader thread probes with `any_glasses()` every second until glasses are found, then calls `any_fusion()`, polls the fusion as fast as `--poll-ms` allows and publishes the raw roll/pitch/yaw into `PoseAtomic` (`f32` stored as `u32` bits in atomics), so the render loop never waits on a lock.
2. The winit event loop reads the latest pose, turns it into a head rotation (`head_rotation_from_pose`), applies the calibration and builds the view matrix (`camera_view`). When a new captured frame is available it passes it to `Scene::upload_frame`, which copies it with `queue.write_texture`, recreating the screen texture and resizing the quad if the frame size changed. Then `Scene::encode` draws the grid and screen quad, and the HUD is drawn on top.
3. Calibration lives on the render thread: clicking the HUD's Calibrate button (hit-tested in physical pixels), pressing `C`, or the menu bar's Recenter or `⌃⌥C` sends `Command::Recenter`, which sets `calibrate_pending`, and the next frame stores the inverse of the current head rotation as a quaternion (`calib_inv`), or with `--recenter-yaw-only` the inverse of just its heading (`recenter_calibration`).
4. Present mode prefers Mailbox, then Immediate, then Fifo, unless `--vsync` is passed.

When changing the hot path (pose reading, frame upload, rendering), don't add locks, allocations or blocking calls per frame.

## Commands

```bash
cargo build --workspace
```

```bash
cargo test --workspace
```

```bash
cargo run --release -p headset-visualizer -- --help
```

```bash
cargo run --release -p capture-check -- --seconds 5 --out capture.bgra
```

```bash
scripts/bundle-macos.sh
```

Unit tests (`#[cfg(test)]` modules at the end of `main.rs`, `macos_capture.rs`, `menu_bar.rs`, `macos_app.rs`, `settings.rs`, `icon.rs` and `scene.rs`) cover what doesn't need hardware: the axis mapping, recentering, camera and neck model, screen placement, CLI validation, app bundle detection and `-psn_` filtering, window matching for `--window` and saved windows, menu IDs, saved settings (parsing, saving, and how they combine with flags), the app icon and iconset, what the scene draws (rendered offscreen on the GPU: the screen is upright and not mirrored, takes the frame's aspect ratio and row stride, moves the right way when you turn, rejects frames that don't fit; skipped with a note when there's no GPU adapter), and the capture frame handoff (through `publish_frame`, the part of the capture callback that doesn't touch ScreenCaptureKit). To verify a change, at minimum build the workspace, run `cargo test --workspace` and run each binary with `--help`. Add or update tests when you change those parts. Real behaviour needs AR glasses over USB and Screen Recording permission for the terminal, which an agent usually can't provide. Say so rather than claiming a change works on hardware.

The workspace crates build with no warnings. Cargo still prints a future-incompatibility note for the `block` dependency, which comes from upstream. Don't introduce new warnings.

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
- When you finish or add planned work, update [ROADMAP.md](ROADMAP.md) so it stays current.
- Don't commit build output, `.DS_Store`, or capture files (`*.bgra`, `*.mp4`). `.gitignore` covers them.
