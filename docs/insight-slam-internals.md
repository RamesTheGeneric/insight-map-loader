# How Insight uses its map (and how we can interact with it)

Investigation into the runtime SLAM pipeline — the goal being to align two
headsets' *actual tracking frames* more accurately than the host-side ORB
overlay we ran at the time. Everything here is observed live from
`dumpsys tracking` on `monterey` (Quest 1), which exposes the tracking service's
internal state read-only and unrestricted. Insight = **VIPER / VegaMapper**,
`libtrackingengines.so`.

> **Read this knowing how it ended.** Option 3 in "Implications" below — native
> colocation, both pucks relocalizing into one shared map so Insight aligns
> their frames itself — is what shipped, and it made the overlay redundant
> rather than more accurate. The wall reported here at "anchor creation (VR
> focus) and map load (cloud)" was climbed from a different side: the map is a
> file, and a file can be copied. See `docs/insight-map-lifecycle.md`. What
> follows is kept because the reasoning about frames, descriptors and DoF is
> what made that recognisable when it appeared.

## The map structure (VegaMapper, Structure-of-Arrays)

From the `* VegaMapper *` dumpsys section, the live map is:

```
Keyrigs .. GraphNodes (L1/L2) .. MapPoints(BA/VIO) .. PointResiduals ..
EnergyEdges .. ImagePatch .. Descriptors(BA/VIO) .. Anchors .. Submaps
```

- **Hierarchical pose graph:** L2 root nodes → L1 nodes → **Keyrigs**
  (keyframes) → **MapPoints** (landmarks). `Submaps` group keyrigs.
- **Bundle-adjusted:** MapPoints carry `PointResiduals` (BA residuals) and
  `EnergyEdges` (pose-graph energy). Points are split BA (optimized) vs VIO
  (odometry-only). So the geometry is globally optimized, not per-frame.
- **Descriptors are DEEP, not ORB.** The `* BoltNN *` section lists the neural
  nets in the loop, including **`UCM_FeatureE2E_InferenceModel`** — a learned
  end-to-end feature extractor — plus `DPEImageBackbone`, `SeacliffFT`, etc.
  This matches the config flags `enable_descriptor_vega`,
  `enable_binary_deep_descriptor`, `use_gravity_aligned_deep_descriptors`.
  **This is why Insight relocalizes across lighting where a plain ORB
  pipeline fails** — the literature's SuperPoint-vs-ORB result, in Meta's own stack.

## The tracking-frame chain (map → output pose)

The output pose we consume (MPT1 / dumpsys `Hmd`) is the end of a transform
chain, all exposed live in the `AnchorService` section:

```
T_SmoothWorld_Odometry   so3[0, -0.149, 0]     gravity-aligned yaw + smoothing
T_Odometry_Submap        so3[1.529, 0, 0.008]  submap pose in raw odometry
T_Submap_Anchor          so3[0,0,0]            anchor at submap origin (here)
```

i.e. **SmoothWorld ← Odometry ← Submap ← Anchor(worldOrigin)**. The
`* VegaMapper *` map context names the pinning anchor:

```
anchor 331f43a7 -> kr <id> -> L1 <id>: registered Y, persistent N, worldOrigin Y
BoltCv: anchor 331f43a7  T_localOrigin_anchor  t=(~0)  q=(0.72,0.69,-0.05,0.05)
```

**Each device's tracking frame is pinned to *its own* transient worldOrigin
anchor.** That is exactly why two headsets don't share a frame: separate maps,
separate origin anchors. `submapTransforms` (re)stitch submaps on loop
closure / relocalization.

Useful corollary: `T_SmoothWorld_Odometry` is a pure **yaw** (so3 y-only) — the
frames really are gravity-aligned Y-up, so a **4-DoF model is correct**;
its accuracy ceiling is matching/geometry quality, not the DoF count.

## How relocalization uses the map

The localizer (`LocalizationAttempt` telemetry, seen on recovery) matches the
current frame's deep descriptors against the map's indexed descriptors →
`inliers` → PnP → `T_mapFromImu` (device pose in the map frame), over
`num_relocalization_steps`. Gate metrics exposed: `num_indexed_descriptors`,
`num_total_matches`, `inliers`, `mean_reprojection_error`,
`largest_cluster_size`, `localized_keyrig_id`. This is the machinery that, if
two devices localized into one shared map, would place both in a common frame
at full SLAM accuracy.

## The interaction surface

| Surface | Access | Use |
|---|---|---|
| `dumpsys tracking` | **read-only, unrestricted** | full frame chain, map summary, per-anchor `T_localOrigin_anchor`, reloc metrics. Our observation window. |
| `ITrackingEnvironment` binder | walled | anchor create needs **VR focus**; map colocation (`getCurrentMapUUID`/`listMaps`) is **cloud-gated**. See `insight-map-access.md`. |
| pose shared memory | VR-focus gated | render-rate pose fast path. |
| `/proc/<pid>/mem` | root, hard to parse | live VegaMap SoA; no symbols. |
| `/vision/insideout/mapdb` | root, but empty | MapDB writes only on the (walled) save trigger. |

`dumpsys` does not expose raw MapPoint positions/descriptors — only counts and
the frame transforms. Verbose dump args are rejected (`unhandled cmd`).

## Implications for accurate two-headset alignment

What we now know that bears on the goal:

1. **4-DoF is the right model** (frames are gravity-aligned). The
   accuracy ceiling is feature matching + correspondence geometry, not DoF.
2. **Insight's edge is deep descriptors + global BA.** Two realistic ways to
   borrow it:
   - *Descriptors:* match cross-device with learned features instead of ORB
     (robustness). Hard: running the deep net off Insight.
   - *Geometry:* use Insight's bundle-adjusted MapPoints as the 3D reference
     instead of our own triangulation. Needs extraction (memory; not exposed
     by dumpsys).
3. **Native colocation** (both devices relocalize into one shared map so
   Insight aligns their frames itself) is the most accurate path but is walled
   at anchor creation (VR focus) and map load (cloud). See
   `insight-map-access.md`.
4. **An observable-only angle:** `dumpsys` gives each device's full frame chain
   and any anchor's `T_localOrigin_anchor` live. If two devices could be made
   to place a transient anchor at the *same physical point* (via cross-device
   feature correspondence, which the overlay already found), then
   `T_A_B = T_localOriginA_anchor ∘ inv(T_localOriginB_anchor)` is a full 6-DoF
   alignment from Insight's own optimized estimates — no persistence, no map
   load, no cloud. The open question is whether a transient placed anchor's
   pose is exposed per-device precisely enough; `placeAnchor` returns a pose
   even with `handle=-1`, and BoltCv lists anchor `T_localOrigin_anchor`, so
   this is the most promising un-walled lever to test next.

## KEY: anchors are map-quality-gated, not permission-walled

Correcting the earlier "anchor route is walled" conclusion. Live evidence:

- The `AnchorService` dumpsys lists a **registered non-origin anchor**
  `c14f8f63` with **Raw Handle 1** and a real 6-DoF pose
  `T_localOrigin_anchor = (-1.27, 0.27, -1.72)` + quaternion. It survives an
  `am force-stop` of our app, and the telemetry logged its creation:
  `{"service":"Anchors","event":"PlaceAnchor","handle":{"version":0,"index":1}}`.
  So a `placeAnchor` **did** succeed and create a usable anchor — the handle is
  a `{version,index}` struct (small indices 0,1), not the `-1` we kept reading.
- Our own `placeAnchor` calls fail with `handle=-1` and a varying `status`
  (0/1/6). The binary's failure strings are all about coverage:
  *"Not enough covisible matches"*, *"Not enough features to attempt
  initialization"*, *"Not enough observation to create map points"*. So
  placement is **gated on local map quality**, not on permission or VR focus.
  The worldOrigin anchor itself is system-placed (*"Placing new World Origin
  Anchor"*).
- The binary exposes **`vegaanchormanager.get_rel_pose_to_world_origin`** — an
  anchor's pose relative to the world origin, which is exactly
  `T_localOrigin_anchor` as seen in dumpsys.

**The un-walled 6-DoF alignment path this opens:**

1. Both headsets place a transient anchor at the *same physical point* —
   correspondence found the way the ORB overlay already did it (cross-device
   camera feature match). No persist, no export, no cloud.
2. Read each anchor's `T_localOrigin_anchor` from `dumpsys tracking` (read-only,
   unrestricted).
3. `T_A_B = T_localOriginA_anchor ∘ inv(T_localOriginB_anchor)` — a full 6-DoF
   alignment computed from Insight's own bundle-adjusted estimates, strictly
   better than the 4-DoF ORB overlay.

Open problem: `placeAnchor` succeeds only where coverage is rich (it failed on
the desk with a 22-keyrig map). The reliability, and getting BOTH devices to
place at the same physical point, are the things to solve — but this is a
tractable engineering problem, not a permission wall.

## Next probes

- Trigger a real `LocalizationAttempt` (cover cameras → recovery) and capture
  the full reloc metrics + `T_mapFromImu` to nail the localization math.
- Test whether a **transient `placeAnchor`** shows up in the BoltCv
  `T_localOrigin_anchor` list with a usable pose — the observable-anchor
  alignment idea above.
- Assess extracting Insight's BA MapPoints from `/proc/mem` to feed a 6-DoF
  cross-device solve (higher-quality 3D than our triangulation).

## Meta's own answer: self-tracked controllers align by SHARING A MAP

Captured live from a Quest 2 (hollywood) with Quest Pro / Starlet self-tracked
controllers connected. The `SelfTrackedControllers` service telemetry shows the
alignment mechanism directly:

```
"service":"SelfTrackedControllers","event":"MapShared","mapSizeBytes":73239,"id":-5953468595469190665
"service":"Mapper","event":"SubmapExport","mapPointCount":151,"keyrigCount":3,"residualCount":592,"uuid":"99c9fc49"
```

**How Meta aligns two independently-SLAMing devices (headset + each controller):**

1. The headset runs VegaMapper and, every ~3 s, exports a **compact submap** —
   here **~73 KB**, **151 map points**, **3 keyrigs** (the `SubmapExport` task
   we found walled on Quest 1 runs automatically here).
2. It **shares** that submap to the controllers (`MapShared`, tagged with a
   64-bit id/hash).
3. Each controller — which has its own cameras + IMU + SLAM — **relocalizes
   into the shared submap** using deep descriptors, which pins its tracking
   frame to the headset's frame, continuously and drift-corrected.

**This was the decisive observation.** Sharing a map and relocalizing into it
*is* how Meta aligns multiple independently-SLAMing devices — so what this
project ended up doing is not a workaround for a walled API, it is the same
architecture, and Insight's deep descriptors and bundle adjustment come along
with it. That is why the ORB overlay was deleted rather than improved.

Where this project now sits against it:

| | this project | Meta's controller alignment |
|---|---|---|
| map unit | one whole map, copied as files | **compact submap (~150 pts, 73 KB)** |
| cadence | shared once, at founding | **re-shared every ~3 s** (drift-corrected) |
| features | Insight's deep descriptors | the same |
| frame result | native 6-DoF relocalization | the same |

Two of the four rows now match; the gap left is **cadence and granularity**, not
accuracy. A one-shot share means the pucks agree at founding and then each holds
its own frame independently — which is exactly why the drift monitor and the
bridge watchdog exist.

**The refinement this points to:** continuous exchange of compact submaps
between pucks plus relocalize-into-latest, instead of a single founding map. One
puck (the hip, say) plays the "headset" role and publishes submaps; the others
relocalize into the freshest one. That is drift-corrected shared-frame alignment
by construction — no re-found, no staleness, and it degrades gracefully as pucks
move apart. The *pattern* is implementable on the map format decoded here, with
no need for Meta's transport. It is unbuilt.

Also confirmed here: the same VegaMapper/`SubmapExport` machinery as Quest 1,
plus a `SlamAnchorServer` shared across `mrsystemservice` / `guardian` /
`vrshell` / `com.facebook.spatial_persistence_service`, and live per-camera
calibration (`so3_device_from_camera`, `t_device_from_camera`, projection
coeffs) in the `Calibration Publication` events — a clean way to pull exact
extrinsics/intrinsics without the calibration-file parsing.
