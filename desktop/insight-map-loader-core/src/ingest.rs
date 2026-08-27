//! Receiving tracker poses.
//!
//! Each puck streams MPT1 to one UDP port. This keeps the newest packet per
//! device slot and nothing else: a pose that has been superseded is worthless,
//! so there is no queue to fall behind. Readers ask for the current state and
//! get an answer that is either fresh or explicitly not.
//!
//! Freshness is judged on *arrival*, deliberately. Each packet carries `t_ns` on
//! the producer's clock, and until the pucks are clock-synced those clocks do
//! not share an epoch — so arrival time is the only thing this layer can compare
//! across devices. `t_ns` is preserved untouched for the layers that do know the
//! offsets.

use std::collections::BTreeMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::mpt1::{Packet, PACKET_LEN};

/// A packet plus when it landed here.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub packet: Packet,
    pub arrived: Instant,
}

impl Sample {
    pub fn age(&self) -> Duration {
        self.arrived.elapsed()
    }
}

/// What a consumer sees for one slot right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SlotState {
    /// Nothing has ever arrived for this slot.
    Absent,
    /// Packets are arriving but the tracker says it is not tracking. Its pose
    /// must not be used; during a dropout Insight reports position as (0,0,0),
    /// so a "degraded" pose is absent rather than merely noisy.
    NotTracking,
    /// Packets stopped. The tracker crashed, lost the network, or was switched
    /// off — distinct from NotTracking, where it is alive and honest about being
    /// lost.
    Stale,
    /// Usable.
    Live,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub received: u64,
    pub malformed: u64,
}

struct Shared {
    /// Keyed by the SOURCE byte the sender stamped, not by role. Resolution
    /// to a role is the host's job and happens downstream, which is what lets
    /// a role be reassigned without touching the puck.
    slots: Mutex<BTreeMap<u8, Sample>>,
    received: AtomicU64,
    malformed: AtomicU64,
}

/// Listens on a UDP port and tracks the newest pose per slot.
pub struct Ingest {
    shared: Arc<Shared>,
    stale_after: Duration,
}

impl Ingest {
    /// Bind and start receiving on a background thread.
    ///
    /// `stale_after` should be a few packet intervals — long enough not to trip
    /// on ordinary jitter, short enough that a dead tracker is noticed before a
    /// consumer acts on its last pose. At 72 Hz, 0.5 s is ~36 missed packets.
    pub fn bind(addr: &str, stale_after: Duration) -> std::io::Result<Ingest> {
        let socket = UdpSocket::bind(addr)?;
        // A read timeout keeps the thread responsive to nothing arriving at all,
        // which is itself the signal that every tracker is gone.
        socket.set_read_timeout(Some(Duration::from_millis(250)))?;

        let shared = Arc::new(Shared {
            slots: Mutex::new(BTreeMap::new()),
            received: AtomicU64::new(0),
            malformed: AtomicU64::new(0),
        });

        let worker = Arc::clone(&shared);
        std::thread::Builder::new().name("mpt1-ingest".into()).spawn(move || {
            let mut buf = [0u8; 1500];
            loop {
                let n = match socket.recv(&mut buf) {
                    Ok(n) => n,
                    Err(_) => continue, // timeout, or a transient socket error
                };
                match Packet::decode(&buf[..n.min(PACKET_LEN.max(n))]) {
                    Ok(packet) => {
                        worker.received.fetch_add(1, Ordering::Relaxed);
                        let sample = Sample { packet, arrived: Instant::now() };
                        if let Ok(mut slots) = worker.slots.lock() {
                            slots.insert(packet.src, sample);
                        }
                    }
                    Err(_) => {
                        worker.malformed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })?;

        Ok(Ingest { shared, stale_after })
    }

    /// Newest sample for a slot, regardless of whether it is usable.
    pub fn sample(&self, src: u8) -> Option<Sample> {
        self.shared.slots.lock().ok()?.get(&src).copied()
    }

    pub fn state(&self, src: u8) -> SlotState {
        match self.sample(src) {
            None => SlotState::Absent,
            Some(s) if s.age() > self.stale_after => SlotState::Stale,
            Some(s) if !s.packet.valid => SlotState::NotTracking,
            Some(_) => SlotState::Live,
        }
    }

    /// Only the slots safe to consume: present, tracking, and fresh.
    pub fn live(&self) -> Vec<Sample> {
        let Ok(slots) = self.shared.slots.lock() else { return Vec::new() };
        slots
            .values()
            .filter(|s| s.packet.valid && s.age() <= self.stale_after)
            .copied()
            .collect()
    }

    /// Every SOURCE heard from, with its verdict — for status display.
    ///
    /// Enumerates what actually arrived rather than the fixed role list, so a
    /// puck whose id the host does not recognise still shows up. That is the
    /// point: an unprovisioned or misconfigured puck should be visible as an
    /// unknown source, not silently absent.
    pub fn all(&self) -> Vec<(u8, SlotState, Option<Sample>)> {
        let Ok(slots) = self.shared.slots.lock() else { return Vec::new() };
        let srcs: Vec<u8> = slots.keys().copied().collect();
        drop(slots);
        srcs.into_iter().map(|s| (s, self.state(s), self.sample(s))).collect()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            received: self.shared.received.load(Ordering::Relaxed),
            malformed: self.shared.malformed.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpt1::{Device, Pose};

    fn packet(device: Device, valid: bool) -> Packet {
        Packet {
            src: device as u8,
            valid,
            battery_pct: 0,
            charging: false,
            t_ns: 1,
            pose: Pose::IDENTITY,
            vel: [0.0; 3],
            angvel: [0.0; 3],
        }
    }

    fn send_to(port: u16, p: &Packet) {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.send_to(&p.encode(), ("127.0.0.1", port)).unwrap();
    }

    fn wait_for(ingest: &Ingest, device: Device, want: SlotState) -> bool {
        for _ in 0..100 {
            if ingest.state(device as u8) == want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn absent_until_something_arrives() {
        let ingest = Ingest::bind("127.0.0.1:0", Duration::from_millis(200)).unwrap();
        assert_eq!(ingest.state(Device::Waist as u8), SlotState::Absent);
        assert!(ingest.live().is_empty());
    }

    #[test]
    fn live_then_stale_and_not_tracking_is_distinct() {
        // Bind a known port by asking the OS for one first.
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let ingest =
            Ingest::bind(&format!("127.0.0.1:{port}"), Duration::from_millis(200)).unwrap();

        send_to(port, &packet(Device::Waist, true));
        assert!(wait_for(&ingest, Device::Waist, SlotState::Live));
        assert_eq!(ingest.live().len(), 1);

        // A tracker that says it is lost is not the same as one that vanished.
        send_to(port, &packet(Device::Waist, false));
        assert!(wait_for(&ingest, Device::Waist, SlotState::NotTracking));
        assert!(ingest.live().is_empty(), "a not-tracking pose must never be consumed");

        // Silence eventually reads as stale.
        assert!(wait_for(&ingest, Device::Waist, SlotState::Stale));

        let stats = ingest.stats();
        assert_eq!(stats.received, 2);
        assert_eq!(stats.malformed, 0);
    }

    #[test]
    fn junk_is_counted_not_fatal() {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let ingest = Ingest::bind(&format!("127.0.0.1:{port}"), Duration::from_secs(5)).unwrap();

        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.send_to(b"not an MPT1 packet", ("127.0.0.1", port)).unwrap();
        send_to(port, &packet(Device::RightFoot, true));

        assert!(wait_for(&ingest, Device::RightFoot, SlotState::Live));
        assert_eq!(ingest.stats().malformed, 1);
    }
}
