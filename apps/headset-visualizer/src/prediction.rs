//! Head-pose prediction: draw the head where it will be, not where it was.
//!
//! Each frame shows a head orientation sampled some milliseconds earlier, and reaches the
//! glasses some milliseconds later, so while you turn your head the virtual screen trails
//! behind and then settles. `PosePredictor` estimates how fast the head is turning from the
//! samples the render loop sees, and rotates the newest sample forward by that speed over the
//! time until the frame is shown (`--predict-ms`, plus the sample's age).
//!
//! The ar-drivers fusion only gives orientation, not the gyro's rate, so the speed comes from
//! successive orientations and their timestamps, smoothed so sensor noise doesn't make a still
//! screen shimmer. Everything is a few quaternion operations per frame: no allocation, no lock.

use glam::{Quat, Vec3};

/// Shortest gap between the samples a speed is measured over. Closer samples differ by less
/// than the sensor's noise, so they're skipped and the next one is measured over a longer gap.
const MIN_DT_NS: u64 = 2_000_000;
/// A longer gap means the tracking stalled (or just reconnected): start measuring again.
const MAX_DT_NS: u64 = 100_000_000;
/// How quickly the measured speed follows changes: the time for it to cover about two thirds
/// of a change. Shorter reacts sooner but lets more noise through.
const SMOOTHING_NS: f32 = 15_000_000.0;
/// Faster than a head can turn (about 860°/s): a glitch, such as the first sample after the
/// glasses reconnect. Start measuring again rather than predicting from it.
const MAX_SPEED_RAD_S: f32 = 15.0;
/// The furthest ahead a prediction goes, however old the sample is.
pub const MAX_HORIZON_NS: u64 = 100_000_000;

#[derive(Clone, Copy, Debug, Default)]
pub struct PosePredictor {
    /// The sample the next speed is measured from.
    last: Option<(Quat, u64)>,
    /// Smoothed rotation rate, as an axis scaled by radians per second, in the frame the
    /// orientations are given in (applied on the left).
    velocity: Vec3,
}

impl PosePredictor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the speed, for example when the glasses disconnect.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The current rotation rate in radians per second.
    pub fn speed(&self) -> f32 {
        self.velocity.length()
    }

    /// Feed the newest orientation and when it was sampled (ns). Call it every frame; a sample
    /// already seen, or one too close to the last, changes nothing.
    pub fn observe(&mut self, orientation: Quat, sampled_ns: u64) {
        let Some((last_q, last_t)) = self.last else {
            self.last = Some((orientation, sampled_ns));
            return;
        };
        if sampled_ns <= last_t {
            return; // the same sample again, or the clock went back
        }
        let dt_ns = sampled_ns - last_t;
        if dt_ns < MIN_DT_NS {
            return;
        }
        self.last = Some((orientation, sampled_ns));
        if dt_ns > MAX_DT_NS {
            self.velocity = Vec3::ZERO;
            return;
        }

        // The rotation from the last sample to this one, the short way round.
        let mut step = (orientation * last_q.inverse()).normalize();
        if step.w < 0.0 {
            step = -step;
        }
        let dt = dt_ns as f32 / 1e9;
        let measured = step.to_scaled_axis() / dt;
        if !measured.is_finite() || measured.length() > MAX_SPEED_RAD_S {
            self.velocity = Vec3::ZERO;
            return;
        }
        // Exponential smoothing that weighs each measurement by the time it covers, so the
        // result doesn't depend on how often samples arrive.
        let alpha = 1.0 - (-(dt_ns as f32) / SMOOTHING_NS).exp();
        self.velocity += (measured - self.velocity) * alpha;
    }

    /// `orientation` turned on by the current speed for `horizon_ns` (capped at
    /// `MAX_HORIZON_NS`).
    pub fn predict(&self, orientation: Quat, horizon_ns: u64) -> Quat {
        let horizon = horizon_ns.min(MAX_HORIZON_NS) as f32 / 1e9;
        (Quat::from_scaled_axis(self.velocity * horizon) * orientation).normalize()
    }
}

/// The prediction lead in use, as set by `--predict-ms`, saved settings, keys or the menu.
/// Turning it off remembers the amount, so turning it back on restores it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lead {
    /// Milliseconds ahead; 0 = off.
    pub ms: f32,
    /// What turning it on again goes back to.
    restore_ms: f32,
}

impl Lead {
    /// Turning on from off with nothing to go back to uses this.
    pub const DEFAULT_MS: f32 = 20.0;
    /// How much one "further" or "less far" step changes it.
    pub const STEP_MS: f32 = 5.0;
    pub const MAX_MS: f32 = 100.0;

    pub fn new(ms: f32) -> Self {
        let ms = ms.clamp(0.0, Self::MAX_MS);
        Self { ms, restore_ms: if ms > 0.0 { ms } else { Self::DEFAULT_MS } }
    }

    pub fn is_on(&self) -> bool {
        self.ms > 0.0
    }

    pub fn ns(&self) -> u64 {
        (f64::from(self.ms) * 1e6) as u64
    }

    pub fn set_enabled(&mut self, on: bool) {
        if on && !self.is_on() { self.ms = self.restore_ms; }
        if !on && self.is_on() { self.restore_ms = self.ms; self.ms = 0.0; }
    }

    pub fn set_ms(&mut self, ms: f32) {
        self.ms = ms.clamp(0.0, Self::MAX_MS);
        if self.ms > 0.0 { self.restore_ms = self.ms; }
    }

    pub fn toggle(&mut self) {
        if self.is_on() {
            self.restore_ms = self.ms;
            self.ms = 0.0;
        } else {
            self.ms = self.restore_ms;
        }
    }

    /// One step further ahead (turning it on if it was off).
    pub fn more(&mut self) {
        self.ms = (self.ms + Self::STEP_MS).min(Self::MAX_MS);
        self.restore_ms = self.ms;
    }

    /// One step less far ahead; reaching 0 turns it off.
    pub fn less(&mut self) {
        self.ms = (self.ms - Self::STEP_MS).max(0.0);
        if self.is_on() {
            self.restore_ms = self.ms;
        }
    }
}

/// How far ahead to predict a sample taken at `sampled_ns` for a frame drawn at `now_ns` and
/// shown `lead_ns` later: the sample's age plus the lead.
pub fn horizon_ns(sampled_ns: u64, now_ns: u64, lead_ns: u64) -> u64 {
    now_ns.saturating_sub(sampled_ns).saturating_add(lead_ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn angle_between(a: Quat, b: Quat) -> f32 {
        a.angle_between(b)
    }

    /// Turn at `rate` rad/s about `axis`, sampled every `step_ms` for `ms`.
    fn turn(p: &mut PosePredictor, axis: Vec3, rate: f32, step_ms: u64, ms: u64) -> (Quat, u64) {
        let mut last = (Quat::IDENTITY, 0);
        for t in (0..=ms).step_by(step_ms as usize) {
            let q = Quat::from_axis_angle(axis, rate * t as f32 / 1000.0);
            p.observe(q, t * MS);
            last = (q, t * MS);
        }
        last
    }

    #[test]
    fn a_steady_turn_is_continued() {
        let mut p = PosePredictor::new();
        let (q, _) = turn(&mut p, Vec3::Y, 2.0, 1, 200);
        assert!((p.speed() - 2.0).abs() < 0.02, "speed {}", p.speed());
        // 20 ms ahead at 2 rad/s is 0.04 rad further round, about the same axis.
        let ahead = p.predict(q, 20 * MS);
        let expected = Quat::from_axis_angle(Vec3::Y, 2.0 * 0.2 + 0.04);
        assert!(angle_between(ahead, expected) < 1e-3, "off by {}", angle_between(ahead, expected));
    }

    #[test]
    fn the_speed_settles_regardless_of_sample_rate() {
        for step_ms in [2, 5, 10] {
            let mut p = PosePredictor::new();
            turn(&mut p, Vec3::X, 1.0, step_ms, 300);
            assert!((p.speed() - 1.0).abs() < 0.02, "every {step_ms} ms: speed {}", p.speed());
        }
    }

    #[test]
    fn a_still_head_is_not_moved() {
        let mut p = PosePredictor::new();
        let q = Quat::from_rotation_z(0.3);
        for t in 0..100 {
            p.observe(q, t * 3 * MS);
        }
        assert_eq!(p.speed(), 0.0);
        assert!(angle_between(p.predict(q, 50 * MS), q) < 1e-6);
    }

    #[test]
    fn stopping_brings_the_prediction_back() {
        let mut p = PosePredictor::new();
        let (q, t) = turn(&mut p, Vec3::Y, 3.0, 2, 200);
        // The head stops; within a few smoothing times the predicted screen stops overshooting.
        for i in 1..=50 {
            p.observe(q, t + i * 2 * MS);
        }
        assert!(p.speed() < 0.01, "speed {}", p.speed());
    }

    #[test]
    fn repeated_and_too_close_samples_change_nothing() {
        let mut p = PosePredictor::new();
        p.observe(Quat::IDENTITY, 0);
        p.observe(Quat::from_rotation_y(0.001), MS); // under MIN_DT
        p.observe(Quat::from_rotation_y(0.5), 0); // not newer
        assert_eq!(p.speed(), 0.0);
        // Measured over the longer gap instead: 0.004 rad in 4 ms.
        p.observe(Quat::from_rotation_y(0.004), 4 * MS);
        assert!(p.speed() > 0.0);
    }

    #[test]
    fn glitches_and_stalls_reset_the_speed() {
        let mut p = PosePredictor::new();
        let (q, t) = turn(&mut p, Vec3::Y, 2.0, 2, 100);
        assert!(p.speed() > 1.0);
        // A jump of half a turn in 2 ms is not a head movement.
        p.observe(Quat::from_rotation_y(3.0) * q, t + 2 * MS);
        assert_eq!(p.speed(), 0.0, "glitch");

        let (q, t) = turn(&mut p, Vec3::Y, 2.0, 2, 100);
        p.observe(q, t + 500 * MS);
        assert_eq!(p.speed(), 0.0, "stall");

        turn(&mut p, Vec3::Y, 2.0, 2, 100);
        p.reset();
        assert_eq!(p.speed(), 0.0);
    }

    #[test]
    fn turning_through_the_back_takes_the_short_way() {
        // Samples either side of a half turn, where the quaternion's sign flips.
        let mut p = PosePredictor::new();
        let mut t = 0;
        for i in 0..100 {
            let angle = std::f32::consts::PI - 0.1 + i as f32 * 0.004;
            let q = Quat::from_rotation_y(angle);
            p.observe(if i % 2 == 0 { q } else { -q }, t);
            t += 2 * MS;
        }
        assert!((p.speed() - 2.0).abs() < 0.05, "speed {}", p.speed());
    }

    #[test]
    fn predictions_are_capped() {
        let mut p = PosePredictor::new();
        let (q, _) = turn(&mut p, Vec3::Y, 2.0, 1, 200);
        let capped = p.predict(q, 10 * MAX_HORIZON_NS);
        let at_cap = p.predict(q, MAX_HORIZON_NS);
        assert!(angle_between(capped, at_cap) < 1e-6);
    }

    #[test]
    fn the_lead_toggles_and_steps() {
        let mut lead = Lead::new(0.0);
        assert!(!lead.is_on());
        lead.toggle();
        assert_eq!(lead.ms, Lead::DEFAULT_MS, "on with nothing to restore");
        lead.more();
        assert_eq!(lead.ms, 25.0);
        lead.toggle();
        assert_eq!((lead.ms, lead.ns()), (0.0, 0));
        lead.toggle();
        assert_eq!(lead.ms, 25.0, "back to the amount it had");

        let mut lead = Lead::new(5.0);
        lead.less();
        assert!(!lead.is_on(), "stepping to 0 turns it off");
        lead.toggle();
        assert_eq!(lead.ms, 5.0, "and on again restores the last amount");
        lead.toggle();
        lead.more();
        assert_eq!(lead.ms, 5.0, "a step up from off");

        let mut lead = Lead::new(98.0);
        lead.more();
        assert_eq!(lead.ms, Lead::MAX_MS);
        assert_eq!(Lead::new(500.0).ms, Lead::MAX_MS);
        assert_eq!(Lead::new(20.0).ns(), 20_000_000);
    }

    #[test]
    fn explicit_enabled_setter_preserves_and_restores_the_lead() {
        let mut lead = Lead::new(35.0);
        lead.set_enabled(false);
        assert_eq!(lead.ms, 0.0);
        lead.set_enabled(false);
        lead.set_enabled(true);
        assert_eq!(lead.ms, 35.0);
        lead.set_ms(0.0);
        lead.set_enabled(true);
        assert_eq!(lead.ms, 35.0);
    }

    #[test]
    fn horizons_add_the_samples_age() {
        assert_eq!(horizon_ns(100 * MS, 103 * MS, 20 * MS), 23 * MS);
        assert_eq!(horizon_ns(105 * MS, 103 * MS, 20 * MS), 20 * MS, "sample newer than now");
    }
}
