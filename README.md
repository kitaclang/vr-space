# vr-space

A low-latency macOS app that turns 3DoF AR glasses into a head-tracked virtual monitor.

`headset-visualizer` reads head orientation from the glasses, captures your Mac's screen with ScreenCaptureKit, and renders it in a 3D scene with wgpu (Metal), so the screen stays fixed in space as you turn your head.

> Status: experimental. Developed and tested only on macOS (Apple Silicon). See [ROADMAP.md](ROADMAP.md) for what's planned.

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
- **Screen Recording permission** for vr-space.app, or for your terminal app if you run it with `scripts/cargo.sh run`: System Settings → Privacy & Security → Screen Recording
- Optional, for `capture-check --mp4-out`: `ffmpeg` on your `PATH`

## Usage

### As a Mac app

Build `vr-space.app` and copy it to `~/Applications`:

```bash
scripts/bundle-macos.sh --install
```

Then open vr-space from Finder, Spotlight or Launchpad. It appears in the Dock and Command–Tab, with standard macOS application and Window menus. Choose **Open at Login** from its menu to start it when you log in (macOS 13 or later). Without `--install` or `--run`, the app is left at `target/release/vr-space.app` as build output only.

Run as an app, vr-space gets the Screen Recording permission itself, so you allow **vr-space** in System Settings instead of your terminal. Its log goes to `~/Library/Logs/vr-space.log`.

For normal use, launch only `~/Applications/vr-space.app`, not the build output in `target/release`. Installation requires a signing certificate; it never silently falls back to ad-hoc signing. List certificates with `security find-identity -v -p codesigning`, then configure yours once:

```bash
VR_SPACE_SIGN_IDENTITY="Apple Development: Your Name (TEAMID)" scripts/bundle-macos.sh --run
```

A successful install remembers the identity in `~/Library/Application Support/vr-space/signing-identity` (outside the repository). Afterwards, build, quit the old installed app, install and launch with:

```bash
scripts/bundle-macos.sh --run
```

If signing fails, the installed app is left untouched. Keeping the same certificate, bundle ID and installed path is intended to preserve Screen Recording permission across updates. Switching from ad-hoc signing may need one new approval; macOS ultimately controls permission retention. Build-only bundles can still be ad-hoc signed, but are not the normal launch location.

Opened from Finder, the app starts the way you left it (see [Saved settings](#saved-settings)). The first time, use the menu bar icon to choose the display and go fullscreen on the glasses. To pass options to the app, use `open`:

```bash
open ~/Applications/vr-space.app --args --screen-distance 3
```

### From a terminal

Use `scripts/cargo.sh` instead of plain `cargo` for builds, tests and runs. It fetches the pinned driver and applies our reconnect patch locally under ignored `target/patched-drivers/`; no fork or upstream PR is needed. The first invocation needs Git and network access. See [patches/README.md](patches/README.md).

Run the visualizer (release build recommended for lowest latency):

```bash
scripts/cargo.sh run --release -p headset-visualizer
```

Put the glasses on, look straight ahead and click **Calibrate** in the HUD (or press `C`, or `⌃⌥C` from any app) to set the forward direction. If the glasses aren't connected yet, plug them in: the app keeps looking for them every second. If you unplug the glasses, the app waits and reconnects when they return. Recenter again after reconnecting: the new tracking session resets calibration. Press `Esc` or close the window to quit.

The HUD's `screen:` line shows whether screen capture is working. If Screen Recording permission is missing, the app asks macOS for it, shows a hint over the virtual screen, and adds a **Screen Recording settings** button. Press `P` (or click the button) to open System Settings at the right page. Allow vr-space there (or your terminal app, if you started it from a terminal), then quit and reopen it: macOS only applies the permission after a restart.

### Latency

The HUD shows where the time goes, as a mean with the 95th percentile and maximum over the last 512 samples, and the same figures go to the log every 10 seconds (with the median and sample count):

| Line | Measures |
|---|---|
| `pose->submit` | From the newest head-tracking sample to the frame drawn with it being handed to the GPU. Shown once glasses are connected |
| `capture->submit` | From when macOS says a screen image was on the Mac's display to the first frame showing it being handed to the GPU |
| `frame upload` | Time spent putting each new screen image on the virtual screen: tiny when the GPU reads it in place, more with `--copy-frames` |

These stop where the frame is handed to the GPU. The GPU's drawing time and the wait for the glasses to show the frame (up to one refresh) aren't included: wgpu 0.17 doesn't expose what would report them. To compare frame sharing with copying, run once with and once without `--copy-frames` and compare the log lines.

For an explicit residual gyro calibration, run with `--calibrate-gyro` and rest the glasses **untouched on a stable surface** after each connection: there is a 3-second countdown followed by 10 seconds of measurement (longer with `--poll-ms`). The HUD shows the countdown and result. Wait for `Gyro calibration applied` in the HUD or log, then put them on and recenter. The measured FRD offset is subtracted before sensor fusion; it is not saved and reconnecting starts a fresh measurement. Obvious motion, non-finite samples, or timestamp gaps reject calibration without applying a correction; restart or reconnect to retry. Slow constant rotation cannot be distinguished from bias, so do not calibrate while wearing or holding the glasses. This is opt-in: a stationary hardware run substantially reduced measured yaw drift, and the user reported no visible slide afterward; normal head movement was also confirmed after correcting the camera rotation order; temperature changes may require recalibration. The HUD’s Calibrate button remains orientation recentering only.

For stationary drift diagnosis, run with `RUST_LOG=warn,headset_visualizer=info,headset_visualizer::tracking::drift=debug`. Every 10 seconds of device time, the log reports calibrated gyro means, extrema and standard deviations (FRD axes, degrees/second) and the fused yaw slope. Rest the glasses on a stable surface: these diagnostics do not detect stillness or correct drift. Gyro means remain factory-calibrated readings **before** the optional residual correction; compare fused yaw slope before and after calibration, not a reduction in the logged gyro means. Duplicate samples are skipped; timestamp reversals and stalls over one second reset the measurement. Gyro axes are body-relative, so the third rate is only directly comparable to yaw slope when the glasses are level.

`--predict-ms` hides some of this delay. The app estimates how fast your head is turning and draws it where it will be, the given number of milliseconds after the frame is drawn (plus the age of the head-tracking sample). Too little and the screen still trails behind when you turn; too much and it overshoots and springs back when you stop. A good starting point is `pose->submit` plus one refresh of the glasses (about 17 ms at 60 Hz). The HUD's `predict:` line shows the amount and how fast your head is turning. To find the right amount, turn prediction on and off with `⌃⌥X` and step it with `⌃⌥]` and `⌃⌥[` while you wear the glasses; the amount you leave it at is saved. `⌃⌥Z` switches frame copying the same way. [docs/glasses-checklist.md](docs/glasses-checklist.md) walks through trying all of this on the glasses.

### Using it with the glasses

The glasses show up on your Mac as an extra display. To get a virtual monitor that stays in place while you turn your head:

1. In System Settings → Displays, set the glasses to **extend** your desktop, not mirror it.
2. Find the display IDs. With the glasses plugged in, they appear as an extra display in the list:

   ```bash
   scripts/cargo.sh run --release -p headset-visualizer -- --list-displays
   ```

3. Run the app with `--display` set to the screen you want to see and `--fullscreen` set to the glasses:

   ```bash
   scripts/cargo.sh run --release -p headset-visualizer -- --display <screen ID> --fullscreen <glasses ID>
   ```

4. Put the glasses on, look straight ahead and press `⌃⌥C` (or choose **Recenter** from the menu bar icon).

You can also start without `--fullscreen` and pick the glasses under **Fullscreen on** in the menu bar icon.

### Showing a single window

Instead of a whole display, the virtual screen can show one window, for example a browser or an editor. Choose it under **Show window** in the menu bar icon, or start with `--window`:

```bash
scripts/cargo.sh run --release -p headset-visualizer -- --window Safari --fullscreen <glasses ID>
```

`--window` takes a window ID from `--list-windows`, an app name, or part of a window title. The window is shown on its own, even when other windows cover it, it's minimised, or it's on another Space. If you resize it, the capture restarts at the new size within a couple of seconds. If you close it, the virtual screen goes back to the last display shown.

The glasses only track rotation (3DoF), not position, so leaning or moving in your seat isn't tracked. The neck model (`--neck-model`, on by default) adds the small eye movement that comes from turning your head around your neck, which makes the screen feel more solidly placed.

### Two virtual screens

Choose the first display or window with **Show display** / **Show window**, then choose another under **Controls > Second screen > Show display** or **Show window** (also in the glasses menu). The first screen sits on the left and the second on the right, each keeping its source's aspect ratio. These show existing Mac displays or windows; they do not create extra macOS desktops/displays.

Size, distance, curvature and recentering apply to the pair together. Resizing either captured window updates its screen within about two seconds. Closing the second window, or choosing **Second screen > Off**, removes it and centres the first screen again. Closing the first window keeps the existing fallback to its last display.

The second-screen choice is session-only: launch starts with one screen. Frame sharing/copying applies to both; the HUD shows each capture's status, while capture/upload latency samples combine both streams. Two simultaneous captures use more GPU and capture resources; sampled layout and latency checks passed, while continuous stutter and wearer comfort remain unverified (see [the checklist](docs/glasses-checklist.md#8-two-virtual-screens)).

### Controls

vr-space is a regular Mac app with a Dock icon and Command–Tab support. Its **Controls** menu and menu bar glasses icon share the controls: recenter, move and resize the screen, choose which display or window to show and which display to go fullscreen on (the glasses), show or hide the HUD, the latency switches (head-pose prediction and frame copying, under **Latency**), open Screen Recording settings, open at login (when run as vr-space.app), and quit. The display lists update when you plug in or remove a display, and the window list updates as windows open and close.

**Controls > Screen > Screen curvature** offers **Flat (0)**, **Gentle (0.5)** and **Full (1)**. Changes apply immediately and are saved between runs; choose whichever feels most comfortable. `--curve` overrides the saved value at startup.

**Keep horizon level** switches to yaw-only recentering. After turning it on, recenter with your head upright to set the level; later recenters only change heading. Recenter upright again after reconnecting. Switching the mode takes effect on the next recenter. The roll trim controls under **Screen** adjust the visible view; trim may need readjustment after reconnecting.

These shortcuts work from any app, so you can adjust the screen while you work:

| Shortcut | Action |
|---|---|
| `⌃⌥C` | Recenter (look straight ahead first) |
| `⌃⌥←` / `⌃⌥→` | Tilt the view left / right by 0.1° to compensate for glasses alignment; saved between runs |
| `⌃⌥↑` / `⌃⌥↓` | Move the screen further away / closer (it keeps its size, so closer looks bigger) |
| `⌃⌥=` / `⌃⌥-` | Make the screen bigger / smaller |
| `⌃⌥R` | Reset distance and size to the `--screen-distance` and `--screen-height` values (or their defaults) |
| `⌃⌥X` | Turn head-pose prediction on or off (see [Latency](#latency)) |
| `⌃⌥]` / `⌃⌥[` | Predict 5 ms further ahead / less far ahead |
| `⌃⌥Z` | Switch between the GPU reading captured frames in place and copying them (for comparison) |

If another app already uses one of these, the log says so and that shortcut is skipped.

When the vr-space window has focus, plain keys work too:

| Key | Action |
|---|---|
| `C` | Recenter |
| `←` / `→` | Tilt the view left / right by 0.1° |
| `↑` / `↓` | Further away / closer |
| `+` / `-` | Bigger / smaller |
| `R` | Reset distance and size |
| `X` | Prediction on or off |
| `]` / `[` | Predict further ahead / less far ahead |
| `Z` | Share or copy captured frames |
| `F` | Toggle fullscreen |
| `H` | Show or hide the HUD |
| `P` | Open Screen Recording settings |
| `Esc` | Quit |

The HUD shows the current distance and height.

### Saved settings

vr-space remembers what you change while it runs and starts that way next time:

- the screen distance and size (the arrow and `+`/`-` keys or shortcuts)
- screen curvature
- the display or window shown on the virtual screen
- the display it's fullscreen on (the glasses), or that it's a window
- whether the HUD is shown
- how far ahead head movement is predicted (or that prediction is off)
- Keep horizon level and the roll trim

A saved window is found again by its app and title, or any window of that app if the title changed. If the app isn't open, the virtual screen shows the saved display instead, and the window stays remembered. If the glasses aren't plugged in at startup, vr-space goes fullscreen on them as soon as they are, so you can open it at login before connecting them.

Flags you pass on the command line win over the saved settings for that run. Reset (`R` or `⌃⌥R`) goes back to the `--screen-distance` and `--screen-height` values and forgets the saved ones. The settings are stored in `~/Library/Application Support/vr-space/settings.json`; delete the file to start over, or pass `--ignore-settings` to neither load nor save it.

Options:

| Flag | Default | Meaning |
|---|---|---|
| `--vsync` | off | Enable vsync (off gives the lowest latency) |
| `--max-fps <n>` | 0 (unlimited) | Cap the frame rate when vsync is off |
| `--poll-ms <ms>` | 0 (as fast as possible) | How often to read head tracking |
| `--calibrate-gyro` | off | After a 3-second countdown, measure residual gyro bias for 10 stationary seconds after each connection; keep glasses on a stable surface until the HUD or log confirms completion. Not saved |
| `--invert-x` / `--invert-y` / `--invert-z` | off | Flip roll / pitch / yaw |
| `--no-hud` | off | Hide the text overlay |
| `--hud-size <px>` | 16 | HUD font size |
| `--display <id>` | 0 (main display) | Which display to show on the virtual screen |
| `--list-displays` | | List display IDs and resolutions, then exit |
| `--window <id\|name>` | off | Show a single window instead of a display: a window ID, an app name or part of a window title. Can't be combined with `--display` |
| `--list-windows` | | List the windows that can be shown, then exit |
| `--screen-height <units>` | 1.4 | Height of the virtual screen (one grid square = 1 unit) |
| `--screen-distance <units>` | 4 | Distance from your eyes to the virtual screen |
| `--fov <deg>` | 60 | Vertical field of view |
| `--fullscreen <id>` | off | Open fullscreen on this display (the glasses); 0 = main display |
| `--neck-model <n>` | 1 | Neck model strength (0 = off, 1 = average head, up to 3) |
| `--no-cursor` | off | Hide the mouse cursor on the virtual screen |
| `--curve <0..1>` | 0 (flat) | Curve the virtual screen around you (overrides saved curvature); 1 puts every part of it the same distance from your eyes, like a curved monitor wrapped around your head |
| `--predict-ms <ms>` | 0 (off) | Predict head movement this far ahead of when a frame is drawn, so the screen doesn't trail behind when you turn your head (up to 100; try 10 to 30). See [Latency](#latency) |
| `--copy-frames` | off | Copy each captured frame through the CPU instead of letting the GPU read it in place (slower; for comparison, or if the screen looks wrong). The HUD shows `frames=shared` or `frames=copied` |
| `--recenter-yaw-only` | off | Enable Keep horizon level, overriding the saved setting; first recenter upright to set level, and again after reconnecting |
| `--no-recenter-yaw-only` | off | Use normal recentering, overriding a saved Keep horizon level setting |
| `--ignore-settings` | off | Don't load or save [saved settings](#saved-settings) |

Check screen capture on its own:

```bash
scripts/cargo.sh run --release -p capture-check -- --seconds 5 --out capture.bgra
```

`capture.bgra` starts with a 32-byte header (magic `BGR4`, version, width, height, stride, bytes per pixel, frame count), followed by raw BGRA frames. Add `--mp4-out capture.mp4` to also encode an H.264 video with ffmpeg. Run with `--help` for all options.

## Acknowledgements

- [3rl-io/spidgets-3dof](https://github.com/3rl-io/spidgets-3dof) by Kiran Kota and the 3rl.io team, the browser-based 3DoF AR project that inspired this one. vr-space is a native rewrite of the idea and shares no code with it.
- [3rl-io/headset-utils](https://github.com/3rl-io/headset-utils), a fork of [badicsalex/ar-drivers-rs](https://github.com/badicsalex/ar-drivers-rs) by Alex Badics, Peng Cheng and the 3rl.io team (MIT). The head-tracking loop follows its `euler_60` example.
- [doom-fish/screencapturekit-rs](https://github.com/doom-fish/screencapturekit-rs) by Per Johansson and contributors (MIT OR Apache-2.0).

## License

vr-space is released under the [MIT License](LICENSE).

Third-party components keep their own licenses:

- **App and menu bar icons** are drawn in code (`apps/headset-visualizer/src/icon.rs`) and are part of vr-space's MIT-licensed source.
- **Noto Sans** (`apps/headset-visualizer/assets/NotoSans-Regular.ttf`, embedded in the app binary): SIL Open Font License 1.1, see [`OFL.txt`](apps/headset-visualizer/assets/OFL.txt).
- **Rust dependencies** are fetched by Cargo and are almost all MIT, Apache-2.0, BSD, ISC or Zlib licensed. `serialport` is MPL-2.0.
- **libusb** (LGPL-2.1) is compiled statically into the binary through `rusb`/`libusb1-sys`. This repository ships source only. If you distribute compiled binaries (including a `vr-space.app` built with `scripts/bundle-macos.sh`), you need to meet the LGPL's terms for libusb (for example, by providing its source and allowing it to be relinked).

Rokid, XREAL, Nreal and other product names are trademarks of their respective owners and are used only to describe compatibility. This project is not affiliated with or endorsed by them.
