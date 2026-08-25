//! MPT1 — the tracker wire format.
//!
//! One datagram carries one tracker's latest pose. The format is shared with the
//! SteamVR driver, so it is a fixed contract: 68 bytes, little-endian, packed.
//!
//! ```text
//! off  size  field
//!   0     4  magic = 'MPT1' (0x3154504D LE)
//!   4     1  device   0 waist / 1 left_foot / 2 right_foot
//!   5     1  valid    1 = tracking, 0 = running but lost
//!   6     1  battery_pct   0 = not reported
//!   7     1  battery_flags bit0 = charging
//!   8     8  t_ns     u64, the instant the pose is FOR, on the producer's clock
//!  16    28  pose[7]  f32  x, y, z, qw, qx, qy, qz
//!  44    12  vel[3]   f32  linear m/s, world frame
//!  56    12  angvel[3] f32 angular rad/s, world frame
//! ```
//!
//! Two details bite if forgotten:
//!
//! * **The quaternion is (w,x,y,z) on the wire**, while OpenXR and most maths
//!   libraries use (x,y,z,w). [`Pose`] stores (x,y,z,w) and converts at the wire
//!   boundary, so the swap lives in exactly one place.
//! * **Poses must already be in the destination world frame.** The driver is a
//!   dumb relay; frame alignment is applied upstream of it. For us that means
//!   the aggregator applies each puck's transform before re-emitting.
//!
//! There are exactly three device slots, which is what tells the driver waist
//! from left foot from right foot.

use std::time::{SystemTime, UNIX_EPOCH};

pub const MAGIC: u32 = 0x3154_504D; // 'MPT1' little-endian
pub const PACKET_LEN: usize = 68;
pub const MAX_DEVICES: usize = 11;

/// Which tracker a packet belongs to. **The id IS the SteamVR role**: the
/// driver's table is indexed by it, so one id means one serial means one FBT
/// role, permanently.
///
/// APPEND ONLY, and never renumber or rename. SteamVR keys device pairings,
/// role bindings and room calibration off the serial derived from this id;
/// changing 0/1/2 would make it forget every existing tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Device {
    Waist = 0,
    LeftFoot = 1,
    RightFoot = 2,
    Chest = 3,
    LeftKnee = 4,
    RightKnee = 5,
    LeftElbow = 6,
    RightElbow = 7,
    LeftShoulder = 8,
    RightShoulder = 9,
    Camera = 10,
}

/// Every role, in id order. The one place to add a new one.
pub const ALL_DEVICES: [Device; MAX_DEVICES] = [
    Device::Waist,
    Device::LeftFoot,
    Device::RightFoot,
    Device::Chest,
    Device::LeftKnee,
    Device::RightKnee,
    Device::LeftElbow,
    Device::RightElbow,
    Device::LeftShoulder,
    Device::RightShoulder,
    Device::Camera,
];

impl Device {
    pub fn from_u8(v: u8) -> Option<Self> {
        ALL_DEVICES.get(v as usize).copied()
    }

    /// Wire/config name, matching the SteamVR serial suffix.
    pub fn label(self) -> &'static str {
        match self {
            Device::Waist => "waist",
            Device::LeftFoot => "left_foot",
            Device::RightFoot => "right_foot",
            Device::Chest => "chest",
            Device::LeftKnee => "left_knee",
            Device::RightKnee => "right_knee",
            Device::LeftElbow => "left_elbow",
            Device::RightElbow => "right_elbow",
            Device::LeftShoulder => "left_shoulder",
            Device::RightShoulder => "right_shoulder",
            Device::Camera => "camera",
        }
    }

    /// Human-facing name for the role picker.
    pub fn pretty(self) -> &'static str {
        match self {
            Device::Waist => "Waist / hip",
            Device::LeftFoot => "Left foot",
            Device::RightFoot => "Right foot",
            Device::Chest => "Chest",
            Device::LeftKnee => "Left knee",
            Device::RightKnee => "Right knee",
            Device::LeftElbow => "Left elbow",
            Device::RightElbow => "Right elbow",
            Device::LeftShoulder => "Left shoulder",
            Device::RightShoulder => "Right shoulder",
            Device::Camera => "Camera",
        }
    }
}

/// Position in metres and orientation as a quaternion in **(x, y, z, w)** order.
///
/// The frame is gravity-aligned Y-up, matching both OpenXR and OpenVR, so no
/// axis conversion is needed anywhere in this pipeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub p: [f32; 3],
    pub q: [f32; 4],
}

impl Pose {
    pub const IDENTITY: Pose = Pose { p: [0.0; 3], q: [0.0, 0.0, 0.0, 1.0] };
}

impl Default for Pose {
    fn default() -> Self {
        Pose::IDENTITY
    }
}

/// A decoded MPT1 datagram.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Packet {
    pub device: Device,
    /// False means the tracker is alive but not tracking. Such a packet still
    /// arrives — silence means the tracker is *gone*, which is a different
    /// problem — but its pose must not be consumed.
    pub valid: bool,
    pub battery_pct: u8,
    pub charging: bool,
    /// The instant the pose is for, on the *producer's* clock. Not arrival time,
    /// and not necessarily this host's clock.
    pub t_ns: u64,
    pub pose: Pose,
    pub vel: [f32; 3],
    pub angvel: [f32; 3],
}

#[derive(Debug, PartialEq)]
pub enum DecodeError {
    /// Not 68 bytes. Almost always something else on the port.
    WrongLength(usize),
    /// Right size, wrong magic — a different protocol, or a byte-order mistake.
    BadMagic(u32),
    /// A device id outside the three slots.
    BadDevice(u8),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::WrongLength(n) => write!(f, "expected {PACKET_LEN} bytes, got {n}"),
            DecodeError::BadMagic(m) => write!(f, "bad magic 0x{m:08X}"),
            DecodeError::BadDevice(d) => write!(f, "device id {d} outside 0..{}", MAX_DEVICES - 1),
        }
    }
}

impl std::error::Error for DecodeError {}

fn f32_at(b: &[u8], off: usize) -> f32 {
    f32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

impl Packet {
    pub fn decode(b: &[u8]) -> Result<Packet, DecodeError> {
        if b.len() != PACKET_LEN {
            return Err(DecodeError::WrongLength(b.len()));
        }
        let magic = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if magic != MAGIC {
            return Err(DecodeError::BadMagic(magic));
        }
        let device = Device::from_u8(b[4]).ok_or(DecodeError::BadDevice(b[4]))?;

        let mut t = [0u8; 8];
        t.copy_from_slice(&b[8..16]);

        // Wire order is qw,qx,qy,qz; Pose holds x,y,z,w.
        let (qw, qx, qy, qz) = (f32_at(b, 28), f32_at(b, 32), f32_at(b, 36), f32_at(b, 40));

        Ok(Packet {
            device,
            valid: b[5] != 0,
            battery_pct: b[6],
            charging: b[7] & 1 != 0,
            t_ns: u64::from_le_bytes(t),
            pose: Pose {
                p: [f32_at(b, 16), f32_at(b, 20), f32_at(b, 24)],
                q: [qx, qy, qz, qw],
            },
            vel: [f32_at(b, 44), f32_at(b, 48), f32_at(b, 52)],
            angvel: [f32_at(b, 56), f32_at(b, 60), f32_at(b, 64)],
        })
    }

    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let mut b = [0u8; PACKET_LEN];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4] = self.device as u8;
        b[5] = self.valid as u8;
        b[6] = self.battery_pct;
        b[7] = self.charging as u8;
        b[8..16].copy_from_slice(&self.t_ns.to_le_bytes());

        let [qx, qy, qz, qw] = self.pose.q;
        let seven = [self.pose.p[0], self.pose.p[1], self.pose.p[2], qw, qx, qy, qz];
        for (i, v) in seven.iter().enumerate() {
            b[16 + i * 4..20 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        for (i, v) in self.vel.iter().enumerate() {
            b[44 + i * 4..48 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        for (i, v) in self.angvel.iter().enumerate() {
            b[56 + i * 4..60 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
}

/// Host wall-clock in nanoseconds, for stamping packets we originate.
pub fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Packet {
        Packet {
            device: Device::LeftFoot,
            valid: true,
            battery_pct: 77,
            charging: true,
            t_ns: 0x0123_4567_89AB_CDEF,
            pose: Pose { p: [1.5, -2.25, 3.125], q: [0.1, 0.2, 0.3, 0.9273618] },
            vel: [0.5, -0.25, 0.125],
            angvel: [-1.0, 2.0, -3.0],
        }
    }

    #[test]
    fn round_trip() {
        let p = sample();
        assert_eq!(Packet::decode(&p.encode()).unwrap(), p);
    }

    #[test]
    fn layout_is_the_documented_one() {
        let b = sample().encode();
        assert_eq!(b.len(), 68);
        assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), MAGIC);
        assert_eq!(b[4], 1); // device
        assert_eq!(b[5], 1); // valid
        // The quaternion goes out w-first even though Pose stores w last.
        assert_eq!(f32_at(&b, 28), 0.9273618);
        assert_eq!(f32_at(&b, 32), 0.1);
    }

    #[test]
    fn rejects_junk() {
        assert_eq!(Packet::decode(&[0u8; 12]), Err(DecodeError::WrongLength(12)));

        let mut b = sample().encode();
        b[0] ^= 0xFF;
        assert!(matches!(Packet::decode(&b), Err(DecodeError::BadMagic(_))));

        // Expressed against MAX_DEVICES rather than a literal: role ids are
        // append-only, so a hardcoded "invalid" id becomes valid the next time
        // a role is added, and the test would then assert the opposite of what
        // it means.
        let bad = MAX_DEVICES as u8;
        let mut b = sample().encode();
        b[4] = bad;
        assert_eq!(Packet::decode(&b), Err(DecodeError::BadDevice(bad)));

        // ...and the last valid id must still decode.
        let mut b = sample().encode();
        b[4] = MAX_DEVICES as u8 - 1;
        assert!(Packet::decode(&b).is_ok(), "the highest role id must be accepted");
    }

    #[test]
    fn every_role_round_trips_through_its_id() {
        for (i, d) in ALL_DEVICES.iter().enumerate() {
            assert_eq!(Device::from_u8(i as u8), Some(*d));
            assert_eq!(*d as u8, i as u8, "{} must keep id {i}", d.label());
        }
        assert_eq!(ALL_DEVICES.len(), MAX_DEVICES);
        // Serials in the SteamVR driver are derived from these; a duplicate
        // would silently collapse two roles onto one tracker.
        let mut labels: Vec<&str> = ALL_DEVICES.iter().map(|d| d.label()).collect();
        labels.sort_unstable();
        let n = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), n, "role labels must be unique");
    }
}
