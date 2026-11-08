//! Where the time goes between a head movement (or a screen change) and the frame that shows it.
//!
//! The render loop measures, without allocating or locking:
//! - **pose → submit**: from the newest head-tracking sample to the frame drawn with it being
//!   handed to the GPU (`queue.submit`), every rendered frame.
//! - **capture → submit** (macOS): from the moment ScreenCaptureKit says a screen image was on
//!   the Mac's display to the first frame showing it being handed to the GPU.
//! - **frame upload** (macOS): the render thread's time putting each new captured frame on the
//!   virtual screen: importing or reusing its IOSurface, or copying its pixels.
//!
//! Not measured: the GPU's own drawing time and the wait for the display to show the frame
//! (up to one refresh). wgpu 0.17 doesn't expose the Metal drawable or command buffers that
//! would report them, so the time to photons is these numbers plus that.

use std::time::Instant;

/// How many recent samples each `Samples` keeps.
pub const WINDOW: usize = 512;

/// The most recent `WINDOW` durations, in microseconds.
pub struct Samples {
    buf: [u32; WINDOW],
    len: usize,
    next: usize,
}

/// Statistics over a `Samples` window, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub count: usize,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

impl Samples {
    pub const fn new() -> Self {
        Self { buf: [0; WINDOW], len: 0, next: 0 }
    }

    /// Record one duration, replacing the oldest once the window is full.
    pub fn push_ns(&mut self, ns: u64) {
        self.buf[self.next] = (ns / 1_000).min(u32::MAX as u64) as u32;
        self.next = (self.next + 1) % WINDOW;
        self.len = (self.len + 1).min(WINDOW);
    }

    /// Mean, median, 95th percentile and maximum of the window; `None` when it's empty.
    /// Sorts a copy on the stack, so call it a few times a second, not per sample.
    pub fn summary(&self) -> Option<Summary> {
        if self.len == 0 {
            return None;
        }
        let mut sorted = [0u32; WINDOW];
        let sorted = &mut sorted[..self.len];
        sorted.copy_from_slice(&self.buf[..self.len]);
        sorted.sort_unstable();
        let ms = |us: u32| f64::from(us) / 1_000.0;
        // Nearest-rank percentile: the smallest value with at least p of the samples at or below it.
        let rank = |p: f64| ((p * self.len as f64).ceil() as usize).clamp(1, self.len) - 1;
        let sum: u64 = sorted.iter().map(|&us| u64::from(us)).sum();
        Some(Summary {
            count: self.len,
            mean_ms: sum as f64 / self.len as f64 / 1_000.0,
            p50_ms: ms(sorted[rank(0.50)]),
            p95_ms: ms(sorted[rank(0.95)]),
            max_ms: ms(sorted[self.len - 1]),
        })
    }
}

impl Default for Samples {
    fn default() -> Self {
        Self::new()
    }
}

/// `label: mean (p95 …, max …) ms`, or `label: -` with no samples yet.
pub fn describe(label: &str, summary: Option<Summary>) -> String {
    match summary {
        Some(s) => format!("{label}: {:.2} ms (p95 {:.2}, max {:.2})", s.mean_ms, s.p95_ms, s.max_ms),
        None => format!("{label}: -"),
    }
}

/// The three measurements above.
#[derive(Default)]
pub struct LatencyMeter {
    pub pose_to_submit: Samples,
    pub capture_to_submit: Samples,
    pub upload: Samples,
}

impl LatencyMeter {
    /// One line for the log, covering all three.
    pub fn log_line(&self) -> String {
        let part = |label: &str, s: &Samples| match s.summary() {
            Some(s) => format!(
                "{label} mean {:.2} p50 {:.2} p95 {:.2} max {:.2} ms (n={})",
                s.mean_ms, s.p50_ms, s.p95_ms, s.max_ms, s.count
            ),
            None => format!("{label} -"),
        };
        format!(
            "{}; {}; {}",
            part("pose->submit", &self.pose_to_submit),
            part("capture->submit", &self.capture_to_submit),
            part("frame upload", &self.upload)
        )
    }
}

/// Convert a span of mach absolute time to nanoseconds with the timebase `numer / denom`.
pub fn mach_ticks_to_ns(ticks: u64, numer: u32, denom: u32) -> u64 {
    if denom == 0 {
        return 0;
    }
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)).min(u128::from(u64::MAX)) as u64
}

/// Mach absolute time: the clock ScreenCaptureKit stamps frames with.
#[cfg(target_os = "macos")]
pub mod mach {
    use std::sync::OnceLock;

    #[repr(C)]
    #[derive(Default)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }

    pub fn now() -> u64 {
        // SAFETY: no arguments, no side effects.
        unsafe { mach_absolute_time() }
    }

    /// Nanoseconds from the mach time `then` until `now` (0 if `then` is later or unknown).
    pub fn ns_between(then: u64, now: u64) -> u64 {
        static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
        let &(numer, denom) = TIMEBASE.get_or_init(|| {
            let mut tb = Timebase::default();
            // SAFETY: writes the two fields of `tb`.
            if unsafe { mach_timebase_info(&mut tb) } == 0 { (tb.numer, tb.denom) } else { (1, 1) }
        });
        if then == 0 {
            return 0;
        }
        super::mach_ticks_to_ns(now.saturating_sub(then), numer, denom)
    }
}

/// Frames per second, updated every half second.
pub struct FpsMeter {
    pub last_instant: Instant,
    pub accum_time: f32,
    pub frames: u32,
    pub fps: f32,
}
impl FpsMeter {
    pub fn new() -> Self {
        Self { last_instant: Instant::now(), accum_time: 0.0, frames: 0, fps: 0.0 }
    }
    pub fn on_frame(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_instant).as_secs_f32();
        self.last_instant = now;
        self.accum_time += dt;
        self.frames += 1;
        if self.accum_time >= 0.5 {
            self.fps = self.frames as f32 / self.accum_time;
            self.accum_time = 0.0;
            self.frames = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_of_a_known_set() {
        let mut s = Samples::new();
        assert_eq!(s.summary(), None);
        // 1..=100 ms, out of order.
        for ms in (1..=100u64).rev() {
            s.push_ns(ms * 1_000_000);
        }
        let sum = s.summary().unwrap();
        assert_eq!(sum.count, 100);
        assert!((sum.mean_ms - 50.5).abs() < 1e-9);
        assert_eq!(sum.p50_ms, 50.0);
        assert_eq!(sum.p95_ms, 95.0);
        assert_eq!(sum.max_ms, 100.0);

        // A single sample is every statistic.
        let mut one = Samples::new();
        one.push_ns(2_500_000);
        let sum = one.summary().unwrap();
        assert_eq!((sum.p50_ms, sum.p95_ms, sum.max_ms, sum.mean_ms), (2.5, 2.5, 2.5, 2.5));
    }

    #[test]
    fn the_window_keeps_only_recent_samples() {
        let mut s = Samples::new();
        s.push_ns(1_000_000_000); // a 1 s outlier, later pushed out
        for _ in 0..WINDOW {
            s.push_ns(1_000_000);
        }
        let sum = s.summary().unwrap();
        assert_eq!(sum.count, WINDOW);
        assert_eq!(sum.max_ms, 1.0, "the oldest sample was replaced");
    }

    #[test]
    fn samples_are_microseconds_and_saturate() {
        let mut s = Samples::new();
        s.push_ns(999); // under a microsecond
        s.push_ns(u64::MAX);
        let sum = s.summary().unwrap();
        assert_eq!(sum.p50_ms, 0.0);
        assert_eq!(sum.max_ms, f64::from(u32::MAX) / 1_000.0);
    }

    #[test]
    fn mach_ticks_convert_with_the_timebase() {
        // Apple Silicon: 125/3 ns per tick, so 24 ticks are 1 µs.
        assert_eq!(mach_ticks_to_ns(24, 125, 3), 1_000);
        assert_eq!(mach_ticks_to_ns(1_000, 1, 1), 1_000, "Intel: ticks are ns");
        assert_eq!(mach_ticks_to_ns(u64::MAX, 125, 3), u64::MAX, "saturates");
        assert_eq!(mach_ticks_to_ns(5, 1, 0), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mach_time_moves_forward_in_nanoseconds() {
        let start = mach::now();
        let t = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let ns = mach::ns_between(start, mach::now());
        let real = t.elapsed().as_nanos() as u64;
        assert!(ns >= 20_000_000 && ns.abs_diff(real) < 5_000_000, "mach {ns} ns, Instant {real} ns");
        assert_eq!(mach::ns_between(0, mach::now()), 0, "unknown start");
        assert_eq!(mach::ns_between(mach::now() + 1_000_000, mach::now()), 0, "start in the future");
    }

    #[test]
    fn descriptions() {
        assert_eq!(describe("upload", None), "upload: -");
        let mut s = Samples::new();
        s.push_ns(1_500_000);
        assert_eq!(describe("upload", s.summary()), "upload: 1.50 ms (p95 1.50, max 1.50)");
        let meter = LatencyMeter::default();
        assert_eq!(meter.log_line(), "pose->submit -; capture->submit -; frame upload -");
    }
}
