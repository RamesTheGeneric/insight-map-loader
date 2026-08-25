//! The always-on service: ingest → align → emit, with nobody pressing buttons.
//!
//! The calibrations this system needs are automatable because each has a
//! machine-detectable trigger and a machine-checkable precondition, so this
//! module runs them as watchdogs instead of waiting for a human:
//!
//! * **Bridge watchdog.** A puck's LOCAL→world bridge is verifiable for the
//!   price of one dumpsys read: predict the world pose through the current
//!   bridge and compare. When the check fails repeatedly (tracker restarted →
//!   new LOCAL frame) or no bridge exists yet (first run), the watchdog waits
//!   for the puck to be STILL — observable from its own stream — and re-solves
//!   silently. Wearing the pucks, stillness happens every time the user stands
//!   still for two seconds.
//! * **Drift monitor.** Worn together, the inter-puck separation holds a
//!   baseline (gait wiggles it by ~centimetres); the alignment going stale
//!   shows up as a step-change that persists. That flips health to Drifted,
//!   and — if `auto_realign` is on — runs the capture+solve pipeline, whose
//!   output the service hot-reloads. The measured caveat applies: separation
//!   detects alignment *change*, not alignment *error*, so a wrong-but-stable
//!   transform must still be caught by a re-solve.
//!
//! The GUI and CLI are thin views over this; their buttons just set the same
//! flags the watchdogs set themselves.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::aggregate::Aggregator;
use crate::bridge::{self, PosePair};
use crate::config::{self, Config};
use crate::fleet;
use crate::ingest::{Ingest, SlotState};
use crate::mpt1::Device;
use crate::transform::{relative_yaw, wrap, Frame4Dof};

const STILL_VEL: f32 = 0.05; // m/s
const STILL_ANGVEL: f32 = 0.15; // rad/s
const STILL_SPAN: Duration = Duration::from_millis(1500);
/// History retention must EXCEED the span the stillness check requires, or the
/// oldest entry is evicted just before it becomes old enough to prove the span
/// and the gate can never pass (measured: span=false on every sweep).
const STILL_RETAIN: Duration = Duration::from_millis(3000);
const STILL_POS_DELTA: f32 = 0.02; // m over the span
const BRIDGE_CHECK_EVERY: Duration = Duration::from_secs(4);
const BRIDGE_POS_TOL: f32 = 0.15; // m — dumpsys quantises at 1 cm
const BRIDGE_YAW_TOL: f32 = 3.0; // deg — dumpsys quantises at ~1.2°
/// Past these, the bridge did not drift -- the puck's Insight WORLD FRAME
/// jumped (a relocalization). That invalidates the stored T_map_world too, not
/// just the bridge, so re-bridging alone leaves the puck correctly bridged into
/// a frame its transform no longer describes. Seen live as a re-join landing
/// 41 deg from the stored value while every health signal looked fine.
const FRAME_JUMP_POS_M: f32 = 0.5;
const FRAME_JUMP_YAW_DEG: f32 = 10.0;
// Drift is separation change the pucks' own reported motion cannot explain:
// physically, |d(sep)/dt| <= |v_A| + |v_B|. Someone carrying the pucks apart
// produces large but fully explained change; a frame jump produces change with
// no motion behind it. The margin absorbs velocity noise and packet timing.
const DRIFT_WINDOW_S: f64 = 3.0;
const DRIFT_GAP_RESET_S: f64 = 0.5;
const DRIFT_UNEXPLAINED_M: f32 = 0.4;
// A pose moving faster than any human wears it is a frame event, not motion.
const TELEPORT_SPEED: f32 = 10.0; // m/s implied between consecutive samples
const TELEPORT_MIN_DIST: f32 = 0.15; // m, below dumpsys/step noise concerns
const AUTO_REALIGN_COOLDOWN: Duration = Duration::from_secs(600);
// A localization needs the puck to have MOVED (measured: stationary queries
// are worse than a stale transform). Spread over a rolling window is the gate.
/// A bridge is only as good as its spread, and the HIP's bridge rotates the
/// whole shared frame -- a 0.95 deg auto-solve was accepted once and showed up
/// as visible tracker misalignment. Held-still solves land at 0.01-0.05 deg,
/// so anything above this is the puck moving mid-solve, not the achievable
/// floor. The watchdog retries on the next still window, so a strict bar costs
/// nothing but a short wait.
const BRIDGE_MAX_SPREAD_DEG: f32 = 0.5;
const JOIN_SPREAD_M: f32 = 0.6;
const JOIN_WINDOW: Duration = Duration::from_secs(12);
/// Minimum gap between opportunistic hip-map folds. Accumulation is meant to
/// be invisible: one short grab whenever the hip happens to be moving, never
/// a stream of them.
const FOLD_MIN_GAP: Duration = Duration::from_secs(120);
/// On-device solves below this many inliers carry no feedback weight. Set
/// from measurement: a 328-inlier solve was allowed to overwrite the
/// 1786-inlier cold-start join and swung the alignment 2.5 deg.
const FEEDBACK_MIN_INLIERS: i64 = 500;
/// Fraction of the measured correction applied per update. A full replace
/// makes the stored transform a random walk over single-frame solves; a
/// partial step averages them instead, so noise cancels and only a consistent
/// bias accumulates.
const FEEDBACK_GAIN: f64 = 0.35;
/// Hard per-update ceiling, so no single batch can move the alignment far.
const FEEDBACK_MAX_STEP_DEG: f64 = 0.5;
const FEEDBACK_MAX_STEP_M: f64 = 0.05;
/// How far on-device refinement may wander from the host cold-start anchor
/// before it is refusing to converge and needs a real re-solve instead. The
/// host join pools ~12 walked viewpoints; a single-frame on-device solve is
/// meant to REFINE that, never redefine it.
const FEEDBACK_MAX_DRIFT_DEG: f64 = 2.0;
const FEEDBACK_MAX_DRIFT_M: f64 = 0.25;
const LOCALIZE_COOLDOWN: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BridgeState {
    Missing,
    WaitingStill,
    Solving,
    Ok,
    /// Consistency checks are failing — a re-solve is pending stillness.
    Suspect,
}

#[derive(Clone, Default)]
pub struct View {
    pub live: Vec<(Device, [f32; 3])>,
    pub sep: Option<f32>,
    pub slots: Vec<(Device, SlotState, f32, f32)>, // state, age s, rate Hz
    pub emitted: u64,
    /// Per-puck T_map_world as applied: (ip, yaw_deg, seeded-not-yet-localized).
    /// Empty in colocated mode — there is no map transform to report.
    pub map_t: Vec<(String, f32, bool)>,
    pub n_transforms: usize,
    /// Native colocation is configured, so the UI should report shared-map
    /// agreement instead of per-puck transforms.
    pub colocated: bool,
    /// Long fleet operations, newest last. Snapshot, like `events`.
    pub jobs: Vec<crate::jobs::Job>,
    /// Pucks with a grab+localize currently running.
    pub localizing: Vec<String>,
    pub bridges: Vec<(String, BridgeState, f32)>, // ip, state, yaw°
    pub drifted: bool,
    pub realign_running: bool,
    pub events: Vec<String>,
    pub error: Option<String>,
}

/// Separation-vs-motion consistency, kept pure so it can be unit-tested.
///
/// Feed it (time, separation, sum of the two pucks' speeds); it answers with
/// the unexplained separation change over the window when that exceeds the
/// tolerance. A gap in samples resets the window — integrating reported speed
/// across a dropout would credit motion nobody measured.
pub struct DriftDetector {
    win: VecDeque<(f64, f32, f32)>, // t, sep, cumulative explained metres
}

impl DriftDetector {
    pub fn new() -> DriftDetector {
        DriftDetector { win: VecDeque::new() }
    }

    pub fn reset(&mut self) {
        self.win.clear();
    }

    pub fn push(&mut self, t: f64, sep: f32, speed_sum: f32) -> Option<f32> {
        let cum = match self.win.back() {
            Some(&(t_last, _, cum_last)) => {
                let dt = t - t_last;
                if dt > DRIFT_GAP_RESET_S || dt < 0.0 {
                    self.win.clear();
                    0.0
                } else {
                    cum_last + speed_sum * dt as f32
                }
            }
            None => 0.0,
        };
        self.win.push_back((t, sep, cum));
        while self.win.front().map_or(false, |&(t0, _, _)| t - t0 > DRIFT_WINDOW_S) {
            self.win.pop_front();
        }
        let &(t0, sep0, cum0) = self.win.front()?;
        if t - t0 < DRIFT_WINDOW_S * 0.5 {
            return None; // not enough history to judge
        }
        let unexplained = (sep - sep0).abs() - (cum - cum0);
        (unexplained > DRIFT_UNEXPLAINED_M).then_some(unexplained)
    }
}

impl Default for DriftDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(PartialEq, Clone, Copy)]
enum LocKind {
    /// Full solve: needs walking, updates the transform.
    Localize,
    /// Score the CURRENT transform against the map with a quick stationary
    /// glance — the direct misalignment oracle after a reboot or gap.
    Verify,
    /// Fold the hip's current view into the reference map (opportunistic
    /// accumulation): triggered by observed natural movement, founds the map
    /// from nothing on the first fire, re-deploys the on-puck subset after.
    /// This is what makes map quality nobody's job.
    Fold,
}

struct LocReq {
    ip: String,
    coldstart: bool,
    kind: LocKind,
}

struct LocState {
    tx: Option<mpsc::Sender<LocReq>>,
    inflight: BTreeSet<String>,
    last_attempt: BTreeMap<String, Instant>,
}

struct Shared {
    view: Mutex<View>,
    loc: Mutex<LocState>,
    /// Recent verification results per puck: (median_deg, when). Two pucks
    /// refuted by SIMILAR amounts is common-mode map drift (relative
    /// alignment intact, playspace cal absorbs it); ONE puck refuted alone is
    /// the harmful, differential kind.
    verdicts: Mutex<BTreeMap<String, (f64, Instant)>>,
    rebuild: AtomicBool,
    bridge_now: AtomicBool,  // manual "bridge as soon as still"
    realign_now: AtomicBool, // manual re-solve request
    realign_running: AtomicBool,
    /// Pucks whose bridge should be verified at the next opportunity — set by
    /// the teleport detector, consumed by the watchdog (which relaxes its
    /// stillness/interval gates for that one check).
    verify: Mutex<std::collections::BTreeSet<String>>,
    /// The hip's IP, so request_kind can refuse to localize it from ANY path.
    hip_ip: Option<String>,
    /// Native colocation: the pucks share one Insight map, so there is no
    /// T_map_world to solve. Localization is refused outright in this mode --
    /// solving one would write a transform that moves a frame which is already
    /// correct by construction.
    colocated: bool,
    /// Pucks whose transform is currently owned by the continuous pair stream
    /// (last paircal write instant). While fresh, every other alignment
    /// writer stands down for that puck.
    paircal: Mutex<BTreeMap<String, Instant>>,
    /// Long fleet operations (map share, map create) with real progress and
    /// real errors. Serialized -- see jobs.rs.
    jobs: Arc<crate::jobs::JobQueue>,
    job_tx: Mutex<Option<mpsc::Sender<crate::jobs::JobRequest>>>,
    /// Where host-side map backups go before a puck's map is overwritten.
    map_backups: std::path::PathBuf,
    /// The LIVE puck roster. Behind a lock because a role change must take
    /// effect without a restart: a stale roster makes build_transforms key off
    /// the old slot, and the puck disappears from SteamVR with no error.
    pucks: std::sync::RwLock<Vec<crate::config::PuckCfg>>,
    /// Bumped on every roster change so long-lived threads can notice.
    roster_gen: std::sync::atomic::AtomicU64,
}

pub struct Service {
    shared: Arc<Shared>,
    pub ingest: Arc<Ingest>,
    /// Path to bridge.json, so a job can wait for a re-bridge to land.
    bridge_path: String,
    /// Enough of the config for jobs that must reconfigure a puck.
    pucks: Vec<crate::config::PuckCfg>,
    host: String,
    listen_port: u16,
}

impl Service {
    pub fn view(&self) -> View {
        self.shared.view.lock().unwrap().clone()
    }

    /// Snapshot of the long fleet operations, for the UI.
    pub fn jobs(&self) -> Vec<crate::jobs::Job> {
        self.shared.jobs.snapshot()
    }

    pub fn job_running(&self) -> bool {
        self.shared.jobs.any_active()
    }

    pub fn cancel_job(&self) {
        self.shared.jobs.request_cancel();
    }

    /// Copy `source`'s Insight map onto every other puck and wait for each to
    /// relocalize into it — after which they all track in ONE frame with no
    /// host-side transform (docs/insight-map-lifecycle.md).
    ///
    /// Returns the job id, or an error if a job is already running. Every
    /// target's existing map is archived on-device AND on the host first; a
    /// backup failure aborts before a single byte is overwritten.
    pub fn share_map(&self, source: &str, targets: Vec<String>) -> Result<u64, String> {
        use crate::jobs::{Job, JobStep};

        if targets.is_empty() {
            return Err("no other pucks to share with".into());
        }
        let id = self.shared.jobs.next_id();
        let mut steps = vec![
            JobStep::new(format!("check {source} has a persistent map")),
            JobStep::new(format!("pull map from {source}")),
        ];
        for t in &targets {
            steps.push(JobStep::new(format!("back up {t}'s map")));
            steps.push(JobStep::new(format!("install map on {t}")));
            steps.push(JobStep::new(format!("restart tracking on {t}")));
            steps.push(JobStep::new(format!("wait for {t} to relocalize")));
        }
        // Restarting trackingservice resets the Insight world frame, which
        // invalidates every affected puck's LOCAL->world bridge. Leaving that
        // to the user means the job reports success while the output is still
        // visibly wrong -- observed as a 180 deg rotation after a share that
        // had in fact worked perfectly.
        steps.push(JobStep::new("re-bridge (hold the pucks still)"));

        let job = Job::new(id, format!("Share map from {source} to {} puck(s)", targets.len()), steps);
        let src = source.to_string();
        let backups = self.shared.map_backups.clone();
        let sh = Arc::clone(&self.shared);
        let bridge_path = self.bridge_path.clone();

        let req = crate::jobs::JobRequest {
            job,
            run: Box::new(move |ctx| {
                share_map_job(ctx, &src, &targets, &backups, &sh, &bridge_path)
            }),
        };
        let tx = self.shared.job_tx.lock().unwrap();
        tx.as_ref()
            .ok_or_else(|| "job runner not started".to_string())?
            .send(req)
            .map_err(|_| "job runner is gone".to_string())?;
        Ok(id)
    }

    /// Mint a persistent map for the space `ip` is standing in.
    ///
    /// The puck must be WORN and tracking at 6DOF. Its tracker app is stopped
    /// for the duration (guardian and the tracker contend for the cameras) and
    /// restarted afterwards, with the stream verified before this reports
    /// success — otherwise the job hands back a puck with a fine new map that
    /// no longer tracks.
    pub fn create_map(&self, ip: &str, roomscale: bool) -> Result<u64, String> {
        use crate::jobs::{Job, JobStep};

        let id = self.shared.jobs.next_id();
        let steps = vec![
            JobStep::new(format!("check {ip} is worn and tracking")),
            JobStep::new("stop the tracker app (guardian needs the cameras)"),
            JobStep::new("enable guardian and place a persistent anchor"),
            JobStep::new("restart the tracker app"),
            JobStep::new("confirm poses are streaming again"),
        ];
        let job = Job::new(id, format!("Create a map on {ip}"), steps);

        let target = ip.to_string();
        let device = self
            .pucks
            .iter()
            .find(|p| p.ip == target)
            .and_then(|p| Device::from_u8(p.device));
        let host = self.host.clone();
        let port = self.listen_port;
        let ingest = Arc::clone(&self.ingest);

        let req = crate::jobs::JobRequest {
            job,
            run: Box::new(move |ctx| {
                create_map_job(ctx, &target, roomscale, device, &host, port, &ingest)
            }),
        };
        let tx = self.shared.job_tx.lock().unwrap();
        tx.as_ref()
            .ok_or_else(|| "job runner not started".to_string())?
            .send(req)
            .map_err(|_| "job runner is gone".to_string())?;
        Ok(id)
    }

    /// Manual override: re-bridge every puck at the next still moment.
    pub fn request_bridge(&self) {
        self.shared.bridge_now.store(true, Ordering::Relaxed);
    }

    /// Manual override: localize every configured puck against the map now.
    pub fn request_realign(&self) {
        self.shared.realign_now.store(true, Ordering::Relaxed);
    }

    pub fn start(cfg: Config) -> Result<Service, String> {
        let ingest = Arc::new(
            Ingest::bind(&cfg.listen, Duration::from_millis(500))
                .map_err(|e| format!("cannot listen on {}: {e}", cfg.listen))?,
        );
        let shared = Arc::new(Shared {
            view: Mutex::new(View::default()),
            loc: Mutex::new(LocState {
                tx: None,
                inflight: BTreeSet::new(),
                last_attempt: BTreeMap::new(),
            }),
            verdicts: Mutex::new(BTreeMap::new()),
            rebuild: AtomicBool::new(true),
            bridge_now: AtomicBool::new(false),
            realign_now: AtomicBool::new(false),
            realign_running: AtomicBool::new(false),
            verify: Mutex::new(std::collections::BTreeSet::new()),
            hip_ip: cfg.hip().map(|p| p.ip.clone()),
            colocated: cfg.colocated,
            paircal: Mutex::new(BTreeMap::new()),
            jobs: Arc::new(crate::jobs::JobQueue::new()),
            job_tx: Mutex::new(None),
            map_backups: map_backup_dir(),
            pucks: std::sync::RwLock::new(cfg.pucks.clone()),
            roster_gen: std::sync::atomic::AtomicU64::new(0),
        });

        // The job runner: one thread, one job at a time (jobs.rs explains why).
        {
            let (tx, rx) = mpsc::channel::<crate::jobs::JobRequest>();
            *shared.job_tx.lock().unwrap() = Some(tx);
            let q = Arc::clone(&shared.jobs);
            let sh = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("job-runner".into())
                .spawn(move || {
                    crate::jobs::run_queue(q, rx, move |job| {
                        // No lock is held here, by contract -- push_event takes
                        // the view lock and that pair deadlocked once already.
                        push_event(&sh, job.headline());
                    })
                })
                .map_err(|e| e.to_string())?;
        }

        // MPT2: the pucks' OWN tracker (q1track) streams map-frame poses on
        // listen-port+2. Pairing it against the Insight stream per puck is the
        // continuous alignment (SpaceCalibrator model): the window of recent
        // pairs IS the calibration.
        let mpt2 = {
            let addr = bump_port(&cfg.listen, 2);
            Ingest::bind(&addr, Duration::from_millis(500))
                .map_err(|e| format!("mpt2 {addr}: {e}"))?
        };
        spawn_aggregate(cfg.clone(), Arc::clone(&ingest), Arc::new(mpt2), Arc::clone(&shared))?;
        spawn_bridge_watchdog(cfg.clone(), Arc::clone(&ingest), Arc::clone(&shared))?;
        spawn_verifier_poll(cfg.clone(), Arc::clone(&shared))?;
        let bridge_path = cfg.bridge.clone();
        let pucks = cfg.pucks.clone();
        let host = cfg.host.clone();
        let listen_port =
            cfg.listen.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(5180);
        spawn_mapd(cfg, Arc::clone(&shared))?;
        Ok(Service { shared, ingest, bridge_path, pucks, host, listen_port })
    }
}

/// Enqueue a grab+localize for one puck. `manual` bypasses the cooldown.
fn request_localize(shared: &Shared, ip: &str, coldstart: bool, manual: bool) -> bool {
    request_kind(shared, ip, coldstart, manual, LocKind::Localize)
}

fn request_verify(shared: &Shared, ip: &str) -> bool {
    request_kind(shared, ip, false, true, LocKind::Verify)
}

fn request_kind(shared: &Shared, ip: &str, coldstart: bool, manual: bool, kind: LocKind) -> bool {
    // The hip IS the map frame. Localizing it solves it into its own map and
    // writes a non-identity transform, which silently moves the frame every
    // other puck is aligned to -- it happened, from the verify-refuted path.
    // Refuse it here so no future caller can reintroduce that.
    if kind == LocKind::Localize && shared.hip_ip.as_deref() == Some(ip) {
        return false;
    }
    // Colocated pucks share one map frame, so there is nothing to localize INTO
    // -- a solve here could only introduce error into a frame that is already
    // identity. Verification still runs; it is read-only.
    if kind == LocKind::Localize && shared.colocated {
        return false;
    }
    let mut loc = shared.loc.lock().unwrap();
    if loc.inflight.contains(ip) {
        return false;
    }
    if !manual {
        if let Some(t) = loc.last_attempt.get(ip) {
            if t.elapsed() < LOCALIZE_COOLDOWN {
                return false;
            }
        }
    }
    let Some(tx) = loc.tx.as_ref() else { return false };
    if tx.send(LocReq { ip: ip.into(), coldstart, kind }).is_ok() {
        loc.inflight.insert(ip.into());
        loc.last_attempt.insert(ip.into(), Instant::now());
        true
    } else {
        false
    }
}

/// Where a puck's existing map is archived before it is overwritten. The
/// directory docs/insight-map-lifecycle.md already names.
fn map_backup_dir() -> std::path::PathBuf {
    std::env::var_os("Q2_MAP_BACKUPS")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("q2slam-backups")))
        .unwrap_or_else(|| std::path::PathBuf::from("q2slam-backups"))
}

fn push_event(shared: &Shared, msg: String) {
    let mut v = shared.view.lock().unwrap();
    v.events.push(msg);
    let n = v.events.len();
    if n > 8 {
        v.events.drain(0..n - 8);
    }
}

fn mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

// ---- aggregation + drift monitoring ---------------------------------------

/// "0.0.0.0:5180" + 2 -> "0.0.0.0:5182".
fn bump_port(listen: &str, by: u16) -> String {
    match listen.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(p) => format!("{host}:{}", p + by),
            Err(_) => format!("{host}:5182"),
        },
        None => "0.0.0.0:5182".into(),
    }
}

fn spawn_aggregate(cfg: Config, ingest: Arc<Ingest>, mpt2: Arc<Ingest>, shared: Arc<Shared>) -> Result<(), String> {
    std::thread::Builder::new()
        .name("service-agg".into())
        .spawn(move || {
            let mut agg = match Aggregator::new(&cfg.out) {
                Ok(a) => a,
                Err(e) => {
                    shared.view.lock().unwrap().error = Some(format!("output {}: {e}", cfg.out));
                    return;
                }
            };
            let mut map_t = config::load_map_transforms(&cfg.transforms)
                .unwrap_or_default();
            let mut t_mtime = mtime(&cfg.transforms);
            let mut last_t: BTreeMap<u8, u64> = BTreeMap::new();
            let mut counts: BTreeMap<u8, u32> = BTreeMap::new();
            let mut window = Instant::now();
            let mut drift = DriftDetector::new();
            let epoch = Instant::now();
            let mut last_pose: BTreeMap<u8, (u64, [f32; 3])> = BTreeMap::new();
            let mut move_hist: BTreeMap<u8, VecDeque<(Instant, [f32; 3])>> = BTreeMap::new();
            let hip_ip: Option<String> = cfg.hip().map(|p| p.ip.clone());
            let mut last_fold: Option<Instant> = None;
            let mut last_hist_push = Instant::now();
            let mut last_join_check = Instant::now();
            // Continuous pairing: per puck, recent (map pose, insight pose)
            // simultaneous observations. The window is the calibration --
            // nothing stored can go stale, jumps roll out with the window.
            let mut pairs: BTreeMap<u8, VecDeque<(Instant, crate::bridge::PosePair)>> =
                BTreeMap::new();
            let mut last_pair_t: BTreeMap<u8, u64> = BTreeMap::new();
            let mut last_paircal = Instant::now();
            let mut last_paircal_event: BTreeMap<String, Instant> = BTreeMap::new();
            let mut live_state: BTreeMap<u8, bool> = BTreeMap::new();
            let mut last_auto_realign: Option<Instant> = None;
            let ip_of: BTreeMap<u8, String> =
                cfg.pucks.iter().map(|p| (p.device, p.ip.clone())).collect();

            loop {
                if shared.rebuild.swap(false, Ordering::Relaxed) {
                    map_t = config::load_map_transforms(&cfg.transforms)
                        .unwrap_or_default();
                    let bridges = config::load_bridges(&cfg.bridge).unwrap_or_default();
                    // From the LIVE roster, so a role change applies without a
                    // restart.
                    let roster = shared.pucks.read().unwrap().clone();
                    agg.transforms =
                        config::build_transforms_for(&roster, cfg.colocated, &map_t, &bridges);
                    // The on-puck verifier scores against the transform it was
                    // last handed; a re-localization that does not reach it makes
                    // it refute a CORRECT alignment. Push each fresh T off the
                    // hot path (adb is slow), so the verifier and the aggregator
                    // never disagree about which transform is live.
                    for p in &cfg.pucks {
                        if let Some(m) = map_t.get(&p.ip) {
                            let (ip, yaw, t) = (p.ip.clone(), m.yaw_deg, m.t);
                            std::thread::spawn(move || {
                                fleet::push_map_transform(&ip, yaw, t).ok();
                            });
                        }
                    }
                    // Any transform change moves everything; old statistics lie.
                    drift.reset();
                    last_pose.clear();
                    shared.view.lock().unwrap().drifted = false;
                }
                if mtime(&cfg.transforms) != t_mtime {
                    t_mtime = mtime(&cfg.transforms);
                    shared.rebuild.store(true, Ordering::Relaxed);
                    push_event(&shared, "map transforms changed — reloading".into());
                }

                let summary = agg.tick(&ingest);
                for s in ingest.live() {
                    let d = s.packet.device as u8;
                    if last_t.get(&d) != Some(&s.packet.t_ns) {
                        last_t.insert(d, s.packet.t_ns);
                        *counts.entry(d).or_default() += 1;
                    }
                }

                // REBOOTS announce themselves on the stream: t_ns is the
                // device's boot clock, so a fresh boot sends timestamps HOURS
                // behind the ones before it. This is the detector the pose
                // statistics can never be — deterministic, gap-proof, and
                // exactly the event that invalidates the stored transform.
                for smp in ingest.live() {
                    let d = smp.packet.device as u8;
                    if let Some(&(t_prev, _)) = last_pose.get(&d) {
                        if smp.packet.t_ns + 3_600_000_000_000 < t_prev {
                            if let Some(ip) = ip_of.get(&d) {
                                push_event(&shared, format!(
                                    "{ip} REBOOTED (boot clock regressed) — verifying alignment"));
                                last_pose.remove(&d);
                                move_hist.remove(&d);
                                request_verify(&shared, ip);
                            }
                        }
                    }
                }
                // A tracker returning after a gap may have relocalized into a
                // moved frame; a quick stationary verification settles it.
                //
                // The clone is bound to a local ON PURPOSE. Written as
                // `for x in shared.view.lock().unwrap().slots.clone()`, the
                // guard is a temporary of the iterator expression and lives
                // until the END of the loop -- so push_event() inside the body
                // re-locks a non-reentrant mutex and deadlocks the aggregate
                // thread while holding `view`, which then freezes the GUI on
                // its next paint. It fired the first time a tracker went
                // Absent->Live, i.e. the instant a puck connected.
                let slots = {
                    let v = shared.view.lock().unwrap();
                    v.slots.clone()
                };
                for (d, st, _, _) in slots {
                    let d8 = d as u8;
                    let now_live = st == SlotState::Live;
                    let was = live_state.insert(d8, now_live);
                    if now_live && was == Some(false) {
                        if let Some(ip) = ip_of.get(&d8) {
                            push_event(&shared, format!(
                                "{ip} back after a gap — verifying alignment"));
                            request_verify(&shared, ip);
                        }
                    }
                }

                // Teleports: a pose jumping faster than a human moves is a
                // frame event. Route it to the bridge watchdog for an
                // immediate verification — the check disambiguates a LOCAL
                // jump (re-bridge fixes it) from an Insight-world jump (only
                // a re-solve fixes it).
                for smp in ingest.live() {
                    let d = smp.packet.device as u8;
                    let p = smp.packet.pose.p;
                    if let Some(&(t_prev, p_prev)) = last_pose.get(&d) {
                        let dt = smp.packet.t_ns.saturating_sub(t_prev) as f32 / 1e9;
                        if dt > 0.004 && dt < 0.15 {
                            let dist = ((p[0] - p_prev[0]).powi(2)
                                + (p[1] - p_prev[1]).powi(2)
                                + (p[2] - p_prev[2]).powi(2))
                            .sqrt();
                            if dist > TELEPORT_MIN_DIST && dist / dt > TELEPORT_SPEED {
                                if let Some(ip) = cfg
                                    .pucks
                                    .iter()
                                    .find(|pk| pk.device == d)
                                    .map(|pk| pk.ip.clone())
                                {
                                    if shared.verify.lock().unwrap().insert(ip.clone()) {
                                        push_event(
                                            &shared,
                                            format!(
                                                "{ip} pose jumped {dist:.2} m in {:.0} ms — verifying bridge",
                                                dt * 1e3
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    last_pose.insert(d, (smp.packet.t_ns, p));
                }

                // Drift: separation change the pucks' own motion cannot
                // explain. Carrying them apart is explained; a frame jump on
                // either side is not.
                if let (Some(s), Some(sp)) = (summary.separation, summary.speed_sum) {
                    let t = epoch.elapsed().as_secs_f64();
                    if let Some(unexplained) = drift.push(t, s, sp) {
                        let mut view = shared.view.lock().unwrap();
                        if !view.drifted {
                            view.drifted = true;
                            drop(view);
                            push_event(
                                &shared,
                                format!(
                                    "separation moved {unexplained:.2} m with no motion to explain it — alignment drifted"
                                ),
                            );
                        }
                    }
                }

                // Movement history: LOCAL-frame positions, ~10 Hz, for the
                // join trigger's spread gate.
                if last_hist_push.elapsed() >= Duration::from_millis(100) {
                    last_hist_push = Instant::now();
                    let now = Instant::now();
                    let live1 = ingest.live();
                    for smp in &live1 {
                        let h = move_hist.entry(smp.packet.device as u8).or_default();
                        h.push_back((now, smp.packet.pose.p));
                        while h.front().map_or(false, |(t, _)| now - *t > JOIN_WINDOW) {
                            h.pop_front();
                        }
                    }
                    // Collect pose pairs: a fresh MPT2 (map-frame, valid) and a
                    // fresh MPT1 (Insight world) for the same puck, both under
                    // 80 ms old, while the puck is SLOW -- arrival-time pairing
                    // has up to ~50 ms of skew, and gating on speed keeps that
                    // skew's position error under a centimetre.
                    for m2 in mpt2.live() {
                        if !m2.packet.valid || m2.arrived.elapsed() > Duration::from_millis(80) {
                            continue;
                        }
                        let dev = m2.packet.device as u8;
                        if last_pair_t.get(&dev) == Some(&m2.packet.t_ns) {
                            continue; // already paired this sample
                        }
                        let Some(m1) = live1.iter().find(|s| {
                            s.packet.device as u8 == dev
                                && s.packet.valid
                                && s.arrived.elapsed() < Duration::from_millis(80)
                        }) else { continue };
                        let sp = (m1.packet.vel[0].powi(2) + m1.packet.vel[1].powi(2)
                            + m1.packet.vel[2].powi(2)).sqrt();
                        if sp > 0.3 {
                            continue;
                        }
                        last_pair_t.insert(dev, m2.packet.t_ns);
                        let q = pairs.entry(dev).or_default();
                        q.push_back((now, crate::bridge::PosePair {
                            world: m2.packet.pose,   // map frame
                            local: m1.packet.pose,   // Insight world frame
                        }));
                        while q.len() > 64
                            || q.front().map_or(false, |(t, _)| now - *t > Duration::from_secs(120))
                        {
                            q.pop_front();
                        }
                    }
                    // Solve each puck's window once a second; the result IS
                    // T_map_world and lands through the same transforms.json ->
                    // rebuild path as every other alignment source.
                    //
                    // Colocated pucks need none of it: T_map_world is identity,
                    // so a solve here would write a correction into a frame that
                    // is already correct. build_transforms ignores the file in
                    // this mode, but leaving the writer running would keep
                    // rewriting entries that describe nothing being applied.
                    if !cfg.colocated && last_paircal.elapsed() >= Duration::from_secs(1) {
                        last_paircal = Instant::now();
                        if std::env::var_os("Q2_PAIRDBG").is_some() {
                            let m2n = mpt2.live().len();
                            let m1n = live1.len();
                            let pn: Vec<usize> = pairs.values().map(|q| q.len()).collect();
                            eprintln!("[pairdbg] mpt1_live={m1n} mpt2_live={m2n} windows={pn:?}");
                        }
                        for (dev, q) in &pairs {
                            if q.len() < 8 { continue; }
                            let ps: Vec<crate::bridge::PosePair> =
                                q.iter().map(|(_, p)| *p).collect();
                            let Some(sol) = crate::bridge::solve(&ps) else { continue };
                            if sol.yaw_spread_deg > 1.5 || sol.t_spread_m > 0.10 {
                                continue; // window not coherent yet
                            }
                            let Some(ip) = ip_of.get(dev) else { continue };
                            let stored = config::load_map_transforms(&cfg.transforms)
                                .and_then(|m| m.get(ip)
                                    .map(|e| (e.yaw_deg, e.t,
                                              e.frame.as_deref() == Some("local"))));
                            let yaw_deg = sol.transform.yaw.to_degrees();
                            // An entry not yet tagged frame=local MUST be
                            // rewritten even if numerically close: the tag is
                            // what stops the output path composing the bridge
                            // on top of it.
                            let moved = stored.map_or(true, |(sy, st, tagged)| {
                                !tagged
                                    || (yaw_deg - sy).abs() > 0.1
                                    || ((sol.transform.t[0] - st[0]).powi(2)
                                        + (sol.transform.t[1] - st[1]).powi(2)
                                        + (sol.transform.t[2] - st[2]).powi(2))
                                        .sqrt() > 0.01
                            });
                            if moved
                                && apply_paircal(&cfg.transforms, ip,
                                                 yaw_deg as f64, sol.transform.t,
                                                 q.len() as i64)
                            {
                                if last_paircal_event.get(ip)
                                    .map_or(true, |t| t.elapsed() > Duration::from_secs(30))
                                {
                                    push_event(&shared, format!(
                                        "{ip} aligned by PAIR STREAM: yaw {yaw_deg:+.2}° \
                                         ({} pairs, spread {:.2}°)",
                                        sol.pairs, sol.yaw_spread_deg));
                                }
                                last_paircal_event.insert(ip.clone(), Instant::now());
                                shared.paircal.lock().unwrap()
                                    .insert(ip.clone(), Instant::now());
                            }
                        }
                    }
                }

                // JOIN: a live puck with no map transform (a new or rebooted
                // device) localizes itself as soon as it has moved enough for
                // the query to carry viewpoint diversity. This is the whole
                // point: joining the shared frame is walking, not a button.
                if last_join_check.elapsed() >= Duration::from_secs(2) {
                    last_join_check = Instant::now();
                    for (dev, hist) in &move_hist {
                        let Some(ip) = ip_of.get(dev) else { continue };
                        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                        for (_, p) in hist {
                            for a in 0..3 {
                                lo[a] = lo[a].min(p[a]);
                                hi[a] = hi[a].max(p[a]);
                            }
                        }
                        let spread = ((hi[0] - lo[0]).powi(2)
                            + (hi[2] - lo[2]).powi(2))
                        .sqrt();
                        // The hip never joins (it IS the frame). Its natural
                        // movement instead accrues the map: fold whenever it
                        // has wandered enough, rate-limited. From-zero this
                        // FOUNDS the map — no mapping step exists for the
                        // user (docs/on-device-alignment.md).
                        if hip_ip.as_deref() == Some(ip.as_str()) {
                            if hist.len() > 20
                                && spread > JOIN_SPREAD_M
                                && last_fold.map_or(true, |t: Instant| t.elapsed() > FOLD_MIN_GAP)
                                && request_kind(&shared, ip, false, false, LocKind::Fold)
                            {
                                last_fold = Some(Instant::now());
                            }
                            continue;
                        }
                        let has_t = map_t.contains_key(ip);
                        if has_t && !shared.view.lock().unwrap().drifted {
                            continue;
                        }
                        if hist.len() > 20 && spread > JOIN_SPREAD_M {
                            if request_localize(&shared, ip, !has_t, false) {
                                push_event(
                                    &shared,
                                    format!(
                                        "{ip} moving ({spread:.1} m spread) — {}",
                                        if has_t {
                                            "re-localizing to clear drift"
                                        } else {
                                            "localizing into the map (join)"
                                        }
                                    ),
                                );
                            }
                        }
                    }
                }

                // Re-solve: manual request always; drift only when opted in.
                let (drifted, bridges_ok) = {
                    let v = shared.view.lock().unwrap();
                    let ok = v.bridges.iter().all(|(_, st, _)| {
                        *st == crate::service::BridgeState::Ok
                    });
                    (v.drifted, ok)
                };
                let auto_due = cfg.auto_realign
                    && drifted
                    && bridges_ok
                    && last_auto_realign.map_or(true, |t| t.elapsed() > AUTO_REALIGN_COOLDOWN);
                let manual = shared.realign_now.swap(false, Ordering::Relaxed);
                if manual || auto_due {
                    last_auto_realign = Some(Instant::now());
                    push_event(
                        &shared,
                        format!(
                            "localize requested ({})",
                            if manual { "MANUAL button press" } else { "AUTO: drift detected" }
                        ),
                    );
                    for p in &cfg.pucks {
                        // The hip IS the shared frame (identity by definition):
                        // localizing it would try to place the reference into
                        // itself and can only add noise. Ankles join to it.
                        if p.is_hip() {
                            continue;
                        }
                        request_localize(&shared, &p.ip, !map_t.contains_key(&p.ip), manual);
                    }
                }

                let el = window.elapsed();
                if el >= Duration::from_millis(250) {
                    let mut v = shared.view.lock().unwrap();
                    v.live = summary.live.clone();
                    v.sep = summary.separation;
                    let rates: BTreeMap<u8, f32> = if el >= Duration::from_secs(1) {
                        let r = counts
                            .iter()
                            .map(|(d, c)| (*d, *c as f32 / el.as_secs_f32()))
                            .collect();
                        counts.clear();
                        window = Instant::now();
                        r
                    } else {
                        v.slots.iter().map(|(d, _, _, r)| (*d as u8, *r)).collect()
                    };
                    v.slots = ingest
                        .all()
                        .into_iter()
                        .map(|(d, st, s)| {
                            let age = s.map(|x| x.age().as_secs_f32()).unwrap_or(f32::NAN);
                            (d, st, age, rates.get(&(d as u8)).copied().unwrap_or(0.0))
                        })
                        .collect();
                    v.emitted = agg.emitted;
                    v.colocated = cfg.colocated;
                    v.jobs = shared.jobs.snapshot();
                    // In colocated mode there IS no map transform: reporting a
                    // stale stored yaw would describe a correction that is not
                    // being applied, which is worse than reporting nothing.
                    v.map_t = if cfg.colocated {
                        Vec::new()
                    } else {
                        map_t
                            .iter()
                            .map(|(ip, m)| (ip.clone(), m.yaw_deg, m.unix_time == 0))
                            .collect()
                    };
                    v.n_transforms = agg.transforms.len();
                    v.localizing =
                        shared.loc.lock().unwrap().inflight.iter().cloned().collect();
                    v.realign_running = !v.localizing.is_empty();
                    if map_t.is_empty() && !cfg.colocated {
                        v.error = Some(format!(
                            "no {} — pucks will join on first walk (or seed it)",
                            cfg.transforms
                        ));
                    }
                }
                std::thread::sleep(Duration::from_millis(4));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Fold an on-device solve back into transforms.json. Read-modify-write of
/// the raw JSON so fields this code does not model (role, residual) survive;
/// atomic replace so the service's mtime watch only ever sees whole files —
/// the update lands through the same rebuild path as any host localize.
fn apply_feedback(path: &str, ip: &str, yaw_deg: f64, t: [f32; 3], inliers: i64) -> bool {
    apply_transform_write(path, ip, yaw_deg, t, inliers, false, "on-device")
}

/// The pair stream is a PRIMARY alignment source: unlike q1verify feedback
/// (which only refines an existing join), it may create a puck's entry -- the
/// old machinery may have invalidated it, and the window solve is exactly the
/// evidence that re-establishes the alignment.
fn apply_paircal(path: &str, ip: &str, yaw_deg: f64, t: [f32; 3], pairs: i64) -> bool {
    if !apply_transform_write(path, ip, yaw_deg, t, pairs, true, "paircal") {
        return false;
    }
    // Mark the frame: this transform maps the tracker app's LOCAL directly
    // into the map (the pair stream solves against MPT1 itself), so the
    // output path must NOT compose the bridge on top.
    let Ok(raw) = std::fs::read_to_string(path) else { return true };
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&raw) {
        if let Some(e) = v.get_mut(ip) {
            e["frame"] = serde_json::json!("local");
            let tmp = format!("{path}.tmp");
            if std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default()).is_ok() {
                std::fs::rename(&tmp, path).ok();
            }
        }
    }
    true
}

fn apply_transform_write(path: &str, ip: &str, yaw_deg: f64, t: [f32; 3],
                         inliers: i64, create: bool, source: &str) -> bool {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".into());
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&raw) else { return false };
    if v.get(ip).is_none() {
        if !create {
            return false;
        }
        if let Some(o) = v.as_object_mut() {
            o.insert(ip.into(), serde_json::json!({"role": "ankle"}));
        }
    }
    let Some(entry) = v.get_mut(ip) else { return false };
    entry["source"] = serde_json::json!(source);
    entry["yaw_deg"] = serde_json::json!(yaw_deg);
    entry["t"] = serde_json::json!(t);
    entry["inliers"] = serde_json::json!(inliers);
    entry["unix_time"] = serde_json::json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()));
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default()).is_err() {
        return false;
    }
    std::fs::rename(&tmp, path).is_ok()
}

/// The stored transform plus where it came from, so the poll can tell a
/// high-confidence host join (the anchor) from its own on-device polish.
fn stored_entry(path: &str, ip: &str) -> Option<(f64, [f32; 3], bool)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let e = v.get(ip)?;
    let y = e["yaw_deg"].as_f64()?;
    let a = e["t"].as_array().filter(|a| a.len() == 3)?;
    let t = [a[0].as_f64().unwrap_or(0.0) as f32,
             a[1].as_f64().unwrap_or(0.0) as f32,
             a[2].as_f64().unwrap_or(0.0) as f32];
    Some((y, t, e["source"].as_str() == Some("on-device")))
}

/// Drop a puck's stored T_map_world. Called when its Insight frame jumps: the
/// transform describes a frame that no longer exists, so every pose built from
/// it is wrong. build_transforms then skips the puck entirely -- "an unaligned
/// pose in an aligned stream is worse than a missing one" -- until a localize
/// writes a fresh one. Never applied to the hip, whose transform is identity by
/// definition.
fn invalidate_transform(path: &str, ip: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else { return false };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&raw) else { return false };
    let Some(o) = v.as_object_mut() else { return false };
    if o.remove(ip).is_none() {
        return false;
    }
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default()).is_err() {
        return false;
    }
    std::fs::rename(&tmp, path).is_ok()
}

fn median(xs: &mut Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    xs[xs.len() / 2]
}

fn spawn_verifier_poll(cfg: Config, shared: Arc<Shared>) -> Result<(), String> {
    std::thread::Builder::new()
        .name("verify-poll".into())
        .spawn(move || {
            // Two consecutive refutes before acting: one snapshot can catch a
            // bad instant (a hand across the cameras), and re-localization is
            // not free. The on-puck verifier already gates on stillness, so a
            // second refute means the frame really moved.
            let mut refutes: BTreeMap<String, u32> = BTreeMap::new();
            // Last seen q1serve capture counter per puck, for stall detection.
            let mut frames: BTreeMap<String, u64> = BTreeMap::new();
            // Convergence feedback: recent on-device solves per ankle, and the
            // report stamp last ingested (so one file is never counted twice).
            let mut feedback: BTreeMap<String, Vec<(f64, [f32; 3], i64)>> = BTreeMap::new();
            let mut feedback_seen: BTreeMap<String, i64> = BTreeMap::new();
            // The host cold-start solution per puck: the high-confidence anchor
            // (it pools ~12 walked viewpoints) that on-device refinement is
            // allowed to polish but never to walk away from. Seeded from any
            // stored entry that is not itself on-device output.
            let mut anchors: BTreeMap<String, (f64, [f32; 3])> = BTreeMap::new();
            if let Ok(raw) = std::fs::read_to_string(&cfg.transforms) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(o) = v.as_object() {
                        for (ip, e) in o {
                            if e["source"].as_str() == Some("on-device") { continue; }
                            if let (Some(y), Some(t)) =
                                (e["yaw_deg"].as_f64(), e["t"].as_array().filter(|a| a.len() == 3))
                            {
                                anchors.insert(ip.clone(), (y, [
                                    t[0].as_f64().unwrap_or(0.0) as f32,
                                    t[1].as_f64().unwrap_or(0.0) as f32,
                                    t[2].as_f64().unwrap_or(0.0) as f32]));
                            }
                        }
                    }
                }
            }
            loop {
                std::thread::sleep(Duration::from_secs(15));
                // q1serve can stall with its process still alive, still
                // answering 200, still holding port 8080 -- it just serves the
                // last frame it ever captured, forever. Nothing downstream
                // notices: the grab throws every stale snapshot out in its
                // bracketing filter and reports "did not move enough", so the
                // puck looks like it is simply sitting still no matter how far
                // it is walked. Only a real capture counter shows it; see
                // fleet::capture_counter for why the obvious fields do not.
                for p in &cfg.pucks {
                    // Ask what the snapshot endpoint is actually SERVING, not
                    // whether counters move: a q1serve can climb `skipped`
                    // forever while publishing one frozen frame, and that mode
                    // silently starves every consumer (see snapshot_age_secs).
                    let Some(age) = fleet::snapshot_age_secs(&p.ip) else { continue };
                    if age > 5.0 {
                        push_event(&shared, format!(
                            "{} q1serve is serving a {age:.0}s-old frame (frozen) — \
                             restarting; map growth and on-device solves are blind until it does",
                            p.ip));
                        fleet::restart_serving(&p.ip);
                    }
                }
                let _ = &mut frames;
                for p in &cfg.pucks {
                    // A rebooted PANEL-LESS puck comes back at 0DOF until the
                    // synthetic-TE module is reloaded, and it has no display
                    // to show anything is wrong. Cheap no-op for panel pucks.
                    let st = fleet::status(&p.ip);
                    if st.reachable && fleet::ensure_sync_module(&p.ip, st.tracking_valid) {
                        push_event(&shared, format!(
                            "{} was 0DOF after reboot — synthetic-TE module reloaded", p.ip));
                    }
                    // A rebooted puck brings its tracker back by itself but not
                    // the verifier (a bare binary, no boot receiver). Without
                    // this the fleet looks healthy while nothing is verifying.
                    let want_localize = !p.is_hip();
                    if !fleet::verifier_running(&p.ip) {
                        if fleet::restart_verifier(&p.ip, want_localize) {
                            push_event(&shared, format!(
                                "{} {} was not running — restarted", p.ip,
                                if want_localize { "on-device localizer" } else { "verifier" }));
                        }
                        continue;
                    }
                    // Role and running mode must agree, or an ankle keeps
                    // verifying (no feedback ever) / a hip keeps localizing
                    // (against itself). Kill and restart in the right mode.
                    if fleet::verifier_localizing(&p.ip) != want_localize {
                        fleet::stop_verifier(&p.ip);
                        if fleet::restart_verifier(&p.ip, want_localize) {
                            push_event(&shared, format!(
                                "{} restarted in {} mode to match its role", p.ip,
                                if want_localize { "localize" } else { "verify" }));
                        }
                        continue;
                    }
                    // While the pair stream owns this puck's alignment, the
                    // q1verify feedback loop stands down -- two writers with
                    // different noise characteristics would fight.
                    let pair_owned = shared.paircal.lock().unwrap()
                        .get(&p.ip)
                        .map_or(false, |t| t.elapsed() < Duration::from_secs(60));
                    if want_localize && pair_owned {
                        continue;  // the pair stream owns this puck entirely
                    }
                    if want_localize {
                        // ---- ankle: the convergence feedback loop ----
                        match fleet::localize_result(&p.ip) {
                            Some((v, _, _, _, _)) if v == "no-input" => {
                                if fleet::ensure_serving(&p.ip) {
                                    push_event(&shared, format!(
                                        "{} localizer had no frames — restarted q1serve", p.ip));
                                }
                            }
                            Some((v, inl, yaw, t, stamp))
                                if v == "localized" && inl >= FEEDBACK_MIN_INLIERS =>
                            {
                                refutes.remove(&p.ip);
                                let seen = feedback_seen.get(&p.ip).copied().unwrap_or(0);
                                if stamp > seen {
                                    feedback_seen.insert(p.ip.clone(), stamp);
                                    let q = feedback.entry(p.ip.clone()).or_default();
                                    q.push((yaw, t, inl));
                                    if q.len() > 8 { q.remove(0); }
                                    if q.len() >= 3 {
                                        let my = median(&mut q.iter().map(|e| e.0).collect());
                                        let mt = [
                                            median(&mut q.iter().map(|e| e.1[0] as f64).collect()) as f32,
                                            median(&mut q.iter().map(|e| e.1[1] as f64).collect()) as f32,
                                            median(&mut q.iter().map(|e| e.1[2] as f64).collect()) as f32,
                                        ];
                                        let minl = q.iter().map(|e| e.2).min().unwrap_or(0);
                                        // A host localize rewrites this entry
                                        // without the on-device marker; that is a
                                        // fresh anchor and replaces the old one,
                                        // or refinement would be measured against
                                        // a transform nobody uses any more.
                                        let stored = stored_entry(&cfg.transforms, &p.ip);
                                        if let Some((sy, st, from_device)) = stored {
                                            if !from_device {
                                                anchors.insert(p.ip.clone(), (sy, st));
                                            }
                                            let dy = (my - sy).abs();
                                            let dt = ((mt[0] - st[0]).powi(2)
                                                + (mt[1] - st[1]).powi(2)
                                                + (mt[2] - st[2]).powi(2))
                                            .sqrt();
                                            let busy = shared.loc.lock().unwrap()
                                                .inflight.contains(&p.ip);
                                            // Below the noise floor there is
                                            // nothing to apply; churn would
                                            // just jitter the output.
                                            if !busy && (dy > 0.2 || dt > 0.03) {
                                                // Step a FRACTION of the way and cap it:
                                                // replacing outright turns the stored
                                                // transform into a random walk over
                                                // single-frame solves.
                                                let clamp = |d: f64, m: f64| d.clamp(-m, m);
                                                let ny = sy + clamp((my - sy) * FEEDBACK_GAIN,
                                                                    FEEDBACK_MAX_STEP_DEG);
                                                let mut nt = [0f32; 3];
                                                for k in 0..3 {
                                                    let d = (mt[k] - st[k]) as f64 * FEEDBACK_GAIN;
                                                    nt[k] = st[k]
                                                        + clamp(d, FEEDBACK_MAX_STEP_M) as f32;
                                                }
                                                // Anchor check: refinement that has wandered
                                                // this far from the host join is not
                                                // converging, and must not keep walking.
                                                let anchor = anchors.get(&p.ip).copied();
                                                let strayed = anchor.map_or(false, |(ay, at): (f64, [f32; 3])| {
                                                    (ny - ay).abs() > FEEDBACK_MAX_DRIFT_DEG
                                                        || (((nt[0] - at[0]).powi(2)
                                                            + (nt[1] - at[1]).powi(2)
                                                            + (nt[2] - at[2]).powi(2))
                                                            .sqrt() as f64)
                                                            > FEEDBACK_MAX_DRIFT_M
                                                });
                                                if strayed {
                                                    push_event(&shared, format!(
                                                        "{} on-device refinement drifted past the \
                                                         host anchor — holding the stored transform \
                                                         and re-localizing on next walk", p.ip));
                                                    shared.view.lock().unwrap().drifted = true;
                                                    request_localize(&shared, &p.ip, false, false);
                                                    feedback.remove(&p.ip);
                                                } else if apply_feedback(&cfg.transforms, &p.ip,
                                                                  ny, nt, minl) {
                                                    push_event(&shared, format!(
                                                        "{} refined ON-DEVICE: yaw {:+.2}° \
                                                         (Δ{:.2}° / Δ{:.0} mm of a {:.2}°/{:.0} mm \
                                                         measurement, {} inliers)",
                                                        p.ip, ny, ny - sy,
                                                        ((nt[0] - st[0]).powi(2)
                                                            + (nt[1] - st[1]).powi(2)
                                                            + (nt[2] - st[2]).powi(2))
                                                            .sqrt() * 1000.0,
                                                        dy, dt * 1000.0, minl));
                                                    fleet::push_map_transform(
                                                        &p.ip, ny as f32, nt).ok();
                                                    feedback.remove(&p.ip);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            // `provisional` means one viewpoint; that is normal
                            // for a parked puck and not evidence of anything, so
                            // it neither refines nor accuses. It is skipped here
                            // and falls through to the catch-all.
                            Some((v, _, _, _, _)) if v == "provisional" => {}
                            Some((v, _, _, _, stamp)) if v == "failed" => {
                                // Count only NEW failed reports; a stale file
                                // must not accumulate suspicion forever.
                                let seen = feedback_seen.get(&p.ip).copied().unwrap_or(0);
                                if stamp > seen {
                                    feedback_seen.insert(p.ip.clone(), stamp);
                                    let n = refutes.entry(p.ip.clone()).or_insert(0);
                                    *n += 1;
                                    if *n == 3 {
                                        // Repeated failure with plenty of MATCHES but
                                        // no inliers means the seed is wrong, not that
                                        // the view is poor -- the puck's Insight frame
                                        // moved under us (a relocalization after a
                                        // tracking loss). The bridge cannot reveal it:
                                        // if the tracker session restarted too, the
                                        // watchdog re-bridges cleanly and consistency
                                        // looks fine while T_map_world is stale. So do
                                        // what a detected frame jump does -- stop
                                        // emitting a wrong limb, and cold-start.
                                        let dropped =
                                            invalidate_transform(&cfg.transforms, &p.ip);
                                        push_event(&shared, format!(
                                            "{} cannot re-solve against the hip map (3 \
                                             consecutive fails) — its Insight frame moved{}; \
                                             WALK IT to re-join (sitting still cannot fix this)",
                                            p.ip,
                                            if dropped { ", output held" } else { "" }));
                                        shared.view.lock().unwrap().drifted = true;
                                        shared.rebuild.store(true, Ordering::Relaxed);
                                        // Cold start: a moved frame makes the old
                                        // transform useless as a seed.
                                        request_localize(&shared, &p.ip, true, false);
                                        feedback.remove(&p.ip);
                                    }
                                }
                            }
                            _ => {}  // moving / unknown / absent: hold
                        }
                        continue;
                    }
                    match fleet::verify_verdict(&p.ip) {
                        // The verifier is alive but blind: its frame source
                        // (q1serve) died, typically to the same reboot. Nothing
                        // else repairs this -- the restart path only runs when
                        // the VERIFIER is dead -- so a puck would sit reporting
                        // no-input forever while looking alive.
                        Some((v, _, _)) if v == "no-input" => {
                            if fleet::ensure_serving(&p.ip) {
                                push_event(&shared, format!(
                                    "{} verifier had no frames — restarted q1serve", p.ip));
                            }
                        }
                        Some((v, _, med)) if v == "refuted" => {
                            let n = refutes.entry(p.ip.clone()).or_insert(0);
                            *n += 1;
                            if *n == 2 {
                                // Only this branch runs for the HIP, and the hip
                                // must never localize: it IS the map frame, so
                                // "localizing" it solves it into its own map and
                                // writes a non-identity transform, silently
                                // moving the frame every other puck is aligned
                                // to. A refuted hip means the MAP has fallen
                                // behind the room, so fold its current view in
                                // instead.
                                if p.is_hip() {
                                    push_event(&shared, format!(
                                        "{} (hip) self-check refuted ({med:.1}° off) — the map \
                                         has fallen behind the room; folding current view",
                                        p.ip));
                                    request_kind(&shared, &p.ip, false, false, LocKind::Fold);
                                } else {
                                    push_event(&shared, format!(
                                        "{} self-check refuted twice ({med:.1}° off) — \
                                         re-localizing on next walk", p.ip));
                                    shared.view.lock().unwrap().drifted = true;
                                    request_localize(&shared, &p.ip,
                                        !config::load_map_transforms(&cfg.transforms)
                                            .map_or(false, |m| m.contains_key(&p.ip)),
                                        false);
                                }
                                refutes.remove(&p.ip);
                            }
                        }
                        Some((v, _, _)) if v == "confirmed" => {
                            refutes.remove(&p.ip);
                        }
                        _ => {}  // unknown / moving / absent: hold the count
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn spawn_mapd(cfg: Config, shared: Arc<Shared>) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<LocReq>();
    shared.loc.lock().unwrap().tx = Some(tx);
    std::thread::Builder::new()
        .name("mapd".into())
        .spawn(move || {
            let mut child: Option<(Child, std::process::ChildStdin, BufReader<std::process::ChildStdout>)> = None;
            let spawn = |shared: &Shared| -> Option<(Child, std::process::ChildStdin, BufReader<std::process::ChildStdout>)> {
                if !std::path::Path::new(&cfg.map).join("landmarks.npz").exists() {
                    return None;
                }
                match Command::new(".venv/bin/python")
                    .args(["tools/q1mapd.py", "daemon", "--map", &cfg.map,
                           "--transforms", &cfg.transforms])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(mut c) => {
                        let stdin = c.stdin.take().unwrap();
                        let stdout = BufReader::new(c.stdout.take().unwrap());
                        push_event(shared, "map daemon started".into());
                        Some((c, stdin, stdout))
                    }
                    Err(e) => {
                        push_event(shared, format!("cannot start q1mapd: {e}"));
                        None
                    }
                }
            };
            for req in rx {
                let done = |shared: &Shared, ip: &str| {
                    shared.loc.lock().unwrap().inflight.remove(ip);
                };
                // A Fold with no map yet FOUNDS it via the CLI (the daemon
                // can only start once landmarks.npz exists) — the whole point
                // of accumulation is that no one runs a bootstrap by hand.
                let founding = req.kind == LocKind::Fold
                    && !std::path::Path::new(&cfg.map).join("landmarks.npz").exists();
                if child.is_none() && !founding {
                    child = spawn(&shared);
                    if child.is_none() {
                        if req.kind != LocKind::Fold {
                            push_event(&shared, format!(
                                "no map yet at {} — it founds itself when the hip moves", cfg.map));
                        }
                        done(&shared, &req.ip);
                        continue;
                    }
                }
                // 1. Grab a query. A verify grab is a quick stationary glance
                //    (2 snapshots, ~5 s); a localize grab needs walking and
                //    fails safe if the puck stops moving.
                let mode = if req.kind == LocKind::Verify { "verify" } else { "localize" };
                let dir_tag = match req.kind {
                    LocKind::Verify => "verify",
                    LocKind::Localize => "localize",
                    LocKind::Fold => "fold",
                };
                // Distinct dir per kind: a verify's 2-snapshot glance used to
                // land on the same path as a localize grab and silently
                // destroy it -- which cost the post-mortem evidence the first
                // time a localization asymmetry needed diagnosing.
                let out = format!("/tmp/q2slam_{}_{}", dir_tag, req.ip.replace('.', "_"));
                let grab = Command::new(".venv/bin/python")
                    .args(["tools/q1grab.py", &req.ip, "--out", &out, "--mode", mode])
                    .output();
                let grab_ok = grab
                    .as_ref()
                    .ok()
                    .and_then(|o| {
                        serde_json::from_slice::<serde_json::Value>(&o.stdout).ok()
                    })
                    .map_or(false, |v| v["ok"].as_bool().unwrap_or(false));
                if !grab_ok {
                    // A fold's movement can simply stop between the trigger
                    // and the grab; that is normal, not news — it re-fires.
                    if req.kind != LocKind::Fold {
                        push_event(&shared, if req.kind == LocKind::Verify {
                            format!("{}: verification grab failed (snapshots unavailable)", req.ip)
                        } else {
                            format!("{}: grab yielded too little movement — will retry when walking", req.ip)
                        });
                    }
                    done(&shared, &req.ip);
                    continue;
                }

                if req.kind == LocKind::Verify {
                    let request = serde_json::json!({
                        "cmd": "verify", "puck": req.ip, "dataset": out,
                    });
                    let reply: Option<serde_json::Value> = (|| {
                        let (_, stdin, stdout) = child.as_mut()?;
                        writeln!(stdin, "{request}").ok()?;
                        let mut line = String::new();
                        stdout.read_line(&mut line).ok()?;
                        if line.is_empty() { return None; }
                        serde_json::from_str(&line).ok()
                    })();
                    match reply.as_ref().and_then(|r| r["verdict"].as_str()) {
                        Some("confirmed") => push_event(&shared, format!(
                            "{} alignment CONFIRMED against the map ({} matches, {:.1}° median)",
                            req.ip,
                            reply.as_ref().unwrap()["matches"].as_i64().unwrap_or(0),
                            reply.as_ref().unwrap()["median_deg"].as_f64().unwrap_or(99.0))),
                        Some("refuted") => {
                            let med = reply.as_ref().unwrap()["median_deg"]
                                .as_f64()
                                .unwrap_or(99.0);
                            let common_mode = {
                                let mut v = shared.verdicts.lock().unwrap();
                                let similar = v.iter().any(|(ip, (m, t))| {
                                    ip != &req.ip
                                        && t.elapsed() < Duration::from_secs(600)
                                        && (m - med).abs() < 1.5
                                });
                                v.insert(req.ip.clone(), (med, Instant::now()));
                                similar
                            };
                            if common_mode {
                                push_event(&shared, format!(
                                    "{} off the map by {med:.1}° — SAME offset as its peer:                                      common-mode map drift, relative alignment intact.                                      Refreshes on next walk; playspace cal absorbs it meanwhile.",
                                    req.ip));
                            } else {
                                push_event(&shared, format!(
                                    "{} alignment REFUTED by the map ({med:.1}° off) —                                      walk the puck to re-localize", req.ip));
                            }
                            // Either way, arm re-localization on movement.
                            shared.view.lock().unwrap().drifted = true;
                        }
                        Some(_) => push_event(&shared, format!(
                            "{}: alignment unverifiable from here (scene coverage) —                              will confirm on movement", req.ip)),
                        None => {
                            push_event(&shared, "map daemon died — restarting".into());
                            if let Some((mut c, _, _)) = child.take() {
                                c.kill().ok();
                                c.wait().ok();
                            }
                        }
                    }
                    done(&shared, &req.ip);
                    continue;
                }
                if req.kind == LocKind::Fold {
                    if founding {
                        let r = Command::new(".venv/bin/python")
                            .args(["tools/q1mapd.py", "bootstrap", "--map", &cfg.map,
                                   "--captures", &out, "--stride", "1"])
                            .output();
                        let n = r.ok()
                            .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
                            .and_then(|v| v["landmarks"].as_i64())
                            .unwrap_or(0);
                        push_event(&shared, if n > 0 {
                            format!("hip map FOUNDED from natural movement ({n} landmarks) —                                      ankles join as they move")
                        } else {
                            "hip map founding produced nothing usable — retrying on movement".into()
                        });
                        done(&shared, &req.ip);
                        continue;
                    }
                    let ankles: Vec<String> = cfg.pucks.iter()
                        .filter(|p| !p.is_hip())
                        .map(|p| p.ip.clone())
                        .collect();
                    let request = serde_json::json!({
                        "cmd": "fold", "dataset": out, "deploy_to": ankles,
                    });
                    let reply: Option<serde_json::Value> = (|| {
                        let (_, stdin, stdout) = child.as_mut()?;
                        writeln!(stdin, "{request}").ok()?;
                        let mut line = String::new();
                        stdout.read_line(&mut line).ok()?;
                        if line.is_empty() { return None; }
                        serde_json::from_str(&line).ok()
                    })();
                    match reply {
                        Some(r) if r["ok"].as_bool().unwrap_or(false) => {
                            push_event(&shared, format!(
                                "hip map now {} landmarks (+{} from natural movement);                                  subset refreshed on {} puck(s)",
                                r["landmarks"].as_i64().unwrap_or(0),
                                r["grew"].as_i64().unwrap_or(0),
                                r["deployed"].as_array().map_or(0, |a| a.len())));
                        }
                        Some(r) => push_event(&shared, format!(
                            "hip map fold failed: {}",
                            r["error"].as_str().unwrap_or("?"))),
                        None => {
                            push_event(&shared, "map daemon died — restarting".into());
                            if let Some((mut c, _, _)) = child.take() {
                                c.kill().ok();
                                c.wait().ok();
                            }
                        }
                    }
                    done(&shared, &req.ip);
                    continue;
                }
                // 2. Ask the daemon. Seed from the current store unless this
                //    is a cold start (new puck, no basin to guide from).
                let seed = config::load_map_transforms(&cfg.transforms)
                    .and_then(|m| m.get(&req.ip).map(|t| (t.yaw_deg, t.t)));
                let coldstart = req.coldstart || seed.is_none();
                let request = serde_json::json!({
                    "cmd": "localize", "puck": req.ip, "dataset": out,
                    "coldstart": coldstart,
                    "seed": seed.map(|(y, t)| serde_json::json!({"yaw_deg": y, "t": t})),
                });
                let reply: Option<serde_json::Value> = (|| {
                    let (_, stdin, stdout) = child.as_mut()?;
                    writeln!(stdin, "{request}").ok()?;
                    let mut line = String::new();
                    stdout.read_line(&mut line).ok()?;
                    if line.is_empty() {
                        return None; // EOF: daemon died
                    }
                    serde_json::from_str(&line).ok()
                })();
                // While the pair stream owns this puck, a host localize may
                // still run (it was queued before ownership), but its WRITE
                // must not stomp the live pair alignment -- the daemon already
                // wrote transforms.json, so restore ownership by letting the
                // next window solve overwrite; here we just skip the seed push
                // and drop our claim to the event. (Deleting this path
                // entirely is the endgame; the guard keeps the A/B honest.)
                let pair_owned_now = shared.paircal.lock().unwrap()
                    .get(&req.ip)
                    .map_or(false, |t| t.elapsed() < Duration::from_secs(60));
                match reply {
                    Some(ref r) if r["accepted"].as_bool().unwrap_or(false) && pair_owned_now => {
                        push_event(&shared, format!(
                            "{}: host localize finished but the pair stream owns this puck — result ignored",
                            req.ip));
                    }
                    Some(r) if r["accepted"].as_bool().unwrap_or(false) => {
                        push_event(&shared, format!(
                            "{} localized: yaw {:+.2}° ({} inliers, {:.2}° residual{})",
                            req.ip,
                            r["yaw_deg"].as_f64().unwrap_or(f64::NAN),
                            r["inliers"].as_i64().unwrap_or(0),
                            r["residual_deg"].as_f64().unwrap_or(f64::NAN),
                            if r["contribute_ok"].as_bool().unwrap_or(false)
                                { "" } else { " — use-grade, not map-grade" }));
                        // Hand the puck the seed for its ON-DEVICE re-join, so
                        // drift recovery during play runs on the 835 instead of
                        // the VR host (docs/on-device-alignment.md). The map
                        // subset + verifycfg are deployed by q1hipref at join;
                        // this keeps the seed in step with every new solve.
                        if let (Some(y), Some(t)) = (
                            r["yaw_deg"].as_f64(),
                            r["t"].as_array().filter(|a| a.len() == 3)) {
                            let tt = [t[0].as_f64().unwrap_or(0.0) as f32,
                                      t[1].as_f64().unwrap_or(0.0) as f32,
                                      t[2].as_f64().unwrap_or(0.0) as f32];
                            fleet::push_map_transform(&req.ip, y as f32, tt).ok();
                        }
                        // transforms.json was written by the daemon; the
                        // mtime watch applies it through the rebuild path.
                    }
                    Some(r) => {
                        push_event(&shared, format!(
                            "{}: localization rejected ({}) — keeping current transform",
                            req.ip,
                            r["reject_reason"].as_str().map(String::from)
                                .unwrap_or_else(|| format!(
                                    "{} inliers", r["inliers"].as_i64().unwrap_or(0)))));
                    }
                    None => {
                        push_event(&shared, "map daemon died — restarting".into());
                        if let Some((mut c, _, _)) = child.take() {
                            c.kill().ok();
                            c.wait().ok();
                        }
                    }
                }
                done(&shared, &req.ip);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---- bridge watchdog -------------------------------------------------------

struct PuckWatch {
    ip: String,
    device: Device,
    history: VecDeque<(Instant, [f32; 3])>,
    suspect: u32,
    last_check: Instant,
    state: BridgeState,
    yaw_deg: f32,
}

fn spawn_bridge_watchdog(
    cfg: Config,
    ingest: Arc<Ingest>,
    shared: Arc<Shared>,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("bridge-watch".into())
        .spawn(move || {
            let mut watches: Vec<PuckWatch> = cfg
                .pucks
                .iter()
                .filter_map(|p| {
                    Some(PuckWatch {
                        ip: p.ip.clone(),
                        device: Device::from_u8(p.device)?,
                        history: VecDeque::new(),
                        suspect: 0,
                        last_check: Instant::now() - BRIDGE_CHECK_EVERY,
                        state: BridgeState::Missing,
                        yaw_deg: f32::NAN,
                    })
                })
                .collect();
            let mut bridges = config::load_bridges(&cfg.bridge).unwrap_or_default();
            for w in &mut watches {
                if let Some(b) = bridges.get(&w.ip) {
                    w.state = BridgeState::Ok;
                    w.yaw_deg = b.yaw_deg;
                }
            }

            loop {
                let force = shared.bridge_now.swap(false, Ordering::Relaxed);
                let verify_now: std::collections::BTreeSet<String> =
                    std::mem::take(&mut *shared.verify.lock().unwrap());
                for w in &mut watches {
                    let verify_hit = verify_now.contains(&w.ip);
                    let sample = ingest.sample(w.device);
                    let Some(s) = sample else { continue };
                    if !s.packet.valid || s.age() > Duration::from_millis(300) {
                        continue;
                    }
                    let now = Instant::now();
                    w.history.push_back((now, s.packet.pose.p));
                    while w.history.front().map_or(false, |(t, _)| now - *t > STILL_RETAIN) {
                        w.history.pop_front();
                    }

                    let vel = (s.packet.vel[0].powi(2)
                        + s.packet.vel[1].powi(2)
                        + s.packet.vel[2].powi(2))
                    .sqrt();
                    let ang = (s.packet.angvel[0].powi(2)
                        + s.packet.angvel[1].powi(2)
                        + s.packet.angvel[2].powi(2))
                    .sqrt();
                    let span_ok =
                        w.history.front().map_or(false, |(t, _)| now - *t >= STILL_SPAN);
                    let wander = w
                        .history
                        .iter()
                        .map(|(_, p)| {
                            let q = w.history.back().unwrap().1;
                            ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2))
                                .sqrt()
                        })
                        .fold(0.0f32, f32::max);
                    let still =
                        span_ok && vel < STILL_VEL && ang < STILL_ANGVEL && wander < STILL_POS_DELTA;

                    if force {
                        w.suspect = 2;
                    }
                    let needs = w.state == BridgeState::Missing || w.suspect >= 2;

                    if needs {
                        if !still {
                            w.state = if w.state == BridgeState::Missing {
                                BridgeState::Missing
                            } else {
                                BridgeState::WaitingStill
                            };
                            continue;
                        }
                        w.state = BridgeState::Solving;
                        // Show "solving" immediately; the full publish happens
                        // at the end of the sweep.
                        if let Ok(mut v) = shared.view.lock() {
                            if let Some(e) = v.bridges.iter_mut().find(|(ip, _, _)| ip == &w.ip) {
                                e.1 = BridgeState::Solving;
                            }
                        }
                        let mut pairs = Vec::new();
                        for _ in 0..10 {
                            let Some(world) = fleet::dumpsys_pose(&w.ip) else { continue };
                            let Some(s2) = ingest.sample(w.device) else { continue };
                            if !s2.packet.valid || s2.age() > Duration::from_millis(120) {
                                continue;
                            }
                            pairs.push(PosePair { world, local: s2.packet.pose });
                        }
                        match bridge::solve(&pairs) {
                            Some(sol) if sol.yaw_spread_deg < BRIDGE_MAX_SPREAD_DEG => {
                                bridges.insert(
                                    w.ip.clone(),
                                    config::BridgeEntry {
                                        yaw_deg: sol.transform.yaw.to_degrees(),
                                        t: sol.transform.t,
                                        yaw_spread_deg: sol.yaw_spread_deg,
                                        unix_time: SystemTime::now()
                                            .duration_since(SystemTime::UNIX_EPOCH)
                                            .map(|d| d.as_secs())
                                            .unwrap_or(0),
                                    },
                                );
                                save_bridges(&cfg.bridge, &bridges);
                                w.state = BridgeState::Ok;
                                w.yaw_deg = sol.transform.yaw.to_degrees();
                                w.suspect = 0;
                                shared.rebuild.store(true, Ordering::Relaxed);
                                push_event(
                                    &shared,
                                    format!(
                                        "auto-bridged {} (yaw {:+.2}°, spread {:.2}°)",
                                        w.ip,
                                        sol.transform.yaw.to_degrees(),
                                        sol.yaw_spread_deg
                                    ),
                                );
                            }
                            Some(sol) => {
                                // Passable arithmetic, but the puck was moving:
                                // accepting it would bake that error into the
                                // frame. Wait for a calmer window instead.
                                w.state = BridgeState::WaitingStill;
                                push_event(&shared, format!(
                                    "{} bridge solve too loose ({:.2}° spread) — \
                                     holding for a still moment", w.ip, sol.yaw_spread_deg));
                            }
                            _ => {
                                // Moved mid-solve, or the tracker vanished.
                                // Stay suspect; the next still window retries.
                                w.state = BridgeState::WaitingStill;
                            }
                        }
                        continue;
                    }

                    // Cheap consistency check: does the current bridge still
                    // map LOCAL onto the Insight world pose?
                    if std::env::var_os("Q2_DEBUG").is_some()
                        && now - w.last_check >= BRIDGE_CHECK_EVERY
                    {
                        eprintln!(
                            "[dbg {}] state={:?} still={still} (span={span_ok} vel={vel:.3} ang={ang:.3} wander={wander:.3}) suspect={}",
                            w.ip, w.state, w.suspect
                        );
                    }
                    // A teleport-triggered verification bypasses the interval
                    // and accepts slow motion — the jump is the interesting
                    // instant, and waiting minutes for perfect stillness would
                    // let a broken bridge keep emitting garbage.
                    let relaxed_still = vel < 0.3 && ang < 0.5;
                    if w.state == BridgeState::Ok
                        && ((still && now - w.last_check >= BRIDGE_CHECK_EVERY)
                            || (verify_hit && relaxed_still))
                    {
                        w.last_check = now;
                        if let (Some(b), Some(world)) =
                            (bridges.get(&w.ip), fleet::dumpsys_pose(&w.ip))
                        {
                            let tr = Frame4Dof { yaw: b.yaw_deg.to_radians(), t: b.t };
                            let pred = tr.apply_point(s.packet.pose.p);
                            let dp = ((pred[0] - world.p[0]).powi(2)
                                + (pred[1] - world.p[1]).powi(2)
                                + (pred[2] - world.p[2]).powi(2))
                            .sqrt();
                            let dyaw = wrap(relative_yaw(world.q, s.packet.pose.q) - tr.yaw)
                                .abs()
                                .to_degrees();
                            let jumped = dp > FRAME_JUMP_POS_M || dyaw > FRAME_JUMP_YAW_DEG;
                            if jumped {
                                // A frame jump is unambiguous, so act on the
                                // first sighting instead of waiting out the
                                // suspect count: the stored transform is
                                // already wrong and every pose emitted until
                                // it is replaced is wrong with it.
                                w.state = BridgeState::Suspect;
                                w.suspect = 0;
                                let is_hip = shared.hip_ip.as_deref() == Some(w.ip.as_str());
                                if is_hip {
                                    // The map lives in the HIP's world frame, so
                                    // a hip jump moves the shared frame itself:
                                    // every ankle's transform is stale at once.
                                    push_event(&shared, format!(
                                        "{} (hip) Insight frame JUMPED ({dp:.2} m / {dyaw:.1}°) — \
                                         the shared frame moved; re-bridging, folding the map, \
                                         and re-joining every ankle", w.ip));
                                    request_kind(&shared, &w.ip, false, false, LocKind::Fold);
                                    shared.view.lock().unwrap().drifted = true;
                                    for q in &cfg.pucks {
                                        if !q.is_hip() {
                                            request_localize(&shared, &q.ip, true, false);
                                        }
                                    }
                                } else {
                                    // Stop emitting this puck until it re-joins:
                                    // its transform maps from a frame that no
                                    // longer exists, so every pose from it is
                                    // wrong, and a wrong limb is worse than a
                                    // missing one.
                                    let dropped = invalidate_transform(&cfg.transforms, &w.ip);
                                    push_event(&shared, format!(
                                        "{} Insight frame JUMPED ({dp:.2} m / {dyaw:.1}°) — \
                                         its stored alignment no longer applies{}; re-bridging \
                                         and re-joining on next movement", w.ip,
                                        if dropped { ", output held" } else { "" }));
                                    shared.view.lock().unwrap().drifted = true;
                                    shared.rebuild.store(true, Ordering::Relaxed);
                                    // Cold-start: the old transform is not a
                                    // valid seed for a frame that moved.
                                    request_localize(&shared, &w.ip, true, false);
                                }
                            } else if dp > BRIDGE_POS_TOL || dyaw > BRIDGE_YAW_TOL {
                                w.suspect += 1;
                                if w.suspect >= 2 {
                                    w.state = BridgeState::Suspect;
                                    push_event(
                                        &shared,
                                        format!(
                                            "{} bridge stale ({dp:.2} m / {dyaw:.1}°) — re-bridging when still",
                                            w.ip
                                        ),
                                    );
                                }
                            } else {
                                w.suspect = 0;
                            }
                        }
                    }
                }
                publish_bridge_states(&shared, &watches);
                std::thread::sleep(Duration::from_millis(500));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn publish_bridge_states(shared: &Shared, watches: &[PuckWatch]) {
    shared.view.lock().unwrap().bridges =
        watches.iter().map(|w| (w.ip.clone(), w.state, w.yaw_deg)).collect();
}

fn save_bridges(path: &str, bridges: &BTreeMap<String, config::BridgeEntry>) {
    let map: serde_json::Map<String, serde_json::Value> = bridges
        .iter()
        .map(|(ip, b)| {
            (
                ip.clone(),
                serde_json::json!({
                    "yaw_deg": b.yaw_deg,
                    "t": b.t,
                    "yaw_spread_deg": b.yaw_spread_deg,
                    "unix_time": b.unix_time,
                }),
            )
        })
        .collect();
    std::fs::write(path, serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap())
        .ok();
}

#[cfg(test)]
mod tests {
    use super::DriftDetector;

    /// Carrying the pucks a metre apart: big change, fully explained. The
    /// user's exact false-positive case.
    #[test]
    fn carried_apart_does_not_flag() {
        let mut d = DriftDetector::new();
        let mut sep = 0.1_f32;
        for i in 0..400 {
            let t = i as f64 * 0.014;
            // both pucks moving ~0.4 m/s each; separation grows at 0.4 m/s
            let flagged = d.push(t, sep, 0.8);
            assert!(flagged.is_none(), "flagged at t={t:.2} sep={sep:.2}");
            sep += 0.4 * 0.014;
        }
        assert!(sep > 2.0, "test must actually cover a large change");
    }

    /// A frame jump: separation steps a metre while both pucks report rest.
    #[test]
    fn frame_jump_flags() {
        let mut d = DriftDetector::new();
        let mut flagged = None;
        for i in 0..400 {
            let t = i as f64 * 0.014;
            let sep = if i < 200 { 0.10 } else { 1.10 };
            if let Some(u) = d.push(t, sep, 0.003) {
                flagged = Some((t, u));
                break;
            }
        }
        let (t, u) = flagged.expect("a metre of unexplained separation must flag");
        assert!(u > 0.4 && t > 2.8, "flagged with unexplained {u:.2} at t={t:.2}");
    }

    /// Losing the stream mid-carry must not flag when it returns: the window
    /// resets rather than crediting motion nobody measured.
    #[test]
    fn gap_resets_instead_of_flagging() {
        let mut d = DriftDetector::new();
        for i in 0..100 {
            assert!(d.push(i as f64 * 0.014, 0.10, 0.003).is_none());
        }
        // 2 s gap during which the pucks were moved apart
        for i in 0..100 {
            let t = 3.4 + i as f64 * 0.014;
            assert!(
                d.push(t, 1.10, 0.003).is_none(),
                "change across a stream gap is unjudgeable, not drift"
            );
        }
    }
}

#[cfg(test)]
mod frame_jump_tests {
    use super::*;

    /// A jumped puck must vanish from the store (so build_transforms stops
    /// emitting it) while every other puck is left untouched.
    #[test]
    fn invalidate_drops_only_the_jumped_puck() {
        let dir = std::env::temp_dir().join("q2slam_fj_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transforms.json");
        let p = path.to_str().unwrap();
        std::fs::write(p, r#"{
            "1.1.1.1": {"yaw_deg": 0.0, "t": [0.0,0.0,0.0], "role": "hip"},
            "2.2.2.2": {"yaw_deg": -59.9, "t": [1.0,2.0,3.0], "role": "ankle"}
        }"#).unwrap();

        assert!(invalidate_transform(p, "2.2.2.2"));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert!(v.get("2.2.2.2").is_none(), "jumped puck must be dropped");
        assert!(v.get("1.1.1.1").is_some(), "the hip must survive");

        // Dropping an absent puck is a no-op, not a corruption.
        assert!(!invalidate_transform(p, "2.2.2.2"));
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert!(v2.get("1.1.1.1").is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// The map-share job body. Every precondition is CHECKED and refused, never
/// assumed — this overwrites maps, and a wrong assumption costs a puck's map.
fn share_map_job(
    ctx: &mut crate::jobs::JobCtx,
    source: &str,
    targets: &[String],
    backups: &std::path::Path,
    shared: &Shared,
    bridge_path: &str,
) -> Result<String, String> {
    let mut step = 0usize;

    // ---- 1. the source must actually have something worth copying
    ctx.begin(step);
    step += 1;
    let src_status = fleet::status(source);
    if !src_status.reachable {
        return Err(format!("{source} is not reachable over adb"));
    }
    if !src_status.map_persistent || src_status.map_root.is_empty() {
        return Err(format!(
            "{source} has no PERSISTENT map context (it reports {}). \
             Only a persistent map is written to disk and can be shared — \
             create one first.",
            if src_status.map_root.is_empty() { "no context" } else { "a transient context" }
        ));
    }
    let root = src_status.map_root.clone();
    let before = fleet::mapdb_info(source).map_err(|e| e.to_string())?;
    if before.is_empty() {
        return Err(format!("{source}'s mapdb is empty despite a persistent context"));
    }
    let age = mapdb_age_secs(before.mtime_unix);
    ctx.finish_ok(format!(
        "root {} · {} files · {} KB · last written {age}s ago",
        fleet::short_root(&root),
        before.files,
        before.bytes / 1024
    ));

    // ---- 2. pull it
    ctx.begin(step);
    step += 1;
    let staging = backups.join("_share").join(fleet::short_root(&root));
    std::fs::remove_dir_all(&staging).ok();
    let local = fleet::pull_dir(source, "/vision/insideout/mapdb", &staging)
        .map_err(|e| format!("pulling {source}'s map: {e}"))?;
    let n = fleet::local_mapdata_count(&local);
    if n == 0 {
        return Err(format!(
            "pulled 0 .mapdata files from {source} into {} — refusing to overwrite anything",
            local.display()
        ));
    }
    // A mapdb write landing mid-pull yields a torn copy. Writes are irregular
    // (gaps to half an hour), so this is rare and cheap to rule out.
    let after = fleet::mapdb_info(source).map_err(|e| e.to_string())?;
    if after.mtime_unix != before.mtime_unix {
        return Err(format!(
            "{source} rewrote its map during the pull (mtime {} → {}); \
             the copy may be torn — run it again",
            before.mtime_unix, after.mtime_unix
        ));
    }
    ctx.finish_ok(format!("{n} files → {}", local.display()));

    // ---- 3. per target: back up, install, restart, confirm
    let mut done = 0usize;
    let mut skipped = 0usize;
    for t in targets {
        if ctx.cancelled() {
            return Err("cancelled".into());
        }
        let st = fleet::status(t);

        // back up
        ctx.begin(step);
        step += 1;
        if !st.reachable {
            ctx.finish_skipped("unreachable");
            // Skip this puck's remaining three steps.
            for _ in 0..3 {
                ctx.begin(step);
                step += 1;
                ctx.finish_skipped("unreachable");
            }
            skipped += 1;
            continue;
        }
        if st.map_persistent && st.map_root == root {
            ctx.finish_skipped("already on this map");
            for _ in 0..3 {
                ctx.begin(step);
                step += 1;
                ctx.finish_skipped("already on this map");
            }
            skipped += 1;
            continue;
        }
        match fleet::backup_mapdb(t, backups) {
            Ok(Some((dev, host))) => {
                ctx.finish_ok(format!("{} · {dev}", host.display()))
            }
            Ok(None) => ctx.finish_ok("nothing to back up (empty mapdb)"),
            // Deliberately fatal: losing a map we could have saved is worse
            // than not sharing.
            Err(e) => return Err(format!("backing up {t}: {e}")),
        }

        // install
        ctx.begin(step);
        step += 1;
        let installed = fleet::seed_mapdb(t, &local, true)
            .map_err(|e| format!("installing map on {t}: {e}"))?;
        ctx.finish_ok(format!("{installed} files, SELinux label verified"));

        // restart tracking so it loads
        ctx.begin(step);
        step += 1;
        fleet::restart_trackingservice(t)
            .map_err(|e| format!("restarting tracking on {t}: {e}"))?;
        ctx.finish_ok("trackingservice restarted");

        // confirm it relocalized
        ctx.begin(step);
        step += 1;
        let ok = fleet::await_map_root(t, &root, Duration::from_secs(120), &mut |m| {
            ctx.progress(m);
        });
        if !ok {
            return Err(format!(
                "{t} loaded the map but did not relocalize into {} within 120 s. \
                 It has to physically SEE mapped territory — check it is in the \
                 right room and not facing a blank wall.",
                fleet::short_root(&root)
            ));
        }
        ctx.finish_ok(format!("relocalized into {}", fleet::short_root(&root)));
        done += 1;
    }

    // ---- 4. the bridges we just invalidated
    //
    // The map layer is now correct, but every puck whose trackingservice we
    // restarted has a stale LOCAL->world bridge, and the output stays visibly
    // wrong until it re-solves. Ask for it, then WAIT -- reporting success
    // while the user is looking at a rotated tracker is the failure this step
    // exists to prevent.
    ctx.begin(step);
    let since = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    shared.bridge_now.store(true, Ordering::Relaxed);
    if done == 0 {
        ctx.finish_skipped("nothing was re-mapped");
    } else {
        let wait_start = Instant::now();
        let deadline = Duration::from_secs(90);
        let mut fresh = 0usize;
        while wait_start.elapsed() < deadline {
            fresh = config::load_bridges(bridge_path)
                .map(|b| {
                    targets
                        .iter()
                        .filter(|t| b.get(*t).map_or(false, |e| e.unix_time >= since))
                        .count()
                })
                .unwrap_or(0);
            if fresh >= done {
                break;
            }
            ctx.progress(format!(
                "hold the pucks STILL — {fresh}/{done} re-bridged ({}s)",
                wait_start.elapsed().as_secs()
            ));
            std::thread::sleep(Duration::from_secs(2));
        }
        if fresh >= done {
            ctx.finish_ok(format!("{fresh} bridge(s) re-solved"));
        } else {
            // Not fatal: the watchdog keeps trying, it just needs stillness.
            ctx.finish_skipped(format!(
                "{fresh}/{done} re-bridged — the watchdog will finish once the pucks are still"
            ));
        }
    }

    Ok(format!(
        "{done} puck(s) colocated on {}{}",
        fleet::short_root(&root),
        if skipped > 0 { format!(", {skipped} skipped") } else { String::new() }
    ))
}

fn mapdb_age_secs(mtime_unix: i64) -> i64 {
    if mtime_unix <= 0 {
        return -1;
    }
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now - mtime_unix
}

/// The create-map job body.
///
/// Guardian and the tracker app contend for the cameras, and guardian must be
/// ENABLED to persist an anchor while the fleet needs it DISABLED to stream
/// poses at all — so this is a bracket, and the final step verifies the stream
/// actually came back.
fn create_map_job(
    ctx: &mut crate::jobs::JobCtx,
    ip: &str,
    roomscale: bool,
    device: Option<Device>,
    host: &str,
    port: u16,
    ingest: &Ingest,
) -> Result<String, String> {
    // ---- 1. it has to be worn; guardian cannot anchor a headset on a desk
    ctx.begin(0);
    let st = fleet::status(ip);
    if !st.reachable {
        return Err(format!("{ip} is not reachable over adb"));
    }
    if st.map_persistent {
        return Err(format!(
            "{ip} already has a persistent map ({}). Share it instead — creating \
             a second one on top is untested.",
            fleet::short_root(&st.map_root)
        ));
    }
    if st.tracking != "6DOF" || !st.tracking_valid {
        return Err(format!(
            "{ip} is at {} (valid={}). It must be WORN and tracking at 6DOF: \
             guardian refuses to place an anchor otherwise.",
            if st.tracking.is_empty() { "no tracking" } else { &st.tracking },
            st.tracking_valid
        ));
    }
    ctx.finish_ok(format!("{} valid, battery {}%", st.tracking, st.battery_pct));

    // ---- 2. free the cameras
    ctx.begin(1);
    fleet::stop_tracker(ip).map_err(|e| format!("stopping the tracker app: {e}"))?;
    std::thread::sleep(Duration::from_secs(2));
    ctx.finish_ok("tracker app stopped");

    // ---- 3. the actual creation
    ctx.begin(2);
    let root = fleet::create_map(ip, roomscale, &mut |m| ctx.progress(m))
        .map_err(|e| e.to_string())?;
    ctx.finish_ok(format!("new persistent map {}", fleet::short_root(&root)));

    // ---- 4. put the tracker back, with its slot re-written
    ctx.begin(3);
    let dev = device.map(|d| d as u8).unwrap_or(0);
    fleet::configure_tracker(ip, host, port, dev)
        .map_err(|e| format!("restarting the tracker app: {e}"))?;
    ctx.finish_ok("tracker app restarted");

    // ---- 5. and PROVE it streams. Without this the job can report success
    // while handing back a puck that no longer tracks for SteamVR.
    ctx.begin(4);
    let start = Instant::now();
    let mut live = false;
    while start.elapsed() < Duration::from_secs(60) {
        if let Some(d) = device {
            if ingest.state(d) == crate::ingest::SlotState::Live {
                live = true;
                break;
            }
        } else {
            // No slot to watch; fall back to the app being up.
            live = fleet::status(ip).tracker_running;
            if live {
                break;
            }
        }
        ctx.progress(format!("waiting for poses from {ip}: {}s", start.elapsed().as_secs()));
        std::thread::sleep(Duration::from_secs(2));
    }
    if !live {
        return Err(format!(
            "{ip} created map {} but is NOT streaming poses. Guardian is disabled \
             again, so relaunching the trackers should recover it.",
            fleet::short_root(&root)
        ));
    }
    ctx.finish_ok("streaming");

    Ok(format!("map {} created", fleet::short_root(&root)))
}

/// Assign a new SteamVR role to a puck and make it take effect.
///
/// The `device` id IS the role, so this rewrites q2slam.json, updates the live
/// roster and pushes the new slot to the puck. Without the roster update the
/// change is worse than useless: `build_transforms` keys off the OLD slot, the
/// aggregator finds no transform for the new one, and the puck vanishes from
/// SteamVR entirely with nothing reporting a problem.
impl Service {
    pub fn set_puck_role(&self, ip: &str, device: u8, cfg_path: &str) -> Result<u64, String> {
        use crate::jobs::{Job, JobStep};

        let Some(role) = Device::from_u8(device) else {
            return Err(format!("device {device} is not a known role"));
        };
        // Refuse a duplicate before touching anything.
        if let Some(other) = self
            .shared
            .pucks
            .read()
            .unwrap()
            .iter()
            .find(|p| p.ip != ip && p.device == device)
        {
            return Err(format!("{} is already {}", other.ip, role.pretty()));
        }

        let id = self.shared.jobs.next_id();
        let steps = vec![
            JobStep::new("save the config"),
            JobStep::new("apply to the running service"),
            JobStep::new(format!("reconfigure {ip} and restart its tracker")),
            JobStep::new("confirm it streams on the new role"),
        ];
        let job = Job::new(id, format!("Set {ip} to {}", role.pretty()), steps);

        let (target, path) = (ip.to_string(), cfg_path.to_string());
        let (host, port) = (self.host.clone(), self.listen_port);
        let sh = Arc::clone(&self.shared);
        let ingest = Arc::clone(&self.ingest);

        let req = crate::jobs::JobRequest {
            job,
            run: Box::new(move |ctx| {
                set_role_job(ctx, &target, device, role, &path, &host, port, &sh, &ingest)
            }),
        };
        let tx = self.shared.job_tx.lock().unwrap();
        tx.as_ref()
            .ok_or_else(|| "job runner not started".to_string())?
            .send(req)
            .map_err(|_| "job runner is gone".to_string())?;
        Ok(id)
    }

    /// The live roster, for the UI.
    pub fn roster(&self) -> Vec<crate::config::PuckCfg> {
        self.shared.pucks.read().unwrap().clone()
    }
}

#[allow(clippy::too_many_arguments)]
fn set_role_job(
    ctx: &mut crate::jobs::JobCtx,
    ip: &str,
    device: u8,
    role: Device,
    cfg_path: &str,
    host: &str,
    port: u16,
    shared: &Shared,
    ingest: &Ingest,
) -> Result<String, String> {
    // ---- 1. persist it first: if the service update succeeds but the write
    // failed, a restart would silently revert the role.
    ctx.begin(0);
    config::set_puck_device(cfg_path, ip, device)?;
    ctx.finish_ok(format!("{cfg_path} updated"));

    // ---- 2. the running service
    ctx.begin(1);
    {
        let mut roster = shared.pucks.write().unwrap();
        if let Some(p) = roster.iter_mut().find(|p| p.ip == ip) {
            p.device = device;
        }
        // Guard dropped here, deliberately: never hold it across push_event or
        // the view lock (service.rs has a deadlock scar from exactly that).
    }
    shared.roster_gen.fetch_add(1, Ordering::Relaxed);
    shared.rebuild.store(true, Ordering::Relaxed);
    ctx.finish_ok("roster updated, transforms rebuilding");

    // ---- 3. the puck itself: config.txt carries the slot it streams on
    ctx.begin(2);
    fleet::configure_tracker(ip, host, port, device)
        .map_err(|e| format!("reconfiguring {ip}: {e}"))?;
    ctx.finish_ok("tracker reconfigured and restarted");

    // ---- 4. prove it
    ctx.begin(3);
    let start = Instant::now();
    let mut live = false;
    while start.elapsed() < Duration::from_secs(45) {
        if ingest.state(role) == crate::ingest::SlotState::Live {
            live = true;
            break;
        }
        ctx.progress(format!(
            "waiting for {ip} on {}: {}s",
            role.label(),
            start.elapsed().as_secs()
        ));
        std::thread::sleep(Duration::from_secs(2));
    }
    if !live {
        return Err(format!(
            "{ip} was set to {} but no poses arrived on that slot. The tracker app \
             may still be starting — check the fleet card.",
            role.pretty()
        ));
    }
    ctx.finish_ok("streaming");
    Ok(format!("now {}", role.pretty()))
}
