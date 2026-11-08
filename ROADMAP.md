# Roadmap

What's planned for vr-space, grouped by priority rather than dates. vr-space is an experimental hobby project, so this is a direction, not a promise. If you'd like to help with something, open an issue or a pull request.

## Now

- **Finish testing on real glasses.** Runs with XREAL Air glasses confirmed that `--fullscreen` opens on the glasses, the screen is sharp and in the right proportions, recentering with `C` works, the screen stays in place as you turn your head, and the neck model feels natural at its default strength. Still to confirm:
  - glasses plugged in after launch are picked up
  - `--screen-height`, `--screen-distance` and `--fov` behave as described
  - `--recenter-yaw-only` keeps the horizon level when you recenter with your head tilted

## Next

- **Finish the macOS app.** `scripts/bundle-macos.sh` now builds `vr-space.app`, which opens from Finder, can open at login, holds its own Screen Recording permission and logs to `~/Library/Logs/vr-space.log`. Still to do: an app icon, confirming Open at Login across a real logout and login, and a signed, notarized download (which also needs the libusb LGPL terms to be met, see the README).
- **Survive unplugged glasses.** Today the app waits for glasses that are missing at startup, but unplugging them mid-session quits the app: `ar-drivers` calls `exit(0)` on a USB read error. Fixing it needs `ar-drivers` to return errors from `Fusion::update()` instead of exiting, which means a change upstream in [headset-utils](https://github.com/3rl-io/headset-utils).
- **Capture a single window** instead of a whole display, using ScreenCaptureKit's window filters.

## Later

- **More automated tests**, for example running the renderer on a headless GPU to check what it draws.
- **Reduce latency further.** Upload frames straight from the capture's IOSurface instead of copying them through the CPU, and measure real motion-to-photon latency rather than the age of the last sample.
- **Curved or multiple virtual screens.**
- **Settings saved between runs** (screen size and distance as set with the keys, display) instead of passing flags each time.

## Known limitations

These are by design or out of scope for now:

- **3DoF only.** Rotation is tracked, so turning your head works, but leaning or moving does not.
- **macOS only**, and developed only on Apple Silicon.
- **Supported glasses** depend on [`ar-drivers`](https://github.com/3rl-io/headset-utils) and on which models work with it on macOS.
