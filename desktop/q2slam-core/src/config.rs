//! Configuration and the on-disk transform files.
//!
//! Three JSON files, one owner each:
//! * `q2slam.json` — the site config: puck IPs, slots, addresses. Human-edited.
//! * `align_result.json` — the inter-puck alignment, written by the solvers
//!   (tools/align_pool.py, tools/align_map.py). Reloaded on change.
//! * `bridge.json` — each puck's XR-LOCAL→Insight-world transform for the
//!   current tracker session, written by `q2slam bridge` (or the GUI).

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
    #[serde(default = "d_align")]
    pub align: String,
    /// Per-puck map transforms (written by q1mapd, watched by the service).
    #[serde(default = "d_transforms")]
    pub transforms: String,
    /// The landmark map directory q1mapd owns.
    #[serde(default = "d_map")]
    pub map: String,
    #[serde(default = "d_bridge")]
    pub bridge: String,
    /// The host address the pucks stream to; they need it spelled out.
    pub host: String,
    /// Run the capture+re-solve pipeline automatically when the drift monitor
    /// fires. Off by default: it records the cameras for `realign_secs` and
    /// takes minutes to solve, which should be a choice, not a surprise.
    #[serde(default)]
    pub auto_realign: bool,
    #[serde(default = "d_realign_secs")]
    pub realign_secs: u32,
    /// **Native colocation.** When the pucks have been given a shared Insight
    /// map (see docs/insight-mapdata-format.md), they already track in ONE
    /// world frame, so there is no `T_map_world` to solve — it is identity by
    /// construction. Setting this makes the service stop solving, storing and
    /// applying per-puck map transforms; only the LOCAL→world bridge remains,
    /// because MPT1 still streams poses in the tracker's LOCAL frame.
    ///
    /// This is the preferred mode. A stored transform is exactly what went
    /// stale on every Insight relocalization and produced the "aligned, then
    /// split apart" failures; with a shared map there is nothing to go stale.
    #[serde(default)]
    pub colocated: bool,
    pub pucks: Vec<PuckCfg>,
}

fn d_listen() -> String { "0.0.0.0:5180".into() }
fn d_out() -> String { "127.0.0.1:5181".into() }
fn d_align() -> String { "align_result.json".into() }
fn d_transforms() -> String { "transforms.json".into() }
fn d_map() -> String { "map".into() }
fn d_bridge() -> String { "bridge.json".into() }
fn d_realign_secs() -> u32 { 25 }

#[derive(Deserialize, Clone)]
pub struct PuckCfg {
    pub ip: String,
    pub device: u8,
    /// Legacy flag from the old two-puck alignment. Kept so existing configs
    /// still parse; `role` is what the hip-referenced path reads.
    #[serde(default)]
    pub reference: bool,
    /// "hip" | "ankle" (default). The hip holds the reference map and IS the
    /// shared frame -- its transform is identity by definition; ankles localize
    /// into it (docs/on-device-alignment.md). A config with no hip falls back
    /// to `reference`, then to the first puck.
    #[serde(default)]
    pub role: Option<String>,
}

impl PuckCfg {
    pub fn is_hip(&self) -> bool {
        self.role.as_deref() == Some("hip")
    }
}

impl Config {
    /// The puck whose Insight frame is the shared frame.
    pub fn hip(&self) -> Option<&PuckCfg> {
        self.pucks.iter().find(|p| p.is_hip())
            .or_else(|| self.pucks.iter().find(|p| p.reference))
            .or_else(|| self.pucks.first())
    }
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

#[derive(Deserialize)]
struct AlignFile {
    yaw_deg: f32,
    translation: [f32; 3],
}

/// The solved inter-frame alignment (world of the non-reference puck → world
/// of the reference puck).
pub fn load_align(path: &str) -> Option<Frame4Dof> {
    let f: AlignFile = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(Frame4Dof { yaw: f.yaw_deg.to_radians(), t: f.translation })
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

#[derive(Deserialize)]
pub struct MapTransform {
    pub yaw_deg: f32,
    pub t: [f32; 3],
    #[serde(default)]
    pub unix_time: u64,
    /// Which frame this transform maps FROM. Default (None) is the Insight
    /// world frame (host localize; composed with the LOCAL bridge). "local"
    /// means the tracker app's LOCAL frame directly -- the pair stream solves
    /// against the MPT1 stream itself, so its result already contains the
    /// bridge and is applied as-is. Composing the bridge on top of a "local"
    /// entry applies it twice (measured: both pucks displaced differently,
    /// visibly misaligned).
    #[serde(default)]
    pub frame: Option<String>,
}

/// The per-puck map transforms written by q1mapd (or the migration seed).
pub fn load_map_transforms(path: &str) -> Option<BTreeMap<String, MapTransform>> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Per-slot LOCAL→map transforms: each puck's session bridge composed with its
/// own T_map_world. There is deliberately NO reference puck any more — the map
/// is the reference, every puck (the founder included) is placed by its store
/// entry, and a puck missing either its bridge or its map transform is not
/// emitted at all: an unaligned pose in an aligned stream is worse than a
/// missing one.
pub fn build_transforms(
    cfg: &Config,
    map_t: &BTreeMap<String, MapTransform>,
    bridges: &BTreeMap<String, BridgeEntry>,
) -> BTreeMap<Device, Frame4Dof> {
    build_transforms_for(&cfg.pucks, cfg.colocated, map_t, bridges)
}

/// As `build_transforms`, but over an explicit roster.
///
/// The service uses this so a role change takes effect without a restart: with
/// a stale roster the output is keyed by the OLD slot, the aggregator finds no
/// transform for the new one, and the puck silently vanishes from SteamVR.
pub fn build_transforms_for(
    pucks: &[PuckCfg],
    colocated: bool,
    map_t: &BTreeMap<String, MapTransform>,
    bridges: &BTreeMap<String, BridgeEntry>,
) -> BTreeMap<Device, Frame4Dof> {
    let mut out = BTreeMap::new();
    for p in pucks {
        let (Some(device), Some(b)) = (Device::from_u8(p.device), bridges.get(&p.ip))
        else {
            continue;
        };
        // Natively colocated: every puck shares one Insight world frame, so
        // T_map_world is identity for ALL of them and stored map transforms are
        // ignored outright (not merely unused -- a stale entry left in
        // transforms.json must not silently reappear in the output). The bridge
        // still applies: MPT1 poses are in the tracker's LOCAL frame.
        if colocated {
            let local_to_world = Frame4Dof { yaw: b.yaw_deg.to_radians(), t: b.t };
            out.insert(device, Frame4Dof::IDENTITY.compose(&local_to_world));
            continue;
        }
        // The hip IS the shared frame, so its T_map_world is identity by
        // definition and it never localizes -- without this it would have no
        // store entry and be dropped from the output entirely. A stored entry
        // still wins, so a hip that was aligned some other way is honoured.
        match map_t.get(&p.ip) {
            // Pair-stream entries map LOCAL -> map directly: no bridge.
            Some(m) if m.frame.as_deref() == Some("local") => {
                out.insert(device, Frame4Dof { yaw: m.yaw_deg.to_radians(), t: m.t });
            }
            Some(m) => {
                let t_map_world = Frame4Dof { yaw: m.yaw_deg.to_radians(), t: m.t };
                let local_to_world = Frame4Dof { yaw: b.yaw_deg.to_radians(), t: b.t };
                out.insert(device, t_map_world.compose(&local_to_world));
            }
            None if p.is_hip() => {
                let local_to_world = Frame4Dof { yaw: b.yaw_deg.to_radians(), t: b.t };
                out.insert(device, Frame4Dof::IDENTITY.compose(&local_to_world));
            }
            None => continue,
        }
    }
    out
}

#[cfg(test)]
mod hip_tests {
    use super::*;

    fn cfg_json(roles: &str) -> Config {
        serde_json::from_str(&format!(
            r#"{{"host":"h","listen":"0.0.0.0:1","out":"127.0.0.1:2",
                 "align":"a","bridge":"b","pucks":[{roles}]}}"#)).unwrap()
    }

    #[test]
    fn hip_emits_identity_without_a_store_entry() {
        let cfg = cfg_json(
            r#"{"ip":"1.1.1.1","device":0,"role":"hip"},
               {"ip":"2.2.2.2","device":1,"role":"ankle"}"#);
        assert_eq!(cfg.hip().unwrap().ip, "1.1.1.1");

        let mut bridges = BTreeMap::new();
        for ip in ["1.1.1.1", "2.2.2.2"] {
            bridges.insert(ip.to_string(), BridgeEntry {
                yaw_deg: 0.0, t: [0.0; 3], yaw_spread_deg: 0.0, unix_time: 0 });
        }
        // No map transforms at all: the hip still comes out (identity), the
        // ankle does not (it has nothing to place it in the hip's frame).
        let out = build_transforms(&cfg, &BTreeMap::new(), &bridges);
        assert!(out.contains_key(&Device::from_u8(0).unwrap()));
        assert!(!out.contains_key(&Device::from_u8(1).unwrap()));
    }

    #[test]
    fn legacy_reference_flag_still_selects_the_hip() {
        let cfg = cfg_json(r#"{"ip":"9.9.9.9","device":0,"reference":true}"#);
        assert_eq!(cfg.hip().unwrap().ip, "9.9.9.9");
    }

    fn colocated_cfg(roles: &str) -> Config {
        serde_json::from_str(&format!(
            r#"{{"host":"h","listen":"0.0.0.0:1","out":"127.0.0.1:2",
                 "align":"a","bridge":"b","colocated":true,"pucks":[{roles}]}}"#)).unwrap()
    }

    #[test]
    fn colocated_ignores_stored_map_transforms_and_emits_every_puck() {
        let cfg = colocated_cfg(
            r#"{"ip":"1.1.1.1","device":0,"role":"hip"},
               {"ip":"2.2.2.2","device":1,"role":"ankle"}"#);
        let mut bridges = BTreeMap::new();
        for ip in ["1.1.1.1", "2.2.2.2"] {
            bridges.insert(ip.to_string(), BridgeEntry {
                yaw_deg: 0.0, t: [0.0; 3], yaw_spread_deg: 0.0, unix_time: 0 });
        }
        // A stale stored transform must NOT leak into the output: with a shared
        // map the frame is already right, so applying this would break it.
        let mut map_t = BTreeMap::new();
        map_t.insert("2.2.2.2".to_string(), MapTransform {
            yaw_deg: 90.0, t: [5.0, 0.0, -3.0], frame: None, unix_time: 1 });

        let out = build_transforms(&cfg, &map_t, &bridges);
        // Both pucks present, and the ankle's 90 deg / 5 m entry ignored.
        assert_eq!(out.len(), 2);
        let ankle = out[&Device::from_u8(1).unwrap()];
        assert_eq!(ankle, Frame4Dof::IDENTITY, "stored transform leaked into colocated output");
    }

    #[test]
    fn colocated_still_applies_the_local_to_world_bridge() {
        // MPT1 streams the tracker's LOCAL frame, so the bridge is still
        // required even when there is no map transform.
        let cfg = colocated_cfg(r#"{"ip":"1.1.1.1","device":0,"role":"hip"}"#);
        let mut bridges = BTreeMap::new();
        bridges.insert("1.1.1.1".to_string(), BridgeEntry {
            yaw_deg: 30.0, t: [1.0, 0.0, 2.0], yaw_spread_deg: 0.0, unix_time: 0 });
        let out = build_transforms(&cfg, &BTreeMap::new(), &bridges);
        let f = out[&Device::from_u8(0).unwrap()];
        assert!((f.yaw - 30f32.to_radians()).abs() < 1e-6);
        assert_eq!(f.t, [1.0, 0.0, 2.0]);
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;

    fn tmpfile(name: &str, body: &str) -> String {
        let p = std::env::temp_dir().join(format!("q2slam_cfgtest_{name}.json"));
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().into_owned()
    }

    const SAMPLE: &str = r#"{
  "host": "192.168.1.2",
  "listen": "0.0.0.0:5180",
  "pucks": [
    { "ip": "1.1.1.1", "device": 0, "role": "hip", "note": "hand-written" },
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
        assert!(cfg.pucks[0].is_hip(), "alignment role must survive a device change");
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
