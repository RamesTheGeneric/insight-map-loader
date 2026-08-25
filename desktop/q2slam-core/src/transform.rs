//! 4-DoF frame transforms.
//!
//! Every frame in this system — each puck's Insight world, each XR session's
//! LOCAL space, the shared output frame — is gravity-aligned Y-up (measured;
//! see docs/multi-device-alignment.md). So the transform between any two of
//! them is yaw about +Y plus a translation, and modelling them as full SE(3)
//! would only give calibration error somewhere to hide.

use crate::mpt1::Pose;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame4Dof {
    /// Rotation about +Y, radians.
    pub yaw: f32,
    pub t: [f32; 3],
}

impl Frame4Dof {
    pub const IDENTITY: Frame4Dof = Frame4Dof { yaw: 0.0, t: [0.0; 3] };

    /// Rotate a vector by the yaw (no translation) — for velocities.
    pub fn rotate(&self, v: [f32; 3]) -> [f32; 3] {
        let (s, c) = self.yaw.sin_cos();
        [c * v[0] + s * v[2], v[1], -s * v[0] + c * v[2]]
    }

    pub fn apply_point(&self, p: [f32; 3]) -> [f32; 3] {
        let r = self.rotate(p);
        [r[0] + self.t[0], r[1] + self.t[1], r[2] + self.t[2]]
    }

    /// The yaw as a quaternion (x,y,z,w).
    pub fn quat(&self) -> [f32; 4] {
        let (s, c) = (self.yaw * 0.5).sin_cos();
        [0.0, s, 0.0, c]
    }

    pub fn apply_pose(&self, pose: &Pose) -> Pose {
        Pose { p: self.apply_point(pose.p), q: quat_mul(self.quat(), pose.q) }
    }

    /// `self` applied AFTER `other`: (self ∘ other)(x) = self(other(x)).
    pub fn compose(&self, other: &Frame4Dof) -> Frame4Dof {
        let rt = self.rotate(other.t);
        Frame4Dof {
            yaw: wrap(self.yaw + other.yaw),
            t: [rt[0] + self.t[0], rt[1] + self.t[1], rt[2] + self.t[2]],
        }
    }

    pub fn inverse(&self) -> Frame4Dof {
        let inv = Frame4Dof { yaw: -self.yaw, t: [0.0; 3] };
        let rt = inv.rotate(self.t);
        Frame4Dof { yaw: -self.yaw, t: [-rt[0], -rt[1], -rt[2]] }
    }
}

pub fn wrap(a: f32) -> f32 {
    let mut a = a;
    while a > std::f32::consts::PI {
        a -= 2.0 * std::f32::consts::PI;
    }
    while a < -std::f32::consts::PI {
        a += 2.0 * std::f32::consts::PI;
    }
    a
}

/// Hamilton product, (x,y,z,w) order.
pub fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

/// Rotation matrix (row-major) from a quaternion (x,y,z,w).
pub fn quat_to_mat(q: [f32; 4]) -> [[f32; 3]; 3] {
    let [x, y, z, w] = q;
    [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - z * w), 2.0 * (x * z + y * w)],
        [2.0 * (x * y + z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - x * w)],
        [2.0 * (x * z - y * w), 2.0 * (y * z + x * w), 1.0 - 2.0 * (x * x + y * y)],
    ]
}

/// Yaw about +Y of a rotation given as a matrix. Same convention as the Python
/// solvers (`yaw_about_y`): atan2(-r20, r00).
pub fn yaw_of_mat(r: &[[f32; 3]; 3]) -> f32 {
    (-r[2][0]).atan2(r[0][0])
}

/// Yaw of the *relative* rotation `a · b⁻¹`, for two quaternions.
///
/// Not the difference of the individual yaws: with tilted devices those don't
/// subtract cleanly, but the relative rotation between two gravity-aligned
/// frames is yaw-only by construction, so extracting yaw from the product is
/// exact.
pub fn relative_yaw(a: [f32; 4], b: [f32; 4]) -> f32 {
    let bi = [-b[0], -b[1], -b[2], b[3]];
    yaw_of_mat(&quat_to_mat(quat_mul(a, bi)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn apply_and_inverse_round_trip() {
        let t = Frame4Dof { yaw: 0.7, t: [1.0, -2.0, 3.0] };
        let p = [0.3, 0.9, -1.4];
        let back = t.inverse().apply_point(t.apply_point(p));
        assert!(close(back[0], p[0]) && close(back[1], p[1]) && close(back[2], p[2]));
    }

    #[test]
    fn compose_matches_sequential_application() {
        let a = Frame4Dof { yaw: 0.5, t: [1.0, 0.0, -1.0] };
        let b = Frame4Dof { yaw: -1.2, t: [0.2, 2.0, 0.4] };
        let p = [3.0, -1.0, 2.0];
        let seq = a.apply_point(b.apply_point(p));
        let comp = a.compose(&b).apply_point(p);
        assert!(close(seq[0], comp[0]) && close(seq[1], comp[1]) && close(seq[2], comp[2]));
    }

    #[test]
    fn pose_rotation_matches_point_rotation() {
        // Rotating a pose then reading its forward axis must equal rotating
        // the original forward axis as a point.
        let t = Frame4Dof { yaw: 1.1, t: [0.0; 3] };
        let pose = Pose { p: [0.0; 3], q: [0.0, 0.0, 0.0, 1.0] };
        let rq = t.apply_pose(&pose).q;
        let m = quat_to_mat(rq);
        // Forward (-Z) of the rotated pose, as world vector = column 2 negated.
        let fwd = [-m[0][2], -m[1][2], -m[2][2]];
        let want = t.rotate([0.0, 0.0, -1.0]);
        assert!(close(fwd[0], want[0]) && close(fwd[1], want[1]) && close(fwd[2], want[2]));
    }

    #[test]
    fn relative_yaw_is_exact_under_tilt() {
        // A tilted device seen in two frames that differ by pure yaw: the
        // relative yaw must come back exactly, tilt notwithstanding.
        let tilt = [0.3f32.sin() * 0.5, 0.0, 0.0, 1.0];
        let tilt = {
            let n = (tilt[0] * tilt[0] + tilt[3] * tilt[3]).sqrt();
            [tilt[0] / n, 0.0, 0.0, tilt[3] / n]
        };
        let yawq = Frame4Dof { yaw: 0.8, t: [0.0; 3] }.quat();
        let in_world = quat_mul(yawq, tilt);
        assert!(close(relative_yaw(in_world, tilt), 0.8));
    }
}
