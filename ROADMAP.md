# Roadmap

What's planned for vr-space, grouped by priority rather than dates. vr-space is an experimental hobby project, so this is a direction, not a promise. If you'd like to help with something, open an issue or a pull request.

## Now

- **Finish testing on real glasses.** A first run with XREAL Air glasses confirmed that screen capture starts, frames arrive at the display's resolution and the glasses connect. Still to confirm:
  - the screen shows up sharp and in the right proportions
  - the Calibrate button and the `C` key both recenter, and the view turns the right way afterwards
  - glasses plugged in after launch are picked up
  - `--display`, `--screen-height`, `--screen-distance` and `--fov` behave as described
  - `--fullscreen` opens on the glasses, and the neck model feels natural at its default strength (it's on by default and was built without glasses)

## Next

- **Survive unplugged glasses.** Today the app waits for glasses that are missing at startup, but unplugging them mid-session quits the app: `ar-drivers` calls `exit(0)` on a USB read error. Fixing it needs `ar-drivers` to return errors from `Fusion::update()` instead of exiting, which means a change upstream in [headset-utils](https://github.com/3rl-io/headset-utils).
- **Capture a single window** instead of a whole display, using ScreenCaptureKit's window filters.
- **Recenter yaw only**, as an option. Recentering currently takes your full head orientation, so calibrating while tilted also tilts the scene's horizon.
- **Show the cursor** on the virtual screen (ScreenCaptureKit can include it).

## Later

- **Automated tests** for the parts that don't need hardware: the calibration maths, the axis mapping and the capture frame handoff.
- **Reduce latency further.** Upload frames straight from the capture's IOSurface instead of copying them through the CPU, and measure real motion-to-photon latency rather than the age of the last sample.
- **Curved or multiple virtual screens.**
- **Settings saved between runs** (screen size, distance, display) instead of passing flags each time.

## Known limitations

These are by design or out of scope for now:

- **3DoF only.** Rotation is tracked, so turning your head works, but leaning or moving does not.
- **macOS only**, and developed only on Apple Silicon.
- **Supported glasses** depend on [`ar-drivers`](https://github.com/3rl-io/headset-utils) and on which models work with it on macOS.
