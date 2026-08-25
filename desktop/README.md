# insight-map-loader desktop

The manager that turns the tracker pucks into one tracking system: it brings the
headsets up, bridges their frames, and streams every puck's pose in a single
shared frame.

```
cp desktop/insight-map-loader.example.json insight-map-loader.json  # edit IPs + host address
cargo build --release --manifest-path desktop/Cargo.toml

insight-map-loader up      # adb connect, write tracker config (one MPT1 slot per puck,
                           # controllers off), defeat the prox gate, launch the app
insight-map-loader status  # per puck: Insight level, battery, tracker up, VPN trap
insight-map-loader run     # the service: ingest -> bridge -> shared-frame MPT1 to `out`
```

## Why there is almost no alignment left

The pucks share **one Insight map**, so they are already in one world frame and
the transform between them is the identity — measured at 3.3 cm horizontally
between two co-located pucks, which is the mount geometry, not a residual. The
whole discrete-alignment track (localization into a landmark map, per-puck
`T_map_world`, camera grabs, an OpenCV solver) was deleted, not disabled: a
shared map does the job better, and keeping a second mechanism that could
disagree with it would only produce two answers.

What remains is the **bridge**, and only because the tracker app publishes poses
in its OpenXR session's LOCAL space rather than in Insight's world space. Both
are gravity-aligned, so one still observation relates them in 4 DoF. It is a
per-puck, per-session quantity: any tracker restart can move the LOCAL frame.

**Nothing needs pressing.** The service solves a missing bridge on first run and
verifies each existing one every few seconds — predict the dumpsys world pose
through the bridge, compare — and re-bridges at the puck's next still moment,
detected from its own stream. Verified live: a 144.6° frame change was detected
and healed unattended, while a restart that happened to keep its frame triggered
nothing. `insight-map-loader bridge` remains as a manual override.

Three things invalidate a bridge, and each announces itself on the stream, so
the service watches for all three:

* **A reboot** — `t_ns` is the device's boot clock, so a fresh boot sends
  timestamps hours behind the previous ones.
* **A teleport** — a pose moving faster than a human can is a frame event, not
  motion.
* **Unexplained drift** — separation changing by more than the pucks' own
  reported motion allows (physically `|d(sep)/dt| <= |v_A| + |v_B|`, so carrying
  the pucks around never flags; a frame jumping under a stationary puck does).

The caveat stands: separation detects a *change* in agreement, not an *error* in
it. A wrong-but-stable frame still needs `tools/q1sep.py` — two co-located pucks
are the only measurement here with real ground truth.

Verified end-to-end against the independent adb/dumpsys path: identical
shared-frame positions to the centimetre, 71–72 Hz per puck through the full
pipeline, timestamps rewritten to the host clock so the downstream latency
estimator sees one epoch.

## Layout

`insight-map-loader-core/` is the UI-free library (wire format, ingest,
transforms, bridge, fleet, job runner, aggregator) plus the
`insight-map-loader` CLI binary. `insight-map-loader-gui/` (egui) is the same
pipeline with a window on it: fleet cards with per-puck health, a role dropdown
per puck, one-button map create and map share, a live top-down view of every
puck in the shared frame with trails and the separation readout, and a status
bar with per-slot Hz and age.

Long-running fleet operations go through a **job queue** rather than running on
the UI thread — sharing a map takes tens of seconds and touches every puck, and
a half-applied share is worse than none, so jobs are serialized and each step
reports a real error instead of a boolean.

```
cargo run --release -p insight-map-loader-gui     # from the repo root
```

The GUI runs the aggregation itself on a background thread, so it IS the service
while it is open — run either the GUI or `insight-map-loader run`, not both
(they would fight over the listen port; the loser tells you).

## SteamVR driver

`steamvr_driver/` turns the aggregator's output into real SteamVR devices:
`GenericTracker`s masquerading as Vive trackers, one per assigned role (waist,
chest, elbows, knees, ankles, feet — 11 slots), velocities filled,
`poseTimeOffset` stamped from each pose's own age. A tracker is registered
lazily, the moment its puck first streams, so an IK solver never sees a phantom
device standing at the origin.

```
cd desktop/steamvr_driver && cmake -B build && cmake --build build
<SteamVR>/bin/linux64/vrpathreg adddriver "$PWD/build/mapper"
# restart SteamVR; it logs "mapper: init ok, listening on udp/5181"
```

It listens on udp/5181 — the service's `out` — because 5180 on this host belongs
to the puck→service ingest. One manual step remains and is expected: the shared
frame and the HMD's universe are different spaces, so run OpenVR-SpaceCalibrator
(or the game's own calibration) once per play session to marry them — the same
step every mixed-tracking FBT setup does.
