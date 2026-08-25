# Insight's persisted map format (`/vision/insideout/mapdb`)

**This supersedes the memory-forensics track.** Everything the RAM scraping was
reaching for — real map points, their descriptors, and the link between the two
— is written to disk in a fully decodable form, with no root, no `setenforce 0`
and no `process_vm_readv` involved. `docs/insight-map-access.md` remains the
record of how the map was found and what the binder/disk walls actually are;
this file is how to *read* it.

## Wire format

Facebook Thrift **compact** protocol. The one deviation from Apache compact —
and the reason a stock Thrift reader chokes — is wire type **13 = FLOAT**, four
bytes **big-endian**. Poses and coordinates are float32, not double. Doubles,
where they appear, are little-endian as usual.

Each file is a bare struct *body* (a field sequence ending in a stop byte), so
it is read exactly like a struct. Decoder: `tools/insightmap/mapdata.py`, which
needs no `.thrift` schema — a struct decodes to `{field_id: value}`.

The hand-decode that cracked it, `nd_<node>_3.mapdata`, all 50 bytes:

```
19 f3 10 <16 bytes>   field 1: list<i8> len 16   = node UUID
19 7d    <28 bytes>   field 1: list<float> len 7 = quat(xyzw) + t(xyz)
00                    stop
```

Type 13 and big-endian were pinned together by one check: only that combination
makes those four floats a **unit** quaternion (to 1e-6).

## Files

```
nd_<node-uuid>_<K>.mapdata    one L1 node, record kind K
ad_<anchor-uuid>.mapdata      an anchor
```

| K | contents |
|---|----------|
| 1 | graph: node uuid, parent, neighbour poses, per-node gravity, keyrigs (pose + covariance + IMU bias), IMU states |
| **2** | **map points** — positions, descriptors, observations, image patches, and the camera rig calibration |
| **3** | `T_root <- node`: `{1: root uuid, 2: pose}` |
| 4, 12 | empty lists on a live map |
| 6, 11 | root-node bookkeeping (uuid, two timestamps, two counts) |
| 9 | sibling node poses |

Kind 2, the ~1 MB record, is the prize:

```
1  list<uuid>            neighbour nodes
2  list<rig>             camera calibration: {model id, 11 intrinsics, extrinsic pose, 640, 480}
3  list<MapPoint>        THE POINTS
4  list<observations>    per point: {node uuid, cam idx, affine, pixel uv, ...}
5  list<patches>         per point: 400-byte (20x20) image patches
```

and each `MapPoint`:

```
1   float[3]      position, see below
3   list<{1: bytes[32]}>   the point's DESCRIPTORS — one per observation
4   float         score
7   float[6]      information matrix (upper triangle)
12  list<i8>      one byte per descriptor
14  descriptor spec: 256-bit
15  list<i8>      descriptor type per descriptor (5)
```

**The pairing is free.** Descriptors live inside the point struct, so
`paired()` is a nested loop, not a graph walk.

## Point coordinates: (azimuth, elevation, inverse depth)

Field 1 is **not xyz**. The giveaway: column 0 saturates at exactly ±π on every
node while columns 1 and 2 never do. It is a bearing plus inverse depth, in a
Y-up frame with azimuth measured from +Z toward +X:

```
r = 1 / inv_depth
x = r·cos(el)·sin(az)     y = r·sin(el)     z = r·cos(el)·cos(az)
```

Three conventions had to be fixed at once — the radial meaning, the quaternion
order, and the direction the node pose applies. One test settles all of them
with no guessing: transform every node into the root frame and measure
cross-node nearest-neighbour overlap. The truth has a sharp optimum.

| convention | median cross-node NN | within 0.15 m |
|---|---|---|
| **inverse depth, quat xyzw, `p = R·p + t`** | **0.28 m** | **26 %** |
| next best of 47 alternatives | 1.17 m | 2.5 % |

So: **inverse depth**, quaternion **(x, y, z, w)**, node pose applies forward.

Points whose inverse depth approaches zero never triangulated; they land
hundreds of metres out and carry no translation information. `max_range`
(default 8 m) drops them — 7 of 1427 on a real map.

## Validation

Decoded `.108`'s mapdb against the live `dumpsys tracking` on the same PID:

- persisted root uuid `8994724e` == live `topNodeUid`
- persisted node set == live `L1 node` list, minus the newest node not yet written
- 1427 persisted points vs 1504 live; 49801 descriptors vs 47071 live
- own-nearest-neighbour spacing 0.075 m (but see the calibration below — the
  agreement with the memory track's figure turns out to be coincidence)
- same-point descriptor Hamming 92.9 vs 126.3 for random pairs

All 43 files decode with zero failures.

**External ground truth**: rendered in 3D (`visualize3d.py`), the cloud is
recognisable to the operator as the actual room the pucks are tested in. Every
other check above is internal consistency — this is the one that says the
(azimuth, elevation, inverse-depth) conversion and the node pose composition are
right in absolute terms, not merely self-consistent.

## Calibrating the memory extractor against truth (2026-08-24)

The persisted map finally gives the memory track a **ground truth**. The result
is mixed, and the order the tests were run in matters, because the cheap tests
mislead in both directions.

**Summary statistics say "not the map". Geometric alignment says "related to
the map, but not clean." Both are true, and the second is what counts.**

Dumping `.108`'s arenas while its mapdb was on disk, on the same PID:

| | persisted (truth) | `MapDump.map_points()` | `point_blocks()` best |
|---|---|---|---|
| clouds found | 6 nodes | 2 runs | 4 room-like blocks |
| sizes | 78/182/205/273/333/349 | 52, 144 | 426, 148, 70, 49 |
| vertical std | 0.71–1.19 m | 0.02–0.03 m | 0.43–1.14 m |
| own-NN spacing | **0.077–0.233 m** | — | **0.023, 0.025, 0.281, 0.000** |

Nothing matches on size, and nothing matches on spacing. `map_points()` returns
flat sheets 0.03 m thick — a floor or boundary structure, not map points — and
its `max_ystd=0.6` gate is below the 0.71–1.19 m that real nodes actually have,
so it *could not* return them anyway. Raising the gate recovers nothing extra,
which says the points are not present as contiguous f64 Cartesian triples at
all. Searching the arenas for the exact float32 values from the persisted file
(any endianness, f4 and f8) finds none of them either.

But statistics alone are the wrong test — a memory block is one node's partial
view, so its size and spacing need not match a whole persisted node even when
the geometry is genuine. The real test is whether a block **aligns** to the
persisted cloud. Sweeping yaw + centroid translation, each block against
**size- and extent-matched random controls** (8 per block):

| block | best median NN | within 0.3 m | matched control (best of 8) |
|---|---|---|---|
| 426 pts | **0.278 m** | 53.8 % | 0.384 m |
| 148 pts | **0.296 m** | 51.4 % | 0.376 m |
| 238 pts | **0.438 m** | 23.9 % | 5.157 m |
| 153 pts | **0.622 m** | 3.9 % | 0.722 m |

Every block beats its own control, so the heap blocks are **not random — they
carry real spatial structure of this room**. That is consistent with the
earlier session's hand-verified 192-point block at `a1+0x13b8e00`.

What they are not is a clean copy of the map: the best residual is 0.278 m,
while the persisted map's own point spacing is 0.075–0.233 m. A faithful
extraction would align an order of magnitude tighter.

Two cautions for anyone repeating this:

- **Use matched controls.** An unmatched control (300 random points over the
  full bounding box) makes small blocks look spectacular — 36- and 70-point
  blocks scored 0.059 m at 100 % within 0.3 m, which is just what any small
  compact cluster does when dropped into a dense 1420-point room.
- The "0.075 m own-NN spacing, identical to the memory track" line above is
  **coincidence** — the memory blocks measure 0.023–0.28 m.

So the ~0.198 m geometry-only alignment plateau is best read as the accuracy
ceiling of an approximate extraction, not as the map's sparsity floor.

The in-memory layout is a `VersionedSoABase` structure-of-arrays, so the fields
live in separate arrays rather than interleaved xyz triples, and the stored
parameterization is bearings + inverse depth rather than metres. Any future
memory work must be calibrated against a persisted map on the same device —
which is now possible, and was not before.

**Practical consequence: read the files, not the heap.** A puck with no mapdb
has no usable map for us today.

## When does the map actually persist?

Both pucks have `persist.trackingservice.enable_map_db=1`, identical props and
identical package sets, yet only `.108` has written anything. The difference is
map maturity, not configuration:

| | `.108` | `.132` |
|---|---|---|
| L1 nodes | 7 | 1 |
| map points | 1504 | 285 |
| anchors in mapdb | 5 | none |
| `anchor_world_origin` | N | **Y** |
| mapdb | 5.7 MB | empty |

### ANSWERED: only a PERSISTENT map context is written

`dumpsys tracking` labels each map context outright, and that label is the gate:

```
.132   9a281d2f   timeCount = 126   (transient)    1 anchor    -> never writes
.108   8994724e   timeCount = 3     (persistent)   5 anchors   -> writes
.108   6d91689e   timeCount = 2     (transient)    1 anchor    -> not written
```

`.108` carries a transient context of its own that is *not* written, so this is
per-CONTEXT, not per-device. And what makes a context persistent is visible one
line down — the per-anchor flags:

```
.108 persistent ctx:  anchor 1a97f72c ... registered Y, persistent Y   <- the only one
.132 transient  ctx:  anchor 13a588a8 ... registered Y, persistent N
```

**A context becomes persistent when at least one of its anchors has been
persisted.** `.108` has exactly one `persistent Y` anchor; `.132` has none.
That is consistent with the AIDL, where `persistAnchor` (code 13) sits beside
`writeMap` (code 2) on `ITrackingEnvironment`.

So the route to making any puck persist its map is to get **one anchor
persisted** on it — not to walk it further, and not to touch guardian. See
`docs/insight-map-and-anchors.md` for the anchor API and the VR-focus wall that
blocked `placeAnchor`; note that both pucks *already have* a registered anchor,
so `persistAnchor` on the existing one may not need `placeAnchor` at all.

### Tested and FALSIFIED: map maturity is not the trigger

The hypothesis was that a map persists once it grows past its first provisional
node. `.132` was walked for ~6 minutes on 2026-08-24 and went from 1 node / 285
points to **4 nodes / 1093 points / 34918 descriptors** — comparable density to
`.108` — with its mapdb still **completely empty**. Growth then plateaued
(16.5k descriptors added in the first 2.5 min, 1.5k in the next 1.5), so the
map had saturated, not stalled early. Maturity is not the gate.

Meanwhile `.108` **rewrote its mapdb during that same window** — 10:44, then
again at 16:18, growing 5.9 MB → 7.0 MB — while sitting still. So persistence
there is an ongoing, repeating process, not a one-off promotion. The question is
not "what promotes a map once" but "what makes `.108` eligible and `.132` not".

The surviving difference is anchors:

| | `.108` (writes) | `.132` (never writes) |
|---|---|---|
| anchors in mapdb | 5 | none |
| guardian record | 10.5 KB | 148 bytes |
| `anchor_world_origin` | **N** | **Y** |

`.132`'s anchor *is* its world origin — the marker of a map that has only ever
been freshly seeded. `.108`'s is not. That fits `persistAnchor` being wired to
`writeMap` in the AIDL, and it fits the anchor route being the thing gated
behind VR focus. Next suspect, still untested.

## Usage

```python
import insightmap as im, mapdata as md

d = im.Device("192.168.1.10")
if d.mapdb_files():
    m = md.Map(d.pull_mapdb("/tmp/mapdb108"))
    pts, desc = m.paired()        # (N,3) metres in root frame, (N,32) uint8
```

```sh
./mapdata.py /tmp/mapdb108                          # summarize every record
./mapdata.py /tmp/mapdb108 --kind 2 --tree          # walk the point record
./visualize.py --dump /tmp/mapdb108 --base 0 --out map.html
```

`visualize.py` prefers a mapdb over arena dumps automatically.

## Colocation achieved by map transplant (2026-08-24)

The persistence gate turned out to have a door in it. `.132` could never be
promoted on its own — `placeAnchor` returns handle `-1` behind the VR-focus
wall, so it can never mint the persisted anchor a persistent context requires.
But a context is promoted by *loading* a map that already contains one, which is
exactly what `.108` does on every restart.

So: copy `.108`'s mapdb onto `.132` and restart its trackingservice.

    adb -s .132 push <backup>/. /data/local/tmp/mapdb_in/
    adb -s .132 shell 'cp /data/local/tmp/mapdb_in/*.mapdata /vision/insideout/mapdb/;
        chown system:system ...; chmod 600 ...;
        chcon u:object_r:vision_file:s0 /vision/insideout/mapdb/*.mapdata'
    adb -s .132 shell 'stop trackingservice; start trackingservice'

**`chcon` is required.** Files arriving via adb push carry the wrong SELinux
label; `trackingservice` reads the directory as `vision_file`, so without
relabelling the map is silently unreadable and the result looks like a failure.

Result on `.132`:

```
Vega Map Context: topNodeUid 8994724e-...  timeCount = 3 (persistent)
  L1 nodes 4c0411aa 8ad07f6c b9ed0756 623ae2cf ad4eeb3d fbce8853   <- .108's
  + de67651c                                                       <- its own new node
"loc_success": true, "localizedRootNodeId": "8994724e-..."
"num_total_matches": 279, "inliers": 21, "new_level": "6dof"
```

**Both pucks now localize against the same map root.** That is one shared
tracking universe obtained through Insight itself, not through host-side
alignment — no `T_worldA_worldB` to solve, maintain, or re-solve on drift, and
no per-frame maintenance layer.

Note the transplanted anchors show `persistent N` on `.132` while the context
still reads persistent, so the promotion travels with the map/context rather
than requiring the anchor flag to survive the copy.

### What this changes

The whole host-side alignment track (`matchmap.py`, the 4-DoF solver, the
overlap gate) becomes a *fallback* rather than the primary path: useful when two
pucks cannot be given a common map, and still the only way to check the result
independently. The primary path is now: map once, transplant, relocalize.

### Physical verification (2026-08-24)

Both pucks held together, reading each one's Insight WORLD pose from `dumpsys`
and comparing with **identity** — no `T_map_world`, no bridge, no MPT1, no host
alignment of any kind in the path. Eight consecutive samples:

```
horizontal:  median 0.033 m   (min 0.020, max 0.100)
3D:          median 0.145 m
vertical dy: -0.130 .. -0.150 m   constant to +-0.01 m across all 8
```

**3.3 cm horizontal median.** The previous best was 9.6 cm *with* the full
q1track alignment stack running, so native colocation is ~3x better while
deleting the machinery that produced the old number.

The vertical offset is not error: it is rigid to +-1 cm across every sample,
which is the signature of a fixed physical offset between the two headsets'
Insight origins as stacked, not drift. A constant offset is removable from known
mount geometry; drift would not be.

This is the only measurement in the system with real ground truth — two
co-located pucks cannot lie to each other — and it now passes with the alignment
layer removed entirely.

## Creating a map — what is and is not possible

### The anchor flag triple is decoded

Record `ad_<uuid>.mapdata` field 2 is exactly the three flags `dumpsys` prints,
confirmed 5/5 on a live map:

```
{1: registered, 2: PERSISTENT, 3: worldOrigin}

1a97f72c  {1:True, 2:True,  3:True }  <-> registered Y, persistent Y, worldOrigin Y
others    {1:True, 2:False, 3:False}  <-> registered Y, persistent N, worldOrigin N
```

Key **2** is the bit that decides whether the whole context is written to disk.
Since the format is writable as well as readable, any map we can get onto disk
can be promoted by authoring that boolean.

### The bootstrap is still circular

    persistent context  <- loaded from mapdb  <- written by a persistent context

A transient context is never written, so there is no file to promote. The only
documented way to break the cycle is `persistAnchor`, which is behind the
VR-focus wall (`placeAnchor` returns handle `-1`). Every current map descends
from one lineage `.108` acquired on 2026-08-20, presumably during a real VR
guardian setup.

### What DOES work

**Extending is solved.** A puck that relocalizes into the persistent map adds
its new nodes to *that* context and writes them — proven: `.132` contributed
node `de67651c` into `8994724e` and persisted it. So the map grows by walking
from mapped territory into unmapped territory, and new rooms connected by a
walkable path can be added indefinitely.

**Seeding a new puck is solved** — transplant, restart, relocalize.

### What does not

A **disconnected** new space (different building, no walkable overlap) cannot be
bootstrapped: the puck would fail to relocalize, open a transient context, and
never write it.

### Mitigations and open routes

1. **Back up the lineage.** It is ~5 MB and currently irreplaceable. Copies live
   in `~/q2slam-backups/`.
2. **Author a mapdb wholesale.** The format is fully decoded, so a synthetic map
   is writable in principle; the hard part is real points and descriptors for a
   room, and memory extraction is too coarse (0.278 m) to supply them.
3. **Beat gate 2 once.** A single successful `persistAnchor` mints a lineage for
   any space. `tools/q1anchorapp` was built for this and clears gate 1 only.
4. **Use a display-equipped headset.** If any Quest can still run the real
   guardian setup flow, it mints a persistent anchor the supported way, and the
   result transplants like any other map.

## Why no puck could create a map: the guardian package is DISABLED

`com.oculus.guardian` is **disabled** on both pucks (`enabled=2`, and it appears
in `pm list packages -d`). Presumably a fleet setup step, to stop guardian
interfering with off-head tracking. The consequence was invisible and total:

- the guardian service never runs,
- so the boundary flow does nothing (selecting a type appears to hang),
- so `persistAnchor` is never called,
- so no context is ever promoted to persistent,
- so no mapdb is ever written.

`.108`'s original lineage dates from 2026-08-20 — before guardian was disabled.

**`libguardian.so` confirms guardian is the right component.** It calls
`persistAnchor` directly and owns the persistence layer:

```
persistAnchor          persistentAnchors_: {}
{}: Attempting to persist an untracked anchor: {}
GuardianMapDataMgr::SetActiveGuardianValid anchorUuid = {}
persist.oculus.guardian_spatial_anchor_max_count
```

Note `com.oculus.vrshell` and `com.oculus.systemux` do **not** hold
`USE_ANCHOR_API`; `com.oculus.guardian` does (`granted=true`). The shell only
draws the boundary — guardian does the anchor work.

### A headless command interface exists

`libguardian.so` carries a JSON command channel, gated by a property:

```
persist.oculus.guardian_json_cmds_user_build      <- the gate
Java_com_oculus_vrguardianservice_VrGuardianService_nativeJsonCmd
ProcessJsonCmd: force_stationary
ProcessJsonCmd: force_stationary done. Location: %f, %f, %f, Height: %f
ProcessJsonCmd: force_roomscale
```

delivered by broadcast `com.oculus.vrguardianservice.JsonCmdUserBroadcast`.
If `force_stationary` works, a boundary — and therefore a persistent map — can
be created with no VR UI at all, which matters because the dashboard is
unusable on these pucks (SystemUX dies on the blocked Meta domains, which must
stay blocked).

The extra key for the broadcast is not yet identified; `command`/`json`/`data`/
`cmd` produced no `ProcessJsonCmd` log line.

### Recipe to retry

```sh
adb root
pm enable com.oculus.guardian
setprop persist.oculus.guardian_json_cmds_user_build 1
am start-foreground-service com.oculus.guardian/com.oculus.vrguardianservice.VrGuardianService
# trackingservice must be (re)started AFTER guardian, or guardian logs
#   SlamAnchorRuntimeIpcClient: InitClientInternal failed!
# and never attaches to the SlamAnchorServer.
```

The headset must be **worn and tracking at 6DOF** — guardian logs
`No Ovr SessionState cannot force Setup` otherwise.

### The headless guardian command channel WORKS

Confirmed end to end on `.108`. No VR UI, no passthrough, no dashboard — which
matters because SystemUX is permanently broken on these pucks (it dies on the
deliberately blocked Meta domains, and those stay blocked).

```sh
adb root
pm enable com.oculus.guardian                     # it ships DISABLED on the pucks
setprop persist.oculus.guardian_json_cmds_user_build 1
am start-foreground-service com.oculus.guardian/com.oculus.vrguardianservice.VrGuardianService
adb shell 'stop trackingservice; start trackingservice'   # AFTER guardian, see below

am broadcast -a com.oculus.vrguardianservice.JsonCmdUserBroadcast \
    -p com.oculus.guardian \
    --es cmd '{"automation":{"guardian":{"force_stationary":true}}}'
```

Each piece was found the hard way and none of it is guessable:

| piece | value | how it was found |
|---|---|---|
| broadcast action | `com.oculus.vrguardianservice.JsonCmdUserBroadcast` | dex strings |
| **extra key** | **`cmd`** | brute-forced 10 candidates; the receiver logs `JsonCmdUserBroadcastReceiver. Cmd: <value>`, so a wrong key prints `null` |
| **JSON shape** | **`{"automation":{"guardian":{...}}}`** | probed; a bare `{"guardian":{...}}` parses (`numObjects: 1`) but never reaches the handler |
| gate property | `persist.oculus.guardian_json_cmds_user_build=1` | libguardian strings |

Verified reaching the handler:

```
GuardianADBCommands: ProcessJsonCmd: {"automation":{"guardian":{"force_stationary":true}}}
GuardianADBCommands: ProcessJsonCmd. numObjects: 1
GuardianADBCommands: ProcessJsonCmd: automation
GuardianADBCommands: ProcessJsonCmd: guardian
GuardianADBCommands: ProcessJsonCmd: force_stationary: Invalid Guardian. ...
```

`force_roomscale` exists alongside it.

**Ordering gotcha:** start guardian FIRST, then restart trackingservice. Otherwise
guardian logs `SlamAnchorRuntimeIpcClient: InitClientInternal failed!` and never
attaches to trackingservice's `SlamAnchorServer` — no anchor work possible.

**Remaining:** "Invalid Guardian" is a state complaint, not a syntax one. It
needs the headset worn and tracking at 6DOF with a valid map. The error itself
names the next lever — "use mapMgr automation cmd to override map if necessary"
— so `mapMgrObj` is the companion command if the map needs forcing.

## SOLVED: creating a map from nothing (2026-08-24)

The bootstrap gap is closed. With the headset worn and tracking at 6DOF, the
headless command mints a lineage:

```
ProcessJsonCmd: force_stationary: CurrentAnchor: 550c10dd-...
persistAnchor: Saving transient anchor: 550c10dd-...

Vega Map Context: topNodeUid e028d9f2-...  timeCount = 1 (persistent)
  anchor 65b98f84 ... registered Y, persistent Y, worldOrigin Y
```

Started from a WIPED mapdb and a transient context — the exact state that never
produced a file all day. Result, decoded from disk:

| | |
|---|---|
| nodes | 1 |
| map points | **574** |
| paired (point, descriptor) rows | **5303** |
| extent | 5.12 x 3.78 x 7.98 m |
| anchor flags | `{registered: True, persistent: True, worldOrigin: True}` |

The flag triple is identical in shape to the original 2026-08-20 lineage's
`1a97f72c`, so this is the same kind of object, not a degenerate one.

**Consequences.** Map creation no longer depends on a historical accident, a
display, passthrough, a working dashboard, or Meta's servers. A lineage can be
minted for any space on demand, in seconds, over adb. Combined with transplant
(seed a puck) and extend (walk from mapped into unmapped territory), the map
lifecycle is complete:

    create   force_stationary via the JSON command channel
    seed     copy mapdb + chcon + restart trackingservice
    extend   relocalize, then walk into new territory
