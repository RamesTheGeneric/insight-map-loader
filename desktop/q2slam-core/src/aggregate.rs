//! The output side: every puck, one frame, one stream.
//!
//! Takes the newest ingested sample per slot, applies that puck's
//! LOCAL→shared transform (its session bridge composed with the solved
//! inter-frame alignment), and re-emits MPT1 to one destination — typically
//! the SteamVR driver. The driver is a dumb relay by contract: poses must
//! arrive already in the destination world frame, and this is the place that
//! makes them so.
//!
//! Timestamps are rewritten to the host clock (arrival-time minus sample age)
//! so the downstream latency estimator sees one epoch instead of three
//! unrelated device clocks — the pucks' clocks differ by thousands of seconds,
//! and a single min-offset estimator fed that mix would misdate two of the
//! three trackers by exactly that much.

use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};

use crate::ingest::Ingest;
use crate::mpt1::{now_ns, Device, Packet};
use crate::transform::Frame4Dof;

pub struct Aggregator {
    sock: UdpSocket,
    dest: SocketAddr,
    /// LOCAL→shared per device slot. A slot with no transform is not emitted:
    /// an unaligned pose in an aligned stream is worse than a missing one.
    pub transforms: BTreeMap<Device, Frame4Dof>,
    last_emitted_t: BTreeMap<Device, u64>,
    pub emitted: u64,
    pub skipped_no_transform: u64,
}

/// What one tick saw, for status display and health checks.
#[derive(Debug, Default)]
pub struct TickSummary {
    pub live: Vec<(Device, [f32; 3])>,
    /// Distance between the first two live pucks, shared frame.
    pub separation: Option<f32>,
    /// Sum of those two pucks' reported speeds (m/s). Physical motion can
    /// change the separation at most this fast, which is what lets a frame
    /// jump be told apart from someone simply carrying the pucks around.
    pub speed_sum: Option<f32>,
}

impl Aggregator {
    pub fn new(dest: &str) -> std::io::Result<Aggregator> {
        Ok(Aggregator {
            sock: UdpSocket::bind("0.0.0.0:0")?,
            dest: dest.parse().map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{dest}: {e}"))
            })?,
            transforms: BTreeMap::new(),
            last_emitted_t: BTreeMap::new(),
            emitted: 0,
            skipped_no_transform: 0,
        })
    }

    /// Forward every live sample not yet emitted. Output rate therefore equals
    /// input rate (~72 Hz per puck), regardless of how often this is called.
    pub fn tick(&mut self, ingest: &Ingest) -> TickSummary {
        let mut summary = TickSummary::default();
        let mut speeds: Vec<f32> = Vec::new();
        for sample in ingest.live() {
            let device = sample.packet.device;
            let Some(tr) = self.transforms.get(&device) else {
                self.skipped_no_transform += 1;
                continue;
            };
            let mut pkt: Packet = sample.packet;
            pkt.pose = tr.apply_pose(&pkt.pose);
            pkt.vel = tr.rotate(pkt.vel);
            pkt.angvel = tr.rotate(pkt.angvel);
            summary.live.push((device, pkt.pose.p));
            speeds.push(
                (pkt.vel[0].powi(2) + pkt.vel[1].powi(2) + pkt.vel[2].powi(2)).sqrt(),
            );

            if self.last_emitted_t.get(&device) == Some(&sample.packet.t_ns) {
                continue; // same sample as last tick; the consumer has it
            }
            self.last_emitted_t.insert(device, sample.packet.t_ns);
            pkt.t_ns = now_ns().saturating_sub(sample.age().as_nanos() as u64);
            if self.sock.send_to(&pkt.encode(), self.dest).is_ok() {
                self.emitted += 1;
            }
        }
        if summary.live.len() >= 2 {
            let (a, b) = (summary.live[0].1, summary.live[1].1);
            summary.separation = Some(
                ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt(),
            );
            summary.speed_sum = Some(speeds[0] + speeds[1]);
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::Ingest;
    use crate::mpt1::Pose;
    use std::time::Duration;

    #[test]
    fn transforms_and_forwards() {
        // in: tracker packets on a private port; out: a local listener.
        let inp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let in_port = inp.local_addr().unwrap().port();
        drop(inp);
        let ingest = Ingest::bind(&format!("127.0.0.1:{in_port}"), Duration::from_secs(2)).unwrap();

        let out_l = UdpSocket::bind("127.0.0.1:0").unwrap();
        out_l.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut agg = Aggregator::new(&format!("127.0.0.1:{}", out_l.local_addr().unwrap().port()))
            .unwrap();
        agg.transforms.insert(
            Device::Waist,
            Frame4Dof { yaw: std::f32::consts::FRAC_PI_2, t: [1.0, 0.0, 0.0] },
        );

        let pkt = Packet {
            device: Device::Waist,
            valid: true,
            battery_pct: 50,
            charging: false,
            t_ns: 42,
            pose: Pose { p: [0.0, 0.0, 2.0], q: [0.0, 0.0, 0.0, 1.0] },
            vel: [0.0, 0.0, 1.0],
            angvel: [0.0; 3],
        };
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(&pkt.encode(), ("127.0.0.1", in_port)).unwrap();

        // Wait for ingest, then tick.
        let mut got = None;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(10));
            let s = agg.tick(&ingest);
            if !s.live.is_empty() {
                got = Some(s);
                break;
            }
        }
        let s = got.expect("sample never became live");
        // +90° yaw about Y maps +Z to +X: (0,0,2) -> (2,0,0), then +1 x.
        let p = s.live[0].1;
        assert!((p[0] - 3.0).abs() < 1e-4 && p[1].abs() < 1e-4 && p[2].abs() < 1e-4);

        let mut buf = [0u8; 128];
        let (n, _) = out_l.recv_from(&mut buf).unwrap();
        let out = Packet::decode(&buf[..n]).unwrap();
        assert!((out.vel[0] - 1.0).abs() < 1e-4, "velocity must rotate too");
        assert_ne!(out.t_ns, 42, "timestamp must be rewritten to the host clock");

        // Same sample again: transformed for display, not re-sent.
        agg.tick(&ingest);
        assert_eq!(agg.emitted, 1);
    }
}
