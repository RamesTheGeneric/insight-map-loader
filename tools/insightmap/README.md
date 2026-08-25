# insightmap — reading the Quest 1 Insight SLAM map

A toolkit for reading and analyzing the live Insight (VIPER / VegaMapper) map
straight from the `trackingservice` process on a rooted Quest 1 (`monterey`,
Snapdragon 835, Android 10). The device is **EOL with OTA updates blocked**, so
the binary (`/system/lib64/libtrackingengines.so`) and its memory layout are
frozen — the signatures and offsets here are stable, which is what makes a
durable library worthwhile.

Full investigation history: `docs/insight-map-access.md` (access + memory
forensics) and `docs/insight-pipeline.md` (how the pipeline + descriptor NN
work). This README is the operational reference.

## Why memory forensics (every other door is walled)

Confirmed closed, exhaustively:
- **Binder export** (`exportMapDataForAnchor`, `keepMap`, `loadMap`) — rejected.
- **Disk save** (`scheduleSaveMap` → `.vega.map` in `/vision/insideout/mapdb`) —
  gated on anchor-persist behind the VR-focus wall; never fires from mapping
  alone.
- **Submap publish** (`PublishSubmap`) — fires continuously but onto an
  in-process `T2MQueue`, never crossing a syscall boundary to tap.
- **Serializer hook** (Frida) — the engine is fully stripped (only `.dynsym`,
  3 defined text symbols); the serializers are internal and unaddressable.

So the map is read from RAM. `process_vm_readv` (root + CAP_SYS_PTRACE + SELinux
permissive) reads the live heap **without stopping tracking**.

## What is validated vs experimental

**Solid (tested, relied on):**
- `memread.c` reads arbitrary heap regions live.
- The VegaMap context is found by its UUIDs (which also appear in
  `dumpsys tracking`): `map_uuid` (topNodeUid), the `anchor` uuid, the `L1 node`
  uuids — stored raw 16-byte big-endian.
- Map points are per-node contiguous **f64 (x,y,z)** blocks (room-scale metres,
  gravity-aligned).
- Descriptors are **32-byte (256-bit) binary** vectors (popcount ~128); the
  store is a high-entropy region found by density.
- **Cross-device descriptor matching works** — validated with controls: real
  matches reach ~33/256 Hamming, far below the ~80–95 shared-library / shuffle
  noise floor. Two different pucks describing the same room produce matchable
  descriptors. No neural model is needed to MATCH existing descriptors.

**Experimental (documented, scaffolded, NOT solid):**
- Exact **point↔descriptor pairing** needs a `GraphNodeSoA` graph walk. Points
  and descriptors are located separately; the per-node observation index that
  links them is not yet reversed. `MapDump.point_blocks()` and
  `.descriptors()` return the two halves; `.paired()` raises `NotImplementedError`.
- **Full geometric alignment** (`align_ransac` + `kabsch`) is provided but only
  as good as the pairing that feeds it.

## The map structure (VegaMapper SoA)

Hierarchical graph: **L2 → L1 nodes → keyrigs → map points**. Per the dumpsys
`Map SoA` line, capacities are `MapPoint 8100`, `Descriptors 283500` (= 8100×35,
~35 descriptors/point across observations), `PointResiduals 97200` (8100×12),
`ImagePatch 52500`, `Anchors 25`. Arrays are pre-allocated at capacity (mostly
zero), populated at the used prefix — which is why point/descriptor scans must
exclude the zero padding. Positions and descriptors are stored **per L1 node**,
not as one flat array. The custom `PersistingHeapDynamic` allocator is why the
struct layout is not plain `std::vector`; all parsing here is value-signature
based to avoid depending on it.

## The descriptor NN (for context; not needed to match existing descriptors)

`DPE_V14` (Deep Patch Embedding, quantization-aware trained) — a PyTorch mobile
flatbuffer embedded in the `.so` (`PTMF` id at 0x2b1be4), run by BoltNN on the
Hexagon DSP. Custom ops (`dpe_operators`) live only in the `.so`, so the model
can't run standalone. Output shares a 32-byte interface with the ORB and FREAK
fallbacks Insight also ships. See `docs/insight-pipeline.md`.

## Requirements

- `adb root` on the target (`adbd` already root; no `su` — Magisk is inert here).
- **SELinux permissive** for the reads: `adb -s <ip>:5555 shell setenforce 0`.
  Reading a system service's memory is denied even for root otherwise. Restore
  with `setenforce 1` when done. (Enabling map_db + keypoint_cache also restarts
  trackingservice, which resets the Insight frame — expected.)

## Usage

```sh
./build.sh                                   # cross-compile memread (aarch64)

# one puck: arm mapping, walk it to build a map, then dump + summarize
./probe.py --dev 192.168.1.11 --out /tmp/imap_132 --enable

# the validated cross-device descriptor experiment (both pucks map one room)
./probe.py --dev 192.168.1.11 --out /tmp/imap_132 \
           --dev2 192.168.1.10 --out2 /tmp/imap_108 --cross
```

Library:

```python
import insightmap as im
dev = im.Device("192.168.1.11")
dev.enable_mapping()          # map_db + keypoint cache (restarts tracking)
dev.set_permissive()          # setenforce 0 (or run it yourself if blocked)
dev.push_memread()
print(dev.map_info())         # counts + uuids from dumpsys
dump = dev.dump_map("/tmp/imap_132")
descs = dump.descriptors()    # (N,32) uint8
blocks = dump.point_blocks()  # list of (n,3) f64 room-point blocks (heuristic)
# cross-device:
rep = im.cross_match_report(descsA, descsB)   # {'verdict': 'cross-match', ...}
```

## The path this enables (map-to-map colocation, host-side)

Read both pucks' maps → match their existing descriptors (Hamming, no NN) →
Kabsch/RANSAC on the matched 3D points → `T_frameA_frameB` → a shared cross-puck
frame. This is Meta's cross-device colocation done ourselves. The one remaining
build is the pairing (`GraphNodeSoA` walk) so descriptor matches carry their 3D
points into `align_ransac`. Trade vs our q1track pair-stream: Insight's
illumination-robust deep descriptors and bundle-adjusted maps, at the cost of
memory-forensics setup (permissive + dump) each session.

## Files
- `memread.c` / `build.sh` — the native live-memory reader.
- `insightmap.py` — Device access, MapDump parsing, matching, alignment.
- `probe.py` — CLI: dump + summarize one or two pucks; run the cross-device test.
