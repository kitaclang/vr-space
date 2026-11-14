//! Head tracking: a thread reads the glasses and publishes the newest orientation in atomics,
//! so the render loop never waits on a lock.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use ar_drivers::try_any_fusion;
use log::{debug, info, warn};

// ================= Low-latency Pose (Atomics) =================

#[inline]
pub fn f32_to_u32(x: f32) -> u32 { x.to_bits() }
#[inline]
pub fn u32_to_f32(x: u32) -> f32 { f32::from_bits(x) }

#[derive(Debug)]
pub struct PoseAtomic {
    pub roll_bits: AtomicU32,
    pub pitch_bits: AtomicU32,
    pub yaw_bits: AtomicU32,
    connection_id: AtomicU64,
    // 0 = off/disconnected, 1 = measuring, 2 = applied, 3 = rejected, otherwise countdown deadline.
    gyro_status: AtomicU64,
    pub has_device: AtomicU32,     // 0/1
    pub sample_time_ns: AtomicU64, // monotonic timestamp of last sample (Instant::now() - START), as nanoseconds
}

impl PoseAtomic {
    pub fn new() -> Self {
        Self {
            roll_bits: AtomicU32::new(f32_to_u32(0.0)),
            pitch_bits: AtomicU32::new(f32_to_u32(0.0)),
            yaw_bits: AtomicU32::new(f32_to_u32(0.0)),
            connection_id: AtomicU64::new(0),
            gyro_status: AtomicU64::new(0),
            has_device: AtomicU32::new(0),
            sample_time_ns: AtomicU64::new(0),
        }
    }
    pub fn write(&self, roll: f32, pitch: f32, yaw: f32, has: bool, now_ns: u64) {
        self.roll_bits.store(f32_to_u32(roll), Ordering::Relaxed);
        self.pitch_bits.store(f32_to_u32(pitch), Ordering::Relaxed);
        self.yaw_bits.store(f32_to_u32(yaw), Ordering::Relaxed);
        self.has_device.store(if has { 1 } else { 0 }, Ordering::Relaxed);
        self.sample_time_ns.store(now_ns, Ordering::Relaxed);
    }
    /// Changes for every fusion, even if the renderer misses the disconnected interval.
    pub fn connection_id(&self) -> u64 {
        self.connection_id.load(Ordering::Relaxed)
    }
    pub fn gyro_status(&self, now_ns: u64) -> Option<String> {
        match self.gyro_status.load(Ordering::Relaxed) {
            0 => None,
            1 => Some("gyro: measuring; keep still (10 s)".into()),
            2 => Some("gyro: calibration applied".into()),
            3 => Some("gyro: calibration failed; reconnect to retry".into()),
            deadline => Some(format!("gyro: starts in {} s; keep still", deadline.saturating_sub(now_ns).div_ceil(1_000_000_000))),
        }
    }
    pub fn read(&self) -> (f32, f32, f32, bool, u64) {
        let r = u32_to_f32(self.roll_bits.load(Ordering::Relaxed));
        let p = u32_to_f32(self.pitch_bits.load(Ordering::Relaxed));
        let y = u32_to_f32(self.yaw_bits.load(Ordering::Relaxed));
        let has = self.has_device.load(Ordering::Relaxed) != 0;
        let t = self.sample_time_ns.load(Ordering::Relaxed);
        (r, p, y, has, t)
    }
}

// Monotonic app start Instant to derive cheap ns timestamps. Set at the top of main();
// get_or_init makes any earlier call safe too.
pub static START_INSTANT: OnceLock<Instant> = OnceLock::new();
#[inline]
pub fn now_ns_from_start() -> u64 {
    let start = *START_INSTANT.get_or_init(Instant::now);
    (Instant::now() - start).as_nanos() as u64
}

// Head orientation sample, as published by the tracking thread.
#[derive(Debug, Clone, Copy)]
pub struct PoseCopy {
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
}

/// Start the thread that reads the glasses: it waits for them to be plugged in, then publishes
/// every orientation sample to `pose`, as fast as they come or every `poll_ms` milliseconds.
pub fn spawn_tracking_thread(pose: Arc<PoseAtomic>, poll_ms: u64, calibrate_gyro: bool) {
    let poll_interval = if poll_ms == 0 {
        None
    } else {
        Some(Duration::from_millis(poll_ms))
    };

    thread::spawn(move || {
        // macOS: bump QoS to user-interactive
        #[cfg(target_os = "macos")]
        unsafe {
            extern "C" {
                fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
            }
            // QOS_CLASS_USER_INTERACTIVE = 0x21
            let _ = pthread_set_qos_class_self_np(0x21, 0);
        }

        // Both connection and reads must return errors: the older driver APIs exit the app.
        loop {
            let mut warned = false;
            let mut fusion = loop {
                match try_any_fusion() {
                    Ok(f) => {
                        info!("Headset connected");
                        // Don't publish a connected pose until the first successful update.
                        break f;
                    }
                    Err(e) => {
                        if !warned {
                            warn!("No headset device found: {e}. Retrying every second.");
                            warned = true;
                        }
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            };

            let diagnose = log::log_enabled!(target: "headset_visualizer::tracking::drift", log::Level::Debug);
            let mut drift = DriftWindow::default();
            let mut calibration = calibrate_gyro.then(GyroCalibration::new);
            let deadline = now_ns_from_start().saturating_add(3_000_000_000);
            let mut waiting = calibrate_gyro;
            if waiting {
                pose.gyro_status.store(deadline, Ordering::Relaxed);
                info!("Gyro calibration starts in 3 seconds; keep glasses stationary, then still for 10 seconds");
            }
            let error = track_session(&pose, poll_interval, || {
                fusion.try_update()?;
                if waiting && now_ns_from_start() >= deadline {
                    waiting = false;
                    pose.gyro_status.store(1, Ordering::Relaxed);
                    info!("Gyro calibration: keep glasses stationary on a stable surface for 10 seconds");
                }
                if let Some(window) = calibration.as_mut().filter(|_| !waiting) {
                    let result = if let Some((rate, timestamp)) = fusion.gyro_sample_frd() {
                        window.observe(rate, timestamp)
                    } else {
                        Err("driver does not expose gyro samples")
                    };
                    match result {
                        Ok(Some(bias)) => {
                            match fusion.set_gyro_bias_frd(bias) {
                                Ok(()) => {
                                    pose.gyro_status.store(2, Ordering::Relaxed);
                                    info!("Gyro calibration applied: FRD bias {:?} deg/s; recenter before use", bias.map(f32::to_degrees));
                                }
                                Err(error) => {
                                    pose.gyro_status.store(3, Ordering::Relaxed);
                                    warn!("Gyro calibration not applied: {error}");
                                },
                            }
                            calibration = None;
                            drift = DriftWindow::default();
                        }
                        Err(reason) => {
                            pose.gyro_status.store(3, Ordering::Relaxed);
                            warn!("Gyro calibration rejected: {reason}; no correction applied. Restart or reconnect with glasses stationary to retry.");
                            calibration = None;
                        }
                        Ok(None) => {}
                    }
                }
                let frd = fusion.attitude_frd_rad();
                if diagnose {
                    if let Some((rate, timestamp)) = fusion.gyro_sample_frd() {
                        if let Some((mean, yaw_rate, _)) = drift.observe(rate, timestamp, frd[2]) {
                            debug!(target: "headset_visualizer::tracking::drift",
                                "Drift (10 s, keep stationary): gyro FRD mean [{:.5}, {:.5}, {:.5}] deg/s; fused yaw slope {:.5} deg/s; roll {:.3} pitch {:.3} yaw {:.3} rad",
                                mean[0].to_degrees(), mean[1].to_degrees(), mean[2].to_degrees(),
                                yaw_rate.to_degrees(), frd[0], frd[1], frd[2]);
                        }
                    }
                }
                Ok([frd[0], frd[1], frd[2]])
            });
            pose.gyro_status.store(0, Ordering::Relaxed);
            warn!("Lost the headset ({error}). Waiting for it to be plugged in again.");
            // Release USB handles before trying to open the glasses again.
            drop(fusion);
            thread::sleep(Duration::from_secs(1));
        }
    });
}

/// A new fusion or simulated reconnect. The renderer resets calibration even if it missed
/// the disconnected sample.
pub fn begin_connection(pose: &PoseAtomic) -> u64 {
    pose.connection_id.fetch_add(1, Ordering::Relaxed) + 1
}

/// The tracking thread's sample write. Disconnected samples are zeros, matching a failed read.
pub fn publish_sample(pose: &PoseAtomic, roll: f32, pitch: f32, yaw: f32, connected: bool, now_ns: u64) {
    if connected {
        pose.write(roll, pitch, yaw, true, now_ns);
    } else {
        pose.write(0.0, 0.0, 0.0, false, now_ns);
    }
}

/// Publish samples until a read fails. A fresh fusion starts a new session after reconnecting.
fn track_session(
    pose: &PoseAtomic,
    poll_interval: Option<Duration>,
    mut read: impl FnMut() -> Result<[f32; 3], ar_drivers::Error>,
) -> ar_drivers::Error {
    begin_connection(pose);
    let mut last = Instant::now();
    loop {
        match read() {
            Ok([r, p, y]) => publish_sample(pose, r, p, y, true, now_ns_from_start()),
            Err(error) => {
                publish_sample(pose, 0.0, 0.0, 0.0, false, now_ns_from_start());
                return error;
            }
        }
        if let Some(dt) = poll_interval {
            let elapsed = last.elapsed();
            if elapsed < dt {
                thread::sleep(dt - elapsed);
            }
            last = Instant::now();
        } else {
            thread::yield_now();
        }
    }
}

/// Explicit stationary measurement only, never automatic stillness detection.
struct GyroCalibration {
    window: DriftWindow,
}

impl GyroCalibration {
    fn new() -> Self {
        Self {
            window: DriftWindow::default(),
        }
    }

    fn observe(&mut self, rate: [f32; 3], timestamp: u64) -> Result<Option<[f32; 3]>, &'static str> {
        if let Some((previous, _)) = self.window.last {
            if timestamp == previous { return Ok(None); }
            if timestamp < previous || timestamp - previous > 1_000_000 {
                return Err("gyro timestamp reversed or stalled");
            }
        }
        // ponytail: these bounds reject obvious movement, not slow constant rotation;
        // the user must keep the glasses still. Add sensor-based stillness checks if needed.
        for value in rate {
            if !value.is_finite() || value.abs() > 1.0_f32.to_radians() {
                return Err("invalid gyro sample or movement above 1 degree/second");
            }
        }
        let Some((mean, _, stddev)) = self.window.observe(rate, timestamp, 0.0) else {
            return Ok(None);
        };
        // Individual-sample extrema grow with sample count and reject normal IMU noise.
        if stddev.iter().any(|v| *v > 0.2_f64.to_radians()) {
            return Err("gyro standard deviation above 0.2 degrees/second; keep glasses still");
        }
        Ok(Some(mean.map(|v| v as f32)))
    }
}

/// Device-time-weighted gyro mean and unwrapped fused-yaw slope. Diagnostic only:
/// never estimates or subtracts a bias, and never assumes the glasses are stationary.
#[derive(Default)]
struct DriftWindow {
    last: Option<(u64, f32)>,
    elapsed_us: u64,
    gyro_sum: [f64; 3],
    gyro_square_sum: [f64; 3],
    gyro_min: [f32; 3],
    gyro_max: [f32; 3],
    yaw_turn: f64,
}

impl DriftWindow {
    fn observe(&mut self, rate: [f32; 3], timestamp: u64, yaw: f32) -> Option<([f64; 3], f64, [f64; 3])> {
        let previous = self.last.replace((timestamp, yaw));
        let (last_time, last_yaw) = previous?;
        if timestamp == last_time {
            // Non-IMU events leave the driver's last gyro sample unchanged.
            self.last = previous;
            return None;
        }
        let dt = timestamp.saturating_sub(last_time);
        if dt == 0 || dt > 1_000_000 {
            *self = Self { last: self.last, ..Self::default() };
            return None;
        }
        if self.elapsed_us == 0 {
            self.gyro_min = rate;
            self.gyro_max = rate;
        }
        for (axis, value) in rate.iter().copied().enumerate() {
            self.gyro_min[axis] = self.gyro_min[axis].min(value);
            self.gyro_max[axis] = self.gyro_max[axis].max(value);
            self.gyro_square_sum[axis] += f64::from(value).powi(2) * dt as f64;
        }
        self.elapsed_us += dt;
        for (sum, rate) in self.gyro_sum.iter_mut().zip(rate) {
            *sum += f64::from(rate) * dt as f64;
        }
        let turn = f64::from(yaw) - f64::from(last_yaw);
        self.yaw_turn += turn.sin().atan2(turn.cos());
        if self.elapsed_us < 10_000_000 {
            return None;
        }
        let mean = self.gyro_sum.map(|sum| sum / self.elapsed_us as f64);
        let stddev = std::array::from_fn(|axis| {
            (self.gyro_square_sum[axis] / self.elapsed_us as f64 - mean[axis].powi(2)).max(0.0).sqrt()
        });
        let yaw_rate = self.yaw_turn * 1_000_000.0 / self.elapsed_us as f64;
        debug!(target: "headset_visualizer::tracking::drift",
            "Gyro spread (10 s): FRD min {:?}, max {:?}, stddev {:?} deg/s",
            self.gyro_min.map(f32::to_degrees), self.gyro_max.map(f32::to_degrees),
            stddev.map(f64::to_degrees));
        *self = Self { last: self.last, ..Self::default() };
        Some((mean, yaw_rate, stddev))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_diagnostics_weight_device_time_unwrap_yaw_and_skip_duplicates() {
        let mut window = DriftWindow::default();
        let mut time = 0;
        assert!(window.observe([0.0; 3], time, 3.13).is_none());
        for i in 0..100 {
            let (dt, rate) = if i % 2 == 0 { (50_000, 0.01) } else { (150_000, 0.02) };
            time += dt;
            let yaw = 3.13_f32 + 0.02 * (time as f32 / 1_000_000.0);
            let yaw = yaw.sin().atan2(yaw.cos());
            let result = window.observe([rate, 0.0, 0.02], time, yaw);
            if i < 99 {
                assert!(result.is_none());
                assert_eq!(window.gyro_min[0], 0.01);
                assert_eq!(window.gyro_max[0], if i == 0 { 0.01 } else { 0.02 });
            } else {
                let (mean, slope, stddev) = result.unwrap();
                assert!((stddev[0] - 0.004330127).abs() < 1e-6);
                assert!((mean[0] - 0.0175).abs() < 1e-6);
                assert!((mean[2] - 0.02).abs() < 1e-6);
                assert!((slope - 0.02).abs() < 1e-6);
            }
            assert!(window.observe([999.0; 3], time, yaw).is_none());
        }
        assert_eq!(window.elapsed_us, 0);
        assert_eq!(window.gyro_sum, [0.0; 3]);
    }

    #[test]
    fn drift_diagnostics_discard_stalls_and_backwards_timestamps() {
        let mut window = DriftWindow::default();
        for timestamp in [1_000_000, 1_100_000, 3_000_000, 3_100_000, 2_000_000] {
            assert!(window.observe([0.1; 3], timestamp, 0.0).is_none());
        }
        assert_eq!(window.elapsed_us, 0);
        assert_eq!(window.gyro_sum, [0.0; 3]);
        assert_eq!(window.last, Some((2_000_000, 0.0)));
    }

    #[test]
    fn gyro_calibration_measures_bias_and_rejects_motion_and_bad_timestamps() {
        let bias = [0.0007, -0.0015, -0.0008];
        let mut calibration = GyroCalibration::new();
        assert!(calibration.observe(bias, 0).unwrap().is_none());
        let mut time = 0;
        for i in 0..100 {
            time += if i % 2 == 0 { 50_000 } else { 150_000 };
            let result = calibration.observe(bias, time).unwrap();
            if i == 99 {
                for (actual, expected) in result.unwrap().into_iter().zip(bias) {
                    assert!((actual - expected).abs() < 1e-8);
                }
            } else {
                assert!(result.is_none());
            }
            assert!(calibration.observe([999.0; 3], time).unwrap().is_none());
        }
        for bad in [[f32::NAN; 3], [f32::INFINITY; 3], [0.02, 0.0, 0.0]] {
            assert!(GyroCalibration::new().observe(bad, 0).is_err());
        }
        let mut varying = GyroCalibration::new();
        varying.observe(bias, 0).unwrap();
        for i in 1..=100 {
            let rate = if i % 2 == 0 { 0.005 } else { -0.005 };
            let result = varying.observe([rate, bias[1], bias[2]], i * 100_000);
            if i == 100 { assert!(result.is_err()); } else { assert!(result.unwrap().is_none()); }
        }
        // Stationary sample noise may exceed 0.2 deg/s peak-to-peak without motion.
        let mut noisy = GyroCalibration::new();
        noisy.observe(bias, 0).unwrap();
        for i in 1..=100 {
            let jitter = (if i % 2 == 0 { 0.15_f32 } else { -0.15_f32 }).to_radians();
            let result = noisy.observe(bias.map(|v| v + jitter), i * 100_000).unwrap();
            if i == 100 {
                for (actual, expected) in result.unwrap().into_iter().zip(bias) {
                    assert!((actual - expected).abs() < 1e-8);
                }
            } else { assert!(result.is_none()); }
        }
        for bad_time in [99, 1_000_101] {
            let mut calibration = GyroCalibration::new();
            calibration.observe(bias, 100).unwrap();
            assert!(calibration.observe(bias, bad_time).is_err());
        }
        // Reconnection starts an empty measurement with no inherited bias.
        assert_eq!(GyroCalibration::new().window.elapsed_us, 0);
    }

    #[test]
    fn gyro_status_counts_down_and_reports_outcome() {
        let pose = PoseAtomic::new();
        assert!(pose.gyro_status(0).is_none());
        pose.gyro_status.store(4_000_000_000, Ordering::Relaxed);
        assert_eq!(pose.gyro_status(1_000_000_000).unwrap(), "gyro: starts in 3 s; keep still");
        assert_eq!(pose.gyro_status(3_100_000_000).unwrap(), "gyro: starts in 1 s; keep still");
        pose.gyro_status.store(1, Ordering::Relaxed);
        assert!(pose.gyro_status(0).unwrap().contains("measuring"));
        pose.gyro_status.store(2, Ordering::Relaxed);
        assert!(pose.gyro_status(0).unwrap().contains("applied"));
        pose.gyro_status.store(3, Ordering::Relaxed);
        assert!(pose.gyro_status(0).unwrap().contains("failed"));
        pose.gyro_status.store(0, Ordering::Relaxed);
        assert!(pose.gyro_status(0).is_none());
    }

    #[test]
    fn pose_atomic_round_trips() {
        let p = PoseAtomic::new();
        assert_eq!(p.read(), (0.0, 0.0, 0.0, false, 0));
        p.write(0.1, -0.2, 3.0, true, 42);
        assert_eq!(p.read(), (0.1, -0.2, 3.0, true, 42));
    }

    #[test]
    fn failed_read_disconnects_and_a_new_session_publishes_again() {
        let pose = PoseAtomic::new();
        assert_eq!(pose.connection_id(), 0);
        for (index, sample) in [[0.1, -0.2, 3.0], [-0.4, 0.5, -1.0]].into_iter().enumerate() {
            assert!(!pose.read().3);
            let mut reads = 0;
            let error = track_session(&pose, None, || {
                reads += 1;
                if reads == 1 {
                    Ok(sample)
                } else {
                    let (r, p, y, connected, _) = pose.read();
                    assert_eq!((r, p, y, connected), (sample[0], sample[1], sample[2], true));
                    Err(ar_drivers::Error::NotFound)
                }
            });
            assert!(matches!(error, ar_drivers::Error::NotFound));
            assert_eq!(reads, 2);
            assert_eq!(pose.connection_id(), index as u64 + 1);
            let (r, p, y, connected, _) = pose.read();
            assert_eq!((r, p, y, connected), (0.0, 0.0, 0.0, false));
        }
    }
}
