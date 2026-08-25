# insightmap — reading the Quest 1 Insight SLAM map

Tools for pulling, decoding, matching and viewing the Insight (VIPER /
VegaMapper) map from a rooted Quest 1 (`monterey`, Snapdragon 835, Android 10).

The map is read from **files**, not from process memory: when trackingservice
persists a map it writes it to `/vision/insideout/mapdb` as Thrift records, and
those files are exact. Whether they exist at all is the whole game — see
`docs/insight-map-lifecycle.md` for what makes a map persist, and `FINDINGS.md`
for the heap-extraction track that this replaced and why it was dropped.

The device is EOL with OTA blocked, so the on-disk format is frozen. That is
what makes a decoder worth writing rather than re-deriving each session.

## The format, in one paragraph

One file per record: `nd_<node-uuid>_<K>.mapdata` for node data (K = record
kind) and `ad_<anchor-uuid>.mapdata` for anchors. The dialect is **Facebook**
Thrift compact — Apache compact plus **wire type 13 = FLOAT, 4 bytes,
big-endian**. That one extension is why the files look like garbage to a stock
Thrift reader: poses are float32. Map points are stored as (azimuth, elevation,
inverse depth) per keyrig, Y-up, azimuth measured from +Z toward +X. Kind 3 is
`T_root←node` with the quaternion in **xyzw** order, applied forward
(`p = R·p + t`). Full derivation: `docs/insight-mapdata-format.md`.

## Files

| | |
|---|---|
| `insightmap.py` | `Device` — query the live map context over adb, pull a mapdb |
| `mapdata.py` | the decoder: `Map`, `NodeMap`, `.paired()` → points + descriptors |
| `matchmap.py` | descriptor matching and the robust rigid solve |
| `selftest_align.py` | validates the whole chain against known ground truth |
| `visualize.py` | flat orthographic views (top / front / side) + stats |
| `visualize3d.py` | orbitable WebGL2 viewer: points, trajectory, floor grid |

## Usage

```sh
# pull a map off a puck
python3 -c "import insightmap as im; im.Device('192.168.1.10').pull_mapdb('/tmp/mapdb108')"

# look at it
./visualize3d.py /tmp/mapdb108 --out map3d.html      # orbit it
./visualize.py --dump /tmp/mapdb108 --out map.html   # read offsets off it

# check the decoder + matcher against ground truth
./selftest_align.py /tmp/mapdb108

# two maps, B placed by a solved 4-DoF transform
./visualize3d.py /tmp/mapdb108 --dump2 /tmp/mapdb132 \
    --yaw 358.2 --t 0.31 0.0 -1.15 --out align3d.html
```

As a library:

```python
import mapdata as md, matchmap as mm
pa, da = md.Map("/tmp/mapdb108").paired()   # points (N,3) + descriptors, index-aligned
pb, db = md.Map("/tmp/mapdb132").paired()
res = mm.align_stable(pa, da, pb, db, yaw_only=True)   # None if overlap is too thin
```

## Two things the matcher gets right that a textbook one does not

1. **A point carries many descriptors** — one per observation, and they are near
   duplicates. A plain Lowe ratio test compares a descriptor against another
   view of the *same* point, gets a ratio near 1, and discards every good match.
   The ratio must be taken against the best match belonging to a **different
   point**.
2. **Match on descriptors, decide on points.** Individual observations match
   noisily, so votes accumulate per `(point_a, point_b)` pair and the pair is
   judged on its best distance *and* its vote count.

`align_stable()` adds a seed-consensus gate: several independent RANSAC seeds
must agree on yaw within `YAW_SPREAD_MAX_DEG`. Thin overlap produces confident
nonsense otherwise, and a wrong transform is worse than a refusal — this is the
one place where returning `None` is the useful answer.

## Why this exists when colocation is native

It does not sit in the live path. Pucks share one map, so they are already in
one frame and the transform between them is identity — nothing here runs during
tracking. These tools are for **inspecting and verifying** that: confirming two
pucks really are on one root, seeing what a map actually contains before
trusting it, and measuring how far off an alignment is when something looks
wrong. `selftest_align.py` is the sharpest of them, because a single puck's own
L1 nodes are independent observations of one room whose correct relative
transform is known to be the identity — so the pipeline can be scored without
any ground truth from outside.
