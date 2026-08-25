//! Bridging a puck's XR LOCAL space to its Insight world frame.
//!
//! The tracker app publishes poses in the OpenXR LOCAL space, whose origin is
//! wherever the session started. The alignment transform T_A_B was solved
//! between the pucks' *Insight world* frames (what `dumpsys tracking` reports).
//! Those are different frames, so applying T_A_B to MPT1 poses directly would
//! be wrong by an unknown 4-DoF offset per session.
//!
//! The bridge is observable, though: at any instant the same physical headset
//! has a pose in both frames. Reading both at (nearly) the same time gives one
//! candidate T_world_local = P_world · P_local⁻¹; both frames are
//! gravity-aligned, so the candidate is projected to yaw+translation and a few
//! candidates are averaged. Motion between the two reads corrupts a candidate,
//! so bridge while the puck is still — the spread across candidates reports
//! whether it was.
//!
//! The bridge lives exactly as long as the XR session's LOCAL frame. The
//! tracker absorbs recenters into its output (g_fix), so within one app run
//! the bridge holds; restart the app, re-bridge.

use crate::mpt1::Pose;
use crate::transform::{relative_yaw, wrap, Frame4Dof};

/// One simultaneous observation of the same headset in both frames.
#[derive(Debug, Clone, Copy)]
pub struct PosePair {
    /// From `dumpsys tracking` — the Insight world frame.
    pub world: Pose,
    /// From the tracker's MPT1 stream — the XR LOCAL frame.
    pub local: Pose,
}

#[derive(Debug, Clone, Copy)]
pub struct BridgeSolution {
    pub transform: Frame4Dof,
    /// Worst yaw disagreement among candidates, degrees. Above ~2° means the
    /// puck was moving between paired reads; re-bridge.
    pub yaw_spread_deg: f32,
    /// Worst per-axis translation disagreement, metres.
    pub t_spread_m: f32,
    pub pairs: usize,
}

pub fn solve(pairs: &[PosePair]) -> Option<BridgeSolution> {
    if pairs.len() < 3 {
        return None;
    }
    let yaws: Vec<f32> = pairs.iter().map(|p| relative_yaw(p.world.q, p.local.q)).collect();
    // Circular mean; candidates are tight when the puck is still, so the mean
    // is fine and the spread is the honest quality signal.
    let (ms, mc) = yaws.iter().fold((0.0f32, 0.0f32), |(s, c), y| (s + y.sin(), c + y.cos()));
    let yaw = ms.atan2(mc);

    let ts: Vec<[f32; 3]> = pairs
        .iter()
        .map(|p| {
            let r = Frame4Dof { yaw, t: [0.0; 3] }.rotate(p.local.p);
            [p.world.p[0] - r[0], p.world.p[1] - r[1], p.world.p[2] - r[2]]
        })
        .collect();
    let mut t = [0.0f32; 3];
    for axis in 0..3 {
        let mut v: Vec<f32> = ts.iter().map(|x| x[axis]).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        t[axis] = v[v.len() / 2];
    }

    let yaw_spread = yaws
        .iter()
        .map(|y| wrap(y - yaw).abs())
        .fold(0.0f32, f32::max)
        .to_degrees();
    let t_spread = ts
        .iter()
        .map(|x| (0..3).map(|a| (x[a] - t[a]).abs()).fold(0.0f32, f32::max))
        .fold(0.0f32, f32::max);

    Some(BridgeSolution {
        transform: Frame4Dof { yaw, t },
        yaw_spread_deg: yaw_spread,
        t_spread_m: t_spread,
        pairs: pairs.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::quat_mul;

    #[test]
    fn recovers_a_known_bridge() {
        let truth = Frame4Dof { yaw: 1.234, t: [0.5, -1.25, 2.0] };
        let mut pairs = Vec::new();
        // Device at several positions/orientations (with tilt), each seen in
        // LOCAL and, through the truth transform, in world.
        for i in 0..6 {
            let f = i as f32;
            let tilt = {
                let h = 0.1 * f;
                [h.sin(), 0.0, 0.0, h.cos()]
            };
            let yawq = Frame4Dof { yaw: 0.4 * f, t: [0.0; 3] }.quat();
            let local = Pose { p: [f * 0.3, 1.0 + 0.1 * f, -f * 0.2], q: quat_mul(yawq, tilt) };
            let world = truth.apply_pose(&local);
            pairs.push(PosePair { world, local });
        }
        let s = solve(&pairs).unwrap();
        assert!((s.transform.yaw - truth.yaw).abs() < 1e-4);
        for a in 0..3 {
            assert!((s.transform.t[a] - truth.t[a]).abs() < 1e-4);
        }
        assert!(s.yaw_spread_deg < 0.01);
        assert!(s.t_spread_m < 1e-4);
    }

    #[test]
    fn too_few_pairs_is_refused() {
        assert!(solve(&[]).is_none());
    }
}
