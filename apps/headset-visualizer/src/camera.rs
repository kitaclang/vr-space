//! From the glasses' orientation to the view: axis mapping, recentering, the neck model and
//! the projection.

use glam::{Mat4, Quat, Vec3};

use crate::tracking::PoseCopy;

// Camera (eye) position in the scene when looking straight ahead. Scene units are roughly
// metres: the grid floor is at y = 0 and one grid square is 1 unit.
pub const CAM_POS: Vec3 = Vec3::new(0.0, 1.5, 4.0);

// Neck model: the eyes sit about 7.5 cm above and 8 cm in front of the point the head turns
// around, so turning or tilting the head also moves the eyes a little. Recreating that adds
// natural parallax, even though 3DoF glasses can't track position.
pub const NECK_TO_EYES: Vec3 = Vec3::new(0.0, 0.075, -0.08);

// Axis remapping: headset pose -> camera orientation in the scene (before calibration).
pub fn head_rotation_from_pose(pose: PoseCopy, invert: (bool, bool, bool)) -> Quat {
    // User mapping: y -> x, z -> y, x -> z
    let mapped_roll = pose.pitch; // new X = old Y
    let mapped_pitch = pose.yaw;  // new Y = old Z
    let mapped_yaw = pose.roll;   // new Z = old X

    // Flip X (roll) by default based on user's feedback (x was inverted)
    let mut rx = -mapped_roll;
    let mut ry = mapped_pitch;
    let mut rz = mapped_yaw;

    if invert.0 { rx = -rx; }
    if invert.1 { ry = -ry; }
    if invert.2 { rz = -rz; }

    // Fusion's FRD Euler convention is Rz(yaw) * Ry(pitch) * Rx(roll).
    // Convert axes as a rotation, preserving that order: F -> -Z, R -> X, D -> -Y.
    // Conjugating remapped XYZ angles reverses the order and mixes roll into pitch
    // after recentering at a nonzero heading.
    Quat::from_rotation_y(-ry) * Quat::from_rotation_x(-rx) * Quat::from_rotation_z(-rz)
}

// When upright, retain the glasses' measured roll as a sensor-to-head mounting correction.
pub fn upright_roll_correction(head: Quat) -> Quat {
    let (_, _, roll) = head.to_euler(glam::EulerRot::YXZ);
    Quat::from_rotation_z(-roll)
}

// The calibration for a recenter: the inverse of the head orientation, or with `yaw_only` the
// inverse of just its turn around the vertical axis (scene Y up, -Z forward), so pitch and
// roll stay measured against the real horizon.
pub fn recenter_calibration(head: Quat, yaw_only: bool) -> Quat {
    if !yaw_only {
        return head.inverse();
    }
    let forward = head * Vec3::NEG_Z;
    // Looking almost straight up or down, the forward direction has no usable heading; the
    // top of the head points where the face would (looking down) or away from it (looking up).
    let dir = if forward.x.hypot(forward.z) > 1e-3 {
        forward
    } else {
        let up = head * Vec3::Y;
        if forward.y < 0.0 { up } else { -up }
    };
    let yaw = (-dir.x).atan2(-dir.z);
    Quat::from_rotation_y(yaw).inverse()
}

// Camera view matrix for a camera orientation (already calibrated). With the neck model on, the
// eye swings around a pivot behind and below it; looking straight ahead it is exactly at CAM_POS.
pub fn camera_view(rotation: Quat, neck_model: f32) -> Mat4 {
    let eye_offset = NECK_TO_EYES * neck_model;
    let pivot = CAM_POS - eye_offset;
    let eye = pivot + rotation * eye_offset;
    Mat4::from_rotation_translation(rotation, eye).inverse()
}

pub fn make_proj(width: u32, height: u32, fov_deg: f32) -> Mat4 {
    let aspect = (width.max(1) as f32) / (height.max(1) as f32);
    let fovy = fov_deg.to_radians();
    let near = 0.01;
    let far = 1000.0;
    Mat4::perspective_rh(fovy, aspect, near, far)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::ScreenPlacement;

    const EPS: f32 = 1e-4;

    fn assert_vec_eq(a: Vec3, b: Vec3) {
        assert!(a.abs_diff_eq(b, EPS), "{a:?} != {b:?}");
    }

    fn assert_rot_eq(a: Quat, b: Quat) {
        // q and -q are the same rotation.
        assert!(a.abs_diff_eq(b, EPS) || a.abs_diff_eq(-b, EPS), "{a:?} != {b:?}");
    }

    fn pose(roll: f32, pitch: f32, yaw: f32) -> Quat {
        head_rotation_from_pose(PoseCopy { roll, pitch, yaw }, (false, false, false))
    }

    /// A head turned by `yaw`, then tilted up by `pitch`, then rolled by `roll` (radians).
    fn head(yaw: f32, pitch: f32, roll: f32) -> Quat {
        Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch) * Quat::from_rotation_z(roll)
    }

    // --- Axis mapping. The signs pin the behaviour confirmed on XREAL Air glasses. ---

    #[test]
    fn zero_pose_looks_straight_ahead() {
        assert_rot_eq(pose(0.0, 0.0, 0.0), Quat::IDENTITY);
    }

    #[test]
    fn pose_yaw_turns_around_the_vertical_axis() {
        let q = pose(0.0, 0.0, 0.5);
        assert_vec_eq(q * Vec3::Y, Vec3::Y);
        assert_vec_eq(q * Vec3::NEG_Z, Vec3::new(0.5f32.sin(), 0.0, -0.5f32.cos()));
    }

    #[test]
    fn pose_pitch_looks_up_and_down() {
        let q = pose(0.0, 0.5, 0.0);
        assert_vec_eq(q * Vec3::X, Vec3::X);
        assert_vec_eq(q * Vec3::NEG_Z, Vec3::new(0.0, 0.5f32.sin(), -0.5f32.cos()));
    }

    #[test]
    fn pose_roll_tilts_around_the_view_direction() {
        let q = pose(0.5, 0.0, 0.0);
        assert_vec_eq(q * Vec3::NEG_Z, Vec3::NEG_Z);
        assert_vec_eq(q * Vec3::Y, Vec3::new(0.5f32.sin(), 0.5f32.cos(), 0.0));
    }

    #[test]
    fn recentered_roll_stays_roll_at_any_heading_and_pitch() {
        for yaw in [-2.5, -0.8, 0.0, 0.8, 2.5] {
            for pitch in [-0.6, 0.0, 0.6] {
                let centred = pose(0.0, pitch, yaw);
                let tilted = pose(0.3, pitch, yaw);
                assert_rot_eq(
                    recenter_calibration(centred, false) * tilted,
                    Quat::from_rotation_z(-0.3),
                );
            }
        }
    }

    #[test]
    fn invert_flags_reverse_each_axis() {
        let p = PoseCopy { roll: 0.3, pitch: 0.2, yaw: 0.1 };
        let flipped = PoseCopy { roll: -0.3, pitch: -0.2, yaw: -0.1 };
        assert_rot_eq(
            head_rotation_from_pose(p, (true, true, true)),
            head_rotation_from_pose(flipped, (false, false, false)),
        );
        // Each flag on its own changes exactly one axis.
        for (flags, expect) in [
            ((true, false, false), PoseCopy { pitch: -0.2, ..p }),
            ((false, true, false), PoseCopy { yaw: -0.1, ..p }),
            ((false, false, true), PoseCopy { roll: -0.3, ..p }),
        ] {
            assert_rot_eq(head_rotation_from_pose(p, flags), head_rotation_from_pose(expect, (false, false, false)));
        }
    }

    // --- Recentering ---

    #[test]
    fn full_recenter_cancels_the_head_rotation() {
        for h in [head(0.0, 0.0, 0.0), head(1.0, 0.3, -0.2), head(-2.9, -0.8, 0.6), head(3.1, 0.0, 0.0)] {
            assert_rot_eq(recenter_calibration(h, false) * h, Quat::IDENTITY);
        }
    }

    #[test]
    fn yaw_only_recenter_without_tilt_matches_full_recenter() {
        for yaw in [-3.0, -1.0, 0.0, 0.7, 2.5] {
            let h = head(yaw, 0.0, 0.0);
            assert_rot_eq(recenter_calibration(h, true), recenter_calibration(h, false));
        }
    }

    #[test]
    fn yaw_only_recenter_keeps_tilt_and_horizon() {
        for (yaw, pitch, roll) in [(1.0, 0.4, 0.0), (-2.0, -0.6, 0.3), (3.0, 0.2, -0.5), (0.0, 1.2, 0.0)] {
            let calib = recenter_calibration(head(yaw, pitch, roll), true);
            // The correction only turns around the vertical axis, so the horizon stays level...
            assert_vec_eq(calib * Vec3::Y, Vec3::Y);
            // ...and what's left after recentering is exactly the tilt.
            assert_rot_eq(calib * head(yaw, pitch, roll), head(0.0, pitch, roll));
        }
    }

    #[test]
    fn upright_roll_reference_survives_yaw_recenters_and_real_tilt() {
        let correction = upright_roll_correction(head(1.2, 0.2, 0.15));
        let upright = head(-0.8, -0.1, 0.15) * correction;
        assert_rot_eq(recenter_calibration(upright, true) * upright, head(0.0, -0.1, 0.0));
        let tilted = head(-0.8, -0.1, 0.45) * correction;
        assert_rot_eq(recenter_calibration(tilted, true) * tilted, head(0.0, -0.1, 0.3));
    }

    #[test]
    fn yaw_only_recenter_works_looking_straight_up_or_down() {
        let half_pi = std::f32::consts::FRAC_PI_2;
        for yaw in [-2.0, 0.0, 1.0] {
            for pitch in [-half_pi, half_pi] {
                let calib = recenter_calibration(head(yaw, pitch, 0.0), true);
                assert_rot_eq(calib, Quat::from_rotation_y(-yaw));
            }
        }
    }

    // --- Camera ---

    #[test]
    fn camera_looking_ahead_is_at_cam_pos() {
        for neck in [0.0, 1.0, 3.0] {
            let eye = camera_view(Quat::IDENTITY, neck).inverse().transform_point3(Vec3::ZERO);
            assert_vec_eq(eye, CAM_POS);
        }
    }

    #[test]
    fn neck_model_off_keeps_the_eye_still() {
        let eye = camera_view(head(1.0, 0.5, 0.2), 0.0).inverse().transform_point3(Vec3::ZERO);
        assert_vec_eq(eye, CAM_POS);
    }

    #[test]
    fn neck_model_moves_the_eye_when_turning() {
        let eye = camera_view(head(1.0, 0.0, 0.0), 1.0).inverse().transform_point3(Vec3::ZERO);
        assert!(eye.distance(CAM_POS) > 0.01);
        // The eye swings around the neck, so it stays at the same height when only turning.
        assert!((eye.y - CAM_POS.y).abs() < EPS);
    }

    #[test]
    fn roll_trim_rotates_the_view_in_the_expected_direction() {
        let right = camera_view(Quat::from_rotation_z(0.1f32.to_radians()), 0.0)
            .transform_vector3(Vec3::X);
        assert!(right.y < 0.0, "positive trim tilts the view clockwise");
    }

    #[test]
    fn screen_centre_is_straight_ahead_in_view_space() {
        let screen = ScreenPlacement { height: 1.4, distance: 4.0 };
        let centre = CAM_POS - Vec3::Z * screen.distance;
        let v = camera_view(Quat::IDENTITY, 1.0).transform_point3(centre);
        assert_vec_eq(v, Vec3::new(0.0, 0.0, -4.0));
    }

    #[test]
    fn projection_handles_a_zero_sized_window() {
        let m = make_proj(0, 0, 60.0);
        assert!(m.to_cols_array().iter().all(|x| x.is_finite()));
    }
}
