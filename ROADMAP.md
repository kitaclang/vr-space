# Roadmap

What's planned for vr-space, grouped by priority rather than dates. vr-space is an experimental hobby project, so this is a direction, not a promise. If you'd like to help with something, open an issue or a pull request.

## Now

[docs/glasses-checklist.md](docs/glasses-checklist.md) lists everything below that needs the glasses, as one session's worth of steps.

- **Finish testing on real glasses.** Runs with XREAL Air glasses confirmed that `--fullscreen` opens on the glasses, the screen is sharp and in the right proportions, recentering with `C` works, the screen stays in place as you turn your head, and the neck model feels natural at its default strength. Still to confirm:
  - glasses plugged in after launch are picked up
  - `--screen-height`, `--screen-distance` and `--fov` behave as described
  - `--recenter-yaw-only` keeps the horizon level when you recenter with your head tilted
  - choosing a window from **Show window**, then resizing and closing it
  - with saved settings, opening vr-space before plugging in the glasses goes fullscreen on them once they're connected

## Next

- **Finish the macOS app.** `scripts/bundle-macos.sh` now builds `vr-space.app`, which opens from Finder, can open at login, holds its own Screen Recording permission and logs to `~/Library/Logs/vr-space.log`. It has its own icon. Still to do: confirming Open at Login across a real logout and login, and a signed, notarized download (which also needs the libusb LGPL terms to be met, see the README).
- **Survive unplugged glasses.** Implemented: driver errors clear tracking, release the device and retry every second; new sessions reset calibration and prediction. `scripts/cargo.sh` applies the bundled driver patch locally, with no published fork or upstream PR. Physical XREAL Air unplug/replug passed, including with the window on the glasses display after fixing a wgpu 0.17 surface-reconfiguration panic (pending HUD map callbacks are drained before configuring). Regression tests cover tracking sessions and the GPU panic. Still to do: further repeated cycles, including while turning and explicitly confirming prediction is enabled, and visually check the disconnected HUD.

## Later

- **More automated tests.** The scene and the HUD are now drawn offscreen on the GPU in `cargo test` and their pixels checked (the HUD tests found, and the HUD now fixes, a Calibrate button too small for its label at large `--hud-size`). The fullscreen logic that doesn't need a window (noticing fullscreen on the display being captured) is tested too. Still to cover: the window calls themselves (borderless, above the menu bar, on every Space), which need a real window and display.
- **Reduce latency further.** Captured frames are now read by the GPU straight from the capture's IOSurface, with no copy through the CPU (`--copy-frames` brings the copy back for comparison). The HUD and log now measure pose → submit, capture → submit and frame upload time; on a 1920×1080 Mac display, sharing cut frame upload from about 1.8 ms to 0.02 ms and mean capture → submit from about 7.9 ms to 4.2 ms. Still to do: check the numbers on the glasses, and measure the rest of motion-to-photon (GPU time and the wait for the display), which needs access to the Metal drawable that wgpu 0.17 doesn't give, or a photodiode. `--predict-ms` (off by default) now predicts head movement to hide part of the delay; still to do: tune it on the glasses (how far ahead, and how much smoothing before a still screen shimmers), then consider turning it on by default.
- **Multiple virtual screens.** A single curved screen is done (`--curve`, off by default; still to judge on the glasses). Several screens, showing several displays or windows around you, need a decision on how they're chosen and laid out.

## Known limitations

These are by design or out of scope for now:

- **3DoF only.** Rotation is tracked, so turning your head works, but leaning or moving does not.
- **macOS only**, and developed only on Apple Silicon.
- **Supported glasses** depend on [`ar-drivers`](https://github.com/3rl-io/headset-utils) and on which models work with it on macOS.
