# Glasses checklist

Everything that still needs real AR glasses to check, in an order that fits one session. Tick what works, and note what doesn't (what you did, what you saw). Those notes are what gets fixed next.

## Before you start

- Build the release app: `scripts/cargo.sh build --release -p headset-visualizer` (or `scripts/bundle-macos.sh --install` to also check the app bundle).
- Make sure Screen Recording is allowed for the terminal (or vr-space.app).
- Run with a log you can keep: `target/release/headset-visualizer --fullscreen <id> --display <id> 2>&1 | grep --line-buffered -v "can't find" | tee glasses-session.log`. It logs at info already; `RUST_LOG=info` would also turn on wgpu's own logging and bury the useful lines. The `grep` drops the probes for other glasses models, and `--line-buffered` keeps the log appearing as it happens (`--list-displays` shows the glasses' display ID).
- Keep the HUD on (`H`). The shortcuts below work from any app with `⌃⌥` (Control-Option), or as plain keys while the vr-space window has focus.

## 1. Basics (main)

- [x] Check **Keep horizon level** in Controls and the glasses menu, recenter upright, adjust roll trim, and restart: the toggle and trim are restored. On 2026-11-13 the user confirmed the checked toggle and −1.0° trim survived restart; gyro calibration succeeded. The user also reported checking restoration after unplugging and confirmed normal tilted recentering after turning the toggle off. The mode changes on recenter; reconnect resets the upright reference.

- [x] Start vr-space **before** plugging in the glasses, then plug them in: the HUD goes from `device: not-found` to `connected`. User confirmed on 2026-11-13; the same process logged the connection. Two gyro calibration attempts were rejected for an invalid sample or excessive movement; a stationary reconnect then succeeded at 10:10:27 UTC (`/tmp/vr-space-plug-after-launch.log`).
- [x] `--screen-height 2` makes the screen taller; `--screen-distance 2` brings it closer; `--fov 45` zooms in (everything looks bigger). On 2026-11-13 the user confirmed height 2 looks taller than height 1, distance 2 looks closer and larger than distance 4, and FOV 45 looks bigger than FOV 60.
- [x] Position-dependent orientation investigation closed by user; XREAL Air provides 3DoF orientation, not reliable positional tracking. This does not establish the cause of the observed orientation changes. Evidence: on 2026-11-13 the user slid the glasses about 123 mm along a rail on a flat surface, leaving them stationary for 15 seconds at each end without recentering. HUD FRD roll/pitch/yaw were 0.000/−0.196/0.050 rad at left, −0.011/−0.207/0.050 at right, and 0.000/−0.196/0.050 on return left (rounded to 0.001 rad). Both axes changed about −0.63° and returned. The rail may still twist or slope, and sensor/environmental effects remain possible; translation is not measured by this 3DoF tracker.
- [x] After connecting with `--calibrate-gyro`, confirm the HUD shows a 3-second countdown, then measurement and its outcome; check whether reconnecting still changes the visible tilt. On 2026-11-13 the user confirmed seeing the countdown; the log recorded a 3-second delay followed by successful calibration on two connections (`/tmp/vr-space-countdown-test.log`). The user also confirmed the HUD showed measuring followed by calibration applied. The user then reported that several reconnects with successful calibration still required different roll trim values each time; the settling delay did not resolve repeatability.
- [x] Yaw-only level investigation closed by user, with the 3DoF limitation noted; the cause of the observed offset remains unconfirmed. With `--recenter-yaw-only`, first recenter upright to establish level, then tilt your head and recenter again (`⌃⌥C`): the horizon stays level. Repeat after reconnecting, which resets the upright reference. On 2026-11-13, after the new upright-reference change, the user confirmed the red line was level after the first upright recenter, remained level on returning upright after a tilted recenter, and stayed level while turning left and right. After reconnect and successful gyro calibration, the user reported the red and green lines tilted anticlockwise from the first upright recenter; reconnect verification failed. A fresh diagnostic run then showed approximately 5° clockwise tilt across both axes and the virtual screen after upright recenter; the log reported mapped yaw/pitch/roll −3.20/−7.77/−0.29° and a +0.29° roll correction (`/tmp/vr-space-roll-diagnostic.log`). Earlier on 2026-11-13 the user initially reported it level, then noticed the red line tilted further clockwise after pressing recenter. A second recenter did not move it; it remained tilted after returning upright and after recentering upright. In another yaw-only run, the user reported both red and green lines tilting clockwise together, with the tilt remaining about the same while turning left and right. The cause remains unconfirmed. In a fresh run without the flag, the user reported the red line level after recentering upright and rotating to their tilted head position when recentering while tilted. Installed-bundle test: user confirmed a slight clockwise tilt persists after recentering and returns after tilting left/right and returning upright; it is stable, not drift. The cause of that earlier offset remains unconfirmed. Restarting without `--recenter-yaw-only` and recentering upright made the scene look much better to the user; the follow-up screenshot shows nearly horizontal screen and grid edges. User also confirmed gentle left/right turns remain comfortable and returning to centre restores the level view.
- [x] **Show window** in the menu bar: pick a window. Only that window shows, at its own aspect ratio.
- [x] Resize that window: within about 2 seconds the virtual screen follows the new size.
- [x] Close that window: the virtual screen goes back to the display shown before.
- [x] Saved settings: with fullscreen on the glasses chosen from the menu, quit, unplug the glasses, start vr-space, then plug them in: it goes fullscreen on them once connected. User confirmed 2026-11-13; the log recorded the saved display missing at startup and fullscreen applied after it connected (`/tmp/vr-space-saved-fullscreen-retry.log`).

## 2. Frames shared with the GPU (zero-copy)

The HUD's `frames=` shows which path is in use; `⌃⌥Z` (or **Latency > Copy frames through the CPU**) switches between them.

- [x] With `frames=shared`, move a window around and scroll a page on the captured screen: no tearing (a horizontal break across moving content), flicker, or old frames flashing back.
- [x] Switch to `frames=copied` and back a few times while content moves: no glitch at the switch.
- [x] Note `capture->submit` and `frame upload` from the HUD for each path (or the `Latency:` lines in the log, every 10 s), with similar movement on screen:

| | frames=shared | frames=copied |
|---|---|---|
| capture->submit mean / p95 (ms) | 4.34 / 13.20 | 5.31 / 14.56 |
| frame upload mean (ms) | 0.01 | 1.37 |
| Looks smoother? | No noticeable difference | No noticeable difference |

If shared frames tear or flicker, the renderer is letting go of frames too early (it keeps three; see `shown_frames` in `gpu.rs`).

## 3. Latency figures

- [ ] With the glasses connected, the HUD's `pose->submit` line shows numbers (it's blank without glasses). Note them:

| pose->submit mean / p95 / max (ms) | fps | Refresh rate of the glasses (Hz) |
|---|---|---|
| | | |

## 4. Head-pose prediction

Observed during testing: with the head held still, the screen slowly slides to the right in both shared and copied frame modes. It also drifts with prediction off, and the rightward slide returns after recentering. With the glasses resting untouched on a stable surface, two HUD observations showed raw fusion yaw changing from −0.731 to −0.741 rad (~0.57°), while pitch stayed at 0.253 rad and roll changed from −0.021 to −0.022 rad; prediction was off. This confirms drift in the reported tracking orientation, before rendering/prediction; the underlying cause still needs investigation.

- [x] Stationary diagnostic measurement: user confirmed the glasses rested on a stable surface for at least 30 seconds. Final three complete windows (2026-11-11 09:50:26–09:50:56 UTC, `/tmp/vr-space-drift.log`) showed fused yaw slopes −0.03073, −0.03124 and −0.03109 deg/s (~1.86°/minute). Calibrated FRD gyro means stayed around [0.042, −0.089, −0.045] deg/s; reported roll/pitch were −0.022/−0.252 rad, so gyro Z alone is not a level yaw-rate comparison. These stationary nonzero rates are consistent with residual gyro bias; its cause and a correction remain unverified.

- [x] Stationary residual gyro calibration completed with `--calibrate-gyro`. The initial 0.2°/s peak-to-peak rejection falsely rejected stationary readings: three full windows had spreads of about 0.47–0.53°/s. Calibration now checks device-time-weighted standard deviation over the full window (limit 0.2°/s), retaining the immediate 1°/s sample bound. On 2026-11-11 at 10:14:01 UTC, calibration applied FRD bias [0.04978, −0.08677, −0.03928] deg/s; sample standard deviations were about 0.054/0.064/0.065°/s. Four subsequent stationary yaw slopes were +0.00028, +0.00033, −0.00063 and −0.00070°/s, versus the earlier ~−0.031°/s baseline (`/tmp/vr-space-gyro-calibrated.log`). Logged gyro means intentionally remain before residual correction.
- [x] After calibration, user confirmed the visible rightward slide is no longer apparent in the app window.
- [x] While wearing the glasses, confirm normal head movement still works after calibration. Turning and nodding were confirmed, but shoulder tilt moved the view mostly up/down with a little roll, even after recentering. The camera rotation-order fix is built and regression-tested. After restarting, stationary gyro calibration completed at 2026-11-11 13:27:47 UTC (`/tmp/vr-space-rotation-fixed.log`); after recentering, the user confirmed shoulder tilt now rolls sideways instead of moving up/down. The user also confirmed nodding up/down and turning left/right still feel correct with this fix.
- [x] Put the glasses on, recenter, then turn and stop: real tracking still works without the previous rightward slide or new jumps. With prediction off, the user reported normal, stable tracking after stationary calibration on 2026-11-13. Prediction at 20 ms felt shaky while turning; 10 ms felt better, but the user could not tell a difference from prediction off and reported no drift or sudden jumps.
- [x] Restart with calibration enabled and deliberately move the glasses during measurement: left/right rotation rejected at 2026-11-11 13:34:59 UTC with `movement above 1 degree/second; no correction applied` (`/tmp/vr-space-gyro-motion-test.log`). Restart stationary to retry; slow constant movement is not reliably detectable.
- [x] Reconnect with calibration enabled: after the motion-rejected startup, the same process (PID 53240) reconnected at 2026-11-11 13:40:14 UTC and completed a fresh stationary calibration at 13:40:24 UTC: FRD bias [0.02833, −0.10932, −0.08722] deg/s (`/tmp/vr-space-gyro-motion-test.log`). First subsequent 10-second yaw slope: −0.00230°/s. Fresh-session zero bias is also covered by the driver test.

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

- [ ] Start with `--curve 1` (and again with `--curve 0.5`): the screen bends around you, and text near its left and right edges is easier to read than on the flat screen at the same size. Installed app at `--curve 1`: after being asked to inspect edge readability and seams, the user reported “everything looks great.” `0.5` comparison and explicit readability preference remain to check.
- [ ] Turn your head along the screen: no visible seams or kinks where its 48 columns meet.
- [ ] Recentering, moving the screen closer and further, and making it bigger and smaller still work with the curve. At `--curve 1`, user confirmed making the screen bigger/smaller and moving it closer/farther through Controls keeps it looking correct; explicit recenter verification remains to check.

| Curve that feels best | Edges easier to read? | Seams visible? |
|---|---|---|
| | | |

## 6. Unplugging

The build wrapper applies the required driver patch automatically; see [patches/README.md](../patches/README.md).

- [x] Unplug the glasses mid-session: the app keeps running (confirmed by the same process and disconnect/retry logs).
- [x] While unplugged, visually confirm the HUD shows `device: not-found` (user confirmed 2026-11-13).
- [x] Plug them back in: tracking resumes without restarting the app. The old calibration is cleared; recenter with `⌃⌥C` and confirm the screen stays in place.
- [x] Repeat with prediction enabled: no stale velocity or wild jump after reconnecting. **Failed on the second unplug cycle with prediction at 20 ms:** tracking logged the disconnect, then the main thread panicked in wgpu-core 0.17.1 (`Map callback was leaked`). Backtrace confirmed wgpu surface reconfiguration dropped pending map callbacks. A workaround now drains them before configuring, with a regression test. One post-fix unplug/replug cycle with the window moved onto the glasses display passed: the same process survived and reconnected; the user confirmed recovery after recentering. A subsequent guided test explicitly confirmed prediction at 20 ms before unplugging and after reconnecting: tracking resumed after recentering with no wild jumps, and the same process (PID 90222) survived; the log recorded `Headset connected` at 08:36:43Z. A second log-verified cycle disconnected at 08:41:15Z and reconnected at 08:43:07Z in the same process; the user again confirmed normal recovery at 20 ms. Prediction itself was not identified as the cause. Crash log: `/tmp/vr-space-reconnect-crash.log` (temporary local file).
- [x] Repeat a few times, including unplugging while turning your head. Three guided cycles are log-verified with prediction at 20 ms in the same process (PID 90222). The final cycle unplugged while gently turning: disconnect at 08:46:10Z, reconnect at 08:46:59Z; the user confirmed normal tracking after recentering, no wild jumps, and prediction still at 20 ms.

## 7. The app bundle

- [x] Application menus and Controls appear; Command–Tab switches to the app; **Window > Minimize**, then clicking its Dock icon restores the window (release binary). Command–Tab switching, **Window > Minimize** followed by Dock restoration, and visibility of screen and latency controls in **Controls** also passed in the installed bundle.
- [x] Installed `~/Applications/vr-space.app` launches through Launch Services, captures frames after granting its own Screen Recording permission, and completes stationary gyro calibration. User confirmed the virtual screen looks normal after wearing and recentering.

- [ ] Install with `scripts/bundle-macos.sh --install`, choose **Open at Login** in the menu, log out and back in: vr-space starts and appears in the menu bar.

## Afterwards

Keep `glasses-session.log` and the filled-in tables. Out of the repository is fine, as long as they're shared with whoever (or whichever agent) picks up the fixes.
