# Local driver patch

Nothing here has been sent upstream. Builds use the pinned upstream `ar-drivers` revision plus `ar-drivers-fallible-update.patch`, applied locally by `scripts/cargo.sh`.

## Build, test and run

From the repository root:

```bash
scripts/cargo.sh build --workspace
scripts/cargo.sh test --workspace
scripts/cargo.sh test-driver
scripts/cargo.sh run --release -p headset-visualizer -- --help
scripts/bundle-macos.sh --install
```

The wrapper fetches the exact `rev` in `apps/headset-visualizer/Cargo.toml`, applies the patch with `git apply`, and caches the result under ignored `target/patched-drivers/`. The cache key includes both the revision and the patch's SHA-256, so changing either prepares a new checkout. First use needs Git and network access; later runs reuse the checkout. Deleting the cache is safe: the next run recreates it. Existing ignored upstream clones are not touched.

Use the wrapper instead of plain Cargo: the pinned upstream alone does not provide the fallible APIs. The command-line Cargo override is not a committed path dependency. `Cargo.lock` records the patched package without a Git source; that is intentional. Do not restore the old lockfile after building. The wrapper changes directory to the repository root; relative `CARGO_TARGET_DIR` values are relative to that root too.

## What the patch changes

The patch was made against upstream commit `1d14c9d529c1a3a85eb614805fca20f3b9028d0a`. It adds `Fusion::try_update()` and `try_any_fusion()`, which return errors instead of exiting the process when glasses are unplugged or missing. Existing APIs remain available. `tracking.rs` uses the fallible calls to disconnect and retry every second. After reconnecting, recenter with `C` or `⌃⌥C`: a new fusion has a new reference frame, so calibration and prediction history are reset.

The patch also exposes `Fusion::gyro_sample_frd()` (factory-calibrated FRD gyro rates and device timestamps) for opt-in drift diagnostics, and `Fusion::set_gyro_bias_frd()` to replace a residual body-frame bias before gyro integration. Bias defaults to zero and is never inferred automatically by the driver. The app’s opt-in `--calibrate-gyro` measures it during 10 seconds of explicitly stationary samples. Diagnostic rates remain before this residual subtraction. The initial gyro sample is also converted to FRD. It retains the original patch authorship and license (upstream `ar-drivers` is MIT). No fork, public repository or pull request is required.

## Verification

- Patched-driver regression (`scripts/cargo.sh test-driver`): stationary bias cancels before integration, genuine three-axis motion survives at a tilted attitude, non-finite offsets are rejected, a new fusion starts without residual bias, and reversed or stalled gyro timestamps are rejected before integration. This runs separately because the dependency is not a workspace member.
- Tracking regression test: samples are published, failed reads clear availability, and later sessions publish again with a new connection ID.
- Metal regression test: pending map callbacks are delivered before surface configuration. Without the workaround it reproduces wgpu 0.17's `Map callback was leaked` panic; with it the test passes.
- Physical XREAL Air: the same process survived unplug/replug and the user confirmed recovery after recentering, including a post-fix cycle with the window moved onto the glasses display.
- Three further guided cycles with prediction at 20 ms passed, including unplugging while turning; the disconnected HUD and stationary gyro calibration were also confirmed. See [the hardware checklist](../docs/glasses-checklist.md).

The local driver exposes six existing upstream compiler warnings; neither workspace crate adds warnings.
