# Plan: live command-line controls

## Goal

Control an already-running vr-space app from scripts or an AI without UI automation. The app keeps listening until it exits; each CLI invocation sends a request, prints a response, and exits. The app can be launched normally from Finder and does not depend on a terminal. This plan does **not** include a glasses simulator.

## Interface

Add a `control` subcommand to the existing app executable. Parse arguments before logging initialization; handle `control` before window creation, tracking, GPU initialization, or capture. All one-shot paths (`control`, `--list-displays`, `--list-windows`, `--write-iconset`, `--help` and `--version`) must exit without opening the GUI log file. This matters because the installed bundle writes logs to a file with `File::create`: even a discovery command with redirected stderr must not truncate the running app's log. Listings may retain their existing minimal display/window discovery setup. Preserve all existing startup flags and `--help` behavior. The installed executable is `~/Applications/vr-space.app/Contents/MacOS/vr-space`; documentation can show that path and an optional shell alias, without requiring another binary or PATH installation.

Illustrative commands; finalize names consistently in `--help`:

```sh
vr-space control status --json
vr-space control recenter
vr-space control horizon on
vr-space control screen bigger
vr-space control screen curvature 0.5
vr-space control show-window 123
vr-space control second-display 456
vr-space control second off
vr-space control fullscreen 789
vr-space control hud on
vr-space control prediction 10
vr-space control copy-frames off
vr-space control open-at-login on
vr-space control quit
```

Cover all items in the glasses menu and its submenus: recenter; horizon; size/distance steps and reset; roll trim steps; curvature; primary display/window; second display/window/off; fullscreen display/windowed; HUD; prediction toggle and steps; frame copying; open Screen Recording settings; Open at Login; and quit. Support explicit on/off setters in addition to steps where applicable, resolved on the event-loop thread against current state so repeated requests are safe. Direct numerical size, distance and roll setters are optional; the existing menu steps are the minimum. Validate numeric values and IDs; use existing ranges and semantics. Display/window listing can initially reuse `--list-displays` and `--list-windows` rather than adding another discovery API.

`status --json` reports actual current values, including primary and second source, fullscreen, screen placement and curve, HUD availability/state, horizon and roll trim, prediction, frame-copy availability/state, connection/capture status, and Open at Login availability/state. Include pending fullscreen and whether horizon changes still need recentering, rather than implying the requested state is already applied. Distinguish unavailable from off. Preserve Open at Login's native states (not registered, enabled, requires approval, not found), with unavailable separate; do not reuse the existing `Option<bool>` helper as the full status. An accepted login registration awaiting user approval must return that state and instructions for System Settings, not claim login launch is already enabled. Repeating `open-at-login on` while approval is pending must not unregister it. Preserve existing persistence: commands act like menu changes, the second source remains session-only, and `--ignore-settings` still applies. Do not edit the settings file behind the running app.

A successful response means the event loop handled the request, not merely that it was queued. Report failures and unavailable actions with a nonzero CLI exit code and actionable stderr. Recenter is queued for the next render only when connected; report that it was queued, not that calibration is already complete. Starting capture does not prove frames have arrived; report that distinction in status. Opening System Settings cannot grant Screen Recording permission. A quit response acknowledges the request before shutdown, not completion of process exit.

A timeout or disconnect after dispatch means **outcome unknown**, not that the command failed or was cancelled: the event loop may still execute it, or it may already have executed before the reply was lost. Return a distinct nonzero client exit code and actionable stderr for that case; a failure known to occur before dispatch has no effect. Do not automatically retry requests. Scripts should read status and prefer explicit setters; steps and toggles cannot safely be retried after an unknown outcome. No exactly-once delivery or retry cache is required.

## Implementation

1. **Transport:** Add a macOS-only module using `std::os::unix::net` and the existing `serde`/`serde_json` dependencies. Use one short, deterministic per-user runtime directory (0700) and socket, shared by Finder-launched servers and shell clients without depending on their working directory or matching `TMPDIR` values; validate ownership/type and fail safely on unsafe paths. Check macOS Unix-socket path length and give a clear error if the chosen path is too long. Use bounded, versioned newline-delimited JSON requests and responses, bounded read/write sizes and total request deadlines (not just a timeout restarted for each byte). Keep outstanding requests and client handling bounded; a serial one-request-per-connection listener is sufficient initially. No TCP listener or new dependency. Any process under the same user can issue commands; filesystem permissions are not protection against malicious processes running as that user.
2. **Lifetime and ownership:** The GUI process holds an exclusive per-user lock for its control-server lifetime before handling the socket path. Under that lock, remove only a verified stale socket, never an active listener or an unexpected file; bind and listen until shutdown. A second app instance must not steal or unlink the first instance's socket: keep the second app usable but log a clear warning that it has no CLI server. On normal exit, shut down the listener and remove only the socket owned by this instance; after a crash, a subsequent owner safely cleans it up. A CLI command with no server returns a clear error and does not start an app. Test races and stale cleanup, including failed or half-complete startup.
3. **Event-loop bridge:** Start the listener after the event-loop proxy exists. A background thread handles socket I/O and forwards each request and a reply channel through `EventLoopProxy` to the main thread. The event loop reads and changes `App` state, then replies; socket I/O and waiting must never block the render loop. Preserve existing keyboard, tray and global-shortcut handling when adapting `EventLoop<Command>` to carry control requests. Bound waiting and shutdown, including clients that disconnect or stop reading. Dropping a reply receiver must not panic the event loop or imply that the command was cancelled; reply sending must not block. No per-frame locks or allocations.
4. **Shared behavior:** Route mutations through the existing command execution, menu/HUD updates and `App::remember` path. Add explicit setters or translate them on the event loop without racing another command. Existing `run_command` often only logs errors (for example capture switching) or silently does nothing (HUD disabled at startup, no connected device, unavailable Open at Login or zero-copy); extend the result path so CLI replies reflect actual outcomes without changing menu behavior. Validate source IDs and fullscreen failures, preserve failed-capture fallback behavior, and never save a failed requested value. Status must read live state rather than saved settings. Extend the Open at Login helper to expose native status and registration errors rather than inferring success only from its current boolean return value.
5. **Docs:** Document the subcommand, executable path, startup requirement, all controls, JSON fields, exit behavior, persistence and security boundary in `README.md`. Update `ROADMAP.md` after implementation. Keep the simulator out of scope.

## Verification

- Unit-test parsing, validation, JSON round trips, response/exit mapping, idempotent setters and unavailable/error results. Cover existing commands and settings persistence, including failed source switches, disabled HUD/zero-copy/login, login registration errors and pending approval, and repeating an on setter while approval is pending.
- Exercise the socket in a temporary private directory: request/reply, bounded input, stalled/disconnected clients, no app, clean shutdown, crash/stale recovery, unsafe path, second instance and competing startup. Check that a rejected second instance cannot remove the first listener. Delay event-loop handling and drop replies after execution to verify unknown-outcome reporting, no automatic retry, and safe handling of a dropped reply receiver.
- Run the installed app from Finder, then use separate CLI invocations to read status and change representative controls, source and second screen; verify menu state, rendered window screenshots and saved settings. Verify control and discovery invocations (including with redirected stderr), help and version do not truncate the app log; closing the terminal does not stop the app; quit works; and restart restores only settings that are normally persistent. A screenshot is not proof of tracking quality through the glasses.
- Run `scripts/cargo.sh build --workspace`, `scripts/cargo.sh test --workspace`, and both binaries with `--help`. Verify the installed signed bundle with the project's normal `scripts/bundle-macos.sh --run` path when available. Hardware-only behavior remains unverified without glasses.
