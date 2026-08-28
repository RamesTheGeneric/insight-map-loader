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
//!   baseline (gait wiggles it by ~centimetres); a bridge going stale shows up
//!   as a step-change that persists. That flips health to Drifted and asks for
//!   a re-bridge. The measured caveat applies: separation detects *change*, not
//!   *error*, so a wrong-but-stable bridge is invisible to it.
//!
//! The GUI and CLI are thin views over this; their buttons just set the same
//! flags the watchdogs set themselves.

use std::collections::{BTreeMap, VecDeque};
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
// A localization needs the puck to have MOVED (measured: stationary queries
// are worse than a stale transform). Spread over a rolling window is the gate.
/// A bridge is only as good as its spread, and the HIP's bridge rotates the
/// whole shared frame -- a 0.95 deg auto-solve was accepted once and showed up
/// as visible tracker misalignment. Held-still solves land at 0.01-0.05 deg,
/// so anything above this is the puck moving mid-solve, not the achievable
/// floor. The watchdog retries on the next still window, so a strict bar costs
/// nothing but a short wait.
const BRIDGE_MAX_SPREAD_DEG: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BridgeState {
    Missing,
    WaitingStill,
    Solving,
    Ok,
    /// Consistency checks are failing — a re-solve is pending stillness.
    Suspect,
}

/// Does this puck need its bridge re-solved?
///
/// Both inputs matter and they are set by different paths, which is how they
/// came to disagree: the drift path counts failed checks and flips the state at
/// two, while a frame jump flips the state IMMEDIATELY and leaves the counter
/// at zero -- deliberately, because a jump is unambiguous and waiting out a
/// count would keep emitting known-wrong poses.
///
/// Gating on the counter alone therefore deadlocked: a jumped puck sat in
/// Suspect forever, because the consistency check that increments the counter
/// only runs while the state is still Ok. Observed live -- the service
/// announced "re-bridging when still", the puck was provably still, and it
/// never re-bridged.
pub fn needs_resolve(state: BridgeState, suspect: u32) -> bool {
    matches!(state, BridgeState::Missing | BridgeState::Suspect | BridgeState::WaitingStill)
        || suspect >= 2
}

#[derive(Clone, Default)]
pub struct View {
    pub live: Vec<(Device, [f32; 3])>,
    pub sep: Option<f32>,
    pub slots: Vec<(Device, SlotState, f32, f32)>, // state, age s, rate Hz
    /// Sources sending valid MPT1 that no config entry claims. Surfaced
    /// rather than dropped: this is what an unprovisioned puck looks like.
    pub unknown_sources: Vec<u8>,
    pub emitted: u64,
    pub n_transforms: usize,
    /// Long fleet operations, newest last. Snapshot, like `events`.
    pub jobs: Vec<crate::jobs::Job>,
    pub bridges: Vec<(String, BridgeState, f32)>, // ip, state, yaw°
    pub drifted: bool,
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




struct Shared {
    view: Mutex<View>,
    rebuild: AtomicBool,
    bridge_now: AtomicBool,  // manual "bridge as soon as still"
    /// Pucks whose bridge should be verified at the next opportunity — set by
    /// the teleport detector, consumed by the watchdog (which relaxes its
    /// stillness/interval gates for that one check).
    verify: Mutex<std::collections::BTreeSet<String>>,
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
    host: String,
    listen_port: u16,
}

// There is deliberately NO puck list cached here. Roles are reassignable at
// runtime, so the only truthful source is `shared.pucks` via `roster()`; a
// convenience copy taken at startup went stale the first time someone changed
// a role, and every reader of it then agreed with itself about the wrong slot.

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
    ///
    /// `force` re-copies a puck that is ALREADY on this map root. That is not a
    /// no-op: pucks sharing a root go on mapping independently, and their maps
    /// diverge in CONTENT while agreeing on identity. Measured on this fleet
    /// after a few hours — 1269 points against 1063, across the same six nodes,
    /// with under 11% of individual points in common. Same frame, different
    /// detail. Without `force` there is no way to re-establish one puck from
    /// another, because the root-uuid check that makes the normal path
    /// idempotent also makes a deliberate refresh impossible.
    pub fn share_map(&self, source: &str, targets: Vec<String>, force: bool) -> Result<u64, String> {
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

        let verb = if force { "Re-sync map from" } else { "Share map from" };
        let job = Job::new(id, format!("{verb} {source} to {} puck(s)", targets.len()), steps);
        let src = source.to_string();
        let backups = self.shared.map_backups.clone();
        let sh = Arc::clone(&self.shared);
        let bridge_path = self.bridge_path.clone();

        let req = crate::jobs::JobRequest {
            job,
            run: Box::new(move |ctx| {
                share_map_job(ctx, &src, &targets, &backups, &sh, &bridge_path, force)
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
        // The LIVE roster, not a startup snapshot. This job ends by rewriting
        // the puck's tracker config with its slot, so reading a stale roster
        // would silently revert a role assigned since launch -- and because the
        // liveness check below reads the same stale id, it would then confirm
        // "streaming" and report success. Wrong slot, green job.
        // The source byte this puck stamps -- what step 5 must watch for to
        // prove the tracker came back. Read from the LIVE roster: a startup
        // snapshot goes stale the moment anyone reassigns a role, and both the
        // write and the check would then agree about the wrong slot.
        let src = self
            .roster()
            .iter()
            .find(|p| p.ip == target)
            .map(|p| p.id.unwrap_or(p.device));
        let host = self.host.clone();
        let port = self.listen_port;
        let ingest = Arc::clone(&self.ingest);

        let req = crate::jobs::JobRequest {
            job,
            run: Box::new(move |ctx| {
                create_map_job(ctx, &target, roomscale, src, &host, port, &ingest)
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

    pub fn start(cfg: Config) -> Result<Service, String> {
        let ingest = Arc::new(
            Ingest::bind(&cfg.listen, Duration::from_millis(500))
                .map_err(|e| format!("cannot listen on {}: {e}", cfg.listen))?,
        );
        let shared = Arc::new(Shared {
            view: Mutex::new(View::default()),
            rebuild: AtomicBool::new(true),
            bridge_now: AtomicBool::new(false),
            verify: Mutex::new(std::collections::BTreeSet::new()),
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

        spawn_aggregate(cfg.clone(), Arc::clone(&ingest), Arc::clone(&shared))?;
        spawn_bridge_watchdog(cfg.clone(), Arc::clone(&ingest), Arc::clone(&shared))?;
        let bridge_path = cfg.bridge.clone();
        let host = cfg.host.clone();
        let listen_port =
            cfg.listen.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(5180);
        Ok(Service { shared, ingest, bridge_path, host, listen_port })
    }
}




/// Where a puck's existing map is archived before it is overwritten. The
/// directory docs/insight-map-lifecycle.md already names.
fn map_backup_dir() -> std::path::PathBuf {
    std::env::var_os("INSIGHT_MAP_BACKUPS")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("insight-map-loader-backups")))
        .unwrap_or_else(|| std::path::PathBuf::from("insight-map-loader-backups"))
}

fn push_event(shared: &Shared, msg: String) {
    let mut v = shared.view.lock().unwrap();
    v.events.push(msg);
    let n = v.events.len();
    if n > 8 {
        v.events.drain(0..n - 8);
    }
}


// ---- aggregation + drift monitoring ---------------------------------------


/// Aggregate every puck's stream into the shared frame and emit it.
///
/// With a shared map there is no `T_map_world` to solve, so this is only:
/// ingest -> apply each puck's LOCAL->world bridge -> emit. What remains
/// besides that is event detection -- the things that invalidate a bridge.
fn spawn_aggregate(cfg: Config, ingest: Arc<Ingest>, shared: Arc<Shared>) -> Result<(), String> {
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
            let mut last_t: BTreeMap<u8, u64> = BTreeMap::new();
            let mut counts: BTreeMap<u8, u32> = BTreeMap::new();
            let mut window = Instant::now();
            let mut drift = DriftDetector::new();
            let epoch = Instant::now();
            let mut last_pose: BTreeMap<u8, (u64, [f32; 3])> = BTreeMap::new();
            let mut live_state: BTreeMap<u8, bool> = BTreeMap::new();
            let mut ip_of: BTreeMap<u8, String> = BTreeMap::new();
            let mut roster_seen = u64::MAX;

            loop {
                // Re-read the roster when it changes, so a role assignment
                // takes effect without a restart. A stale roster does not
                // merely fail to help: build_transforms would key off the OLD
                // slot and the puck would vanish from the output silently.
                let generation = shared.roster_gen.load(Ordering::Relaxed);
                if generation != roster_seen || shared.rebuild.swap(false, Ordering::Relaxed) {
                    roster_seen = generation;
                    let roster = shared.pucks.read().unwrap().clone();
                    ip_of = roster.iter().map(|p| (p.device, p.ip.clone())).collect();
                    let bridges = config::load_bridges(&cfg.bridge).unwrap_or_default();
                    agg.transforms = config::build_transforms_for(&roster, &bridges);
                    // Rebuilt with the transforms: a role change alters which
                    // role a source publishes as, and the two must never
                    // disagree or a puck emits under one role with another's
                    // transform.
                    agg.roles = config::source_to_role(&roster);
                    // Any transform change moves everything; old statistics lie.
                    drift.reset();
                    last_pose.clear();
                    shared.view.lock().unwrap().drifted = false;
                }

                let summary = agg.tick(&ingest);
                for s in ingest.live() {
                    let d = s.packet.src;
                    if last_t.get(&d) != Some(&s.packet.t_ns) {
                        last_t.insert(d, s.packet.t_ns);
                        *counts.entry(d).or_default() += 1;
                    }
                }

                // REBOOTS announce themselves on the stream: t_ns is the
                // device's boot clock, so a fresh boot sends timestamps HOURS
                // behind the ones before it. A reboot resets the tracker's
                // LOCAL frame, which is exactly what the bridge describes.
                for smp in ingest.live() {
                    let d = smp.packet.src;
                    if let Some(&(t_prev, _)) = last_pose.get(&d) {
                        if smp.packet.t_ns + 3_600_000_000_000 < t_prev {
                            if let Some(ip) = ip_of.get(&d) {
                                push_event(&shared, format!(
                                    "{ip} REBOOTED (boot clock regressed) — re-bridging"));
                                last_pose.remove(&d);
                                shared.verify.lock().unwrap().insert(ip.clone());
                            }
                        }
                    }
                }

                // A tracker returning after a gap may have relocalized into a
                // moved frame; a quick stationary check settles it.
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
                                "{ip} back after a gap — verifying bridge"));
                            shared.verify.lock().unwrap().insert(ip.clone());
                        }
                    }
                }

                // Teleports: a pose jumping faster than a human moves is a
                // frame event, not motion. Route it to the bridge watchdog.
                for smp in ingest.live() {
                    let d = smp.packet.src;
                    let p = smp.packet.pose.p;
                    if let Some(&(t_prev, p_prev)) = last_pose.get(&d) {
                        let dt = smp.packet.t_ns.saturating_sub(t_prev) as f32 / 1e9;
                        if dt > 0.004 && dt < 0.15 {
                            let dist = ((p[0] - p_prev[0]).powi(2)
                                + (p[1] - p_prev[1]).powi(2)
                                + (p[2] - p_prev[2]).powi(2))
                            .sqrt();
                            if dist > TELEPORT_MIN_DIST && dist / dt > TELEPORT_SPEED {
                                if let Some(ip) = ip_of.get(&d) {
                                    if shared.verify.lock().unwrap().insert(ip.clone()) {
                                        push_event(&shared, format!(
                                            "{ip} pose jumped {dist:.2} m in {:.0} ms — verifying bridge",
                                            dt * 1e3));
                                    }
                                }
                            }
                        }
                    }
                    last_pose.insert(d, (smp.packet.t_ns, p));
                }

                // Drift: separation changing by more than the pucks' own motion
                // can explain. Carrying them apart is explained; a frame jump on
                // either side is not. With a shared map the usual cause is a
                // stale bridge, so this reports rather than re-solves.
                if let (Some(s), Some(sp)) = (summary.separation, summary.speed_sum) {
                    let t = epoch.elapsed().as_secs_f64();
                    if let Some(unexplained) = drift.push(t, s, sp) {
                        let mut view = shared.view.lock().unwrap();
                        if !view.drifted {
                            view.drifted = true;
                            drop(view);
                            push_event(&shared, format!(
                                "separation moved {unexplained:.2} m with no motion to explain it \
                                 — hold the pucks still and re-bridge"));
                        }
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
                    // Report by ROLE, since that is what the operator assigned
                    // and what SteamVR shows -- but keep a separate list of
                    // sources with no role, so an unprovisioned or misconfigured
                    // puck is visible rather than merely absent.
                    let mut slots = Vec::new();
                    let mut unknown = Vec::new();
                    for (src, st, s) in ingest.all() {
                        let age = s.map(|x| x.age().as_secs_f32()).unwrap_or(f32::NAN);
                        let rate = rates.get(&src).copied().unwrap_or(0.0);
                        match agg.roles.get(&src) {
                            Some(&role) => slots.push((role, st, age, rate)),
                            None => unknown.push(src),
                        }
                    }
                    v.slots = slots;
                    v.unknown_sources = unknown;
                    v.emitted = agg.emitted;
                    v.n_transforms = agg.transforms.len();
                    v.jobs = shared.jobs.snapshot();
                }
                std::thread::sleep(Duration::from_millis(4));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}









// ---- bridge watchdog -------------------------------------------------------

struct PuckWatch {
    ip: String,
    /// The byte this puck stamps, NOT its role. A role reassignment must not
    /// move the watch: the bridge belongs to the device, not to the slot it
    /// currently publishes under.
    src: u8,
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
            // Each watch is keyed by the puck's SLOT, and a role change moves
            // that slot. Built once from the startup config, the watchdog would
            // go on reading a device id nothing feeds any more: `ingest.sample`
            // returns None forever, every check is skipped, and the puck simply
            // stops being bridged -- with no error, because "no sample yet" is
            // indistinguishable from a puck that has not started streaming.
            // So the roster is re-read whenever it changes, exactly as the
            // aggregate thread does.
            let mut watches: Vec<PuckWatch> = Vec::new();
            let mut roster_seen = u64::MAX;
            let mut bridges = config::load_bridges(&cfg.bridge).unwrap_or_default();

            loop {
                // Re-key the watches when the roster changes. Existing entries
                // keep their state and history by ip -- a role change moves a
                // puck's slot, it does not invalidate what we know about that
                // puck's bridge.
                let generation = shared.roster_gen.load(Ordering::Relaxed);
                if generation != roster_seen {
                    roster_seen = generation;
                    let roster = shared.pucks.read().unwrap().clone();
                    let mut rebuilt: Vec<PuckWatch> = Vec::with_capacity(roster.len());
                    for p in &roster {
                        // Follow the SOURCE, not the role. A puck that gets
                        // reassigned keeps streaming the same bytes from the
                        // same device, so its pose history stays valid and its
                        // bridge -- which describes that device, not that slot
                        // -- is untouched. Under the old role-keyed scheme a
                        // reassignment looked like a new puck and threw both away.
                        let src = p.id.unwrap_or(p.device);
                        match watches.iter().position(|w| w.ip == p.ip) {
                            Some(i) => {
                                let mut w = watches.swap_remove(i);
                                if w.src != src {
                                    // Only a re-provision changes this, and then
                                    // the history really does belong elsewhere.
                                    w.src = src;
                                    w.history.clear();
                                }
                                rebuilt.push(w);
                            }
                            None => rebuilt.push(PuckWatch {
                                ip: p.ip.clone(),
                                src,
                                history: VecDeque::new(),
                                suspect: 0,
                                last_check: Instant::now() - BRIDGE_CHECK_EVERY,
                                state: bridges
                                    .get(&p.ip)
                                    .map_or(BridgeState::Missing, |_| BridgeState::Ok),
                                yaw_deg: bridges.get(&p.ip).map_or(f32::NAN, |b| b.yaw_deg),
                            }),
                        }
                    }
                    watches = rebuilt;
                }

                let force = shared.bridge_now.swap(false, Ordering::Relaxed);
                let verify_now: std::collections::BTreeSet<String> =
                    std::mem::take(&mut *shared.verify.lock().unwrap());
                for w in &mut watches {
                    let verify_hit = verify_now.contains(&w.ip);
                    let sample = ingest.sample(w.src);
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
                    let needs = needs_resolve(w.state, w.suspect);

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
                            let Some(s2) = ingest.sample(w.src) else { continue };
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
                    if std::env::var_os("INSIGHT_DEBUG").is_some()
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
                                // With a shared map there is no stored
                                // per-puck transform to invalidate: an Insight
                                // frame jump means this puck's LOCAL->world
                                // bridge describes a frame that no longer
                                // exists, and re-bridging is the whole fix.
                                push_event(&shared, format!(
                                    "{} Insight frame JUMPED ({dp:.2} m / {dyaw:.1}\u{00b0}) — \
                                     its bridge no longer applies; re-bridging when still",
                                    w.ip));
                                shared.view.lock().unwrap().drifted = true;
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
    use super::{needs_resolve, BridgeState, DriftDetector};

    /// Carrying the pucks a metre apart: big change, fully explained. The
    /// user's exact false-positive case.
    #[test]
    fn a_jumped_bridge_re_solves_even_with_a_zero_counter() {
        // The frame-jump path sets the STATE and leaves the counter at zero on
        // purpose. Gating on the counter alone stranded the puck in Suspect
        // forever, which is exactly what happened on the fleet.
        assert!(needs_resolve(BridgeState::Suspect, 0));
        assert!(needs_resolve(BridgeState::Missing, 0));
        assert!(needs_resolve(BridgeState::WaitingStill, 0));
    }

    #[test]
    fn a_healthy_bridge_is_left_alone() {
        assert!(!needs_resolve(BridgeState::Ok, 0));
        assert!(!needs_resolve(BridgeState::Ok, 1));
        // ...until the drift path has failed twice.
        assert!(needs_resolve(BridgeState::Ok, 2));
        // Solving must not re-enter itself.
        assert!(!needs_resolve(BridgeState::Solving, 0));
    }

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

/// The map-share job body. Every precondition is CHECKED and refused, never
/// assumed — this overwrites maps, and a wrong assumption costs a puck's map.
fn share_map_job(
    ctx: &mut crate::jobs::JobCtx,
    source: &str,
    targets: &[String],
    backups: &std::path::Path,
    shared: &Shared,
    bridge_path: &str,
    force: bool,
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

    // Warn when the source is the ODD ONE OUT. Sharing from a puck whose map
    // the rest of the fleet does not use overwrites an established shared map
    // with a lone one -- legitimate when seeding a new space, and a disaster
    // when the intended direction was the other way. Nearly happened here: a
    // freshly set-up puck was picked as the source, which would have replaced
    // the fleet's map on every other puck with that new puck's private one.
    {
        let mut agree = 0usize;
        let mut differ = Vec::new();
        for t in targets {
            let st = fleet::status(t);
            if st.map_persistent && !st.map_root.is_empty() {
                if st.map_root == root { agree += 1; } else { differ.push(t.clone()); }
            }
        }
        if agree == 0 && differ.len() > 1 {
            ctx.progress(format!(
                "NOTE: {} pucks already share a DIFFERENT map; sharing from {source} \
                 replaces it on all of them. If {source} is the new puck, share the \
                 other way instead.",
                differ.len()
            ));
        }
    }

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
        // Same root uuid does NOT mean same map. Each puck keeps mapping on its
        // own copy, so content diverges while identity does not — which makes
        // this skip correct for "get everyone onto one frame" and wrong for
        // "re-establish them from a known-good copy". `force` is that second
        // case, and it is the only way to reach it.
        if !force && st.map_persistent && st.map_root == root {
            ctx.finish_skipped("already on this map (use Re-sync to copy anyway)");
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
        "{done} puck(s) {} on {}{}",
        if force { "re-synced" } else { "colocated" },
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
    src: Option<u8>,
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
    // Write the puck's SOURCE byte -- its id if provisioned, its role under
    // the legacy arrangement. Either way it is what the puck stamps.
    fleet::configure_tracker(ip, host, port, src.unwrap_or(0))
        .map_err(|e| format!("restarting the tracker app: {e}"))?;
    ctx.finish_ok("tracker app restarted");

    // ---- 5. and PROVE it streams. Without this the job can report success
    // while handing back a puck that no longer tracks for SteamVR.
    ctx.begin(4);
    let start = Instant::now();
    let mut live = false;
    while start.elapsed() < Duration::from_secs(60) {
        if let Some(d) = src {
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
/// The `device` id IS the role, so this rewrites insight-map-loader.json, updates the live
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

    // ---- 3. the puck itself -- ONLY if it cannot be relabelled host-side.
    //
    // A provisioned puck stamps a stable id and the host maps that id to a
    // role, so reassigning a role is the config edit above and nothing more:
    // no push, no tracker restart, and critically no restart-induced LOCAL
    // frame change, which would otherwise invalidate the puck's bridge and
    // force a re-solve for what is purely a relabel.
    //
    // A legacy puck has no id and stamps its role directly, so for those the
    // old push-and-restart is still the only way.
    ctx.begin(2);
    let src = shared.pucks.read().unwrap().iter()
        .find(|p| p.ip == ip).and_then(|p| p.id);
    match src {
        Some(_) => ctx.finish_ok("no device change needed (puck has a stable id)"),
        None => {
            fleet::configure_tracker(ip, host, port, device)
                .map_err(|e| format!("reconfiguring {ip}: {e}"))?;
            ctx.finish_ok("legacy puck: tracker reconfigured and restarted");
        }
    }

    // ---- 4. prove it. Watch the SOURCE byte: for a provisioned puck that is
    // unchanged and should still be Live within a packet or two.
    ctx.begin(3);
    let watch = src.unwrap_or(device);
    let start = Instant::now();
    let mut live = false;
    while start.elapsed() < Duration::from_secs(45) {
        if ingest.state(watch) == crate::ingest::SlotState::Live {
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
