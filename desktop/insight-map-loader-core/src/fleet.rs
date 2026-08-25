//! Managing the pucks over adb.
//!
//! Everything here shells out to `adb` — the pucks are rooted Quest 1s on
//! wifi adb, and the host drives them (nothing on-device survives a reboot as
//! root). Two rules learned the hard way and honoured throughout:
//!
//! * Never pipe `dumpsys tracking` into a head/grep that closes the pipe
//!   early — that leaves the tracking service unavailable for many seconds
//!   ("Can't find service: tracking"). Dump to a file on-device, grep the file.
//! * Batch device queries into ONE shell round trip; each costs 50–150 ms.

use std::process::Command;
use std::time::Duration;

use crate::mpt1::Pose;

const TRACKER_PKG: &str = "com.mapperlocalizer.questtracker";


/// Every adb call is wrapped in `timeout`. std's Command has no deadline, and
/// a single hung adb wedges its caller forever: a launch-style `adb shell`
/// (setsid ... &) was measured stuck for over an HOUR, which blocked the whole
/// verify-poll thread -- so stall detection and the on-device feedback loop
/// silently stopped while every puck still looked healthy. A bounded call that
/// occasionally returns empty is strictly better than one that never returns.
fn adb_timeout(ip: &str, args: &[&str], secs: u32) -> std::io::Result<String> {
    let serial = format!("{ip}:5555");
    let out = Command::new("timeout")
        .arg(secs.to_string())
        .arg("adb")
        .arg("-s")
        .arg(&serial)
        .args(args)
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn adb(ip: &str, args: &[&str]) -> std::io::Result<String> {
    adb_timeout(ip, args, 15)
}

fn shell(ip: &str, cmd: &str) -> std::io::Result<String> {
    adb(ip, &["shell", cmd])
}

// ---------------------------------------------------------------- checked adb
//
// `adb_timeout` above deliberately swallows exit status and stderr: its callers
// are watchdogs where "occasionally empty" beats "never returns", and that
// tradeoff is load-bearing (see its doc comment). But the map operations below
// are destructive and multi-step -- a failure at step 6 of 9 must not be
// indistinguishable from success. So this is a PARALLEL path, not a change to
// the old one. Do not "unify" them.

/// A failure from a checked adb call, with enough detail to show a user.
#[derive(Debug)]
pub enum FleetError {
    /// `timeout`/`adb` could not be spawned at all.
    Spawn(std::io::Error),
    /// The `timeout` wrapper killed the call (coreutils exits 124).
    Timeout { secs: u32 },
    /// adb itself failed on the host side.
    Adb { code: i32, stderr: String },
    /// The command ran on the puck and exited non-zero.
    Remote { code: i32, out: String },
    /// A precondition we refuse to proceed without.
    Precondition(String),
}

impl std::fmt::Display for FleetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FleetError::Spawn(e) => write!(f, "could not run adb: {e}"),
            FleetError::Timeout { secs } => write!(f, "adb timed out after {secs}s"),
            FleetError::Adb { code, stderr } => {
                write!(f, "adb failed (exit {code}): {}", stderr.trim())
            }
            FleetError::Remote { code, out } => {
                write!(f, "command failed on puck (exit {code}): {}", out.trim())
            }
            FleetError::Precondition(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for FleetError {}

/// adb with the exit status and stderr kept: stdout on success, and on failure
/// an error carrying the real reason instead of an empty string.
fn adb_raw(ip: &str, args: &[&str], secs: u32) -> Result<String, FleetError> {
    let serial = format!("{ip}:5555");
    let out = Command::new("timeout")
        .arg(secs.to_string())
        .arg("adb")
        .arg("-s")
        .arg(&serial)
        .args(args)
        .output()
        .map_err(FleetError::Spawn)?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    match out.status.code() {
        Some(0) => Ok(stdout),
        // coreutils `timeout` reports 124 when it had to kill the child.
        Some(124) => Err(FleetError::Timeout { secs }),
        Some(code) => Err(FleetError::Adb { code, stderr }),
        None => Err(FleetError::Timeout { secs }),
    }
}

/// `shell`, but the REMOTE exit status is real.
///
/// `adb shell` exit-code propagation is unreliable on this adbd (it reports the
/// transport's status, not the command's), so the status is carried back in
/// band on a trailer line and parsed off. Every destructive step below depends
/// on knowing that a `chcon` or a `cp` actually failed.
fn shell_checked(ip: &str, cmd: &str, secs: u32) -> Result<String, FleetError> {
    let wrapped = format!("{cmd}; echo \"@@RC:$?\"");
    let text = adb_raw(ip, &["shell", &wrapped], secs)?;
    let Some(idx) = text.rfind("@@RC:") else {
        return Err(FleetError::Remote {
            code: -1,
            out: format!("no status trailer; output was: {}", text.trim()),
        });
    };
    let code: i32 = text[idx + 5..].trim().parse().unwrap_or(-1);
    let body = text[..idx].to_string();
    if code == 0 {
        Ok(body)
    } else {
        Err(FleetError::Remote { code, out: body })
    }
}

/// Re-acquire adb root. A REBOOT drops adbd back to the `shell` user, and the
/// path that needs it must do this first or it fails SILENTLY -- setprop,
/// chown and chcon all appear to succeed and do nothing, so the breakage
/// surfaces anywhere except at root. Cheap and idempotent when already root.
pub fn ensure_root(ip: &str) -> bool {
    if shell(ip, "id -u").map_or(false, |o| o.trim() == "0") {
        return true;
    }
    Command::new("adb").arg("-s").arg(format!("{ip}:5555")).arg("root").output().ok();
    std::thread::sleep(Duration::from_millis(1500));
    connect(ip).ok();
    std::thread::sleep(Duration::from_millis(500));
    shell(ip, "id -u").map_or(false, |o| o.trim() == "0")
}

pub fn connect(ip: &str) -> std::io::Result<bool> {
    let out = Command::new("adb").arg("connect").arg(format!("{ip}:5555")).output()?;
    let s = String::from_utf8_lossy(&out.stdout);
    Ok(s.contains("connected"))
}

#[derive(Debug, Default, Clone)]
pub struct PuckStatus {
    pub reachable: bool,
    /// e.g. "6DOF" — what Insight itself reports.
    pub tracking: String,
    pub tracking_valid: bool,
    pub battery_pct: i32,
    pub tracker_running: bool,
    pub guardian_disabled: bool,
    /// An app-root kill-switch VPN eats every packet while the tracker looks
    /// perfectly healthy. Surfaced first-class because it looks exactly like a
    /// tracking failure and is not one.
    pub vpn_trap: bool,
    /// Short root-node uuid of the puck's current Insight map context. Pucks
    /// sharing a map report the SAME root — that is what colocation looks like
    /// from the outside, and it is the field the UI compares across pucks.
    pub map_root: String,
    /// Whether that context is persistent. Only persistent contexts are written
    /// to /vision/insideout/mapdb, and only a loaded persistent map colocates.
    pub map_persistent: bool,
}

/// Whole-headset status in one shell round trip.
/// The headset pose in its Insight WORLD frame, from dumpsys. ~300 ms round
/// trip and 2-decimal precision — this is the bridge's world-side observation,
/// not a pose source for tracking (the MPT1 stream is that).
pub fn dumpsys_pose(ip: &str) -> Option<Pose> {
    let cmd = "dumpsys tracking > /data/local/tmp/q2p.txt 2>/dev/null; \
               grep -A4 '  Hmd:' /data/local/tmp/q2p.txt";
    let out = shell(ip, cmd).ok()?;
    if !out.contains("6DOF") || !out.contains("Valid: Yes") {
        return None;
    }
    let rot = floats_after(&out, "rot=(", 4)?;
    let tr = floats_after(&out, "trans=(", 3)?;
    // dumpsys rot=() is (x,y,z,w) — same order every pose CSV in this repo uses.
    Some(Pose { p: [tr[0], tr[1], tr[2]], q: [rot[0], rot[1], rot[2], rot[3]] })
}

pub fn status(ip: &str) -> PuckStatus {
    let cmd = concat!(
        "dumpsys tracking > /data/local/tmp/q2s.txt 2>/dev/null; ",
        "grep -m1 'Tracking Level' /data/local/tmp/q2s.txt; echo '@@'; ",
        "pidof com.mapperlocalizer.questtracker; echo '@@'; ",
        "dumpsys battery | grep -m1 level:; echo '@@'; ",
        "getprop persist.oculus.guardian_disable; echo '@@'; ",
        "ip route 2>/dev/null | grep -c tun0; echo '@@'; ",
        "grep -m1 'Vega Map Context' /data/local/tmp/q2s.txt"
    );
    let Ok(out) = shell(ip, cmd) else { return PuckStatus::default() };
    if out.trim().is_empty() {
        return PuckStatus::default();
    }
    let parts: Vec<&str> = out.split("@@").collect();
    let mut st = PuckStatus { reachable: true, ..Default::default() };
    if let Some(p) = parts.first() {
        if let Some(i) = p.find("Tracking Level:") {
            st.tracking = p[i + 15..].trim().split_whitespace().next().unwrap_or("").into();
        }
        st.tracking_valid = p.contains("Valid: Yes");
    }
    st.tracker_running = parts.get(1).map_or(false, |p| !p.trim().is_empty());
    if let Some(p) = parts.get(2) {
        if let Some(i) = p.find("level:") {
            st.battery_pct = p[i + 6..].trim().parse().unwrap_or(0);
        }
    }
    st.guardian_disabled = parts.get(3).map_or(false, |p| p.trim() == "1");
    st.vpn_trap = parts.get(4).map_or(false, |p| p.trim() != "0" && !p.trim().is_empty());
    // "Vega Map Context: topNodeUid <uuid>, timeCount = N (persistent|transient)"
    //
    // The FULL uuid is kept: it is the success criterion for a map transplant,
    // and 8 hex characters is a display convenience, not an identity. Truncate
    // with `short_root()` at the point of display.
    if let Some(p) = parts.get(5) {
        if let Some(i) = p.find("topNodeUid ") {
            let rest = &p[i + 11..];
            let uuid = rest.split(|c: char| c == ',' || c.is_whitespace()).next().unwrap_or("");
            st.map_root = uuid.to_string();
        }
        st.map_persistent = p.contains("(persistent)");
    }
    st
}

fn floats_after(s: &str, tag: &str, n: usize) -> Option<Vec<f32>> {
    let start = s.find(tag)? + tag.len();
    let end = s[start..].find(')')? + start;
    let v: Vec<f32> = s[start..end]
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect();
    (v.len() == n).then_some(v)
}

/// Blink the front status LED so a physical headset can be matched to its
/// slot. The Quest 1 front indicator is a PMI8998 tri-colour LED at
/// /sys/class/leds/{red,green,blue}; the light HAL reasserts its own colour on
/// power events but not continuously, so a ~2 Hz rewrite loop dominates it.
/// The whole loop runs in ONE on-device shell so it finishes and restores the
/// original colour even if adb drops mid-blink, and a lock file makes an
/// overlapping blink inherit the original saved colour instead of
/// "restoring" the first blink's off-phase. (Port of tools/q1blink.sh.)
///
/// Blocks for ~`secs` seconds — call it from a worker thread.
pub fn blink(ip: &str, r: u8, g: u8, b: u8, secs: u32) -> std::io::Result<()> {
    let cycles = secs.max(1) * 2;
    let script = format!(
        "L=/sys/class/leds; LK=/data/local/tmp/q1blink.state;          if [ -f $LK ]; then read sr sg sb < $LK; else            sr=$(cat $L/red/brightness); sg=$(cat $L/green/brightness); sb=$(cat $L/blue/brightness);            echo \"$sr $sg $sb\" > $LK; fi;         restore(){{ echo $sr > $L/red/brightness; echo $sg > $L/green/brightness;                      echo $sb > $L/blue/brightness; rm -f $LK; }};          trap restore EXIT INT TERM;          i=0; while [ $i -lt {cycles} ]; do            echo {r} > $L/red/brightness; echo {g} > $L/green/brightness; echo {b} > $L/blue/brightness;            sleep 0.25;            echo 0 > $L/red/brightness; echo 0 > $L/green/brightness; echo 0 > $L/blue/brightness;            sleep 0.25; i=$((i+1)); done; restore"
    );
    shell(ip, &script)?;
    Ok(())
}

/// The LED colour a role identifies with.
///
/// A full-on/off tri-colour LED yields only SEVEN distinguishable colours, and
/// there are more roles than that -- so colour alone cannot identify a puck.
/// `identify()` adds a flash COUNT, which is what actually disambiguates;
/// the colour is a fast first cut.
pub fn slot_led_rgb(device: u8) -> (u8, u8, u8, &'static str) {
    const WHEEL: [(u8, u8, u8, &str); 7] = [
        (0, 255, 255, "cyan"),
        (255, 255, 0, "yellow"),
        (0, 255, 0, "green"),
        (255, 0, 255, "magenta"),
        (255, 0, 0, "red"),
        (0, 0, 255, "blue"),
        (255, 255, 255, "white"),
    ];
    WHEEL[device as usize % WHEEL.len()]
}

/// Identify a puck physically: flash its role colour `device + 1` times.
///
/// The count is the reliable part. Seven LED colours cannot name eleven roles,
/// and several of the mixes are hard to tell apart on a small indicator, so
/// "cyan, three flashes" is unambiguous where "cyan" alone is not.
pub fn identify(ip: &str, device: u8) -> std::io::Result<()> {
    let (r, g, b, _) = slot_led_rgb(device);
    let flashes = device as u32 + 1;
    // Raw string: this is shell, full of $ and quotes, and escaping it twice
    // is how it silently turns into something that does nothing.
    let script = format!(
        r#"L=/sys/class/leds; LK=/data/local/tmp/q1blink.state;
if [ -f $LK ]; then read sr sg sb < $LK; else
  sr=$(cat $L/red/brightness); sg=$(cat $L/green/brightness); sb=$(cat $L/blue/brightness);
  echo "$sr $sg $sb" > $LK; fi;
restore(){{ echo $sr > $L/red/brightness; echo $sg > $L/green/brightness;
            echo $sb > $L/blue/brightness; rm -f $LK; }};
trap restore EXIT INT TERM;
grp=0; while [ $grp -lt 2 ]; do
  i=0; while [ $i -lt {flashes} ]; do
    echo {r} > $L/red/brightness; echo {gg} > $L/green/brightness; echo {b} > $L/blue/brightness;
    sleep 0.18;
    echo 0 > $L/red/brightness; echo 0 > $L/green/brightness; echo 0 > $L/blue/brightness;
    sleep 0.18; i=$((i+1)); done;
  sleep 0.9; grp=$((grp+1)); done; restore"#,
        flashes = flashes, r = r, gg = g, b = b
    );
    shell(ip, &script)?;
    Ok(())
}

/// Bring the tracker app up: defeat the proximity gate, clear the USB dialog
/// that hijacks the foreground when a cable was attached, and launch.
pub fn start_tracker(ip: &str) -> std::io::Result<()> {
    shell(ip, "am broadcast -a com.oculus.vrpowermanager.prox_close")?;
    shell(ip, "am force-stop com.oculus.os.vrusb")?;
    shell(ip, &format!("am start -n {TRACKER_PKG}/.MainActivity"))?;
    Ok(())
}





/// Restart the on-puck verifier if its binary is already deployed. The
/// verifier is a plain native binary, so unlike the tracker (which has a boot
/// receiver) a REBOOT kills it permanently -- verification would silently stop
/// and the fleet would look healthy while nothing was checking it. Returns
/// false when the binary is absent, i.e. the puck needs a real deploy.
/// Make sure the on-device snapshot server is up (idempotent). It is the
/// verifier's only frame source, so a verifier running without it reports
/// "no-input" forever while looking perfectly alive.
///
/// It is deliberately run from /data/local/tmp rather than the deploy
/// directory: files under /data/nativetest64 carry the `system_data_file`
/// SELinux label, which the `su` domain may NOT exec under enforcing
/// ("setsid: exec ...: Permission denied" even as root with -rwxr-xr-x).
/// /data/local/tmp is `shell_data_file` and execs fine — which is exactly why
/// q1verify, living there, always restarted cleanly while q1serve did not.
/// Copying inherits the destination label.
/// A monotonic counter that advances only while q1serve is really capturing.
///
/// std-only HTTP because fleet has no HTTP dependency and this is one line of
/// protocol. Picking the right field matters more than it looks:
///
///   * `frames` is the ENCODE counter -- it moves only when a client fetches,
///     so it sits still all through normal idle operation. Watching it would
///     declare a stall every cycle and restart q1serve every 15 s.
///   * `capture_fps` is recomputed only once per 30 delivered framesets
///     (q1cam.cpp), so on a stall it holds its last value forever and never
///     decays to zero.
///   * `skipped` increments per captured-but-not-encoded frameset, ~32/s while
///     idle. It is monotonic and stops dead the moment capture stops.
///
/// `pidof` and a 200 from `/snapshot` both stay healthy-looking through a
/// stall, which is why neither can be used here.
pub fn capture_counter(ip: &str) -> Option<u64> {
    use std::io::{Read, Write};
    let addr = format!("{ip}:8080").parse().ok()?;
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET /stats HTTP/1.0\r\nHost: {ip}\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    let rest = &buf[buf.find("\"skipped\":")? + 10..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Age of the frame `/snapshot` is actually serving, in seconds.
///
/// This is the ONLY honest liveness signal. Every cheaper one lies through the
/// failure mode that matters: `pidof` and a 200 from `/snapshot` stay healthy,
/// `capture_fps` holds its last value, and -- measured on .108 and .132 -- even
/// the `skipped` counter keeps CLIMBING while the published frame stays frozen,
/// so a stall-detector built on it never fires. Meanwhile every consumer is
/// quietly broken: grabs collect zero images ("did not move enough"), and the
/// on-device localizer matches a frame minutes old against live poses, which
/// reads as a weak-but-plausible solve rather than an error.
///
/// Compares the snapshot's X-Capture-Boot-Ns against the device's own uptime,
/// so it needs no host/device clock agreement.
pub fn snapshot_age_secs(ip: &str) -> Option<f64> {
    use std::io::{Read, Write};
    let addr = format!("{ip}:8080").parse().ok()?;
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(4))).ok()?;
    write!(s, "GET /snapshot HTTP/1.0\r\nHost: {ip}\r\nConnection: close\r\n\r\n").ok()?;
    // Headers are all we need; stop before dragging the whole JPEG over wifi.
    let mut buf = vec![0u8; 512];
    let n = s.read(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let key = "X-Capture-Boot-Ns:";
    let rest = &head[head.find(key)? + key.len()..];
    let cap: f64 = rest.trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let up = shell(ip, "cat /proc/uptime").ok()?;
    let now: f64 = up.split_whitespace().next()?.parse().ok()?;
    Some(now - cap / 1e9)
}







pub fn stop_tracker(ip: &str) -> std::io::Result<()> {
    shell(ip, &format!("am force-stop {TRACKER_PKG}"))?;
    Ok(())
}

/// One-time per-device setup so the puck streams on its own after any boot,
/// with nobody launching anything. Four gates gate an unattended start; two
/// ship in the APK's manifest and these are the other two:
///
/// * **BAL exemption.** A BOOT_COMPLETED receiver calling startActivity is
///   blocked outright by background-activity-launch policy. Holding
///   SYSTEM_ALERT_WINDOW is the documented exemption; the app declares the
///   permission, the appop still has to be allowed here.
/// * **Guardian.** Until disabled, the session reaches FOCUSED and emits
///   nothing at all. `persist.oculus.guardian_disable` lands in persistent
///   properties, so init restores it every subsequent boot with no root.
///
/// Returns whether the appop stuck. **Appops are written to disk lazily** — a
/// reboot within a few seconds of granting silently loses the grant, so this
/// waits for the flush before reporting success. That costs ~45 s, once per
/// device, and is the whole reason this is a separate command rather than part
/// of `up`.
pub fn provision_autostart(ip: &str) -> std::io::Result<bool> {
    shell(ip, &format!("appops set {TRACKER_PKG} SYSTEM_ALERT_WINDOW allow"))?;
    shell(ip, "setprop persist.oculus.guardian_disable 1")?;
    // Un-worn is the normal state for a body puck, and a headset that believes
    // it is off a head powers down before anything can start. Both of these are
    // persist.* so init restores them every boot -- the volatile
    // debug.oculus.forceHeadsetOn is cleared by a reboot, which is precisely
    // when it is needed. (Unlike the guardian-property guessing, these names are
    // real: SELinux refuses to create unknown persist.* properties, and these
    // read back set.)
    shell(ip, "setprop persist.oculus.forceHeadsetOn 1")?;
    shell(ip, "setprop persist.ovr.disable.sensorproxy true")?;
    // Screen/standby timeouts are settings, not properties, and persist as-is.
    shell(ip, "settings put system screen_off_timeout 86400000")?;
    shell(ip, "settings put global wifi_sleep_policy 2")?;
    // Let the appop reach disk before anyone can reboot.
    std::thread::sleep(Duration::from_secs(45));
    let st = shell(ip, &format!("appops get {TRACKER_PKG} SYSTEM_ALERT_WINDOW"))?;
    let guardian = shell(ip, "getprop persist.oculus.guardian_disable")?;
    let headset = shell(ip, "getprop persist.oculus.forceHeadsetOn")?;
    Ok(st.contains("allow") && guardian.trim() == "1" && headset.trim() == "1")
}

/// Point the tracker's MPT1 stream at `host:port` as device `device`, with the
/// controller slots off (each puck owns exactly one slot). The config file is
/// read once at app startup, so this restarts the app.
pub fn configure_tracker(ip: &str, host: &str, port: u16, device: u8) -> std::io::Result<()> {
    let dir = format!("/sdcard/Android/data/{TRACKER_PKG}/files");
    let cfg = format!("host={host}\\nport={port}\\ndevice={device}\\ncontrollers=0\\n");
    shell(ip, &format!("mkdir -p {dir}"))?;
    shell(ip, &format!("printf '{cfg}' > {dir}/config.txt"))?;
    stop_tracker(ip)?;
    std::thread::sleep(Duration::from_millis(300));
    start_tracker(ip)
}

// ------------------------------------------------------------------ the map
//
// Insight persists its SLAM map to /vision/insideout/mapdb once a map context
// holds a persisted anchor. Pucks that LOAD the same map track in one frame
// with no host-side transform -- see docs/insight-map-lifecycle.md, which these
// functions implement. Everything here uses the checked adb path above:
// a silent failure would mean overwriting a puck's map for nothing.

const MAPDB: &str = "/vision/insideout/mapdb";
const MAPDB_STAGE: &str = "/data/local/tmp/mapdb_in";
/// SELinux label trackingservice reads that directory as. Files arriving over
/// `adb push` do NOT carry it, and without a relabel the map is silently
/// unreadable -- which looks exactly like "the transplant did not work".
const MAPDB_CONTEXT: &str = "u:object_r:vision_file:s0";

#[derive(Debug, Default, Clone)]
pub struct MapdbInfo {
    pub files: usize,
    pub bytes: u64,
    /// Last write, unix seconds. 0 if the directory is empty.
    pub mtime_unix: i64,
}

impl MapdbInfo {
    pub fn is_empty(&self) -> bool {
        self.files == 0
    }
}

/// File count, total bytes and last-write time in ONE shell round trip.
pub fn mapdb_info(ip: &str) -> Result<MapdbInfo, FleetError> {
    let out = shell_checked(
        ip,
        &format!(
            "ls {MAPDB}/*.mapdata 2>/dev/null | wc -l; \
             du -sk {MAPDB} 2>/dev/null | cut -f1; \
             stat -c %Y {MAPDB} 2>/dev/null"
        ),
        20,
    )?;
    let mut lines = out.lines().map(str::trim);
    Ok(MapdbInfo {
        files: lines.next().and_then(|s| s.parse().ok()).unwrap_or(0),
        bytes: lines.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) * 1024,
        mtime_unix: lines.next().and_then(|s| s.parse().ok()).unwrap_or(0),
    })
}

/// `adb pull` a directory, with a real result.
///
/// 180 s: a mapdb is a few MB over wifi adb, and 40-odd files each costing a
/// round trip is the realistic slow case. Returns the directory the files
/// actually landed in -- `adb pull <dir> <out>` writes into `<out>/mapdb` when
/// `<out>` already exists but drops the files directly into `<out>` when it
/// does not, and getting that wrong yields a silently empty map.
pub fn pull_dir(ip: &str, remote: &str, local: &std::path::Path) -> Result<std::path::PathBuf, FleetError> {
    std::fs::create_dir_all(local).map_err(FleetError::Spawn)?;
    let leaf = remote.rsplit('/').next().unwrap_or("");
    adb_raw(
        ip,
        &["pull", remote, &local.to_string_lossy()],
        180,
    )?;
    let inner = local.join(leaf);
    Ok(if inner.is_dir() { inner } else { local.to_path_buf() })
}

/// `adb push` a directory's contents, with a real result.
pub fn push_dir(ip: &str, local: &std::path::Path, remote: &str) -> Result<(), FleetError> {
    let src = format!("{}/.", local.to_string_lossy());
    adb_raw(ip, &["push", &src, remote], 180)?;
    Ok(())
}

/// Count `.mapdata` files in a local directory -- used to check a pull landed.
pub fn local_mapdata_count(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map_or(false, |x| x == "mapdata"))
                .count()
        })
        .unwrap_or(0)
}

/// `stop trackingservice; start trackingservice`.
///
/// NOTE this does NOT flush the map: verified, the mapdb mtime is unchanged
/// across a clean stop. Anything mapped since the last automatic write is lost
/// here. Callers must POLL for the map context afterwards, never assume one.
pub fn restart_trackingservice(ip: &str) -> Result<(), FleetError> {
    shell_checked(ip, "stop trackingservice", 30)?;
    std::thread::sleep(Duration::from_secs(3));
    shell_checked(ip, "start trackingservice", 30)?;
    Ok(())
}

/// Snapshot the puck's CURRENT mapdb before it is overwritten: on-device first
/// (fast, and recoverable without the host), then pulled to `host_dir`.
///
/// Returns (on-device path, host path). An empty mapdb is not an error -- there
/// is simply nothing to save -- and reports `None`.
pub fn backup_mapdb(
    ip: &str,
    host_dir: &std::path::Path,
) -> Result<Option<(String, std::path::PathBuf)>, FleetError> {
    let info = mapdb_info(ip)?;
    if info.is_empty() {
        return Ok(None);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let on_device = format!("/data/local/tmp/mapdb_backup_{stamp}");
    // NOTE toybox `cp -a` does NOT preserve the SELinux label, so a restore
    // from this copy must re-run the same chcon seed_mapdb does.
    shell_checked(ip, &format!("mkdir -p {on_device}; cp -a {MAPDB}/. {on_device}/"), 60)?;

    let host = host_dir.join(ip).join(stamp.to_string());
    let landed = pull_dir(ip, MAPDB, &host)?;
    if local_mapdata_count(&landed) == 0 {
        return Err(FleetError::Precondition(format!(
            "backup of {ip} pulled no .mapdata files to {} -- refusing to overwrite its map",
            landed.display()
        )));
    }
    Ok(Some((on_device, landed)))
}

/// Install a map onto a puck: stage, (optionally) clear, copy, fix ownership
/// and the SELinux label, then VERIFY the label stuck.
///
/// `replace` clears the target's mapdb first. Prefer it: copying one lineage
/// over another merges two directories, filenames can collide, and which
/// context then loads is undefined -- so the success check ("same root uuid")
/// becomes ambiguous. Does NOT restart trackingservice; the caller does that.
pub fn seed_mapdb(ip: &str, local_dir: &std::path::Path, replace: bool) -> Result<usize, FleetError> {
    if !ensure_root(ip) {
        return Err(FleetError::Precondition(format!(
            "{ip} is not adb root -- chown/chmod/chcon would fail silently"
        )));
    }
    let n_local = local_mapdata_count(local_dir);
    if n_local == 0 {
        return Err(FleetError::Precondition(format!(
            "no .mapdata files in {}",
            local_dir.display()
        )));
    }
    // Stale files from a previous transplant would be copied in alongside.
    shell_checked(ip, &format!("rm -rf {MAPDB_STAGE}; mkdir -p {MAPDB_STAGE}"), 30)?;
    push_dir(ip, local_dir, MAPDB_STAGE)?;

    if replace {
        shell_checked(ip, &format!("rm -f {MAPDB}/*.mapdata"), 30)?;
    }
    shell_checked(ip, &format!("cp {MAPDB_STAGE}/*.mapdata {MAPDB}/"), 60)?;
    shell_checked(
        ip,
        &format!(
            "chown system:system {MAPDB}/*.mapdata; \
             chmod 600 {MAPDB}/*.mapdata; \
             chcon {MAPDB_CONTEXT} {MAPDB}/*.mapdata"
        ),
        60,
    )?;

    // Verify the label rather than infer it. A missing chcon is invisible until
    // trackingservice quietly fails to read the map, and SELinux running
    // Permissive would hide it entirely until the next reboot.
    let labels = shell_checked(ip, &format!("ls -Z {MAPDB}/*.mapdata"), 30)?;
    let bad = labels
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.contains(MAPDB_CONTEXT))
        .count();
    if bad > 0 {
        return Err(FleetError::Precondition(format!(
            "{bad} file(s) on {ip} are not labelled {MAPDB_CONTEXT}; \
             trackingservice would silently ignore the map"
        )));
    }
    let after = mapdb_info(ip)?;
    Ok(after.files)
}

/// Poll until the puck reports `root_full` as its map context AND that context
/// is persistent.
///
/// Built on `status()` so it inherits the dump-to-file/grep-the-file discipline
/// (rule 1 at the top of this file) and costs one round trip per poll.
/// Relocalization needs the puck to physically SEE mapped territory, so a
/// timeout here usually means "wrong room / blank wall", not "broken".
pub fn await_map_root(
    ip: &str,
    root_full: &str,
    deadline: Duration,
    progress: &mut dyn FnMut(&str),
) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        let st = status(ip);
        if st.map_persistent && !st.map_root.is_empty() && st.map_root == root_full {
            return true;
        }
        progress(&format!(
            "waiting for {ip} to relocalize: {}s (now {})",
            start.elapsed().as_secs(),
            if st.map_root.is_empty() { "no context".into() } else { short_root(&st.map_root) }
        ));
        std::thread::sleep(Duration::from_secs(3));
    }
    false
}

/// The 8-char form used for display and comparison.
pub fn short_root(root: &str) -> String {
    root.chars().take(8).collect()
}

/// Is `com.oculus.guardian` enabled? It ships DISABLED on the pucks, which is
/// why no puck could ever create a map: no service, so no persistAnchor, so no
/// persistent context, so no file.
pub fn guardian_enabled(ip: &str) -> Result<bool, FleetError> {
    let out = shell_checked(ip, "pm list packages -d 2>/dev/null | grep -c oculus.guardian", 20)?;
    Ok(out.trim() == "0")
}

// --------------------------------------------------------- creating a map
//
// A map context is only written to disk once one of its anchors is PERSISTED,
// and only guardian can persist one. Guardian ships disabled on the pucks --
// which is why no puck could ever create a map -- so this enables it, drives
// its JSON command channel, and puts it back. See docs/insight-map-lifecycle.md.

const GUARDIAN_PKG: &str = "com.oculus.guardian";
const GUARDIAN_SVC: &str = "com.oculus.guardian/com.oculus.vrguardianservice.VrGuardianService";

/// Enable guardian and start its service, in the ONE order that works.
///
/// trackingservice must be restarted AFTER guardian, or guardian logs
/// `SlamAnchorRuntimeIpcClient: InitClientInternal failed!` and never attaches
/// to the SlamAnchorServer, leaving it unable to do any anchor work at all.
fn guardian_up(ip: &str, progress: &mut dyn FnMut(&str)) -> Result<(), FleetError> {
    progress("enabling guardian");
    shell_checked(ip, &format!("pm enable {GUARDIAN_PKG}"), 30)?;
    shell_checked(
        ip,
        "setprop persist.oculus.guardian_json_cmds_user_build 1; \
         setprop persist.oculus.guardian_disable 0",
        20,
    )?;
    progress("starting VrGuardianService");
    shell_checked(ip, &format!("am start-foreground-service {GUARDIAN_SVC}"), 30)?;
    std::thread::sleep(Duration::from_secs(4));
    progress("restarting trackingservice (must follow guardian)");
    restart_trackingservice(ip)?;
    Ok(())
}

/// Put guardian back the way the fleet needs it: package DISABLED.
///
/// Not merely the property -- with the package enabled the tracker app reaches
/// FOCUSED and emits nothing at all while looking perfectly healthy, and the
/// display shows passthrough instead of going dark.
fn guardian_down(ip: &str, progress: &mut dyn FnMut(&str)) -> Result<(), FleetError> {
    progress("disabling guardian");
    shell_checked(ip, "setprop persist.oculus.guardian_disable 1", 20)?;
    shell_checked(ip, &format!("am force-stop {GUARDIAN_PKG}"), 30)?;
    shell_checked(ip, &format!("pm disable-user --user 0 {GUARDIAN_PKG}"), 30)?;
    Ok(())
}

/// Wait for Insight to be tracking at 6DOF with a valid pose.
pub fn await_6dof(ip: &str, deadline: Duration, progress: &mut dyn FnMut(&str)) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        let st = status(ip);
        if st.tracking == "6DOF" && st.tracking_valid {
            return true;
        }
        progress(&format!(
            "waiting for 6DOF on {ip}: {}s (now {})",
            start.elapsed().as_secs(),
            if st.tracking.is_empty() { "no tracking" } else { &st.tracking }
        ));
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

/// Mint a persistent map for the space the puck is currently in.
///
/// Returns the new root uuid. The puck must be WORN and tracking at 6DOF:
/// guardian refuses otherwise with `force_stationary: Invalid Guardian`, and
/// the broadcast still exits 0, so that refusal has to be read out of logcat
/// rather than inferred from a status code.
///
/// Leaves guardian disabled again, but does NOT restart the tracker app --
/// the caller owns that, because it also owns verifying the stream came back.
pub fn create_map(
    ip: &str,
    roomscale: bool,
    progress: &mut dyn FnMut(&str),
) -> Result<String, FleetError> {
    if !ensure_root(ip) {
        return Err(FleetError::Precondition(format!("{ip} is not adb root")));
    }
    let before = status(ip);
    if before.map_persistent {
        return Err(FleetError::Precondition(format!(
            "{ip} already has a persistent map ({}). Creating another is untested; \
             share this one instead.",
            short_root(&before.map_root)
        )));
    }

    guardian_up(ip, progress)?;

    // The restart above dropped tracking; guardian cannot place an anchor
    // until Insight is back at 6DOF.
    if !await_6dof(ip, Duration::from_secs(90), progress) {
        guardian_down(ip, progress).ok();
        return Err(FleetError::Precondition(format!(
            "{ip} did not reach 6DOF within 90 s. It must be WORN (or at least \
             awake and seeing the room) for guardian to place an anchor."
        )));
    }

    // Timestamp for the logcat window, so a failure quotes only THIS attempt.
    let since = shell_checked(ip, "date '+%m-%d %H:%M:%S.000'", 20)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let which = if roomscale { "force_roomscale" } else { "force_stationary" };
    progress(&format!("sending {which}"));
    // Single quotes survive: the JSON contains braces, quotes and colons but
    // no single quote. The extra key is `cmd`; a wrong one logs `Cmd: null`.
    shell_checked(
        ip,
        &format!(
            "am broadcast -a com.oculus.vrguardianservice.JsonCmdUserBroadcast \
             -p {GUARDIAN_PKG} --es cmd '{{\"automation\":{{\"guardian\":{{\"{which}\":true}}}}}}'"
        ),
        30,
    )?;

    // `am broadcast` exits 0 whatever happened, so the real verdict is a new
    // PERSISTENT context appearing.
    let start = std::time::Instant::now();
    let mut root = String::new();
    while start.elapsed() < Duration::from_secs(45) {
        let st = status(ip);
        if st.map_persistent && !st.map_root.is_empty() {
            root = st.map_root;
            break;
        }
        progress(&format!("waiting for a persistent map: {}s", start.elapsed().as_secs()));
        std::thread::sleep(Duration::from_secs(3));
    }

    if root.is_empty() {
        // Quote guardian's own refusal rather than guessing at one.
        let detail = shell_checked(
            ip,
            &format!(
                "logcat -d -t '{since}' 2>/dev/null | \
                 grep -iE 'ProcessJsonCmd|force_stationary|force_roomscale|persistAnchor' | tail -4"
            ),
            30,
        )
        .unwrap_or_default();
        guardian_down(ip, progress).ok();
        let detail = detail.trim();
        return Err(FleetError::Precondition(if detail.is_empty() {
            format!("{ip}: no persistent map appeared, and guardian logged nothing")
        } else {
            format!("{ip}: no persistent map appeared. guardian said:\n{detail}")
        }));
    }

    guardian_down(ip, progress)?;
    Ok(root)
}
