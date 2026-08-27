//! insight-map-loader-gui — the control surface for the puck fleet.
//!
//! All behaviour lives in insight-map-loader-core::service, which runs the ingest, the
//! aggregation, the bridge watchdog and the drift monitor on its own threads.
//! This window paints that service's view and forwards button presses to the
//! same flags the watchdogs use themselves — the buttons are overrides, not
//! requirements. Run from the repo root.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Sense, Stroke, Vec2};
use insight_map_loader_core::config::Config;
use insight_map_loader_core::ingest::SlotState;
use insight_map_loader_core::mpt1::Device;
use insight_map_loader_core::jobs::{JobState, StepState};
use insight_map_loader_core::service::{BridgeState, Service};
use insight_map_loader_core::fleet;

/// A stable colour per role. The first three keep the colours they have always
/// had, so existing habits and screenshots still read correctly; the rest are
/// spread around the wheel. Generated rather than matched so adding a role does
/// not mean touching the UI.
fn device_color(d: Device) -> Color32 {
    match d {
        Device::Waist => Color32::from_rgb(0x37, 0xc2, 0xe0),
        Device::LeftFoot => Color32::from_rgb(0xf2, 0xa2, 0x4b),
        Device::RightFoot => Color32::from_rgb(0x54, 0xd0, 0x8a),
        other => {
            // Golden-angle hue steps keep neighbouring ids visually distinct.
            let h = ((other as u8 as f32) * 137.508) % 360.0;
            let (r, g, b) = hsv(h, 0.55, 0.88);
            Color32::from_rgb(r, g, b)
        }
    }
}

fn hsv(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

#[derive(Clone)]
struct FleetRow {
    ip: String,
    device: u8,
    status: fleet::PuckStatus,
}

struct App {
    cfg: Config,
    /// The file roles are written back to. main() used to drop this, which
    /// would send a `--config other.json` launch's edits to insight-map-loader.json.
    cfg_path: String,
    service: Service,
    fleet_rows: Arc<Mutex<Vec<FleetRow>>>,
    trails: BTreeMap<Device, VecDeque<[f32; 3]>>,
    busy: Arc<AtomicBool>, // launch action; adb is slow, one at a time
    /// Per-puck "LED currently blinking" flags; blinking is fire-and-forget
    /// but the button should not stack blinks needlessly.
    blinking: BTreeMap<String, Arc<AtomicBool>>,
    /// Which puck's map the Share button copies FROM. Behind a lock because
    /// fleet_panel takes &self (it paints from a service snapshot).
    map_source: Mutex<Option<String>>,
}

impl App {
    fn new(cfg: Config, cfg_path: String) -> Result<App, String> {
        let service = Service::start(cfg.clone())?;

        // Fleet status poller: adb costs 100-300 ms per puck, so it lives off
        // the paint path and refreshes every few seconds.
        let fleet_rows = Arc::new(Mutex::new(Vec::new()));
        {
            let (cfg, rows) = (cfg.clone(), Arc::clone(&fleet_rows));
            std::thread::Builder::new().name("fleet-poll".into()).spawn(move || loop {
                let mut fresh = Vec::new();
                for p in &cfg.pucks {
                    let mut st = fleet::status(&p.ip);
                    if !st.reachable {
                        // A rebooted puck drops off adb until someone connects.
                        // The tracker auto-starts and streams regardless, so
                        // without this the card says "gone" about a puck that
                        // is visibly tracking — reconnect and ask again.
                        if fleet::connect(&p.ip).unwrap_or(false) {
                            st = fleet::status(&p.ip);
                        }
                    }
                    fresh.push(FleetRow {
                        ip: p.ip.clone(),
                        device: p.device,
                        status: st,
                    });
                }
                *rows.lock().unwrap() = fresh;
                std::thread::sleep(Duration::from_secs(3));
            }).map_err(|e| e.to_string())?;
        }

        let blinking = cfg
            .pucks
            .iter()
            .map(|p| (p.ip.clone(), Arc::new(AtomicBool::new(false))))
            .collect();
        Ok(App {
            cfg,
            cfg_path,
            service,
            fleet_rows,
            trails: BTreeMap::new(),
            busy: Arc::new(AtomicBool::new(false)),
            blinking,
            map_source: Mutex::new(None),
        })
    }

    fn fleet_panel(&self, ui: &mut egui::Ui) {
        let view = self.service.view();
        ui.heading("Fleet");
        ui.add_space(4.0);
        let rows = self.fleet_rows.lock().unwrap().clone();
        if rows.is_empty() {
            ui.label("querying pucks…");
        }
        // The fleet's shared map root: the most common persistent root across
        // reachable pucks. Colocation is exactly "everyone reports this one",
        // so it is the reference each row is judged against below.
        let shared_root: Option<String> = {
            let mut tally: std::collections::BTreeMap<&str, usize> = Default::default();
            for r in &rows {
                if r.status.map_persistent && !r.status.map_root.is_empty() {
                    *tally.entry(r.status.map_root.as_str()).or_default() += 1;
                }
            }
            tally.into_iter().max_by_key(|&(_, n)| n).map(|(k, _)| k.to_string())
        };
        {
            let n_shared = rows
                .iter()
                .filter(|r| {
                    r.status.map_persistent
                        && shared_root.as_deref() == Some(r.status.map_root.as_str())
                })
                .count();
            let reachable = rows.iter().filter(|r| r.status.reachable).count();
            let (txt, col) = match &shared_root {
                Some(root) if n_shared == reachable && reachable > 1 => (
                    format!("Colocated: all {reachable} pucks on shared map {} — no transforms applied", fleet::short_root(root)),
                    Color32::LIGHT_GREEN,
                ),
                Some(root) => (
                    format!("Colocated mode: {n_shared}/{reachable} pucks on map {} — the rest are NOT aligned", fleet::short_root(root)),
                    Color32::LIGHT_RED,
                ),
                None => (
                    "Colocated mode: no puck has a persistent map — transplant one (docs/insight-mapdata-format.md)"
                        .to_string(),
                    Color32::YELLOW,
                ),
            };
            ui.label(egui::RichText::new(txt).small().color(col));
            ui.add_space(2.0);
        }
        let job_active = view.jobs.iter().any(|j| j.is_active());
        // Roles come from the LIVE roster, not from FleetRow: the fleet-poll
        // thread iterates a clone of the config taken at startup, so its
        // `device` never changes and a role picked here would keep showing the
        // old one while the puck had already moved.
        let roster: BTreeMap<String, u8> =
            self.service.roster().into_iter().map(|p| (p.ip, p.device)).collect();
        for r in &rows {
            let dev = roster.get(&r.ip).copied().unwrap_or(r.device);
            let d = Device::from_u8(dev);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(d) = d {
                        ui.colored_label(device_color(d), "●");
                        ui.strong(format!("{}  ({})", r.ip, d.label()));
                    }
                    let (_, _, _, name) = fleet::slot_led_rgb(dev);
                    let flag = self.blinking.get(&r.ip).cloned();
                    let active = flag.as_ref().map_or(false, |f| f.load(Ordering::Relaxed));
                    ui.add_enabled_ui(!active, |ui| {
                        if ui
                            .small_button("⚑")
                            .on_hover_text(format!(
                                "identify: flashes {name}, {} time(s)",
                                dev as u32 + 1
                            ))
                            .clicked()
                        {
                            if let Some(flag) = flag {
                                if !flag.swap(true, Ordering::Relaxed) {
                                    let (ip, id) = (r.ip.clone(), dev);
                                    std::thread::spawn(move || {
                                        fleet::identify(&ip, id).ok();
                                        flag.store(false, Ordering::Relaxed);
                                    });
                                }
                            }
                        }
                    });
                });
                // SteamVR role. The id IS the role, so changing it moves this
                // puck to a different SteamVR tracker -- config, service and
                // puck all updated by the job.
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("role").small().weak());
                    let cur = Device::from_u8(dev);
                    let taken: Vec<u8> = roster
                        .iter()
                        .filter(|(ip, _)| *ip != &r.ip)
                        .map(|(_, d)| *d)
                        .collect();
                    ui.add_enabled_ui(!job_active, |ui| {
                        egui::ComboBox::from_id_salt(("role", &r.ip))
                            .selected_text(cur.map_or("?".to_string(), |d| d.pretty().to_string()))
                            .show_ui(ui, |ui| {
                                for d in insight_map_loader_core::mpt1::ALL_DEVICES {
                                    let id = d as u8;
                                    // A duplicate would make two pucks fight
                                    // over one tracker at packet rate.
                                    let dup = taken.contains(&id);
                                    let label = if dup {
                                        format!("{} (taken)", d.pretty())
                                    } else {
                                        d.pretty().to_string()
                                    };
                                    ui.add_enabled_ui(!dup, |ui| {
                                        if ui.selectable_label(cur == Some(d), label).clicked()
                                            && cur != Some(d)
                                        {
                                            if let Err(e) = self.service.set_puck_role(
                                                &r.ip, id, &self.cfg_path,
                                            ) {
                                                eprintln!("set_puck_role: {e}");
                                            }
                                        }
                                    });
                                }
                            });
                    });
                });
                if !r.status.reachable {
                    // adb being down is not the same as the puck being gone:
                    // the pose stream is the ground truth the plot draws from.
                    let streaming = Device::from_u8(dev).map_or(false, |d| {
                        view.slots.iter().any(|(sd, st, _, _)| {
                            *sd == d && *st == SlotState::Live
                        })
                    });
                    if streaming {
                        ui.colored_label(
                            Color32::YELLOW,
                            "streaming · adb offline (reconnecting…)",
                        );
                    } else {
                        ui.colored_label(Color32::LIGHT_RED, "unreachable");
                    }
                } else {
                    let s = &r.status;
                    ui.label(format!(
                        "{}{}   battery {}%",
                        s.tracking,
                        if s.tracking_valid { "" } else { " INVALID" },
                        s.battery_pct
                    ));
                    ui.horizontal(|ui| {
                        ui.label(if s.tracker_running { "tracker up" } else { "tracker DOWN" });
                        if !s.guardian_disabled {
                            ui.colored_label(Color32::YELLOW, "guardian!");
                        }
                        if s.vpn_trap {
                            ui.colored_label(Color32::LIGHT_RED, "VPN TRAP");
                        }
                    });
                }
                // There is no per-puck transform to report — it is identity.
                // What matters is whether this puck shares the others' map,
                // which is visible as the root node uuid: same root everywhere
                // is one tracking universe.
                //
                // Compare FULL uuids; show the short form. The 8-char prefix is
                // a display convenience, not an identity.
                {
                    let root = &r.status.map_root;
                    let shown = fleet::short_root(root);
                    // No persistent map = this puck cannot be colocated and
                    // will never write one. Offer to mint one right here,
                    // which is the only way out of that state.
                    if !r.status.map_persistent {
                        let ready = r.status.tracking == "6DOF" && r.status.tracking_valid;
                        ui.add_enabled_ui(!job_active && ready, |ui| {
                            let b = ui.small_button("✚ Create map").on_hover_text(
                                "Mints a persistent map for the space this puck is standing in,\n\
                                 by enabling guardian just long enough to place an anchor.\n\n\
                                 The puck must be WORN and tracking at 6DOF. Its tracker app is\n\
                                 stopped for the duration (guardian needs the cameras) and\n\
                                 restarted afterwards.",
                            );
                            if b.clicked() {
                                if let Err(e) = self.service.create_map(&r.ip, false) {
                                    eprintln!("create_map: {e}");
                                }
                            }
                        });
                        if !ready {
                            ui.label(
                                egui::RichText::new("wear it and reach 6DOF to create a map")
                                    .small()
                                    .weak(),
                            );
                        }
                    }
                    let (txt, col) = if root.is_empty() {
                        ("map: no context yet".to_string(), Color32::YELLOW)
                    } else if !r.status.map_persistent {
                        // A transient context is never written to disk and was
                        // not loaded from a shared map: this puck is on its own
                        // frame and its output will not agree with the others.
                        (format!("map {shown} TRANSIENT — not colocated"), Color32::LIGHT_RED)
                    } else if shared_root.as_deref() == Some(root.as_str()) {
                        (format!("colocated — shared map {shown}"), Color32::LIGHT_GREEN)
                    } else {
                        (format!("map {shown} — DIFFERENT map from the fleet"), Color32::LIGHT_RED)
                    };
                    ui.label(egui::RichText::new(txt).small().color(col));
                }
                // The watchdog's verdict on this puck's frame bridge.
                if let Some((_, st, yaw)) = view.bridges.iter().find(|(ip, _, _)| ip == &r.ip) {
                    let (txt, col) = match st {
                        BridgeState::Ok => (format!("bridge ok ({yaw:+.1}°)"), Color32::GRAY),
                        BridgeState::Missing => ("bridge: waiting for stillness".into(), Color32::YELLOW),
                        BridgeState::WaitingStill => ("re-bridge pending stillness".into(), Color32::YELLOW),
                        BridgeState::Solving => ("bridging…".into(), Color32::LIGHT_BLUE),
                        BridgeState::Suspect => ("bridge STALE".into(), Color32::LIGHT_RED),
                    };
                    ui.label(egui::RichText::new(txt).small().color(col));
                }
            });
            ui.add_space(2.0);
        }
        ui.add_space(6.0);
        ui.separator();

        let busy = self.busy.load(Ordering::Relaxed);
        ui.add_enabled_ui(!busy, |ui| {
            if ui.button("⟳ Launch trackers").on_hover_text(
                "adb connect, write config (one MPT1 slot per puck), prox override, start app.\n\
                 The watchdog re-bridges automatically once each puck is still.").clicked()
            {
                let cfg = self.cfg.clone();
                let busy = Arc::clone(&self.busy);
                if !busy.swap(true, Ordering::Relaxed) {
                    std::thread::spawn(move || {
                        let port = cfg.listen.rsplit(':').next()
                            .and_then(|p| p.parse().ok()).unwrap_or(5180);
                        for p in &cfg.pucks {
                            fleet::connect(&p.ip).ok();
                            fleet::configure_tracker(&p.ip, &cfg.host, port, p.device).ok();
                        }
                        busy.store(false, Ordering::Relaxed);
                    });
                }
            }
        });

        // Still needed, and with colocation it is the ONLY calibration left:
        // MPT1 streams the tracker's LOCAL frame, so something must map it to
        // the Insight world frame. Restarting the tracker app or
        // trackingservice invalidates it.
        if ui.button("⌖ Bridge now").on_hover_text(
            "Re-solves every puck's LOCAL→world offset at its next still moment.\n\
             The watchdog does this automatically; the button skips the wait.\n\n\
             Needed after relaunching trackers or sharing a map — both reset a\n\
             frame the stored bridge described. Hold the pucks STILL.").clicked()
        {
            self.service.request_bridge();
        }

        // ---- map sharing: the whole colocation workflow in one button.
        ui.add_space(6.0);
        ui.separator();
        ui.label(egui::RichText::new("shared map").small().strong());

        let job_running = job_active;
        let sources: Vec<&FleetRow> = rows
            .iter()
            .filter(|r| r.status.reachable && r.status.map_persistent && !r.status.map_root.is_empty())
            .collect();

        if sources.is_empty() {
            ui.label(
                egui::RichText::new("no puck has a persistent map to share — create one first")
                    .small()
                    .color(Color32::YELLOW),
            );
        } else {
            // Default the source to the puck the fleet already agrees on, so
            // the common case is one click with nothing to choose.
            let mut chosen = self.map_source.lock().unwrap();
            if chosen.is_none() || !sources.iter().any(|r| Some(&r.ip) == chosen.as_ref()) {
                *chosen = shared_root
                    .as_ref()
                    .and_then(|sr| sources.iter().find(|r| &r.status.map_root == sr))
                    .or_else(|| sources.first())
                    .map(|r| r.ip.clone());
            }
            let cur = chosen.clone().unwrap_or_default();
            egui::ComboBox::from_id_salt("map_source")
                .selected_text(format!("from {cur}"))
                .show_ui(ui, |ui| {
                    for r in &sources {
                        let label = format!("{} ({})", r.ip, fleet::short_root(&r.status.map_root));
                        let mut sel = *chosen == Some(r.ip.clone());
                        if ui.selectable_label(sel, label).clicked() {
                            sel = true;
                            *chosen = Some(r.ip.clone());
                        }
                        let _ = sel;
                    }
                });

            let targets: Vec<String> =
                rows.iter().filter(|r| r.ip != cur).map(|r| r.ip.clone()).collect();
            let n = targets.len();
            drop(chosen);

            ui.add_enabled_ui(!job_running && n > 0, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .button(format!("⇄ Share map to {n} puck(s)"))
                        .on_hover_text(
                            "Copies this puck's Insight map onto every other puck and waits for\n\
                             each to relocalize into it. They then track in ONE frame with no\n\
                             host-side transform.\n\n\
                             Pucks ALREADY on this map are skipped — use ⟲ Re-sync for those.\n\n\
                             Each target's existing map is archived on-device and on the host\n\
                             first. Restarting tracking resets each target's Insight frame, so\n\
                             stand the pucks still for a moment afterwards to re-bridge, and\n\
                             anything they mapped since their last automatic write is lost.",
                        )
                        .clicked()
                    {
                        match self.service.share_map(&cur, targets.clone(), false) {
                            Ok(_) => {}
                            Err(e) => eprintln!("share_map: {e}"),
                        }
                    }

                    // Same root uuid is NOT the same map. Pucks sharing a root
                    // keep mapping independently, so their CONTENT drifts apart
                    // while their identity does not -- and the skip that makes
                    // Share idempotent then blocks every refresh. This is the
                    // way back to one known-good copy.
                    if ui
                        .button("⟲ Re-sync")
                        .on_hover_text(
                            "Copy this puck's map onto the others EVEN IF they already report\n\
                             the same map.\n\n\
                             Pucks sharing a map root go on mapping independently, so their\n\
                             maps diverge in content while still agreeing on identity — same\n\
                             frame, different detail. Share skips them for exactly that\n\
                             reason; this does not.\n\n\
                             Same safety as Share: every target's map is archived on-device\n\
                             and on the host first. But it DISCARDS what each target mapped\n\
                             on its own, and restarts its tracking — hold the pucks still\n\
                             afterwards to re-bridge.",
                        )
                        .clicked()
                    {
                        match self.service.share_map(&cur, targets.clone(), true) {
                            Ok(_) => {}
                            Err(e) => eprintln!("re-sync map: {e}"),
                        }
                    }
                });
            });
        }

        // ---- jobs: per-step progress, and errors that are never swallowed.
        if !view.jobs.is_empty() {
            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("jobs").small().strong());
                if job_running && ui.small_button("✖").on_hover_text("cancel after the current step").clicked() {
                    self.service.cancel_job();
                }
            });
            for job in view.jobs.iter().rev() {
                let (mark, col) = match job.state {
                    JobState::Ok => ("✔", Color32::LIGHT_GREEN),
                    JobState::Failed => ("✖", Color32::LIGHT_RED),
                    JobState::Cancelled => ("–", Color32::GRAY),
                    _ => ("…", Color32::LIGHT_BLUE),
                };
                egui::CollapsingHeader::new(
                    egui::RichText::new(format!("{mark} {}", job.title)).small().color(col),
                )
                .id_salt(job.id)
                .default_open(job.is_active())
                .show(ui, |ui| {
                    if let Some(e) = &job.error {
                        ui.label(egui::RichText::new(e).small().color(Color32::LIGHT_RED));
                    }
                    for s in &job.steps {
                        let (m, c) = match s.state {
                            StepState::Ok => ("✔", Color32::GRAY),
                            StepState::Failed => ("✖", Color32::LIGHT_RED),
                            StepState::Running => ("…", Color32::LIGHT_BLUE),
                            StepState::Skipped => ("–", Color32::DARK_GRAY),
                            StepState::Pending => ("·", Color32::DARK_GRAY),
                        };
                        let detail =
                            if s.detail.is_empty() { String::new() } else { format!("  — {}", s.detail) };
                        ui.label(
                            egui::RichText::new(format!("{m} {}{detail}", s.name)).small().color(c),
                        );
                    }
                });
            }
        }

        ui.add_space(6.0);
        ui.separator();
        ui.label(egui::RichText::new("events").small().strong());
        for line in view.events.iter().rev() {
            ui.label(egui::RichText::new(line).small().weak());
        }
    }

    fn plot(&mut self, ui: &mut egui::Ui) {
        let v = self.service.view();
        for (d, p) in &v.live {
            let t = self.trails.entry(*d).or_default();
            if t.back().map_or(true, |q| {
                let dx = q[0] - p[0];
                let dz = q[2] - p[2];
                (dx * dx + dz * dz) > 1e-6
            }) {
                t.push_back(*p);
                if t.len() > 900 {
                    t.pop_front();
                }
            }
        }

        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::hover());
        let rect = resp.rect;
        painter.rect_filled(rect, 4.0, Color32::from_rgb(0x0a, 0x0e, 0x12));

        if v.drifted {
            painter.text(
                rect.center_top() + Vec2::new(0.0, 18.0),
                Align2::CENTER_CENTER,
                "separation drifted — hold the pucks still and re-bridge",
                FontId::proportional(14.0),
                Color32::from_rgb(0xe5, 0x68, 0x6a),
            );
        }

        let mut lo = [f32::MAX; 2];
        let mut hi = [f32::MIN; 2];
        let mut any = false;
        for t in self.trails.values() {
            for p in t {
                lo[0] = lo[0].min(p[0]);
                hi[0] = hi[0].max(p[0]);
                lo[1] = lo[1].min(p[2]);
                hi[1] = hi[1].max(p[2]);
                any = true;
            }
        }
        if !any {
            painter.text(rect.center(), Align2::CENTER_CENTER,
                "no live pucks — Launch trackers (bridging is automatic)",
                FontId::proportional(15.0), Color32::GRAY);
            return;
        }
        let cx = (lo[0] + hi[0]) * 0.5;
        let cz = (lo[1] + hi[1]) * 0.5;
        let span = ((hi[0] - lo[0]).max(hi[1] - lo[1])).max(2.0) * 1.2;
        let scale = rect.width().min(rect.height()) / span;
        let to_px = |x: f32, z: f32| -> Pos2 {
            Pos2::new(rect.center().x + (x - cx) * scale, rect.center().y + (z - cz) * scale)
        };

        let grid = Stroke::new(1.0, Color32::from_rgba_unmultiplied(120, 155, 175, 26));
        let (x0, x1) = (cx - span, cx + span);
        let (z0, z1) = (cz - span, cz + span);
        let mut gx = x0.floor();
        while gx <= x1 {
            painter.line_segment([to_px(gx, z0), to_px(gx, z1)], grid);
            gx += 1.0;
        }
        let mut gz = z0.floor();
        while gz <= z1 {
            painter.line_segment([to_px(x0, gz), to_px(x1, gz)], grid);
            gz += 1.0;
        }

        for (d, t) in &self.trails {
            let c = device_color(*d);
            let faded = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 90);
            let pts: Vec<Pos2> = t.iter().map(|p| to_px(p[0], p[2])).collect();
            for w in pts.windows(2) {
                painter.line_segment([w[0], w[1]], Stroke::new(1.5, faded));
            }
        }
        if v.live.len() >= 2 {
            let a = v.live[0].1;
            let b = v.live[1].1;
            painter.line_segment(
                [to_px(a[0], a[2]), to_px(b[0], b[2])],
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(233, 241, 246, 60)),
            );
            if let Some(s) = v.sep {
                let mid = to_px((a[0] + b[0]) * 0.5, (a[2] + b[2]) * 0.5);
                painter.text(mid + Vec2::new(0.0, -10.0), Align2::CENTER_BOTTOM,
                    format!("{s:.2} m"), FontId::monospace(12.0), Color32::LIGHT_GRAY);
            }
        }
        for (d, p) in &v.live {
            let c = device_color(*d);
            let px = to_px(p[0], p[2]);
            painter.circle_filled(px, 6.0, c);
            painter.circle_stroke(px, 9.0, Stroke::new(1.0, c.gamma_multiply(0.5)));
            painter.text(px + Vec2::new(12.0, 0.0), Align2::LEFT_CENTER,
                format!("{}  y {:+.2}", d.label(), p[1]),
                FontId::monospace(12.0), c);
        }
        painter.text(rect.left_top() + Vec2::new(8.0, 6.0), Align2::LEFT_TOP,
            "top-down · shared frame · 1 m grid", FontId::monospace(11.0),
            Color32::from_gray(120));
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let v = self.service.view();
        ui.horizontal(|ui| {
            if let Some(e) = &v.error {
                ui.colored_label(Color32::LIGHT_RED, e);
                ui.separator();
            }
            for (d, st, age, rate) in &v.slots {
                let (txt, col) = match st {
                    SlotState::Live => {
                        (format!("{} {rate:.0} Hz {:.0} ms", d.label(), age * 1e3), device_color(*d))
                    }
                    SlotState::NotTracking => (format!("{} NO-TRACK", d.label()), Color32::YELLOW),
                    SlotState::Stale => (format!("{} STALE", d.label()), Color32::LIGHT_RED),
                    SlotState::Absent => continue,
                };
                ui.colored_label(col, txt);
                ui.separator();
            }
            // A puck streaming valid MPT1 that no config entry claims. Shown
            // rather than dropped: this is exactly what an unprovisioned or
            // wrongly-configured puck looks like, and silence would send
            // someone hunting a network fault that is not there.
            if !v.unknown_sources.is_empty() {
                let list = v.unknown_sources.iter()
                    .map(|s| s.to_string()).collect::<Vec<_>>().join(", ");
                ui.colored_label(
                    Color32::YELLOW,
                    format!("⚠ unclaimed puck id(s) {list} — add to insight-map-loader.json"),
                );
                ui.separator();
            }
            ui.label(format!("out {} pkts → {}", v.emitted, self.cfg.out));
            ui.separator();
            ui.label(format!("{} aligned transforms", v.n_transforms));
            if let Some(s) = v.sep {
                ui.separator();
                ui.label(format!("sep {s:.2} m"));
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::left("fleet").default_size(260.0).show(ui, |ui| self.fleet_panel(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.plot(ui));
        // Repaint cadence is the GPU-load knob: an unconditional short
        // request keeps the swapchain hot and the driver's present threads
        // spinning even when nothing on screen changes (measured: several
        // cores of vk* threads). Idle at 10 Hz; 30 Hz only while a live puck
        // is actually moving.
        let moving = self.service.view().live.iter().any(|(d, p)| {
            self.trails
                .get(d)
                .and_then(|t| t.back())
                .map_or(true, |q| {
                    let dx = q[0] - p[0];
                    let dz = q[2] - p[2];
                    (dx * dx + dz * dz) > 1e-6
                })
        });
        ui.ctx().request_repaint_after(Duration::from_millis(
            if moving { 33 } else { 100 },
        ));
    }
}

fn main() -> eframe::Result<()> {
    // The eframe 0.36 / winit Wayland backend busy-loops on this compositor:
    // an EMPTY window burns a full core, compute-bound in the main thread,
    // regardless of the requested repaint cadence (bisected: every panel
    // skipped, still 99%). The same binary through XWayland idles at 3%. Until
    // that is fixed upstream, prefer X11; INSIGHT_FORCE_WAYLAND opts back in for
    // testing newer versions.
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var_os("INSIGHT_FORCE_WAYLAND").is_none()
    {
        std::env::remove_var("WAYLAND_DISPLAY");
        eprintln!("insight-map-loader-gui: using X11/XWayland (Wayland backend busy-loops;                    set INSIGHT_FORCE_WAYLAND=1 to override)");
    }
    let cfg_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .unwrap_or_else(|| "insight-map-loader.json".into());
    let cfg = match Config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot read {cfg_path}: {e}");
            eprintln!("copy desktop/insight-map-loader.example.json to insight-map-loader.json and run from the repo root");
            std::process::exit(2);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_title("insight-map-loader"),
        ..Default::default()
    };
    eframe::run_native(
        "insight-map-loader",
        options,
        Box::new(|_cc| {
            let app = App::new(cfg, cfg_path.clone()).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                e.into()
            })?;
            Ok(Box::new(app))
        }),
    )
}
