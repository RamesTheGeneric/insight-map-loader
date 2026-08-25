# q2slam desktop

The manager that turns the tracker pucks into one tracking system: it brings
the headsets up, bridges their frames, and streams every puck's pose in a
single shared frame.

```
cp desktop/q2slam.example.json q2slam.json    # edit IPs + host address
cargo build --release --manifest-path desktop/Cargo.toml

q2slam up        # adb connect, write tracker config (one MPT1 slot per puck,
                 # controllers off), defeat the prox gate, launch the app
q2slam status    # per puck: Insight level, battery, tracker up, VPN trap
q2slam run       # the service: ingest -> bridge -> alignment -> shared-frame
                 # MPT1 to `out`. Bridging is AUTOMATIC (see below); `q2slam
                 # bridge` remains as a manual override.
```

Why the bridge exists: the tracker app publishes poses in its OpenXR session's
LOCAL space, but the inter-puck alignment (`align_result.json`, solved by
`tools/align_pool.py` and refined by `tools/align_map.py`) relates the pucks'
*Insight world* frames. Both are gravity-aligned, so one still observation of
the same headset in both frames gives the 4-DoF between them.

**Nothing needs pressing.** Alignment is LOCALIZATION into a persistent
landmark map (`map/`, owned by `tools/q1mapd.py`, spawned as a child of the
service). Every puck — there is no reference puck — carries a `T_map_world`
in `transforms.json`; the service watches that file and hot-applies changes.
A puck with no transform (brand new, or rebooted into a moved frame) simply
**joins by walking**: once its pose spreads ~0.6 m the service grabs a dozen
timestamped snapshots from the puck's own cameras (`tools/q1grab.py`, gated
on viewpoint diversity and calm instants), the daemon localizes them against
the map (cold-start if there is no seed), and an accepted result (>=3000
inliers, <=1.5° residual) lands in `transforms.json`. A puck that stays still
is deliberately never localized — measured worse than keeping the stored
transform. The service runs two watchdogs besides:

* The *bridge watchdog* solves a missing bridge on first run and verifies the
  existing one every few seconds (predict the dumpsys world pose through the
  bridge; compare). A failing check -- a tracker restart that moved the LOCAL
  frame -- re-bridges at the puck's next still moment, detected from its own
  stream. Verified live: a 144.6-degree frame change was detected and healed
  unattended; a restart that happened to keep its frame triggered nothing.
* The *drift monitor* flags separation change the pucks' own reported motion
  cannot explain (physically |d(sep)/dt| <= |v_A|+|v_B|, so carrying the pucks
  around never flags; a frame jumping under a stationary puck does). A pose
  teleporting faster than a human moves additionally routes that puck straight
  to bridge verification. On a confirmed drift it flags the alignment, and
  with `"auto_realign": true` runs `tools/q1realign.sh` itself (cooldown 10
  min; off by default because it records cameras for ~25 s and solves for
  minutes). `run` hot-reloads `align_result.json` either way.

The caveat measured in Phase 2 stands: separation detects alignment *change*,
not alignment *error* -- a wrong-but-stable transform still needs a re-solve.

Verified end-to-end against the independent adb/dumpsys path (q1track.py):
identical shared-frame positions to the centimetre, 71-72 Hz per puck through
the full pipeline, timestamps rewritten to the host clock so the downstream
latency estimator sees one epoch.

Layout: `q2slam-core/` is the UI-free library (wire format, ingest, transforms,
bridge, fleet, aggregator -- 13 unit tests) plus the `q2slam` CLI binary.
`q2slam-gui/` (egui) is the same pipeline with a window on it: fleet cards with
per-puck health, buttons for launch / bridge / re-solve, a live top-down view
of every puck in the shared frame with trails and the separation readout, and
a status bar (per-slot Hz and age, packets out, current alignment yaw).
The GUI runs the aggregation itself on a background thread, so it IS the
service while it is open -- run either the GUI or `q2slam run`, not both (they
would fight over the listen port; the loser tells you).

```
cargo run --release -p q2slam-gui     # from the repo root
```

The alignment *solvers* stay in `tools/` (they need OpenCV); the core consumes
their JSON output.

## SteamVR driver

`steamvr_driver/` (ported from Mapper-Localizer, built unchanged apart from the
port number) turns the aggregator's output into real SteamVR devices:
`GenericTracker`s masquerading as Vive trackers with waist / left_foot /
right_foot roles pre-assigned, velocities filled, `poseTimeOffset` stamped
from each pose's own age. Unfed slots report disconnected so an IK solver
never drags a phantom around — a tracker appears the moment its puck streams.

```
cd desktop/steamvr_driver && cmake -B build && cmake --build build
<SteamVR>/bin/linux64/vrpathreg adddriver "$PWD/build/mapper"
# restart SteamVR; it logs "mapper: init ok, listening on udp/5181"
```

It listens on udp/5181 — the q2slam service's `out` — because 5180 on this
host belongs to the puck→service ingest. One manual step remains and is
expected: the shared frame and the HMD's universe are different spaces, so run
OpenVR-SpaceCalibrator (or the game's own calibration) once per play session
to marry them — the same step every mixed-tracking FBT setup does.
