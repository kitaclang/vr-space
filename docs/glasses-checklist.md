# Glasses checklist

Everything that still needs real AR glasses to check, in an order that fits one session. Tick what works, and note what doesn't (what you did, what you saw). Those notes are what gets fixed next.

## Before you start

- Build the release app: `scripts/cargo.sh build --release -p headset-visualizer` (or `scripts/bundle-macos.sh --install` to also check the app bundle).
- Make sure Screen Recording is allowed for the terminal (or vr-space.app).
- Run with a log you can keep: `target/release/headset-visualizer --fullscreen <id> --display <id> 2>&1 | grep --line-buffered -v "can't find" | tee glasses-session.log`. It logs at info already; `RUST_LOG=info` would also turn on wgpu's own logging and bury the useful lines. The `grep` drops the probes for other glasses models, and `--line-buffered` keeps the log appearing as it happens (`--list-displays` shows the glasses' display ID).
- Keep the HUD on (`H`). The shortcuts below work from any app with `⌃⌥` (Control-Option), or as plain keys while the vr-space window has focus.

## 1. Basics (main)

- [ ] Start vr-space **before** plugging in the glasses, then plug them in: the HUD goes from `device: not-found` to `connected` within a couple of seconds.
- [ ] `--screen-height 2` makes the screen taller; `--screen-distance 2` brings it closer; `--fov 45` zooms in (everything looks bigger). Each behaves as the README describes.
- [ ] Tilt your head to one side and recenter with `--recenter-yaw-only` (`⌃⌥C`): the horizon stays level. Without the flag, recentering with a tilted head tilts the scene.
- [ ] **Show window** in the menu bar: pick a window. Only that window shows, at its own aspect ratio.
- [ ] Resize that window: within about 2 seconds the virtual screen follows the new size.
- [ ] Close that window: the virtual screen goes back to the display shown before.
- [ ] Saved settings: with fullscreen on the glasses chosen from the menu, quit, unplug the glasses, start vr-space, then plug them in: it goes fullscreen on them once connected.

## 2. Frames shared with the GPU (zero-copy)

The HUD's `frames=` shows which path is in use; `⌃⌥Z` (or **Latency > Copy frames through the CPU**) switches between them.

- [ ] With `frames=shared`, move a window around and scroll a page on the captured screen: no tearing (a horizontal break across moving content), flicker, or old frames flashing back.
- [ ] Switch to `frames=copied` and back a few times while content moves: no glitch at the switch.
- [ ] Note `capture->submit` and `frame upload` from the HUD for each path (or the `Latency:` lines in the log, every 10 s), with similar movement on screen:

| | frames=shared | frames=copied |
|---|---|---|
| capture->submit mean / p95 (ms) | | |
| frame upload mean (ms) | | |
| Looks smoother? | | |

If shared frames tear or flicker, the renderer is letting go of frames too early (it keeps three; see `shown_frames` in `gpu.rs`).

## 3. Latency figures

- [ ] With the glasses connected, the HUD's `pose->submit` line shows numbers (it's blank without glasses). Note them:

| pose->submit mean / p95 / max (ms) | fps | Refresh rate of the glasses (Hz) |
|---|---|---|
| | | |

## 4. Head-pose prediction

Prediction is off by default. `⌃⌥X` turns it on and off (it starts at 20 ms the first time); `⌃⌥]` and `⌃⌥[` predict 5 ms further or less far ahead. The HUD's `predict:` line shows the amount and how fast your head is turning. The amount you leave it at is saved for next time.

- [ ] Look at the screen's edge and turn your head left and right at a steady speed, with prediction off: the screen trails behind and settles when you stop. Turn it on: it should trail less.
- [ ] Find the amount where turning feels stuck in place. Too little: it still trails. Too much: it overshoots and springs back when you stop.
- [ ] Hold your head still with prediction on: the screen doesn't shimmer or drift any more than with it off.
- [ ] Quick head shakes and sudden stops: no wild jumps.
- [ ] Recenter (`⌃⌥C`) while turning: the new forward is where you were looking, not ahead of it.

| Best amount (ms) | Shimmer when still? | Overshoot when stopping? | Better than off? |
|---|---|---|---|
| | | | |

If a still screen shimmers at the amount that's right for turning, the smoothing (`SMOOTHING_NS` in `prediction.rs`) needs to be longer.

## 5. Curved screen

- [ ] Start with `--curve 1` (and again with `--curve 0.5`): the screen bends around you, and text near its left and right edges is easier to read than on the flat screen at the same size.
- [ ] Turn your head along the screen: no visible seams or kinks where its 48 columns meet.
- [ ] Recentering, moving the screen closer and further, and making it bigger and smaller still work with the curve.

| Curve that feels best | Edges easier to read? | Seams visible? |
|---|---|---|
| | | |

## 6. Unplugging

The build wrapper applies the required driver patch automatically; see [patches/README.md](../patches/README.md).

- [x] Unplug the glasses mid-session: the app keeps running (confirmed by the same process and disconnect/retry logs).
- [ ] While unplugged, visually confirm the HUD shows `device: not-found`.
- [x] Plug them back in: tracking resumes without restarting the app. The old calibration is cleared; recenter with `⌃⌥C` and confirm the screen stays in place.
- [ ] Repeat with prediction enabled: no stale velocity or wild jump after reconnecting. **Failed on the second unplug cycle with prediction at 20 ms:** tracking logged the disconnect, then the main thread panicked in wgpu-core 0.17.1 (`Map callback was leaked`). Backtrace confirmed wgpu surface reconfiguration dropped pending map callbacks. A workaround now drains them before configuring, with a regression test. One post-fix unplug/replug cycle with the window moved onto the glasses display passed: the same process survived and reconnected; the user confirmed recovery after recentering. Prediction-enabled recovery still needs explicit confirmation. Prediction itself was not identified as the cause. Crash log: `/tmp/vr-space-reconnect-crash.log` (temporary local file).
- [ ] Repeat a few times, including unplugging while turning your head.

## 7. The app bundle

- [ ] Install with `scripts/bundle-macos.sh --install`, choose **Open at Login** in the menu, log out and back in: vr-space starts and appears in the menu bar.

## Afterwards

Keep `glasses-session.log` and the filled-in tables. Out of the repository is fine, as long as they're shared with whoever (or whichever agent) picks up the fixes.
