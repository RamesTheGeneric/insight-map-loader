//! insight-map-loader — bring up the pucks, bridge their frames, stream them as one.
//!
//!     insight-map-loader status              what every puck is doing, one line each
//!     insight-map-loader up                  connect + configure + launch the trackers
//!     insight-map-loader bridge              measure each puck's LOCAL→world transform
//!     insight-map-loader run                 ingest → bridge → emit shared-frame MPT1
//!     insight-map-loader identify            blink every puck's LED in its slot colour
//!     insight-map-loader provision           one-time: make every puck stream after any boot
//!
//! Config is insight-map-loader.json (see insight-map-loader.example.json).
//!
//! There is no inter-puck alignment step: the pucks share one Insight map, so
//! their world frames are already the same frame. The only stored calibration
//! is each puck's LOCAL→world bridge in bridge.json, which `run` solves and
//! re-verifies on its own — `bridge` is the manual override.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use insight_map_loader_core::bridge::{self, PosePair};
use insight_map_loader_core::ingest::{Ingest, SlotState};
use insight_map_loader_core::config::Config;
use insight_map_loader_core::fleet;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    let cfg_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "insight-map-loader.json".into());

    let cfg = match Config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot read {cfg_path}: {e}");
            eprintln!("copy desktop/insight-map-loader.example.json to insight-map-loader.json and edit it");
            std::process::exit(2);
        }
    };

    match cmd {
        "status" => status(&cfg),
        "up" => up(&cfg),
        "bridge" => std::process::exit(bridge_cmd(&cfg)),
        "run" => std::process::exit(run(&cfg)),
        "identify" => identify(&cfg),
        "provision" => provision(&cfg, &cfg_path),
        "mapdb" => mapdb_cmd(&cfg),
        _ => {
            eprintln!("usage: insight-map-loader [status|up|bridge|run|identify|provision|mapdb] [--config insight-map-loader.json]");
            std::process::exit(2);
        }
    }
}

/// Report each puck's persisted Insight map: the file count, size, age and the
/// FULL root uuid. Colocation is exactly "every puck reports the same root", so
/// this is the one-glance check for it -- and it exercises the checked adb path
/// that the map transplant is built on.
fn mapdb_cmd(cfg: &Config) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for p in &cfg.pucks {
        print!("{:<16} ", p.ip);
        let st = fleet::status(&p.ip);
        if !st.reachable {
            println!("unreachable");
            continue;
        }
        match fleet::mapdb_info(&p.ip) {
            Ok(info) if info.is_empty() => println!(
                "mapdb EMPTY   context {} ({}) -- no persistent map to share",
                if st.map_root.is_empty() { "none".into() } else { fleet::short_root(&st.map_root) },
                if st.map_persistent { "persistent" } else { "transient" },
            ),
            Ok(info) => {
                let age = if info.mtime_unix > 0 { now - info.mtime_unix } else { -1 };
                println!(
                    "{:>3} files {:>6} KB  written {}  root {} ({})",
                    info.files,
                    info.bytes / 1024,
                    if age < 0 { "?".into() } else { format!("{age}s ago") },
                    if st.map_root.is_empty() { "none".into() } else { st.map_root.clone() },
                    if st.map_persistent { "persistent" } else { "transient" },
                );
            }
            // The whole point of the checked path: a real reason, not "".
            Err(e) => println!("ERROR {e}"),
        }
    }
}

/// Give every puck a stable id, then grant boot-start.
///
/// The id is what makes a role reassignable host-side: the puck stamps it into
/// every packet and the host maps it to a role, so changing a role afterwards
/// is a config edit rather than a push plus a tracker restart -- and a tracker
/// restart moves the OpenXR LOCAL frame, which would invalidate that puck's
/// bridge for what is only a relabel.
///
/// Assigning one costs exactly one restart, here, once per puck ever. A puck
/// that already has an id keeps it, so this is safe to re-run.
fn provision(cfg: &Config, cfg_path: &str) {
    let port: u16 = cfg.listen.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(5180);
    let mut roster = cfg.pucks.clone();
    let need: Vec<String> =
        roster.iter().filter(|p| p.id.is_none()).map(|p| p.ip.clone()).collect();

    if need.is_empty() {
        println!("every puck already has a stable id\n");
    } else {
        println!("assigning stable ids to {} puck(s) — one tracker restart each\n", need.len());
        for ip in &need {
            let Some(id) = insight_map_loader_core::config::next_free_id(&roster) else {
                println!("  {ip:15} no free id left (all 100..255 are taken)");
                continue;
            };
            // Config first: if the push succeeds but the write fails, the puck
            // streams an id the host does not know and vanishes from the output.
            // The other order merely leaves it on the legacy path, which works.
            if let Err(e) = insight_map_loader_core::config::set_puck_id(cfg_path, ip, id) {
                println!("  {ip:15} could not record id {id}: {e}");
                continue;
            }
            match fleet::configure_tracker(ip, &cfg.host, port, id) {
                Ok(()) => {
                    println!("  {ip:15} id {id} assigned; role is now host-side only");
                    if let Some(p) = roster.iter_mut().find(|p| &p.ip == ip) {
                        p.id = Some(id);
                    }
                }
                Err(e) => println!("  {ip:15} id {id} recorded but the puck push failed: {e}"),
            }
        }
        println!();
    }

    println!("granting boot-start to each puck (~45 s each for the appop to flush)\n");
    let handles: Vec<_> = cfg
        .pucks
        .iter()
        .map(|p| {
            let ip = p.ip.clone();
            std::thread::spawn(move || (ip.clone(), fleet::provision_autostart(&ip)))
        })
        .collect();
    let mut all = true;
    for h in handles {
        match h.join() {
            Ok((ip, Ok(true))) => println!("  {ip:15} ready — will stream after every boot"),
            Ok((ip, Ok(false))) => {
                println!("  {ip:15} FAILED — appop or guardian prop did not stick");
                all = false;
            }
            Ok((ip, Err(e))) => {
                println!("  {ip:15} error: {e}");
                all = false;
            }
            Err(_) => all = false,
        }
    }
    println!(
        "\n{}",
        if all {
            "Done. Power-cycle a puck to confirm: it should appear in the stream unaided."
        } else {
            "Some pucks are not provisioned; they still need `insight-map-loader up` after a boot."
        }
    );
}

fn identify(cfg: &Config) {
    let handles: Vec<_> = cfg
        .pucks
        .iter()
        .map(|p| {
            let (r, g, b, name) = fleet::slot_led_rgb(p.device);
            println!("{:15} dev{} blinking {}", p.ip, p.device, name);
            let ip = p.ip.clone();
            std::thread::spawn(move || fleet::blink(&ip, r, g, b, 6))
        })
        .collect();
    for h in handles {
        h.join().ok();
    }
    println!("LEDs restored");
}

fn status(cfg: &Config) {
    for p in &cfg.pucks {
        let s = fleet::status(&p.ip);
        if !s.reachable {
            println!("{:15} UNREACHABLE (adb connect {}:5555?)", p.ip, p.ip);
            continue;
        }
        println!(
            "{:15} dev{} {}{} batt {:3}%  tracker {}  guardian-off {}{}",
            p.ip,
            p.device,
            s.tracking,
            if s.tracking_valid { "" } else { " INVALID" },
            s.battery_pct,
            if s.tracker_running { "up" } else { "DOWN" },
            s.guardian_disabled,
            if s.vpn_trap { "  VPN-TRAP: packets are being eaten" } else { "" },
        );
    }
}

fn up(cfg: &Config) {
    let port: u16 = cfg.listen.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(5180);
    for p in &cfg.pucks {
        print!("{:15} ", p.ip);
        if !fleet::connect(&p.ip).unwrap_or(false) {
            println!("adb connect failed");
            continue;
        }
        // Write the puck's SOURCE byte: its id once provisioned, else its role.
        let src = p.id.unwrap_or(p.device);
        match fleet::configure_tracker(&p.ip, &cfg.host, port, src) {
            Ok(()) => println!("tracker configured (src={src}, role={}) and launched", p.device),
            Err(e) => println!("configure failed: {e}"),
        }
    }
    println!("give the XR sessions ~10 s, then `insight-map-loader bridge`");
}

fn bridge_cmd(cfg: &Config) -> i32 {
    let ingest = match Ingest::bind(&cfg.listen, Duration::from_millis(500)) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("cannot listen on {}: {e}", cfg.listen);
            return 1;
        }
    };
    println!("waiting for tracker streams on {}...", cfg.listen);
    std::thread::sleep(Duration::from_secs(2));

    let mut out = BTreeMap::new();
    for p in &cfg.pucks {
        // Bridging pairs this puck's OWN stream with its own dumpsys pose, so
        // it keys on the source byte, not the role it publishes under.
        let src = p.id.unwrap_or(p.device);
        print!("{:15} pairing dumpsys with MPT1 (hold the puck STILL)... ", p.ip);
        let mut pairs = Vec::new();
        for _ in 0..10 {
            let Some(world) = fleet::dumpsys_pose(&p.ip) else { continue };
            let Some(s) = ingest.sample(src) else { continue };
            // The dumpsys read takes ~300 ms; only pair it with an MPT1 sample
            // that is current, so the two describe the same instant.
            if !s.packet.valid || s.age() > Duration::from_millis(120) {
                continue;
            }
            pairs.push(PosePair { world, local: s.packet.pose });
        }
        match bridge::solve(&pairs) {
            Some(sol) => {
                println!(
                    "{} pairs, yaw {:+.2}°, spread {:.2}° / {:.0} mm{}",
                    sol.pairs,
                    sol.transform.yaw.to_degrees(),
                    sol.yaw_spread_deg,
                    sol.t_spread_m * 1000.0,
                    if sol.yaw_spread_deg > 2.0 { "  UNSTABLE — was it moving?" } else { "" }
                );
                out.insert(
                    p.ip.clone(),
                    serde_json::json!({
                        "yaw_deg": sol.transform.yaw.to_degrees(),
                        "t": sol.transform.t,
                        "yaw_spread_deg": sol.yaw_spread_deg,
                        "t_spread_m": sol.t_spread_m,
                        "unix_time": SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
                    }),
                );
            }
            None => {
                println!("FAILED — {} usable pairs (tracker up? 6DOF?)", pairs.len());
                return 1;
            }
        }
    }
    std::fs::write(&cfg.bridge, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    println!("wrote {}", cfg.bridge);
    0
}

fn run(cfg: &Config) -> i32 {
    let service = match insight_map_loader_core::service::Service::start(cfg.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!("service up → {} (bridging is automatic; watching for drift)", cfg.out);
    let mut seen_events = 0usize;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let v = service.view();
        // Events land as their own lines so they survive the status line.
        if v.events.len() > seen_events || v.events.len() < seen_events {
            for e in v.events.iter().skip(seen_events.min(v.events.len())) {
                println!("\n[{e}]");
            }
            seen_events = v.events.len();
        }
        let mut line = format!("
{:>7} pkts out", v.emitted);
        for (d, p) in &v.live {
            line += &format!("  {}:({:+.2},{:+.2},{:+.2})", d.label(), p[0], p[1], p[2]);
        }
        // A puck streaming valid MPT1 that no config entry claims. Without this
        // it simply does not appear, and "my puck vanished" sends someone after
        // a network fault that is not there -- the GUI says so, and so must this.
        if !v.unknown_sources.is_empty() {
            let list = v.unknown_sources.iter()
                .map(|x| x.to_string()).collect::<Vec<_>>().join(", ");
            line += &format!("  [UNCLAIMED id {list} — add it to the config]");
        }
        for (d, st, _, _) in &v.slots {
            match st {
                SlotState::NotTracking => line += &format!("  {}:NO-TRACK", d.label()),
                SlotState::Stale => line += &format!("  {}:STALE", d.label()),
                _ => {}
            }
        }
        if let Some(s) = v.sep {
            line += &format!("  sep {s:.3} m");
        }
        if v.drifted {
            line += "  [DRIFTED]";
        }
        print!("{line}   ");
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
}

