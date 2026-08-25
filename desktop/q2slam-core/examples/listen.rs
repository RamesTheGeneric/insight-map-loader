//! Watch what the pucks are actually sending.
//!
//! Prints one line per slot per second: state, pose, rate, and age. Rate is
//! counted here rather than assumed, because "the tracker is running" and "poses
//! are arriving at the rate you think" are different claims — a tracker can sit
//! at FOCUSED and emit nothing, or emit only invalid poses, and both look alike
//! from the outside.
//!
//!     cargo run --example listen -- 5180

use std::time::{Duration, Instant};

use q2slam_core::ingest::SlotState;
use q2slam_core::{Device, Ingest};

fn main() -> std::io::Result<()> {
    let port: u16 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5180);
    let ingest = Ingest::bind(&format!("0.0.0.0:{port}"), Duration::from_millis(500))?;
    println!("listening for MPT1 on :{port}  (ctrl-c to stop)\n");

    let start = Instant::now();
    let mut last = ingest.stats();
    let mut last_at = Instant::now();

    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now = ingest.stats();
        let dt = last_at.elapsed().as_secs_f64();
        let rate = (now.received - last.received) as f64 / dt;
        last = now;
        last_at = Instant::now();

        let mut line = format!("[{:5.1}s] {:6.1} pkt/s", start.elapsed().as_secs_f64(), rate);
        for (device, state, sample) in ingest.all() {
            let tag = match state {
                SlotState::Absent => "absent".to_string(),
                SlotState::Stale => "STALE".to_string(),
                SlotState::NotTracking => "no-track".to_string(),
                SlotState::Live => sample
                    .map(|s| {
                        let p = s.packet.pose.p;
                        format!(
                            "({:+.2},{:+.2},{:+.2}) {:.0}ms",
                            p[0],
                            p[1],
                            p[2],
                            s.age().as_secs_f64() * 1e3
                        )
                    })
                    .unwrap_or_default(),
            };
            // Only mention slots we have ever heard from, so an unused slot does
            // not look like a fault.
            if state != SlotState::Absent || device == Device::Waist {
                line.push_str(&format!("  {}={}", device.label(), tag));
            }
        }
        if now.malformed > 0 {
            line.push_str(&format!("  malformed={}", now.malformed));
        }
        println!("{line}");
    }
}
