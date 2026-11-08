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
- **Screen Recording permission** for vr-space.app, or for your terminal app if you run it with `cargo run`: System Settings → Privacy & Security → Screen Recording
- Optional, for `capture-check --mp4-out`: `ffmpeg` on your `PATH`

## Usage

### As a Mac app

Build `vr-space.app` and copy it to `~/Applications`:

```bash
scripts/bundle-macos.sh --install
```

Then open vr-space from Finder, Spotlight or Launchpad. It appears in the menu bar (there's no Dock icon). Choose **Open at Login** from its menu to start it when you log in (macOS 13 or later). Without `--install`, the app is left at `target/release/vr-space.app`.

Run as an app, vr-space gets the Screen Recording permission itself, so you allow **vr-space** in System Settings instead of your terminal. Its log goes to `~/Library/Logs/vr-space.log`.

The script signs the app with an ad-hoc signature, which changes with every build, so macOS forgets the Screen Recording permission after you rebuild. To keep it between builds, sign with a certificate from your keychain (`security find-identity -v -p codesigning` lists them):

```bash
VR_SPACE_SIGN_IDENTITY="Apple Development: Your Name (TEAMID)" scripts/bundle-macos.sh --install
```

Opened from Finder, the app starts the way you left it (see [Saved settings](#saved-settings)). The first time, use the menu bar icon to choose the display and go fullscreen on the glasses. To pass options to the app, use `open`:

```bash
open ~/Applications/vr-space.app --args --screen-distance 3
```

### From a terminal

Run the visualizer (release build recommended for lowest latency):

```bash
cargo run --release -p headset-visualizer
```

Put the glasses on, look straight ahead and click **Calibrate** in the HUD (or press `C`, or `⌃⌥C` from any app) to set the forward direction. If the glasses aren't connected yet, plug them in: the app keeps looking for them every second. Press `Esc` or close the window to quit.

The HUD's `screen:` line shows whether screen capture is working. If Screen Recording permission is missing, the app asks macOS for it, shows a hint over the virtual screen, and adds a **Screen Recording settings** button. Press `P` (or click the button) to open System Settings at the right page. Allow vr-space there (or your terminal app, if you started it from a terminal), then quit and reopen it: macOS only applies the permission after a restart.

### Using it with the glasses

The glasses show up on your Mac as an extra display. To get a virtual monitor that stays in place while you turn your head:

1. In System Settings → Displays, set the glasses to **extend** your desktop, not mirror it.
2. Find the display IDs. With the glasses plugged in, they appear as an extra display in the list:

   ```bash
   cargo run --release -p headset-visualizer -- --list-displays
   ```

3. Run the app with `--display` set to the screen you want to see and `--fullscreen` set to the glasses:

   ```bash
   cargo run --release -p headset-visualizer -- --display <screen ID> --fullscreen <glasses ID>
   ```

4. Put the glasses on, look straight ahead and press `⌃⌥C` (or choose **Recenter** from the menu bar icon).

You can also start without `--fullscreen` and pick the glasses under **Fullscreen on** in the menu bar icon.

### Showing a single window

Instead of a whole display, the virtual screen can show one window, for example a browser or an editor. Choose it under **Show window** in the menu bar icon, or start with `--window`:

```bash
cargo run --release -p headset-visualizer -- --window Safari --fullscreen <glasses ID>
```

`--window` takes a window ID from `--list-windows`, an app name, or part of a window title. The window is shown on its own, even when other windows cover it, it's minimised, or it's on another Space. If you resize it, the capture restarts at the new size within a couple of seconds. If you close it, the virtual screen goes back to the last display shown.

The glasses only track rotation (3DoF), not position, so leaning or moving in your seat isn't tracked. The neck model (`--neck-model`, on by default) adds the small eye movement that comes from turning your head around your neck, which makes the screen feel more solidly placed.

### Controls

vr-space lives in the menu bar (it has no Dock icon). While it runs, its glasses icon has the controls: recenter, move and resize the screen, choose which display or window to show and which display to go fullscreen on (the glasses), show or hide the HUD, open Screen Recording settings, open at login (when run as vr-space.app), and quit. The display lists update when you plug in or remove a display, and the window list updates as windows open and close.

These shortcuts work from any app, so you can adjust the screen while you work:

| Shortcut | Action |
|---|---|
| `⌃⌥C` | Recenter (look straight ahead first) |
| `⌃⌥↑` / `⌃⌥↓` | Move the screen further away / closer (it keeps its size, so closer looks bigger) |
| `⌃⌥=` / `⌃⌥-` | Make the screen bigger / smaller |
| `⌃⌥R` | Reset distance and size to the `--screen-distance` and `--screen-height` values (or their defaults) |

If another app already uses one of these, the log says so and that shortcut is skipped.

When the vr-space window has focus, plain keys work too:

| Key | Action |
|---|---|
| `C` | Recenter |
| `↑` / `↓` | Further away / closer |
| `+` / `-` | Bigger / smaller |
| `R` | Reset distance and size |
| `F` | Toggle fullscreen |
| `H` | Show or hide the HUD |
| `P` | Open Screen Recording settings |
| `Esc` | Quit |

The HUD shows the current distance and height.

### Saved settings

vr-space remembers what you change while it runs and starts that way next time:

- the screen distance and size (the arrow and `+`/`-` keys or shortcuts)
- the display or window shown on the virtual screen
- the display it's fullscreen on (the glasses), or that it's a window
- whether the HUD is shown

A saved window is found again by its app and title, or any window of that app if the title changed. If the app isn't open, the virtual screen shows the saved display instead, and the window stays remembered. If the glasses aren't plugged in at startup, vr-space goes fullscreen on them as soon as they are, so you can open it at login before connecting them.

Flags you pass on the command line win over the saved settings for that run. Reset (`R` or `⌃⌥R`) goes back to the `--screen-distance` and `--screen-height` values and forgets the saved ones. The settings are stored in `~/Library/Application Support/vr-space/settings.json`; delete the file to start over, or pass `--ignore-settings` to neither load nor save it.

Options:

| Flag | Default | Meaning |
|---|---|---|
| `--vsync` | off | Enable vsync (off gives the lowest latency) |
| `--max-fps <n>` | 0 (unlimited) | Cap the frame rate when vsync is off |
| `--poll-ms <ms>` | 0 (as fast as possible) | How often to read head tracking |
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
| `--recenter-yaw-only` | off | Recenter only left-right, so the horizon stays level even if your head is tilted |
| `--ignore-settings` | off | Don't load or save [saved settings](#saved-settings) |

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
- **libusb** (LGPL-2.1) is compiled statically into the binary through `rusb`/`libusb1-sys`. This repository ships source only. If you distribute compiled binaries (including a `vr-space.app` built with `scripts/bundle-macos.sh`), you need to meet the LGPL's terms for libusb (for example, by providing its source and allowing it to be relinked).

Rokid, XREAL, Nreal and other product names are trademarks of their respective owners and are used only to describe compatibility. This project is not affiliated with or endorsed by them.
