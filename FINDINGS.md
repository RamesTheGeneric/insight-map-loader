# Findings

What was tried, what worked, and — more usefully — what didn't. Quest 1
(`monterey`, Android 10), all of it observed on hardware.

If you are doing similar work, the dead ends below are probably worth more than
the successes.

---

## The map is on disk, and it is readable

Insight persists its SLAM map to `/vision/insideout/mapdb` as **Facebook Thrift
compact** — Apache compact plus wire type **13 = FLOAT, four bytes big-endian**.
That single deviation is why the files look undecodable to a stock Thrift
reader: poses are float32, not double.

Each map point stores **its own descriptors inline**, so the point↔descriptor
pairing that a memory-based approach has to reconstruct is simply nesting on
disk.

Point coordinates are **(azimuth, elevation, inverse depth)**, not xyz — the
giveaway is that column 0 saturates at exactly ±π on every node while columns 1
and 2 never do. Full format: [docs/insight-mapdata-format.md](docs/insight-mapdata-format.md).

## Colocation needs no transform

Two headsets that load the same map relocalize into the same root node and
therefore track in the same frame. Measured at **3.3 cm horizontal median** with
the pucks held together and identity applied.

Comparing the two pucks' own copies of a shared map afterwards: **node poses
identical, point content diverged substantially** (232 vs 108 points on one
node). Since node poses define the frame, the copies can diverge freely without
the frame drifting. That is why colocation is stable rather than lucky.

---

## Dead ends

### Every clean export path is walled

`exportMapDataForAnchor`, `keepMap`, `loadMap`, `readMap` — all reachable over
binder, all refused. `listMaps()` returns count 0 even on a device with 5.7 MB
of map on disk, so the binder's map store is a *different* store from the mapdb
files. `scheduleSaveMap` sits behind a VR-focus wall. The serializer is not
hookable; the binary is stripped.

### Memory forensics: works, but not well enough

The map *can* be read out of `trackingservice` RAM with `process_vm_readv`
(no ptrace stop, tracking keeps running). Descriptors cross-match between
devices, which is a real result.

But once the persisted map gave us ground truth, the heap extraction failed it.
Blocks recovered from memory do align to the real map — better than
size-and-extent-matched random controls, so they are genuinely spatial — yet the
best residual is **0.278 m against a map whose own point spacing is
0.075–0.233 m**. An order of magnitude short.

Two things this taught:

- **A statistic that agrees can still be coincidence.** An earlier "own-NN
  spacing 0.075 m, identical to the memory track" reading was treated as
  corroboration. It was not; the memory blocks measure 0.023–0.28 m and the
  match was chance.
- **Use matched controls.** Against a naive control (300 random points over the
  full bounding box), 36- and 70-point blocks scored 0.059 m at 100 % — which is
  just what any small compact cluster does when dropped into a dense cloud.

Read the files, not the heap.

### Three falsified theories about persistence

Only a **persistent** map context is ever written to disk. Before finding that,
these were each tested and each wrong:

| theory | killed by |
|---|---|
| map maturity — a map persists once it grows past its seed node | walked a puck to 4 nodes / 1093 points; mapdb stayed empty |
| the guardian boundary record gates it | deleted it; writes continued |
| `enable_map_db` property | set on both pucks; only one ever wrote |

The actual gate: a context is promoted when **one of its anchors is persisted**,
and the anchor record's field 2 is the flag triple `{registered, persistent,
worldOrigin}` — matched 5/5 against `dumpsys`.

### The real blocker was a disabled package

`com.oculus.guardian` ships **disabled** on a puck set up for tracking. Guardian
is the only thing that calls `persistAnchor`. So: no service, no persisted
anchor, no persistent context, no file — and nothing anywhere reporting a
problem. Every device-level theory above failed because the answer was
per-package.

### Guardian and pose streaming are mutually exclusive

Creating a map needs the guardian package **enabled**. Streaming poses needs it
**disabled — before the tracker app starts**. The property alone is not enough.

With the package enabled the tracker app runs, takes window focus, has correct
config, reports `6DOF Valid: Yes` — and emits nothing at all. It also puts the
displays into passthrough instead of dark. So map creation is a bracket: stop
tracker → enable → create → disable → restart → *verify streaming resumed*.

### A stale bridge looks exactly like a broken map

A correctly shared map showed a puck rotated 180°. The map was perfect — 259
matches, 72 inliers. Both bridges were 26 hours old, and restarting either
`trackingservice` or the tracker app invalidates them.

The bridge is now the only piece of stored calibration left in the system, and
therefore the only thing that can go stale.

### Things that silently do nothing

- **`chcon` after `adb push`.** Files arrive with the wrong SELinux label and
  `trackingservice` cannot read them. Looks like "the transplant failed".
  Running SELinux Permissive hides it until the next reboot.
- **`adb root` after a reboot.** Gone. `setprop`, `chown` and `chcon` then fail
  without saying so.
- **`am broadcast` exit codes.** Always 0. The verdict has to come from polling
  state, with logcat read only for the error text.
- **`stop trackingservice`.** Does *not* flush the map. Anything mapped since
  the last automatic write is lost, and writes are irregular — gaps up to 31
  minutes observed.
- **Piping `dumpsys tracking` into `grep -m1`.** The early pipe close leaves the
  tracking service unavailable for seconds. Dump to a file, grep the file.

### The headless guardian channel

`libguardian.so` carries a JSON command channel, gated by
`persist.oculus.guardian_json_cmds_user_build`, delivered by broadcast. It can
mint a map with no VR UI at all — which matters, because the dashboard is
permanently broken on a headset kept off Meta's servers.

Neither half was guessable: the extra key is **`cmd`** (a wrong key logs
`Cmd: null`, which is how it was brute-forced), and the payload must be wrapped
in **`automation`** — a bare `{"guardian":{…}}` parses, reports `numObjects: 1`,
and silently never reaches the handler.

---

## Method notes

- **Take the layout from the binary's own type names**, then find owners by
  scanning for pointers into a known-good region. Pattern-matching memory for
  containers found nothing; `CompactVersionedHandleImpl<MapPointSoA,false,21,10>`
  spelled out the handle format directly.
- **Build a visualiser early.** Two bugs were caught by eye that every statistic
  had passed: a set of unit vectors masquerading as map points (it rendered as a
  semicircle, and edge-on as a straight line), and per-node local frames being
  concatenated (real 90° corners read as 77–88°).
- **Find a ground truth that cannot lie.** One puck's own L1 nodes are
  independent observations of the same room with known relative poses, so the
  correct answer for every node pair is the identity — which tests the whole
  chain at once. Two co-located pucks serve the same purpose physically.
