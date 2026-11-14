//! Synthetic FRD head tracking for `--simulate-glasses`. Never opens or polls a headset.
//!
//! The worker thread is the only writer of simulated samples. The event loop forwards commands
//! and sends the CLI reply after the worker reports back.

use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

use crate::control::{validate_simulator_duration, validate_simulator_pose, Action};
use crate::tracking::{self, begin_connection, publish_sample, PoseAtomic};

const CADENCE: Duration = Duration::from_millis(10);
const QUEUE: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
    pub connected: bool,
    pub time_ns: u64,
    pub generation: u64,
    pub revision: u64,
    pub settled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Submission {
    pub revision: u64,
    pub settled: bool,
    pub frame: u64,
    pub time_ns: u64,
    pub generation: u64,
}

pub struct Published {
    seq: AtomicU64,
    roll: AtomicU32,
    pitch: AtomicU32,
    yaw: AtomicU32,
    connected: AtomicU32,
    time_ns: AtomicU64,
    generation: AtomicU64,
    revision: AtomicU64,
    settled: AtomicU32,
    sub_seq: AtomicU64,
    sub_revision: AtomicU64,
    sub_settled: AtomicU32,
    sub_frame: AtomicU64,
    sub_time: AtomicU64,
    sub_generation: AtomicU64,
}

impl Published {
    fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            roll: AtomicU32::new(0),
            pitch: AtomicU32::new(0),
            yaw: AtomicU32::new(0),
            connected: AtomicU32::new(0),
            time_ns: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            revision: AtomicU64::new(0),
            settled: AtomicU32::new(0),
            sub_seq: AtomicU64::new(0),
            sub_revision: AtomicU64::new(0),
            sub_settled: AtomicU32::new(0),
            sub_frame: AtomicU64::new(0),
            sub_time: AtomicU64::new(0),
            sub_generation: AtomicU64::new(0),
        }
    }

    fn write_sample(&self, s: &Sample) {
        let seq = self.seq.load(Ordering::Relaxed).wrapping_add(1);
        self.seq.store(seq, Ordering::Relaxed);
        fence(Ordering::Release);
        self.roll.store(tracking::f32_to_u32(s.roll), Ordering::Relaxed);
        self.pitch.store(tracking::f32_to_u32(s.pitch), Ordering::Relaxed);
        self.yaw.store(tracking::f32_to_u32(s.yaw), Ordering::Relaxed);
        self.connected.store(u32::from(s.connected), Ordering::Relaxed);
        self.time_ns.store(s.time_ns, Ordering::Relaxed);
        self.generation.store(s.generation, Ordering::Relaxed);
        self.revision.store(s.revision, Ordering::Relaxed);
        self.settled.store(u32::from(s.settled), Ordering::Relaxed);
        fence(Ordering::Release);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
    }

    pub fn read_sample(&self) -> Sample {
        loop {
            let a = self.seq.load(Ordering::Acquire);
            if a & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            fence(Ordering::Acquire);
            let sample = Sample {
                roll: tracking::u32_to_f32(self.roll.load(Ordering::Relaxed)),
                pitch: tracking::u32_to_f32(self.pitch.load(Ordering::Relaxed)),
                yaw: tracking::u32_to_f32(self.yaw.load(Ordering::Relaxed)),
                connected: self.connected.load(Ordering::Relaxed) != 0,
                time_ns: self.time_ns.load(Ordering::Relaxed),
                generation: self.generation.load(Ordering::Relaxed),
                revision: self.revision.load(Ordering::Relaxed),
                settled: self.settled.load(Ordering::Relaxed) != 0,
            };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == a {
                return sample;
            }
        }
    }

    /// Copies the pose sample the frame actually used. A later command counter is not an input.
    pub fn record_submission(&self, sample: &Sample, frame: u64) {
        let seq = self.sub_seq.load(Ordering::Relaxed).wrapping_add(1);
        self.sub_seq.store(seq, Ordering::Relaxed);
        fence(Ordering::Release);
        self.sub_revision.store(sample.revision, Ordering::Relaxed);
        self.sub_settled.store(u32::from(sample.settled), Ordering::Relaxed);
        self.sub_frame.store(frame, Ordering::Relaxed);
        self.sub_time.store(sample.time_ns, Ordering::Relaxed);
        self.sub_generation.store(sample.generation, Ordering::Relaxed);
        fence(Ordering::Release);
        self.sub_seq.store(seq.wrapping_add(1), Ordering::Relaxed);
    }

    fn read_submission(&self) -> Submission {
        loop {
            let a = self.sub_seq.load(Ordering::Acquire);
            if a & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            fence(Ordering::Acquire);
            let sub = Submission {
                revision: self.sub_revision.load(Ordering::Relaxed),
                settled: self.sub_settled.load(Ordering::Relaxed) != 0,
                frame: self.sub_frame.load(Ordering::Relaxed),
                time_ns: self.sub_time.load(Ordering::Relaxed),
                generation: self.sub_generation.load(Ordering::Relaxed),
            };
            fence(Ordering::Acquire);
            if self.sub_seq.load(Ordering::Relaxed) == a {
                return sub;
            }
        }
    }
}

struct Request {
    action: Action,
    reply: SyncSender<Value>,
}

pub struct Handle {
    stop: Arc<std::sync::atomic::AtomicBool>,
    tx: Option<SyncSender<Request>>,
    published: Arc<Published>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Handle {
    pub fn sample(&self) -> Sample {
        self.published.read_sample()
    }

    pub fn record_submission(&self, sample: &Sample, frame: u64) {
        self.published.record_submission(sample, frame);
    }

    pub fn forward(&self, action: Action, reply: SyncSender<Value>) -> Result<(), (SyncSender<Value>, &'static str)> {
        let Some(tx) = &self.tx else {
            return Err((reply, "simulator thread is not running"));
        };
        tx.try_send(Request { action, reply }).map_err(|err| {
            let (req, msg) = queue_error(err);
            (req.reply, msg)
        })
    }
}

fn queue_error<T>(err: TrySendError<T>) -> (T, &'static str) {
    match err {
        TrySendError::Full(value) => (value, "simulator command queue is full"),
        TrySendError::Disconnected(value) => (value, "simulator thread is not running"),
    }
}

pub fn spawn(pose: Arc<PoseAtomic>, notify: impl Fn(SyncSender<Value>, Value) + Send + 'static) -> Handle {
    let (tx, rx) = sync_channel(QUEUE);
    let published = Arc::new(Published::new());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread_published = published.clone();
    let thread = thread::Builder::new()
        .name("glasses-sim".into())
        .spawn(move || worker(pose, thread_published, rx, thread_stop, notify))
        .expect("spawn glasses simulator");
    log::info!("Glasses simulator running; not opening a headset");
    Handle { stop, tx: Some(tx), published, thread: Some(thread) }
}

fn worker(
    pose: Arc<PoseAtomic>,
    published: Arc<Published>,
    rx: Receiver<Request>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    notify: impl Fn(SyncSender<Value>, Value),
) {
    let mut state = SimState::new();
    let mut last_gen = 0;
    let now = tracking::now_ns_from_start();
    state.start(now);
    publish(&pose, &published, &mut last_gen, &state);
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let mut batch = Vec::new();
        match rx.recv_timeout(CADENCE) {
            Ok(req) => {
                batch.push(req);
                while let Ok(req) = rx.try_recv() {
                    batch.push(req);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let now = tracking::now_ns_from_start();
        let mut pending = Vec::new();
        for req in batch {
            let value = match state.apply(&req.action, now) {
                Err(error) => crate::control::failure(error),
                Ok(kind) => {
                    state.tick(now);
                    publish(&pose, &published, &mut last_gen, &state);
                    crate::control::success(state.reply(kind, published.read_submission()))
                }
            };
            pending.push((req.reply, value));
        }
        if pending.is_empty() {
            state.tick(now);
            publish(&pose, &published, &mut last_gen, &state);
        }
        for (reply, value) in pending {
            notify(reply, value);
        }
    }
}

fn publish(pose: &PoseAtomic, published: &Published, last_gen: &mut u64, state: &SimState) {
    let mut sample = state.sample();
    if sample.generation != *last_gen {
        *last_gen = begin_connection(pose);
        if *last_gen != sample.generation {
            log::warn!("simulator generation {} != connection id {}", sample.generation, *last_gen);
        }
        sample.generation = *last_gen;
    }
    published.write_sample(&sample);
    publish_sample(pose, sample.roll, sample.pitch, sample.yaw, sample.connected, sample.time_ns);
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Angles {
    roll: f32,
    pitch: f32,
    yaw: f32,
}

impl Angles {
    const ZERO: Self = Self { roll: 0.0, pitch: 0.0, yaw: 0.0 };
    fn deg(self) -> Value {
        json!({"roll": self.roll.to_degrees(), "pitch": self.pitch.to_degrees(), "yaw": self.yaw.to_degrees()})
    }
}

#[derive(Clone, Copy)]
struct Move {
    start: Angles,
    target: Angles,
    started_ns: u64,
    duration_ns: u64,
    revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Activity {
    Idle,
    Moving,
    Completed,
    Disconnected,
}

impl Activity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Moving => "moving",
            Self::Completed => "completed",
            Self::Disconnected => "disconnected",
        }
    }
}

enum Kind {
    Status,
    Pose,
    Move,
    Connection { changed: bool },
}

struct SimState {
    connected: bool,
    generation: u64,
    pose: Angles,
    target: Angles,
    motion: Option<Move>,
    revision: u64,
    settled: bool,
    completed_revision: Option<u64>,
    completed_sample_time_ns: Option<u64>,
    cancelled_revision: Option<u64>,
    time_ns: u64,
    activity: Activity,
}

impl SimState {
    fn new() -> Self {
        Self {
            connected: false,
            generation: 0,
            pose: Angles::ZERO,
            target: Angles::ZERO,
            motion: None,
            revision: 0,
            settled: false,
            completed_revision: None,
            completed_sample_time_ns: None,
            cancelled_revision: None,
            time_ns: 0,
            activity: Activity::Disconnected,
        }
    }

    fn start(&mut self, now: u64) {
        self.generation = 1;
        self.connected = true;
        self.pose = Angles::ZERO;
        self.target = Angles::ZERO;
        self.settled = true;
        self.activity = Activity::Idle;
        self.time_ns = now;
    }

    fn sample(&self) -> Sample {
        Sample {
            roll: self.pose.roll,
            pitch: self.pose.pitch,
            yaw: self.pose.yaw,
            connected: self.connected,
            time_ns: self.time_ns,
            generation: self.generation,
            revision: self.revision,
            settled: self.settled,
        }
    }

    fn cancel_motion(&mut self) {
        if let Some(motion) = self.motion.take() {
            self.cancelled_revision = Some(motion.revision);
        }
    }

    fn apply(&mut self, action: &Action, now: u64) -> Result<Kind, String> {
        match action {
            Action::SimulateStatus => Ok(Kind::Status),
            Action::SimulatePose { roll, pitch, yaw } => {
                let angles = degrees(*roll, *pitch, *yaw)?;
                if !self.connected {
                    return Err("pose requires a connected simulator; connect first".into());
                }
                self.cancel_motion();
                self.revision += 1;
                self.pose = angles;
                self.target = angles;
                self.settled = true;
                self.activity = Activity::Idle;
                self.time_ns = now;
                Ok(Kind::Pose)
            }
            Action::SimulateMove { roll, pitch, yaw, duration_ms } => {
                let angles = degrees(*roll, *pitch, *yaw)?;
                validate_simulator_duration(*duration_ms)?;
                if !self.connected {
                    return Err("move requires a connected simulator; connect first".into());
                }
                self.cancel_motion();
                self.revision += 1;
                self.motion = Some(Move {
                    start: self.pose,
                    target: angles,
                    started_ns: now,
                    duration_ns: duration_ms.saturating_mul(1_000_000),
                    revision: self.revision,
                });
                self.target = angles;
                self.settled = false;
                self.activity = Activity::Moving;
                self.time_ns = now;
                Ok(Kind::Move)
            }
            Action::SimulateDisconnect => {
                if !self.connected {
                    return Ok(Kind::Connection { changed: false });
                }
                self.cancel_motion();
                self.revision += 1;
                self.connected = false;
                self.pose = Angles::ZERO;
                self.target = Angles::ZERO;
                self.settled = true;
                self.activity = Activity::Disconnected;
                self.time_ns = now;
                Ok(Kind::Connection { changed: true })
            }
            Action::SimulateConnect => {
                if self.connected {
                    return Ok(Kind::Connection { changed: false });
                }
                self.revision += 1;
                self.generation += 1;
                self.connected = true;
                self.pose = Angles::ZERO;
                self.target = Angles::ZERO;
                self.settled = true;
                self.activity = Activity::Idle;
                self.time_ns = now;
                Ok(Kind::Connection { changed: true })
            }
            _ => Err("not a simulator command".into()),
        }
    }

    fn tick(&mut self, now: u64) {
        self.time_ns = now;
        let Some(motion) = self.motion else { return };
        let elapsed = now.saturating_sub(motion.started_ns);
        if elapsed >= motion.duration_ns {
            self.pose = motion.target;
            self.motion = None;
            self.settled = true;
            self.activity = Activity::Completed;
            self.completed_revision = Some(motion.revision);
            self.completed_sample_time_ns = Some(now);
        } else {
            let t = elapsed as f32 / motion.duration_ns as f32;
            self.pose = lerp(motion.start, motion.target, t);
            self.settled = false;
            self.activity = Activity::Moving;
        }
    }

    fn reply(&self, kind: Kind, sub: Submission) -> Value {
        match kind {
            Kind::Status => self.status(sub),
            Kind::Pose => json!({
                "simulator": true, "published": true, "revision": self.revision,
                "sample_time_ns": self.time_ns, "connection_generation": self.generation,
                "settled": self.settled, "pose_deg": self.pose.deg(),
            }),
            Kind::Move => json!({
                "simulator": true, "accepted": true, "completed": false, "revision": self.revision,
                "sample_time_ns": self.time_ns, "connection_generation": self.generation,
                "target_deg": self.target.deg(),
            }),
            Kind::Connection { changed } => json!({
                "simulator": true, "changed": changed, "connected": self.connected,
                "revision": self.revision, "connection_generation": self.generation,
                "message": if changed { "updated" } else if self.connected { "already connected" } else { "already disconnected" },
            }),
        }
    }

    fn status(&self, sub: Submission) -> Value {
        json!({
            "simulator": true,
            "connected": self.connected,
            "pose_deg": self.pose.deg(),
            "target_deg": self.target.deg(),
            "motion": self.activity.as_str(),
            "sample_time_ns": self.time_ns,
            "connection_generation": self.generation,
            "revision": self.revision,
            "settled": self.settled,
            "completed_revision": self.completed_revision,
            "completed_sample_time_ns": self.completed_sample_time_ns,
            "cancelled_revision": self.cancelled_revision,
            "submitted": {
                "revision": sub.revision,
                "settled": sub.settled,
                "frame": sub.frame,
                "sample_time_ns": sub.time_ns,
                "connection_generation": sub.generation,
            }
        })
    }
}

fn degrees(roll: f32, pitch: f32, yaw: f32) -> Result<Angles, String> {
    validate_simulator_pose(roll, pitch, yaw)?;
    Ok(Angles { roll: roll.to_radians(), pitch: pitch.to_radians(), yaw: yaw.to_radians() })
}

fn wrap_pi(x: f32) -> f32 {
    let (pi, tau) = (std::f32::consts::PI, std::f32::consts::TAU);
    let x = (x + pi) % tau;
    let x = if x < 0.0 { x + tau } else { x };
    x - pi
}

/// Shortest-path interpolation. Endpoints are exact; a 180° delta takes the negative wrap.
fn lerp(a: Angles, b: Angles, t: f32) -> Angles {
    let axis = |a: f32, b: f32| {
        if t <= 0.0 { a } else if t >= 1.0 { b } else { wrap_pi(a + wrap_pi(b - a) * t) }
    };
    Angles { roll: axis(a.roll, b.roll), pitch: axis(a.pitch, b.pitch), yaw: axis(a.yaw, b.yaw) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::head_rotation_from_pose;
    use crate::tracking::PoseCopy;
    use glam::Vec3;
    use std::sync::mpsc;

    fn pose(roll: f32, pitch: f32, yaw: f32) -> Action {
        Action::SimulatePose { roll, pitch, yaw }
    }
    fn mov(yaw: f32, duration_ms: u64) -> Action {
        Action::SimulateMove { roll: 0.0, pitch: 0.0, yaw, duration_ms }
    }
    fn near(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-4, "{a} != {b}");
    }

    #[test]
    fn degrees_map_through_the_existing_camera() {
        let mut state = SimState::new();
        state.start(0);
        state.apply(&pose(10.0, 30.0, -20.0), 1).unwrap();
        let expected = PoseCopy { roll: 10.0_f32.to_radians(), pitch: 30.0_f32.to_radians(), yaw: (-20.0_f32).to_radians() };
        let got = PoseCopy { roll: state.pose.roll, pitch: state.pose.pitch, yaw: state.pose.yaw };
        let q = head_rotation_from_pose(got, (false, false, false));
        let want = head_rotation_from_pose(expected, (false, false, false));
        assert!(q.abs_diff_eq(want, 1e-5) || q.abs_diff_eq(-want, 1e-5));
        let up = head_rotation_from_pose(PoseCopy { roll: 0.0, pitch: 30.0_f32.to_radians(), yaw: 0.0 }, (false, false, false));
        assert!((up * Vec3::NEG_Z).y > 0.0);
        let right = head_rotation_from_pose(PoseCopy { roll: 0.0, pitch: 0.0, yaw: 30.0_f32.to_radians() }, (false, false, false));
        assert!((right * Vec3::NEG_Z).x > 0.0);
    }

    #[test]
    fn interpolation_endpoints_middle_and_replacement() {
        let mut state = SimState::new();
        state.start(0);
        state.apply(&mov(90.0, 1000), 0).unwrap();
        let rev = state.revision;
        state.tick(0);
        near(state.pose.yaw.to_degrees(), 0.0);
        assert!(!state.settled);
        state.tick(500_000_000);
        near(state.pose.yaw.to_degrees(), 45.0);
        state.tick(1_000_000_000);
        near(state.pose.yaw.to_degrees(), 90.0);
        assert!(state.settled);
        assert_eq!(state.completed_revision, Some(rev));
        assert_eq!(state.activity, Activity::Completed);
        state.tick(1_100_000_000);
        near(state.pose.yaw.to_degrees(), 90.0);
        assert_eq!(state.time_ns, 1_100_000_000);

        let mut wrap = SimState::new();
        wrap.start(0);
        wrap.apply(&pose(0.0, 0.0, -170.0), 0).unwrap();
        wrap.apply(&mov(170.0, 1000), 0).unwrap();
        wrap.tick(500_000_000);
        near(wrap.pose.yaw.to_degrees(), -180.0);

        let mut replaced = SimState::new();
        replaced.start(0);
        replaced.apply(&mov(90.0, 1000), 0).unwrap();
        let old = replaced.revision;
        replaced.tick(500_000_000);
        let mid = replaced.pose;
        replaced.apply(&mov(-20.0, 1000), 500_000_000).unwrap();
        assert_eq!(replaced.cancelled_revision, Some(old));
        assert_eq!(replaced.pose, mid);
        assert_ne!(replaced.completed_revision, Some(old));
        replaced.tick(1_500_000_000);
        near(replaced.pose.yaw.to_degrees(), -20.0);
        assert_eq!(replaced.completed_revision, Some(replaced.revision));
        assert_ne!(replaced.completed_revision, Some(old));
    }

    #[test]
    fn rejects_bad_input_without_mutating_and_blocks_motion_while_disconnected() {
        let mut state = SimState::new();
        state.start(0);
        let rev = state.revision;
        for bad in [pose(f32::NAN, 0.0, 0.0), pose(0.0, 91.0, 0.0), pose(0.0, 0.0, 181.0), mov(0.0, 0), mov(0.0, 60_001)] {
            assert!(state.apply(&bad, 1).is_err());
            assert_eq!(state.revision, rev);
        }
        state.apply(&Action::SimulateDisconnect, 2).unwrap();
        assert!(state.apply(&pose(0.0, 0.0, 1.0), 3).is_err());
        assert!(state.apply(&mov(1.0, 10), 3).is_err());
        assert_eq!(state.revision, rev + 1);
    }

    #[test]
    fn startup_is_neutral_and_timestamps_advance_while_still() {
        let mut state = SimState::new();
        state.start(5);
        assert!(state.connected);
        assert_eq!(state.pose, Angles::ZERO);
        assert_eq!(state.generation, 1);
        assert_eq!(state.revision, 0);
        assert_eq!(state.activity, Activity::Idle);
        state.tick(15);
        state.tick(25);
        assert_eq!(state.pose, Angles::ZERO);
        assert_eq!(state.time_ns, 25);
        assert!(state.settled);
    }

    #[test]
    fn connect_is_idempotent_and_reconnect_cancels_motion() {
        let mut state = SimState::new();
        state.start(0);
        let rev = state.revision;
        let gen = state.generation;
        let kind = state.apply(&Action::SimulateConnect, 1).unwrap();
        assert!(matches!(kind, Kind::Connection { changed: false }));
        assert_eq!((state.revision, state.generation, state.pose), (rev, gen, Angles::ZERO));

        state.apply(&mov(40.0, 1000), 2).unwrap();
        let move_rev = state.revision;
        state.tick(100);
        state.apply(&Action::SimulateDisconnect, 100).unwrap();
        assert!(!state.connected);
        assert_eq!(state.pose, Angles::ZERO);
        assert_eq!(state.generation, gen);
        assert_eq!(state.cancelled_revision, Some(move_rev));
        assert_ne!(state.completed_revision, Some(move_rev));
        state.apply(&Action::SimulateDisconnect, 101).unwrap();
        assert_eq!(state.revision, move_rev + 1);

        state.apply(&Action::SimulateConnect, 200).unwrap();
        assert!(state.connected);
        assert_eq!(state.generation, gen + 1);
        assert_eq!(state.pose, Angles::ZERO);
        assert_ne!(state.completed_revision, Some(state.revision));
        assert_eq!(state.cancelled_revision, Some(move_rev));
        let (rev, gen) = (state.revision, state.generation);
        state.apply(&Action::SimulateConnect, 300).unwrap();
        assert_eq!((state.revision, state.generation), (rev, gen));
    }

    #[test]
    fn ack_means_published_or_accepted_not_finished() {
        let mut state = SimState::new();
        state.start(0);
        state.apply(&pose(0.0, 0.0, 15.0), 8).unwrap();
        state.tick(8);
        let pose_ack = state.reply(Kind::Pose, Submission { revision: 0, settled: false, frame: 0, time_ns: 0, generation: 0 });
        assert_eq!(pose_ack["published"], true);
        assert_eq!(pose_ack["revision"], 1);
        assert_eq!(pose_ack["sample_time_ns"], 8);
        state.apply(&mov(-30.0, 500), 9).unwrap();
        let move_ack = state.reply(Kind::Move, Submission { revision: 0, settled: false, frame: 0, time_ns: 0, generation: 0 });
        assert_eq!(move_ack["accepted"], true);
        assert_eq!(move_ack["completed"], false);
        assert_eq!(move_ack["revision"], 2);
    }

    #[test]
    fn submission_uses_the_frame_sample_not_a_later_revision() {
        let published = Published::new();
        let mut state = SimState::new();
        state.start(0);
        state.apply(&pose(0.0, 0.0, 30.0), 10).unwrap();
        let used = state.sample();
        state.apply(&pose(0.0, 0.0, -10.0), 20).unwrap();
        state.apply(&Action::SimulateDisconnect, 30).unwrap();
        published.record_submission(&used, 4);
        let sub = published.read_submission();
        assert_eq!(sub.revision, used.revision);
        assert_ne!(sub.revision, state.revision);
        assert!(sub.settled);
        assert_eq!(sub.frame, 4);
        assert_eq!(sub.time_ns, used.time_ns);
        assert_eq!(sub.generation, used.generation);
        let status = state.status(sub);
        assert_eq!(status["submitted"]["revision"], used.revision);
        assert_ne!(status["completed_revision"], status["revision"]);
    }

    #[test]
    fn identical_commands_match_and_a_full_queue_is_rejected() {
        let run = || {
            let mut state = SimState::new();
            state.start(0);
            state.apply(&pose(0.0, 0.0, 10.0), 1).unwrap();
            state.apply(&mov(20.0, 100), 2).unwrap();
            state.tick(52);
            state.sample()
        };
        assert_eq!(run(), run());
        let (tx, _rx) = mpsc::sync_channel(1);
        tx.try_send(1).unwrap();
        assert_eq!(queue_error(tx.try_send(2).unwrap_err()).1, "simulator command queue is full");
        drop(_rx);
        assert_eq!(queue_error(tx.try_send(3).unwrap_err()).1, "simulator thread is not running");
    }

    #[test]
    fn generation_sync_bumps_only_when_the_session_changes() {
        let pose = PoseAtomic::new();
        let mut last = 0;
        let mut state = SimState::new();
        state.start(0);
        publish(&pose, &Published::new(), &mut last, &state);
        assert_eq!(pose.connection_id(), 1);
        assert!(pose.read().3);
        publish(&pose, &Published::new(), &mut last, &state);
        assert_eq!(pose.connection_id(), 1);
        state.apply(&Action::SimulateDisconnect, 1).unwrap();
        publish(&pose, &Published::new(), &mut last, &state);
        assert_eq!(pose.read(), (0.0, 0.0, 0.0, false, 1));
        assert_eq!(pose.connection_id(), 1);
        state.apply(&Action::SimulateConnect, 2).unwrap();
        publish(&pose, &Published::new(), &mut last, &state);
        assert_eq!(pose.connection_id(), 2);
        assert!(pose.read().3);
    }
}
