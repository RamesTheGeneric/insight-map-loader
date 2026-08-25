# Insight's own map, anchors and relocalization

Short answer to "does Insight have a relocalize system we can use": **yes, but
not for moving map data between our pucks.** Insight's own SLAM map, anchors
and relocalization are readable and even callable without reverse-engineering
anything exotic -- but every route that could get map bytes off one headset
and onto another turned out to be closed, either by a signature-level Android
permission we cannot obtain, or by a runtime capability tied to Meta's cloud
service, which this project has deliberately declined to enable. See
[Can we call these ourselves?](#can-we-call-these-ourselves-the-permission-split----and-its-correction)
for the full, tested verdict. The shared tracking universe has to be built
from this repo's own camera + IMU pipeline.

## State of play

Established so far:

| | status |
|---|---|
| Insight has a full SLAM map, anchors and relocalization | confirmed |
| Live state readable via `dumpsys TrackingEnvironment` | confirmed |
| Relocalization forceable on demand | confirmed, `stop`/`start trackingservice` |
| Rich JSON telemetry incl. `T_mapFromImu` | confirmed, `dumpsys tracking` |
| Cross-session relocalization into a persisted map | confirmed, identical root node across 3 restarts |
| Map lives in RAM in the persistence service, not on disk | confirmed |
| Anchor and map API reachable over binder | **confirmed, called successfully** |
| `getCurrentMapUUID` returns the live map id from shell (root) | **confirmed** |
| Map read/write over an fd, keyed by uuid | **blocked** -- needs a `signature\|preinstalled` permission we cannot obtain |
| Per-anchor map export/import | **blocked** -- also needs an unheld `signature` permission; the `normal` `USE_ANCHOR_API` does *not* gate it, ruled out by a controlled test |
| `isLocalAnchor` -- foreign anchors are a first-class concept | confirmed to exist; untested (blocked upstream by `placeAnchor`) |
| Map *store* to enumerate | **empty** -- `listMaps()` returns nothing, on every check |
| `MapSharingHeadset` capability | loads every session |
| `enable_map_db` property | **confirmed dead end** -- no effect even with the service restarted |
| `placeAnchor` and the anchor lifecycle | **blocked** -- capability gate, caller-independent (needs `SpatialPersistenceService`, the cloud component) |
| Cross-device: does B accept A's map? | **moot** -- there is no map data reachable to test with |

## Read this first: it is already exposed

```bash
adb shell dumpsys TrackingEnvironment
```

```
TrackingEnvironment Status
  Internal SLAM Anchor Info
    Odometry Uuid: 1667b0fb-c3bb-0c37-3038-066af96b0ada
    Device IS NOT relocalizing
    Submap #0  ID: 2@1127886948  UUID: 00000000-...  T_odometry_submap
      translation: -1.335237 -0.498358  0.275335
      rotation: -0.037963 -0.971994  0.029774 -0.230001
    Anchor #1  Handle: 30064771073  UUID: 1a97f72c-f94b-e4d4-2f3f-24f1fe0a2031  Submap: 1  T_submap_anchor
      translation:  0.005081  0.013726 -0.000577
      rotation:  0.000370 -0.000717  0.000098  1.000000
```

That is the live SLAM state: an odometry frame UUID, whether the device is
currently relocalizing, the submaps with their `T_odometry_submap` transforms,
and anchors with UUIDs and their `T_submap_anchor` transforms.

**That is the alignment primitive, handed to us.** If two devices hold the same
anchor UUID, each one can read its own pose relative to that anchor and the
shared frame follows -- no SLAM, no map merging of our own, no markers.

## The persistence is real, and it links to the live state

`/data/data/com.facebook.spatial_persistence_service/files/com.oculus.guardian/`
holds a UUID-named blob (10.5 KB here). It is Thrift-encoded, and the anchor UUID
printed by `dumpsys` above appears at offset 74 of that file. So the anchors you
can see live are the anchors that persist across sessions -- which is what makes
Guardian survive a reboot.

## What the engine actually contains

`/system/lib64/libtrackingengines.so` is 26 MB and exports exactly three symbols
(`capabilityRegistryCreate` / `Destroy` / `GetAbiVersion`), so there is no public
API. But its internal symbols and strings describe the system fully:

- The map format is **Vega Map**, namespaced `arvr::thrift_if::vega_map`, with
  `relocalization::L1RelocData`, `pose_graph::L1PGData`, `anchor::AnchorPath`.
- **Submaps** are the unit of map storage -- `batchLoadSubmapList`,
  `SubmapRecords`, `CameraFromSubmapLUT`, `WifiSubmapListProvider`.
- **Persistence** through `FileSystemMapDatabase`, `MapDatabaseInterface`,
  `VegaMapLoaderInterface`, `savePGParentBowAnchor` (pose graph + bag-of-words +
  anchor together).
- **Relocalization** in foreground and background:
  `num_frames_to_relocalize_avg`, `num_frames_to_background_relocalize_p50/p90`,
  `isRelocalizing transitions false -> true`.
- **Map merging exists**, with alignment: *"Attempting to run Vega offline map
  merge..."*, *"Aborting map merge attempt - Map alignment failed"*, *"Map
  enrichment mode: There are not enough matched map points hosted by incoming
  anchors nodes"*, *"Map merge execution has been enabled"*, and *"Enabling Map
  Merge requires the tracking service to restart"*.
- **Deep descriptors** -- there is a PyTorch runtime inside
  (`monterey_tracker_torch_ops`), with gravity-aligned variants. Note that they
  exploit the same gravity observation we identified as free.

## Configuration surface

The engine reads `persist.trackingservice.*` properties. All are unset on this
device, i.e. at defaults:

```
persist.trackingservice.enable_map_db
persist.trackingservice.enable_descriptor_vega
persist.trackingservice.enable_binary_deep_descriptor
persist.trackingservice.use_gravity_aligned_deep_descriptors
persist.trackingservice.enable_half_rate_slam
persist.trackingservice.enable_relaxed_map_point_culling
persist.trackingservice.enable_more_ransac_iterations_localization
persist.trackingservice.enable_tracking_init_at_framerate
persist.trackingservice.enable_fallback_descriptors
persist.trackingservice.enable_keypoint_cache_size_5
persist.trackingservice.enable_short_vit
```

Debug outputs the engine knows about, in a world-writable directory that is
currently empty:

```
/data/misc/tracking/relocalization_log.json
/data/misc/tracking/frame_log.json
/data/misc/tracking/recordings/debug_sequences
```

**Nothing here has been enabled.** These are persistent properties that change
how the headset tracks, and turning them on is a decision to make deliberately,
not a probe. `enable_map_db` is the interesting one: in the current dump the
submap UUIDs are all zero and only one anchor is real, which is consistent with
the map database being off.

## Forcing a relocalization attempt

`stop` and `start` on the init service creates a new tracking session, and a new
frontend session always relocalizes:

```bash
adb shell logcat -c
adb shell stop trackingservice; sleep 2; adb shell start trackingservice
adb shell dumpsys TrackingEnvironment | grep -E 'Odometry Uuid|relocalizing'
```

The odometry UUID changes (new session) and the device reports
`Device IS relocalizing` for roughly two seconds. Proximity broadcasts do **not**
do this -- they affect display power, not the tracking session, and the odometry
UUID stays put.

Tracking recovers on its own; `dumpsys tracking` shows 6DOF valid again within a
few seconds.

### The telemetry is the useful part

`dumpsys tracking` ends with a JSON block of recent tracking events, and logcat
carries the same pipeline under the `[CT]` tag. A forced attempt yields:

```
Localizer::LocalizationAttempt
   loc_success: True
   num_frames_to_loc: 16
   num_indexed_descriptors: 17454
   num_unique_points: 1240
   num_total_matches: 321
   inliers: 66
   num_images: 4
   mean_reprojection_error: 0.000131
   localizedRootNodeId: 8994724e-236f-76ff-3e0c-768e3a0a5628
   localized_keyrig_id: 75f9b421-f48c-4641-5f37-dc249e5321d8
   T_mapFromImu: {translation, rotation}
   descriptorTypes: ["orb"]
```

**`T_mapFromImu` is the device's pose in the map frame.** If two devices localize
against the same `localizedRootNodeId`, their `T_mapFromImu` are expressed in a
common frame, and the shared universe falls out of subtracting them. That is the
whole alignment problem, answered by a field Insight already prints.

The matching logcat sequence:

```
VEGA_MAPPER: == Initializing a new Map Context ==
VEGA_MAPPER: Scheduling localization on root nodes 8994724e-...
VEGA_MAPPER: Reset Foreground reloc timer for new frontend session and start relocalizing.
VEGA_MAPPER:ANCHOR_MANAGER: isRelocalizing transitions false -> true
Vega:SubmapPublisher: Tracking parent changed from 00000000-... to 75f9b421-... -> Publish
VegaTrackTracker: Starting map tracking on map. SubmapId Submap id = 1
VEGA_MAPPER:ANCHOR_MANAGER: isRelocalizing transitions true -> false
VEGA_MAPPER:ANCHOR_MANAGER: Gained map tracking. placeAnchor() will now work.
VEGA_MAPPER:SubmapTrackingValidator: Pausing map building until validation is complete. 26.3 seconds remaining
VEGA_LOCALIZER: Added 72 orb descriptors to the index. It has 17526 descriptors for 1244 points
```

Note `placeAnchor()` -- there is an anchor placement API, and the telemetry also
shows `Anchors::PlaceAnchorFailure reason: "invalid handle"`, so it is reachable
and currently being called with a bad handle by something.

### What this proves

The pipeline runs on demand, uses all four cameras and ORB descriptors, and
reports a full result including a map-frame pose.

`T_mapFromImu` came back as identity on this first attempt, which initially
looked like the device localizing against a map it had just built in the same
session. Repeating the restart settled it the other way -- see the next section.

## Where the maps come from

**Created** by the Vega mapper inside `trackingservice`. Watching a session
start, in order:

```
VEGA_MAPPER: == Initializing a new Map Context ==
Vega:MapBuilderBase: Created keyRig with frameID 27 and uid 380924fc
Vega:MapMetaDataUtils: Initialize metaTopNode to L2: 94419e33
VEGA_MAPPER:ANCHOR_MANAGER: Placing new World Origin Anchor Uid 9cafc52d-... in Context 94419e33-...
VegaBAOptimize: Num points adjusted: 51 ... Cameras 4
VEGA_MAPPER: Scheduling localization on root nodes 8994724e-236f-76ff-3e0c-768e3a0a5628.
VEGA_MAPPER: == Switching Map Context from 94419e33 to 8994724e ==
VEGA_MAPPER:SubmapTrackingValidator: Relocalized into new context, starting mapper validation.
```

So each session **builds a fresh context, then relocalizes into the pre-existing
one and switches to it**. Keyrigs are the map unit, bundle-adjusted across all
four cameras, gathered into versioned submaps and published by `SubmapPublisher`.

**Persisted** by `com.facebook.spatial_persistence_service`, whose log tag is
`AnchorPersistenceService`, reached over RuntimeIPC on an endpoint called
`anchor_persistence_server`. `com.oculus.vrguardianservice` is another client of
it. It logs *"Legacy (ephemeral) SPS is requested"*, which explains the apparent
contradiction below.

**This is cross-session relocalization, confirmed.** Restarting `trackingservice`
three times gave an identical `localizedRootNodeId`
(`8994724e-236f-76ff-3e0c-768e3a0a5628`) and keyrig (`75f9b421-...`) each time,
with descriptor counts drifting slightly (17454 / 17438 / 17457) as the map keeps
being refined. An earlier note in this document guessed this was in-session
localization against a freshly built map; that was wrong.

**The map is not on disk.** A scan of all of `/data` for the root node UUID as
raw bytes found nothing, yet the map survives a `trackingservice` restart --
because `spatial_persistence_service` keeps its own pid (2604, ~49 MB RSS) across
those restarts and is holding it in memory. Combined with the "ephemeral" log
line, the map most likely does not survive a reboot in the current
configuration. `persist.trackingservice.enable_map_db` is presumably what moves
it to `FileSystemMapDatabase`.

## The bit that matters most: sharing is built in

Three capabilities load at every session start:

```
TrackingService: Loading InternalAnchorApiServer capability v2
TrackingService: Loading MapSharingHeadset capability v2
```

plus a `SpatialPersistenceCapability` referenced alongside them.

`trackingservice` carries a full anchor API behind that registry -- the strings
are the "capability not available for" guards on each entry point:

```
placeAnchor      persistAnchor    locateAnchor
registerAnchor   deregisterAnchor getAnchorUuid
isLocalAnchor    setAnchorUpdateCallback
```

And `libtrackingengines.so` contains map export and import as a first-class
feature, with alignment on import:

```
Exporting map to {}                       exportMapFile
Exported selected submaps with stats:     exportMapDataForAnchor
Imported entire map with stats: {}        importMapDataForAnchor
All anchors attempting to be imported already exist in the map,
    skipping alignement and insertion!
Attempted to import map before streaming map to server
Context storage is empty, cannot export map!
```

Note *"streaming map to server"* and the ability to export **selected submaps**
rather than everything.

So the machinery for the entire multi-device problem -- place an anchor, persist
it, export a map, import someone else's, align on import, locate an anchor --
already exists on the device, is loaded at runtime, and we have root.

These sit behind the capability registry and the binder service below -- and that
service turned out to be directly callable, which the next section covers.

## The colocation API, with transaction codes

`oculus.internal.ITrackingEnvironment` is a normal AIDL interface, declared in
`/system/framework/com.oculus.os.platform.jar`. Parsing the `Stub` class's
`TRANSACTION_*` constants out of the dex gives the exact call numbers:

| code | method | | code | method |
|---|---|---|---|---|
| 1 | `getSharedMemory` | | 12 | `deregisterAnchor` |
| **2** | **`writeMap`** | | 13 | `persistAnchor` |
| **3** | **`readMap`** | | 14 | `locateAnchor` |
| **4** | **`getCurrentMapUUID`** | | 15 | `isLocalAnchor` |
| 5 | `keepMap` | | 16 | `getAnchorUuid` |
| 6 | `removeAllMaps` *(destructive)* | | **17** | **`exportMapDataForAnchor`** |
| **7** | **`loadMap`** | | **18** | **`importMapDataForAnchor`** |
| **8** | **`listMaps`** | | 19 | `getDebugInfo` |
| 9 | `stopMapExpansion` | | 20 | `setOrientationOnlyMode` |
| 10 | `placeAnchor` | | 21 | `isInOrientationOnlyMode` |
| 11 | `registerAnchor` | | 22 | `setControllerMotionModelSettings` |

**Do not call 6.** `removeAllMaps` would destroy the device's map.

### Verified working from a shell

```bash
$ adb shell service call TrackingEnvironment 4      # getCurrentMapUUID
Result: Parcel(... '8.9.9.4.7.2.4.e.-.2.3.6.f.-.7.6.f.f.-.3.e.0.c.-.7.6.8.e.3.a.0.a.5.6.2.8' ...)
```

`8994724e-236f-76ff-3e0c-768e3a0a5628` -- **the same id the localization
telemetry reports as `localizedRootNodeId`.** So the map identity is readable
with one shell command, and it matches what relocalization localizes against.

`readMap` (3), `isLocalAnchor` (15) and `getAnchorUuid` (16) also returned data
rather than errors when called bare. `listMaps` (8) returned a well-formed **empty array** -- no exception, zero
entries. Read alongside `getCurrentMapUUID` working, that says there *is* a
current map but the *store* it would enumerate is empty, which fits everything
else: the map lives in the persistence service's RAM and the map database is not
active. So `listMaps` / `loadMap` / `readMap`-by-uuid have nothing to operate on
yet.

### The signatures: this is a map transport API

Pulling the AIDL parameter types out of the same dex settles what these do:

```java
// map, by uuid, over a file descriptor
long     readMap(ParcelFileDescriptor fd, String mapUuid)          // map  -> fd
boolean  writeMap(ParcelFileDescriptor fd, String mapUuid, long n) // fd   -> map
boolean  loadMap(String mapUuid, double timeout)
String[] listMaps()
boolean  keepMap(String mapUuid)
boolean  removeAllMaps()

// per-anchor slice of map data, same shape
long     exportMapDataForAnchor(ParcelAnchorUuid a, ParcelFileDescriptor fd)
long     importMapDataForAnchor(ParcelAnchorUuid a, ParcelFileDescriptor fd, long n)

// anchors
ParcelAnchorPlacementData placeAnchor()
ParcelAnchorUuid          persistAnchor(ParcelAnchorHandle h)
ParcelAnchorHandle        registerAnchor(ParcelAnchorUuid a)
boolean                   locateAnchor(ParcelAnchorUuid a, double timeout)
boolean                   isLocalAnchor(ParcelAnchorUuid a)
String                    getCurrentMapUUID()
```

`readMap` serialises a map out to an fd; `writeMap` takes one back in. That is a
map transport API, and the intended flow reads straight off the signatures:

```
device A:  placeAnchor() -> persistAnchor() -> uuid
           exportMapDataForAnchor(uuid, fd)        # or readMap(fd, mapUuid)
                     -- any transport you like --
device B:  importMapDataForAnchor(uuid, fd, n)     # or writeMap(fd, mapUuid, n)
           registerAnchor(uuid) -> handle
           locateAnchor(uuid, timeout)             # B now knows where A's anchor is
```

**`isLocalAnchor(uuid)` is the tell.** The API has a first-class notion of an
anchor that is *not* local -- one that arrived from somewhere else. That only
exists in a system meant to consume other devices' anchors.

This is Meta's Shared Spatial Anchors, implemented at the tracking layer, on a
Quest 1 -- the feature the marketing pages say needs Horizon OS and enhanced
spatial services.

### The permission names say what this is for

`trackingservice` checks these:

```
com.oculus.permission.ACCESS_TRACKING_ENV
com.oculus.permission.COLOCATION_API_GET_MAP_UUID
com.oculus.permission.COLOCATION_API_READ_MAP
com.oculus.permission.COLOCATION_API_WRITE_MAP
com.oculus.permission.IMPORT_EXPORT_IOT_MAP_DATA
```

There is a **Colocation API**, named as such, with separate read and write map
permissions, on a device we have root on. `getDebugInfo` (19) returns
"Permission denied in TrackingEnvironment", so the gating is real and some calls
will need the permission granted or a system-signed caller.

## Dead end: `enable_map_db`

Setting `persist.trackingservice.enable_map_db` to `1` and then `TRUE`, with a
service restart each time, changed nothing observable -- same root node, no new
files, no map-db logging. The reason showed up in logcat:

```
TrackingService: Attempted to control leds, but gatekeeper is disabled.
[CT] Gatekeeper querying not available
MobileConfigAccessor: Failed to update sessionless configs within timeout 20000 ms
```

**Gatekeeper is disabled and the device cannot reach Meta's config service**, so
every `oculus_*` flag sits at its compiled default. The engine reads 72 of them,
including several we would want:

```
oculus_enable_map_streaming        oculus_enable_offline_map_merge
oculus_offline_map_merge           oculus_save_kc_in_map_db
oculus_use_mega_submap             oculus_enable_multiframe_localization_query
oculus_vps_query_lsa               oculus_enable_wifi_collection
```

Only fifteen of those have a `persist.trackingservice.*` local override, and
**map streaming and offline map merge are not among them**. The property was
reverted; the device is as it was.

The engine does log *"Applied config file {}.conf"* and *"Clearing Constellation
config file overrides"*, so a file-based override path exists and is the thread
to pull if these flags turn out to be needed.

## The IPC surface

Two binder services are registered:

```
43  tracking:            oculus.internal.tracking.ITrackingService
44  TrackingEnvironment: oculus.internal.ITrackingEnvironment
```

`dumpsys TrackingEnvironment` is the human-readable view of the second. For real
use we would want the binder interface itself rather than scraping text -- that
is undocumented and would need the usual transaction-code work, but it is a far
smaller job than writing a SLAM system, and `dumpsys` is enough to prototype
against.

## Why this changes the plan

The earlier conclusion in
[multi-device-alignment.md](multi-device-alignment.md) was that a shared map is
load-bearing and we would have to build it. We do not. Insight hosts the map,
relocalizes into it, exposes anchor poses, and ships an API whose entire purpose
is moving maps and anchors between devices. The job is plumbing, not SLAM.

## Where this stands, and what is next

Settled:

- Insight builds, persists and relocalizes into a map, across sessions.
- The live map identity, submaps, anchors and relocalization state are readable
  with `dumpsys`, and the binder service is directly callable --
  `service call TrackingEnvironment 4` returns the current map uuid.
- The API is a map transport API, by signature, with a first-class notion of
  non-local anchors.

**Update: items 1-3 below are now closed, not just blocked.** Read
[Can we call these ourselves? The permission split -- and its correction](#can-we-call-these-ourselves-the-permission-split----and-its-correction)
below for the full evidence. Left in place as the historical record of how the
investigation actually proceeded.

1. ~~Nothing to transport yet.~~ **Closed.** `listMaps()` returns an empty
   array on every check across this whole investigation; the map only ever
   lives in the persistence service's RAM.
2. ~~`enable_map_db` cannot open it.~~ **Closed, dead end confirmed.** Set to
   `1` and `trackingservice` restarted: `listMaps()` still `0`, `keepMap()`
   still `false`. Reverted.
3. ~~The fd-based calls need a real client.~~ **Built (`tools/q1mapjava`,
   `tools/q1anchorapp`), and closed.** A real client reaches every method,
   including the fd-based ones. `readMap`/`writeMap`/`getCurrentMapUUID` need
   a `signature|preinstalled` Android permission (Meta's key, or `/system`
   preinstall -- fastboot is off-limits). `exportMapDataForAnchor`/
   `importMapDataForAnchor` looked reachable via the **normal**
   `USE_ANCHOR_API` permission, which any installed app gets automatically --
   but a controlled test proved that permission does not gate them either; a
   different, unheld signature permission does. No path produces map bytes on
   this build.

What is left, now that the colocation API is closed as a source of shared map
data:

4. **Build the shared universe ourselves.** The camera + IMU pipeline in this
   repo is the actual path forward -- see [multi-device-alignment.md](multi-device-alignment.md).
5. **The puck viewpoint problem.** Insight relocalizing well from a head is not
   evidence it will relocalize from an ankle, and is now moot for Insight's own
   map specifically, but remains relevant for whatever SLAM this project ends
   up running.

## Reproducing all of this

```bash
# live state
adb shell dumpsys TrackingEnvironment
adb shell dumpsys tracking                    # ends with JSON telemetry

# force a relocalization attempt
adb shell logcat -c
adb shell stop trackingservice; sleep 2; adb shell start trackingservice
adb shell logcat -d | grep -E "VEGA|Vega|Localizer"

# call the colocation API  (NEVER call 6, removeAllMaps)
adb shell service call TrackingEnvironment 4  # getCurrentMapUUID
adb shell service call TrackingEnvironment 8  # listMaps
```

Transaction codes were recovered by parsing `TRANSACTION_*` constants out of
`/system/framework/com.oculus.os.platform.jar`; the throwaway dex parser used for
that is not checked in, but the codes and signatures are tabulated above.

---

# The fd-capable client, and what it revealed

Open item: *"`service call` cannot pass a `ParcelFileDescriptor`, so `readMap`,
`writeMap`, `exportMapDataForAnchor` and `importMapDataForAnchor` are
unexercised."* That is now closed -- they are reachable.

## tools/q1mapjava

A small Java tool run under `app_process`, so it can build a real `Parcel`
with a real fd without needing an APK, signing, or any permission grant:

```bash
./tools/q1mapjava/build.sh            # -> tools/q1mapjava/q1maptool.jar
adb push tools/q1mapjava/q1maptool.jar /data/local/tmp/
adb shell "CLASSPATH=/data/local/tmp/q1maptool.jar app_process / com.q1.MapTool uuid"
```

Commands: `uuid`, `list`, `debug`, `keep`, `load`, `export <uuid> <file>`,
`import <uuid> <file>`. **`removeAllMaps` (6) is deliberately not implemented.**

Every wire format was read out of the real `ITrackingEnvironment$Stub$Proxy`
bytecode rather than guessed -- `dexdump -d` on `classes.dex` from
`/system/framework/com.oculus.os.platform.jar`. For the record:

```
readMap  (3): token, [int 1 + PFD | int 0], String uuid            -> readLong
writeMap (2): token, [int 1 + PFD | int 0], String uuid, long n    -> readInt
keepMap  (5): token, String uuid                                   -> readInt
loadMap  (7): token, String uuid, double timeout                   -> readInt
getUuid  (4): token                                                -> readString
listMaps (8): token                                       -> createStringArray
```

The PFD prologue is the standard AIDL nullable-parcelable pattern (`writeInt(1)`
then `writeToParcel`, else `writeInt(0)`), confirmed in both `readMap` and
`writeMap`.

## Three findings, in the order they bit

**1. Root does not bypass the service's own permission checks.**
`getDebugInfo` fails with `SecurityException: Permission denied in
TrackingEnvironment::getDebugInfo for caller 0` -- caller 0 being uid 0.
`getCurrentMapUUID` and `listMaps` are ungated; some methods are not.

**2. The fd must live somewhere `trackingserver` can reach.**
Exporting to `/data/local/tmp` fails with a misleading
`DeadObjectException: ... remote process probably died`. The service had not
died -- it was SELinux:

```
avc: denied { read } for path="/data/local/tmp/map_puck1.bin"
scontext=u:r:trackingserver:s0 tcontext=u:object_r:shell_data_file:s0
```

`file_contexts` maps `/data/misc/tracking(/.*)?` to `u:object_r:tracking_file:s0`,
which is the label the service can use. **Put any fd you hand this service under
`/data/misc/tracking/`.** Doing so removes the denial and the call reaches the
service properly. (Note this is with SELinux *enforcing*; no need to go
permissive.)

**3. The map store is empty and cannot be filled locally.**
With the fd path working, `readMap` returns a genuine application-level error:

```
ServiceSpecificException: Failed to export map (code 0)
```

because there is nothing to export -- `listMaps()` is `count=0`, and
`keepMap(currentUuid)` returns `false`. Setting
`persist.trackingservice.enable_map_db=1` and restarting `trackingservice`
changed **none** of that (still `count=0`, still `ok=false`, still the same
export error), which finally settles that property as a dead end on its own.
The real gates are the MobileConfig flags in `libtrackingengines.so` --
`oculus_enable_map_streaming` and `oculus_enable_offline_map_merge` -- which
are server-side. The property was reverted and 6DoF confirmed healthy.

Incidentally the map uuid `8994724e-236f-76ff-3e0c-768e3a0a5628` survived a
full `trackingservice` restart unchanged, further reinforcing that the root
node is persistent across sessions.

## Where that leaves map sharing

The transport is reachable; the payload is not available. Two paths remain:

1. **The anchor route** -- `exportMapDataForAnchor` (17) /
   `importMapDataForAnchor` (18) take a `ParcelAnchorUuid` rather than a map
   uuid, so they may not depend on the map *store* at all. They need custom
   parcelables (`ParcelAnchorUuid`, `ParcelAnchorHandle`,
   `ParcelAnchorPlacementData`) marshalled by hand, readable the same way from
   the proxy bytecode. **This is the most promising untried lead.**
2. Find whether the MobileConfig gate can be satisfied locally at all.

---

# The anchor route, and why the whole thing is gated

Following the anchor lead end to end. It does not work on this build, but the
reason is now fully understood rather than guessed, and it explains every
earlier failure at once.

## The Java parcelables are stubs -- and it does not matter

`ParcelAnchorUuid`, `ParcelAnchorHandle` and `ParcelAnchorPlacementData` have
**no instance fields**, and both `writeToParcel` and the private
`<init>(Parcel)` throw `UnsupportedOperationException("not implemented")` --
in `com.oculus.os.platform.jar` *and* in `oculus-system-services.jar`. Both
jars only contain client code (`MapManager.connect()` does
`ServiceManager.checkService("TrackingEnvironment")`).

That turns out to be irrelevant, because **the server is native**.
`/system/bin/trackingservice` contains the format string
`"Permission denied in %s for caller %d"` -- the exact error we got back -- and
the earlier avc denial named `scontext=u:r:trackingserver:s0`. So the parcel is
parsed by C++ and we can marshal it by hand.

## placeAnchor works, and reveals the layout

`placeAnchor` takes no arguments, so plain `service call` reaches it and dumps
the raw reply:

```
service call TrackingEnvironment 10
  0x00: 00000000 00000001 00000003 00000000
  0x10: ffffffff ffffffff <7 floats>
```

Decoded, `ParcelAnchorPlacementData` is:

```
long  status      = 3
long  handle      = -1
float position[3]
float quaternion[4]      <- norm exactly 1.000000
```

The pose is live: it is bit-identical across repeated calls when the headset is
still, and changes when it moves. But `status=3` / `handle=-1` never change,
even at `Status=6DoF,TRACKING`. That is the gate, not a tracking problem.

## The gate

`strings /system/bin/trackingservice` lists the whole family:

```
Capability not available for placeAnchor / registerAnchor / persistAnchor /
locateAnchor / getAnchorUuid / isLocalAnchor / deregisterAnchor /
setAnchorUpdateCallback
Monterey capability not available or keepMap function not provided
Failed to lock visionInterface/SpatialPersistenceCapability
Failed to lock visionInterface/montereyCapability
com.oculus.permission.USE_ANCHOR_API
```

`Monterey capability not available or keepMap function not provided` is
exactly why `keepMap()` returns `false`. `libtrackingengines.so` contains the
implementations -- `SpatialPersistenceCapabilityImpl`,
`InternalAnchorApiServerCapabilityImpl` -- so the code is present but never
created.

Notably **`exportMapDataForAnchor` and `importMapDataForAnchor` are NOT in the
"Capability not available" list**, and their errors are file errors:

```
Map file (%s) not found in exportMapDataForAnchor
Failed to open given map file (%s) in exportMapDataForAnchor
Unable to complete sendfile operation in exportMapDataForAnchor: %s
Failed to open given map file (%s) in readMap
```

So export/import are a **file copy** (`sendfile`) of a `.map` file keyed by
uuid. If a `.map` file existed, that transport is likely usable. Nothing
creates one, because of the gate above.

## Where the gate actually lives

Not in `trackingservice`: the only gatekeepers it queries are `arvr_gk_nimble_*`
(hand tracking), none for maps or anchors. The gate is a separate component.

`/data/data/com.oculus.horizon/shared_prefs/gatekeeper_preferences.xml` holds
the cached gatekeepers:

| gatekeeper | value |
|---|---|
| `oculus_guardian_internal_anchor` | **true** |
| `oculus_mobile_guardian_spatial_anchor` | **true** |
| `oculus_enable_vega` / `oculus_enable_vega_mapper` | **true** |
| `oculus_spatial_anchor_iaapi_v2` | **false** |
| `oculus_anchorplatform_shared_spatial_anchors` | **false** |
| `oculus_enable_offline_map_merge` | **false** |
| `oculus_gk_is_employee` | **false** |
| `oculus_gk_is_spatial_ai` | **false** |

`oculus_spatial_anchor_iaapi_v2` is read by
`/system/priv-app/SpatialPersistenceService/SpatialPersistenceService.apk`
(package `com.facebook.spatial_persistence_service`). That package is
**installed and enabled but has never run** -- no process, no registered binder
service.

### The chain, end to end

```
oculus_spatial_anchor_iaapi_v2 = false   (server-provided gatekeeper)
      -> SpatialPersistenceService never starts
      -> nothing locks visionInterface/SpatialPersistenceCapability
      -> InternalAnchorApiServerCapability is never created in trackingservice
      -> "Capability not available for placeAnchor"  (status=3, handle=-1)
      -> no anchors -> no .map file is ever written
      -> readMap / exportMapDataForAnchor have nothing to send
      -> listMaps() == 0, keepMap() == false
```

Every symptom in this document falls out of that one flag. It also explains why
`persist.trackingservice.enable_map_db=1` changed nothing: it was never the
gate.

## Untried, in order of promise

1. **Flip the cached gatekeepers** (`oculus_spatial_anchor_iaapi_v2`,
   `oculus_anchorplatform_shared_spatial_anchors`,
   `oculus_enable_offline_map_merge`, possibly `oculus_gk_is_employee`) in
   Horizon's `shared_prefs` and see whether `SpatialPersistenceService` starts.
   Reversible with a backup, but it edits system-app state and Horizon may
   re-fetch and revert on the next server sync.
2. Start `com.facebook.spatial_persistence_service` directly and see whether it
   self-gates or comes up.
3. If a `.map` file can be produced by any route, test the `sendfile` transport
   between pucks directly.

## Side note: a tracking scare that was not one

Puck 1 dropped to `Status=3DoF,INITIALIZING,Inl=-1` and stayed there through a
reboot, which looked like something we had broken. It was the room going dark.
The exposure telemetry says it plainly:

```
6DoF:  Exp=(14.0ms,g=15.0,I=58,Ir=277.0) ... I=75 Ir=482
3DoF:  Exp=(14.0ms,g=15.0,I=5, Ir=24.6)  ... I=5  Ir=28
```

Mean intensity `I` collapsed from ~60-75 to 5 on all four cameras. In daylight
it returned to `Status=6DoF,TRACKING` on its own with `I=67-81, Ir=4952-8653`
-- **and the same map uuid `8994724e-236f-76ff-3e0c-768e3a0a5628` came back**,
after a reboot and hours of darkness. That is the strongest confirmation yet
that the root node is persistent and relocalization is real. When judging
tracking health, read `I`/`Ir` before suspecting anything else.

## SpatialPersistenceService is the cloud service -- do not pursue it

Checked before enabling anything, because the whole point of these pucks is a
local tracking universe. `SpatialPersistenceService.apk` (26 MB,
`com.facebook.spatial_persistence_service`) is Meta's cloud-backed spatial
anchor service -- "Spaces". Evidence, not inference:

- **Permissions**: `INTERNET`, `ACCESS_NETWORK_STATE`, `ACCESS_WIFI_STATE`
  alongside the colocation set (`COLOCATION_API_READ_MAP`,
  `COLOCATION_API_WRITE_MAP`, `IMPORT_EXPORT_IOT_MAP_DATA`, `USE_ANCHOR_API`).
  It is the app that actually holds those colocation permissions.
- **Native stack**: `libovrplatformloader.so` (Oculus Platform SDK -- account
  and entitlement services), `libgraphbase/libgraphstore/libgraphutil.so`
  (Meta's graph client), `libxplat_proxygen_parse_url.so` (HTTP), `libsodium.so`,
  and `libarvr_libraries_spatial_persistence_thrift_*`.
- **Vocabulary in `libplugin.so`**: `CLOUD`, `DOWNLOADER`, `DownloadEvent`,
  `saveSpaces`, `eraseSpaces`, `QuerySpaces`, `AnchorFrameworkDiscovery`.
- Its services are `AnchorPersistenceService`,
  `AnchorPersistenceDurableService`, `AnchorPersistenceServiceSweeper`.
- A matching gatekeeper exists: `oculus_trex_sps_cloudanchorclient_killsw`
  (SPS = SpatialPersistenceService).

The dex itself carries no endpoints -- they live in the native graph/platform
libs -- so "no URLs in classes.dex" is not evidence of a local-only design.

**Conclusion.** Unlocking Meta's map persistence on this build means turning on
a cloud-backed component that wants a Meta account, network access, and
server-side gatekeepers we do not control (the local `shared_prefs` cache can be
flipped, but the server re-asserts it). Even if it worked it would push spatial
data about the user's home to Meta. That is the opposite of the goal.

**Not pursued.** No gatekeepers were flipped and
`com.facebook.spatial_persistence_service` was left as found: installed,
enabled, never run.

### What this means for the pucks

Meta's own map sharing is off the table on this hardware, so the shared
tracking universe has to be **ours**, which was the original plan anyway. What
is already in hand for that:

- all four tracking cameras, 60 fps, via `libq1cam`
- the 1 kHz IMU and syncboss timing
- factory + online camera calibration (fisheye62 intrinsics, extrinsics)
- two pucks on wireless adb with fleet tooling and a GUI

Insight stays useful as a **reference and a fallback**: it still relocalizes
into its own persistent map across reboots (verified again -- same uuid after a
reboot and hours of darkness), so per-puck 6DoF is available for free even while
our own cross-puck alignment is being built. See
[multi-device-alignment.md](multi-device-alignment.md).

---

# Can we call these ourselves? The permission split -- and its correction

Short answer: **no, not for moving real map data.** This section originally
concluded otherwise, on string-proximity evidence alone (`USE_ANCHOR_API` sits
next to `exportMapDataForAnchor` in the binary's string table). Building and
testing a real app against `trackingservice` disproved that. The corrected,
evidence-based picture is below; the wrong reasoning is kept rather than
deleted, because the mistake -- trusting string-table adjacency as a proxy for
call-site reference -- is worth not repeating.

## What is real

The native `trackingservice` does check permissions with
`android::checkCallingPermission(...)` (`Permission denied in %s for
caller %d` is that check's format string), and `dumpsys package permissions`
does show the protection levels below:

| permission | protection | granted to a normal installed app? |
|---|---|---|
| `USE_ANCHOR_API` | **normal** | **yes**, automatically |
| `ACCESS_TRACKING_ENV` | signature\|preinstalled | no |
| `COLOCATION_API_GET_MAP_UUID` | signature\|preinstalled | no |
| `COLOCATION_API_READ_MAP` | signature\|preinstalled | no |
| `COLOCATION_API_WRITE_MAP` | signature\|preinstalled | no |
| `IMPORT_EXPORT_IOT_MAP_DATA` | signature\|preinstalled | no |

`tools/q1anchorapp` is a minimal APK (self-signed, no special key) that
requests all six. Installed and verified: `dumpsys package com.q1.anchor`
shows only `USE_ANCHOR_API: granted=true` -- the five signature permissions
are silently withheld, exactly as the protection level predicts. This part of
the earlier reasoning was right.

**Also confirmed: this cannot be worked around from root.** `pm grant
com.q1.anchor com.oculus.permission.IMPORT_EXPORT_IOT_MAP_DATA` (and the other
four), run as root, is refused by the framework itself:

```
SecurityException: Permission com.oculus.permission.IMPORT_EXPORT_IOT_MAP_DATA
requested by com.q1.anchor is not a changeable permission type
  at PermissionManagerService.grantRuntimePermission
```

`PackageManagerService` enforces this in `system_server`, not something a
rooted shell can bypass. Root gets root's *own* answer from the service (see
below) but cannot lend that access to another uid.

## What was wrong: `USE_ANCHOR_API` does not gate export/import

Calling `exportMapDataForAnchor` from `com.q1.anchor` -- holding
`USE_ANCHOR_API`, denied the other five -- fails. Across a dozen trials
(both fd-transport types, three uuid wire encodings, repeated runs) it never
once returned map bytes. Two distinct failure shapes appeared, and neither is
`USE_ANCHOR_API` being missing:

```
SecurityException: Permission denied in TrackingEnvironment::exportMapDataForAnchor for caller 10074
DeadObjectException: Transaction failed on small parcel; remote process probably died
```

The `DeadObjectException` is misleading in the same way it was for `readMap`
earlier in this document: `trackingservice`'s pid was checked before and after
several such calls and never changed (`8860` throughout) -- the "remote
process" did not die. This is very likely another case of a native-side
SELinux/fd failure surfacing as a garbled binder error rather than a clean
one; a controlled test with a real file did independently show
`trackingserver` denied `{ read write }` on our app's `app_data_file`-labeled
fd. What could not be pinned down in the time available is *why the two
failure shapes did not appear consistently for the identical call* -- repeats
of the exact same `export`/`raw` request sometimes produced one, sometimes the
other. That inconsistency is recorded rather than papered over.

None of that ambiguity affects the answer to the actual question, though,
because of a clean control test:

## The control test that settles it

If `USE_ANCHOR_API` genuinely gated `exportMapDataForAnchor`, then a call that
*is* gated only by that permission should succeed for our app. `placeAnchor`
is exactly that call, and it does not distinguish the two callers at all:

```
root  (uid 0,    holds no Android permissions):  status=3  handle=-1
app   (uid 10074, holds USE_ANCHOR_API):         status=3  handle=-1
```

Bit-for-bit the same failure code, live pose included, regardless of which
caller made the call. `USE_ANCHOR_API` being granted changed **nothing**
observable about `placeAnchor`'s outcome. The real blocker for the anchor
lifecycle calls is the capability gate from the previous section --
`SpatialPersistenceCapability` is never locked because
`SpatialPersistenceService` (the cloud component) never runs -- and that gate
is global service state, not tied to who is calling.

A second control makes the export/import picture unambiguous too:
`getCurrentMapUUID` **succeeds for root** (no permission held at all) but
**fails for our app** (`USE_ANCHOR_API` held) with the identical
`Permission denied ... for caller 10074` shape as export/import gave it.
Root is evidently special-cased past *some* of these checks (`getCurrentMapUUID`,
`listMaps`, `keepMap`) but not others (`getDebugInfo`, `exportMapDataForAnchor`,
`importMapDataForAnchor` all deny root too, as `caller 0`) -- inconsistent in a
way that was not fully traced, but consistent enough to prove the point: an
app holding only `USE_ANCHOR_API` is in the *same* denied set as root for
every one of the calls that actually move map data.

## The verdict

| call | works for root? | works for a `USE_ANCHOR_API` app? |
|---|---|---|
| `getCurrentMapUUID`, `listMaps` | yes | **no** -- signature-gated |
| `keepMap` | returns `false` (capability) | untested, expect same |
| `placeAnchor` and the rest of the anchor lifecycle | returns failure code (capability) | **same failure code** -- capability gate is caller-independent |
| `exportMapDataForAnchor`, `importMapDataForAnchor` | denied | **denied** -- some other, unheld permission |
| `readMap`, `writeMap` | denied | would be denied (signature) |

No path through this API, on this device, with this key material, produces
actual map or anchor bytes -- not as root, not as an installed app holding
every permission Android will hand out for free. The two remaining doors are
the ones already on the table and already declined or deferred: Meta's
signing key (unavailable), or enabling `SpatialPersistenceService`, which is
the cloud component -- ruled out per the previous section.

**This closes the Insight colocation API as a source of shared map data for
the pucks.** The shared tracking universe has to be built from the raw camera
+ IMU pipeline this repo already has, not borrowed from Insight's own map
store.

## tools/q1anchorapp

The test app used above. Two entry points, both calling the same marshalling
code as `tools/q1mapjava`:

```bash
./tools/q1anchorapp/build.sh                 # -> tools/q1anchorapp/q1anchor.apk
adb install -r -g tools/q1anchorapp/q1anchor.apk
adb shell am broadcast -n com.q1.anchor/.AnchorReceiver -e op uuid
adb shell am broadcast -n com.q1.anchor/.AnchorReceiver -e op place
adb shell am broadcast -n com.q1.anchor/.AnchorReceiver \
    -e op export -e anchor <uuid> -e layout raw
adb logcat -s Q1ANCHOR
```

`AnchorReceiver` (a `BroadcastReceiver`, runs synchronously in-process on
`onReceive`) is the one to use. `AnchorService` was tried first and has a real
pitfall worth keeping as a comment in the source rather than rediscovering:
`startForegroundService` without a prompt `startForeground()` call gets the
service killed by the platform within a couple of seconds on this OS version,
which silently truncates whatever it was doing -- a couple of early "no
output" results were this, not a real failure.
