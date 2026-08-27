# The Insight map lifecycle

**Operational reference.** How to create, share, grow and read the SLAM map that
puts every puck in one tracking universe. For the reverse-engineering record —
including the wrong turns — see `docs/insight-mapdata-format.md`; for the walls
on the binder/anchor APIs see `docs/insight-map-and-anchors.md`.

Everything here is verified on Quest 1 pucks as of 2026-08-24, and none of it
needs a display, passthrough, a working dashboard, or Meta's servers.

---

## The one idea

Insight is **always** building a map whenever it tracks at 6DOF. That map lives
in a *map context*, and a context is either:

| | written to `/vision/insideout/mapdb`? |
|---|---|
| **transient** | never |
| **persistent** | yes, automatically |

A context becomes persistent when **one of its anchors is persisted**. That is
the entire gate. Nothing else matters — not node count, not map density, not the
guardian boundary file, not `enable_map_db`. All four were tested and falsified.

Two pucks that load the same persistent map track in **one shared frame**, which
is colocation with no host-side transform to solve or maintain.

---

## CREATE — mint a map for a new space

Needed once per space. The headset must be **worn and tracking at 6DOF**.

```sh
adb -s $IP root                                  # root does NOT survive reboot
adb -s $IP shell pm enable com.oculus.guardian   # ships DISABLED on the pucks
adb -s $IP shell setprop persist.oculus.guardian_json_cmds_user_build 1
adb -s $IP shell am start-foreground-service \
    com.oculus.guardian/com.oculus.vrguardianservice.VrGuardianService
adb -s $IP shell 'stop trackingservice; start trackingservice'   # AFTER guardian

adb -s $IP shell am broadcast \
    -a com.oculus.vrguardianservice.JsonCmdUserBroadcast \
    -p com.oculus.guardian \
    --es cmd '{"automation":{"guardian":{"force_stationary":true}}}'
```

Success looks like:

```
ProcessJsonCmd: force_stationary: CurrentAnchor: <uuid>
persistAnchor: Saving transient anchor: <uuid>
Vega Map Context: topNodeUid <uuid>  timeCount = 1 (persistent)
  anchor <uuid> ... registered Y, persistent Y, worldOrigin Y
```

**This command creates no map content.** It persists an anchor, which promotes
the context Insight had *already* been filling; trackingservice then flushes what
it has. So you need not walk the space first — fire it early and everything you
cover afterwards accumulates into the lineage and is written automatically.
Measured: 574 points at creation, 918 points and 3 nodes five minutes later,
with no further commands.

`force_roomscale` exists alongside `force_stationary`.

### Why each piece is there

| piece | value | note |
|---|---|---|
| package enable | `pm enable com.oculus.guardian` | **the** blocker; disabled on both pucks by fleet setup |
| gate property | `persist.oculus.guardian_json_cmds_user_build=1` | without it the JSON channel is inert |
| broadcast action | `com.oculus.vrguardianservice.JsonCmdUserBroadcast` | |
| extra key | **`cmd`** | a wrong key logs `Cmd: null`, which is how it was found |
| JSON shape | **`{"automation":{"guardian":{…}}}`** | a bare `{"guardian":{…}}` parses, logs `numObjects: 1`, and silently never reaches the handler |
| start order | guardian **before** trackingservice | else `SlamAnchorRuntimeIpcClient: InitClientInternal failed!` and no anchor work is possible |

---

## SEED — put an existing map on another puck

Gives the receiving puck the same frame. It must be **physically in that space**
so it can relocalize.

```sh
adb -s $IP push <mapdb>/. /data/local/tmp/mapdb_in/
adb -s $IP shell 'cp /data/local/tmp/mapdb_in/*.mapdata /vision/insideout/mapdb/'
adb -s $IP shell 'chown system:system /vision/insideout/mapdb/*.mapdata;
                  chmod 600 /vision/insideout/mapdb/*.mapdata;
                  chcon u:object_r:vision_file:s0 /vision/insideout/mapdb/*.mapdata'
adb -s $IP shell 'stop trackingservice; start trackingservice'
```

**`chcon` is mandatory.** Files arriving over `adb push` carry the wrong SELinux
label and trackingservice reads that directory as `vision_file`. Skip it and the
map is silently unreadable — which reads as "the transplant failed" rather than
as a permissions problem.

Confirm with `dumpsys tracking`: the receiver should report the **same
`topNodeUid`** as the source, marked `(persistent)`, and log
`loc_success: true` with that root as `localizedRootNodeId`.

---

## EXTEND — grow a map into new territory

Nothing to run. A puck that has relocalized into the persistent map adds new L1
nodes to *that* context and they get written automatically. Walk from mapped
territory into unmapped territory and the lineage grows.

Proven: `.132` contributed node `de67651c` into `8994724e` and persisted it.

A **disconnected** space (no walkable overlap) cannot be reached this way — use
CREATE there instead.

---

## RE-SYNC — same root, diverged content

**Sharing a map root does not keep two pucks' maps identical.** Each goes on
mapping into its own copy, so content diverges while identity does not. Measured
on this fleet after a few hours on one shared root `8994724e`:

| | `.108` | `.132` |
|---|---|---|
| map points | 1269 | 1063 |
| L1 nodes | 6 | the same 6 uuids |
| mapdb size | 5184 KB | 3908 KB |
| points in common (to 0.1 mm) | under 11% per node | |

Neither puck had added or removed a node — each had *refined* the same six. And
they still agreed geometrically: `.132`'s points sat a median **0.066 m** from
the nearest `.108` point, against `.108`'s own point spacing of **0.068 m**.
Agreement to within the map's own resolution, so colocation was intact.

That is the normal case and needs no action. Diverged point counts are not a
fault. But when you do want every puck back on one known-good copy, **`⟲ Re-sync`
in the GUI** copies the source's map across *even to pucks already reporting that
root*.

Plain `⇄ Share map` deliberately skips those pucks — it exists to get a fleet
onto one frame, and re-copying a puck already on that frame would be a tracking
outage for nothing. That skip is right for Share and is exactly what blocks a
refresh, which is why Re-sync is a separate button rather than a change to it.

Re-sync runs every step Share does, both backups included. It is still
destructive: the target **loses whatever it mapped independently**, its tracking
restarts, and its bridge must be re-solved afterwards.

> Restoring a backup needs the `chcon` above re-run. Toybox `cp -a` does not
> preserve the SELinux label, so a restored map is silently unreadable without it.

---

## READ — get points and descriptors on the host

Independent of everything: no VR session, no tracking, device may be asleep.
Needs root, because the directory is `700 system:system`.

```python
import insightmap as im, mapdata as md
m = md.Map(im.Device("192.168.1.10").pull_mapdb("/tmp/mapdb108"))
pts, desc = m.paired()      # (N,3) metres in root frame, (N,32) uint8
```

```sh
./mapdata.py     /tmp/mapdb108            # summarize every record
./visualize3d.py /tmp/mapdb108 --out map.html   # interactive 3D
./selftest_align.py /tmp/mapdb108        # ground-truth alignment check
```

The file is the **last write, not the live map** — see the caveat below.

---

## Verified numbers

| claim | evidence |
|---|---|
| decode is correct | 3D render recognised by the operator as the actual room |
| decode matches the service | same root uuid, node set and point counts as live `dumpsys` |
| pairing is exact | 49,688 index-aligned (point, descriptor) rows from `.108` |
| alignment pipeline | exact on synthetic ground truth; mixed on real maps (see README caveat) |
| colocation is real | two pucks held together: **3.3 cm horizontal median**, identity transform, 8 samples |
| creation works from nothing | wiped mapdb → 574 points, 5,303 paired rows, anchor flags all true |

---

## Caveats

**There is no flush command.** Writes are automatic but irregular — observed
bursts a minute apart, and gaps up to 31 minutes. `libguardian.so` has no
save/write/flush JSON command.

**`stop trackingservice` does NOT flush.** Verified: mtime unchanged across a
clean stop. So mapping since the last write is lost on restart or reboot. After
covering somewhere that matters, check `stat -c %y /vision/insideout/mapdb`
before restarting anything.

**Root does not survive a reboot.** Re-run `adb root`, or `setprop`/`setenforce`
fail silently and everything downstream looks broken for the wrong reason.

**Copies diverge.** After seeding, each puck evolves its own mapdb. The shared
frame stays pinned by the common root node; the files do not stay identical.

**Same hardware only.** The map embeds the originating device's camera rig
calibration. Proven between two Quest 1s; untested across models.

**Back up the lineages.** They are a few MB and, before CREATE existed, were
irreplaceable. `~/insight-map-loader-backups/`.

---

## Troubleshooting

| symptom | cause |
|---|---|
| boundary flow does nothing / dashboard never appears | `com.oculus.guardian` disabled, and/or SystemUX dying on the blocked Meta domains (which must stay blocked) |
| `ProcessJsonCmd` never logs | wrong extra key — must be `cmd` |
| logs `numObjects: 1` then nothing | missing the `automation` wrapper |
| `force_stationary: Invalid Guardian` | headset not worn / not at 6DOF |
| `SlamAnchorRuntimeIpcClient: InitClientInternal failed!` | trackingservice started before guardian |
| transplanted map ignored | missing `chcon u:object_r:vision_file:s0` |
| `setprop` / `setenforce` "Permission denied" | lost root after a reboot |
| puck healthy but never writes | its context is **transient** — check `dumpsys tracking` for `(persistent)` |

## Guardian vs. pose streaming: an ordering constraint

These two requirements are in direct conflict and must be sequenced, not
merely configured:

| | needs |
|---|---|
| **creating a map** | `com.oculus.guardian` package **enabled**, service running |
| **streaming poses** | package **disabled**, and disabled BEFORE the tracker app starts |

Verified the hard way. With the package enabled, the tracker app looks
completely healthy — running, window-focused, correct `config.txt`, Insight at
`6DOF Valid: Yes` — and emits **nothing**. `persist.oculus.guardian_disable=1`
is not sufficient on its own; the package itself must be disabled, and an app
started before that happens stays mute until restarted.

An enabled guardian package also puts the headset into **passthrough** rather
than a dark display, if the loaded map carries guardian anchors but the device
has no boundary record — which is exactly the state a transplanted map creates.

So a create-map flow is a bracket, not a toggle:

```
stop tracker app  →  pm enable guardian  →  create  →
pm disable-user guardian  →  start tracker app  →  VERIFY it streams again
```

The last step matters: without it the job reports success and leaves a puck
with a fine new map that no longer tracks for SteamVR.

### Consequence for the LOCAL→world bridge

`android/q1tracker/.../quest_tracker.cpp` uses `XR_REFERENCE_SPACE_TYPE_LOCAL`
and says why: *"STAGE depends on a configured guardian, and the plan is to run
with the boundary disabled."* Colocation reverses half of that premise — a
guardian boundary is now how a map becomes persistent — which suggested STAGE
poses would already agree across pucks and the bridge could be deleted
entirely.

But the constraint above cuts the other way: the fleet's **streaming** state
requires the guardian package disabled, and STAGE requires a configured
guardian. Whether a boundary that merely *exists in the map* is enough for
STAGE, with the package disabled, is **untested**. Test it on hardware before
spending an APK change on it.

## After any share or tracker relaunch: RE-BRIDGE

Restarting `trackingservice` resets the Insight world frame; restarting the
tracker app resets the OpenXR LOCAL frame. The stored bridge describes neither,
and a stale one is not subtle — a 26-hour-old bridge showed a correctly shared
puck rotated 180 degrees. The map layer was perfect (259 matches, 72 inliers);
only the bridge was wrong.

The GUI's share job now requests a re-bridge and waits for it. Manually:
hold the pucks **still** and press ⌖ Bridge now.
