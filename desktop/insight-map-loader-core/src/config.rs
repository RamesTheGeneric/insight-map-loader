//! Configuration and the on-disk transform files.
//!
//! Three JSON files, one owner each:
//! * `insight-map-loader.json` — the site config: puck IPs, slots, addresses. Human-edited.
//! * `align_result.json` — the inter-puck alignment, written by the solvers
//!   (tools/align_pool.py, tools/align_map.py). Reloaded on change.
//! * `bridge.json` — each puck's XR-LOCAL→Insight-world transform for the
//!   current tracker session, written by `insight-map-loader bridge` (or the GUI).

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::mpt1::Device;
use crate::transform::Frame4Dof;

#[derive(Deserialize, Clone)]
pub struct Config {
    #[serde(default = "d_listen")]
    pub listen: String,
    #[serde(default = "d_out")]
    pub out: String,
    #[serde(default = "d_bridge")]
    pub bridge: String,
    /// The host address the pucks stream to; they need it spelled out.
    pub host: String,
    pub pucks: Vec<PuckCfg>,
}

fn d_listen() -> String { "0.0.0.0:5180".into() }
fn d_out() -> String { "127.0.0.1:5181".into() }
fn d_bridge() -> String { "bridge.json".into() }

#[derive(Deserialize, Clone)]
pub struct PuckCfg {
    pub ip: String,
    pub device: u8,
}

impl Config {
    pub fn load(path: &str) -> Result<Config, String> {
        let s = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let cfg: Config = serde_json::from_str(&s).map_err(|e| format!("{path}: {e}"))?;
        cfg.validate().map_err(|e| format!("{path}: {e}"))?;
        Ok(cfg)
    }

    /// Reject configurations that would fail silently at runtime.
    ///
    /// Duplicate `device` ids are the dangerous one: the ingest keeps only the
    /// newest sample per slot, so two pucks sharing an id fight at packet rate
    /// and the SteamVR tracker flickers between two bodies -- with nothing
    /// anywhere reporting a problem.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen: BTreeMap<u8, &str> = BTreeMap::new();
        for p in &self.pucks {
            if let Some(prev) = seen.insert(p.device, &p.ip) {
                return Err(format!(
                    "pucks {prev} and {} both use device {} -- ids must be unique, \
                     they select the SteamVR tracker",
                    p.ip, p.device
                ));
            }
        }
        Ok(())
    }
}

/// Surgically set one puck's `device` (its SteamVR role) in the config file.
///
/// Deliberately edits the JSON as a `Value` rather than re-serializing a
/// `Config`: the struct does not model every key a human may have put there,
/// and a round trip through it would drop them. Written atomically
/// (tmp + fsync + rename) so a watcher never sees a partial file, matching the
/// pattern service.rs uses for transforms.json.
pub fn set_puck_device(path: &str, ip: &str, device: u8) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut root: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;

    let pucks = root
        .get_mut("pucks")
        .and_then(|p| p.as_array_mut())
        .ok_or_else(|| format!("{path}: no \"pucks\" array"))?;

    // Refuse a duplicate here too: this is the write path, and letting it
    // through would produce a file that Config::load then rejects on restart.
    for p in pucks.iter() {
        let other_ip = p.get("ip").and_then(|v| v.as_str()).unwrap_or("");
        let other_dev = p.get("device").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
        if other_ip != ip && other_dev == device as u64 {
            return Err(format!("device {device} is already assigned to {other_ip}"));
        }
    }

    let entry = pucks
        .iter_mut()
        .find(|p| p.get("ip").and_then(|v| v.as_str()) == Some(ip))
        .ok_or_else(|| format!("{path}: no puck with ip {ip}"))?;
    entry["device"] = serde_json::Value::from(device);

    // One backup per process, taken before the first mutation -- enough to
    // recover a hand-maintained file from a bad edit without accumulating
    // clutter on every role change.
    static BACKED_UP: std::sync::Once = std::sync::Once::new();
    BACKED_UP.call_once(|| {
        std::fs::copy(path, format!("{path}.bak")).ok();
    });

    let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    let tmp = format!("{path}.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("{tmp}: {e}"))?;
        f.write_all(pretty.as_bytes()).map_err(|e| format!("{tmp}: {e}"))?;
        f.write_all(b"\n").map_err(|e| format!("{tmp}: {e}"))?;
        // Without this a crash between write and rename can expose a
        // zero-length file on some filesystems -- for a site config, worth it.
        f.sync_all().map_err(|e| format!("{tmp}: {e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{path}: {e}"))
}

#[derive(Deserialize, Clone)]
pub struct BridgeEntry {
    pub yaw_deg: f32,
    pub t: [f32; 3],
    #[serde(default)]
    pub yaw_spread_deg: f32,
    #[serde(default)]
    pub unix_time: u64,
}

pub fn load_bridges(path: &str) -> Option<BTreeMap<String, BridgeEntry>> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Per-slot LOCAL→map transforms: each puck's session bridge composed with its
/// own T_map_world. There is deliberately NO reference puck any more — the map
/// is the reference, every puck (the founder included) is placed by its store
/// entry, and a puck missing either its bridge or its map transform is not
/// emitted at all: an unaligned pose in an aligned stream is worse than a
/// missing one.
/// As `build_transforms`, but over an explicit roster.
///
/// The service uses this so a role change takes effect without a restart: with
/// a stale roster the output is keyed by the OLD slot, the aggregator finds no
/// transform for the new one, and the puck silently vanishes from SteamVR.
/// Per-device output transform: **identity composed with each puck's
/// LOCAL→world bridge**.
///
/// There is no `T_map_world` term. The pucks share one Insight map, so their
/// world frames already coincide — a stored per-puck transform is exactly what
/// used to go stale on every relocalization. The bridge remains because MPT1
/// streams the tracker's OpenXR LOCAL frame, not the Insight world frame.
///
/// Takes an explicit roster so a role change applies without a restart: with a
/// stale one the output is keyed by the OLD slot, the aggregator finds no
/// transform for the new one, and the puck silently vanishes from SteamVR.
pub fn build_transforms_for(
    pucks: &[PuckCfg],
    bridges: &BTreeMap<String, BridgeEntry>,
) -> BTreeMap<Device, Frame4Dof> {
    let mut out = BTreeMap::new();
    for p in pucks {
        let (Some(device), Some(b)) = (Device::from_u8(p.device), bridges.get(&p.ip)) else {
            continue;
        };
        out.insert(device, Frame4Dof { yaw: b.yaw_deg.to_radians(), t: b.t });
    }
    out
}

#[cfg(test)]
mod transform_tests {
    use super::*;

    fn cfg_of(pucks: &str) -> Config {
        serde_json::from_str(&format!(
            r#"{{"host":"h","listen":"0.0.0.0:1","out":"127.0.0.1:2",
                 "bridge":"b","pucks":[{pucks}]}}"#)).unwrap()
    }

    fn bridge(yaw_deg: f32, t: [f32; 3]) -> BridgeEntry {
        BridgeEntry { yaw_deg, t, yaw_spread_deg: 0.0, unix_time: 0 }
    }

    #[test]
    fn output_is_the_bridge_alone() {
        // No T_map_world term exists: the pucks share one map, so the only
        // transform is each puck's LOCAL->world bridge.
        let cfg = cfg_of(r#"{"ip":"1.1.1.1","device":0}"#);
        let mut bridges = BTreeMap::new();
        bridges.insert("1.1.1.1".to_string(), bridge(30.0, [1.0, 0.0, 2.0]));
        let out = build_transforms_for(&cfg.pucks, &bridges);
        let f = out[&Device::from_u8(0).unwrap()];
        assert!((f.yaw - 30f32.to_radians()).abs() < 1e-6);
        assert_eq!(f.t, [1.0, 0.0, 2.0]);
    }

    #[test]
    fn a_puck_without_a_bridge_is_omitted() {
        // Emitting it un-bridged would place it in a frame nothing shares.
        // A missing limb beats a wrong one.
        let cfg = cfg_of(r#"{"ip":"1.1.1.1","device":0},{"ip":"2.2.2.2","device":1}"#);
        let mut bridges = BTreeMap::new();
        bridges.insert("1.1.1.1".to_string(), bridge(0.0, [0.0; 3]));
        let out = build_transforms_for(&cfg.pucks, &bridges);
        assert!(out.contains_key(&Device::from_u8(0).unwrap()));
        assert!(!out.contains_key(&Device::from_u8(1).unwrap()));
    }

    #[test]
    fn an_unknown_role_id_is_omitted_not_panicked() {
        let cfg = cfg_of(r#"{"ip":"1.1.1.1","device":99}"#);
        let mut bridges = BTreeMap::new();
        bridges.insert("1.1.1.1".to_string(), bridge(0.0, [0.0; 3]));
        assert!(build_transforms_for(&cfg.pucks, &bridges).is_empty());
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;

    fn tmpfile(name: &str, body: &str) -> String {
        let p = std::env::temp_dir().join(format!("insight_map_loader_cfgtest_{name}.json"));
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().into_owned()
    }

    const SAMPLE: &str = r#"{
  "host": "192.168.1.2",
  "listen": "0.0.0.0:5180",
  "pucks": [
    { "ip": "1.1.1.1", "device": 0, "note": "hand-written" },
    { "ip": "2.2.2.2", "device": 1 }
  ],
  "colocated": true,
  "some_future_key": { "nested": [1, 2, 3] }
}"#;

    #[test]
    fn set_puck_device_preserves_unknown_keys_and_order() {
        let path = tmpfile("preserve", SAMPLE);
        set_puck_device(&path, "2.2.2.2", 6).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();

        // The key a human added, and the one this struct does not model, both
        // survive -- that is the whole reason for the Value-level edit.
        assert!(out.contains("some_future_key"), "unknown top-level key was dropped");
        assert!(out.contains("hand-written"), "unknown per-puck key was dropped");

        // Order is preserved (needs serde_json/preserve_order), so a
        // hand-maintained file is not visibly scrambled by a role change.
        let host = out.find("\"host\"").unwrap();
        let listen = out.find("\"listen\"").unwrap();
        let pucks = out.find("\"pucks\"").unwrap();
        assert!(host < listen && listen < pucks, "key order changed: {out}");

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.pucks[1].device, 6);
        assert_eq!(cfg.pucks[0].device, 0, "the other puck must be untouched");
    }

    #[test]
    fn set_puck_device_refuses_a_duplicate() {
        let path = tmpfile("dup", SAMPLE);
        let err = set_puck_device(&path, "2.2.2.2", 0).unwrap_err();
        assert!(err.contains("already assigned"), "{err}");
        // And the file is untouched, not half-written.
        assert_eq!(Config::load(&path).unwrap().pucks[1].device, 1);
    }

    #[test]
    fn set_puck_device_refuses_an_unknown_puck() {
        let path = tmpfile("unknown", SAMPLE);
        assert!(set_puck_device(&path, "9.9.9.9", 4).is_err(), "must not invent a puck");
    }

    #[test]
    fn load_rejects_duplicate_devices() {
        let path = tmpfile(
            "loaddup",
            r#"{"host":"h","pucks":[{"ip":"1.1.1.1","device":2},{"ip":"2.2.2.2","device":2}]}"#,
        );
        let err = match Config::load(&path) {
            Err(e) => e,
            Ok(_) => panic!("duplicate device ids must be rejected"),
        };
        assert!(err.contains("both use device 2"), "{err}");
    }
}
