//! Head tracking: a thread reads the glasses and publishes the newest orientation in atomics,
//! so the render loop never waits on a lock.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use ar_drivers::{any_fusion, any_glasses};
use log::{info, warn};

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
    pub has_device: AtomicU32,     // 0/1
    pub sample_time_ns: AtomicU64, // monotonic timestamp of last sample (Instant::now() - START), as nanoseconds
}

impl PoseAtomic {
    pub fn new() -> Self {
        Self {
            roll_bits: AtomicU32::new(f32_to_u32(0.0)),
            pitch_bits: AtomicU32::new(f32_to_u32(0.0)),
            yaw_bits: AtomicU32::new(f32_to_u32(0.0)),
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
pub fn spawn_tracking_thread(pose: Arc<PoseAtomic>, poll_ms: u64) {
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

        // Connect to fusion, retrying until glasses are plugged in. The renderer keeps
        // running meanwhile and the HUD shows "device: not-found".
        let mut warned = false;
        let mut fusion = loop {
            // Probe with any_glasses() first: ar_drivers::any_fusion() calls
            // any_glasses_or_fail(), which exits the whole process when no glasses are found.
            // any_glasses() returns an error instead. Drop the probe to release the device,
            // then let any_fusion() open it again.
            let probe = any_glasses().map(drop);
            match probe.and_then(|()| any_fusion()) {
                Ok(f) => {
                    info!("Headset connected");
                    pose.write(0.0, 0.0, 0.0, true, now_ns_from_start());
                    break f;
                }
                Err(e) => {
                    if !warned {
                        warn!("No headset device found: {e}. Retrying every second.");
                        warned = true;
                    }
                    pose.write(0.0, 0.0, 0.0, false, now_ns_from_start());
                    thread::sleep(Duration::from_secs(1));
                }
            }
        };

        let mut last = Instant::now();
        loop {
            fusion.update();
            let frd = fusion.attitude_frd_rad();

            // Publish the raw pose; calibration is applied on the render thread.
            let now_ns = now_ns_from_start();
            pose.write(frd[0], frd[1], frd[2], true, now_ns);

            if let Some(dt) = poll_interval {
                let now = Instant::now();
                if now - last < dt {
                    let sleep_for = dt - (now - last);
                    thread::sleep(sleep_for);
                }
                last = Instant::now();
            } else {
                // run as fast as possible, but yield cooperatively
                thread::yield_now();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pose_atomic_round_trips() {
        let p = PoseAtomic::new();
        assert_eq!(p.read(), (0.0, 0.0, 0.0, false, 0));
        p.write(0.1, -0.2, 3.0, true, 42);
        assert_eq!(p.read(), (0.1, -0.2, 3.0, true, 42));
    }
}
