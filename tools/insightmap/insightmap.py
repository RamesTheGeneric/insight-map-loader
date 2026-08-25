"""insightmap -- read and analyze the Quest 1 Insight (VIPER/VegaMapper) SLAM
map straight from the live `trackingservice` process.

This is a toolkit for the reverse-engineered runtime map, distilled from the
investigation in docs/insight-map-access.md and docs/insight-pipeline.md. The
device is EOL with updates blocked, so the binary and its memory layout are
frozen -- these offsets and structures are stable.

What is SOLID (validated, used below):
  * process_vm_readv reads the live heap without stopping tracking (memread.c).
  * The VegaMap context is locatable by its UUIDs (map_uuid / anchor / L1 node),
    which also appear verbatim in `dumpsys tracking`.
  * Map points are per-node contiguous f64 (x,y,z) blocks, room-scale metres,
    gravity-aligned.
  * Descriptors are 32-byte (256-bit) binary vectors (popcount ~128); the store
    is a high-entropy region locatable by density.
  * Descriptors CROSS-MATCH between two devices for the same physical feature
    (validated with controls: real matches reach ~34/256 Hamming, well below the
    ~80 shared-library / shuffle noise floor). No neural model needed to MATCH
    existing descriptors -- only to compute new ones.

What is EXPERIMENTAL / TODO (documented, scaffolded, not yet solid):
  * Exact point<->descriptor pairing needs the GraphNodeSoA walk (points and
    descriptors are located separately; the per-node index linking them is not
    yet reversed). `MapDump.paired()` is a best-effort stub.
  * Full geometric alignment (mutual-NN + ratio + RANSAC + Kabsch) is provided
    but only as good as the pairing.

Requirements: `adb root` on the target, and SELinux permissive
(`setenforce 0`) for the memory reads -- reading a system service's memory is
denied even for root otherwise. `Device.set_permissive()` issues it; on a
locked-down shell run it yourself first (see README).
"""
from __future__ import annotations

import os
import re
import subprocess
import struct
from dataclasses import dataclass, field

import numpy as np

HERE = os.path.dirname(os.path.realpath(__file__))
MEMREAD_LOCAL = os.path.join(HERE, "memread")
MEMREAD_DEVICE = "/data/local/tmp/memread"

# persist props that make the tracker build a rich, keypoint-cached map.
MAP_PROPS = {
    "persist.trackingservice.enable_map_db": "1",
    "persist.trackingservice.enable_keypoint_cache_size_5": "1",
}


# --------------------------------------------------------------------- device
class Device:
    """adb wrapper for one Quest 1 (wifi adb serial `<ip>:5555`)."""

    def __init__(self, ip: str, port: int = 5555):
        self.serial = ip if ":" in ip else f"{ip}:{port}"

    def sh(self, cmd: str, timeout: int = 30) -> str:
        r = subprocess.run(["adb", "-s", self.serial, "shell", cmd],
                           capture_output=True, timeout=timeout)
        return r.stdout.decode(errors="replace")

    def push(self, local: str, remote: str, timeout: int = 120):
        subprocess.run(["adb", "-s", self.serial, "push", local, remote],
                       capture_output=True, timeout=timeout)

    def pull(self, remote: str, local: str, timeout: int = 180):
        subprocess.run(["adb", "-s", self.serial, "pull", remote, local],
                       capture_output=True, timeout=timeout)

    def pid(self) -> int | None:
        out = self.sh("pidof trackingservice").strip()
        return int(out.split()[0]) if out else None

    def enable_mapping(self, restart: bool = True):
        """Arm map_db + keypoint cache so the map carries descriptors; restart
        trackingservice for the props to take (this RESETS the Insight frame)."""
        for k, v in MAP_PROPS.items():
            self.sh(f"setprop {k} {v}")
        if restart:
            self.sh("setprop ctl.stop trackingservice; sleep 2; "
                    "setprop ctl.start trackingservice; sleep 3")

    def set_permissive(self) -> bool:
        """setenforce 0 (needed for the memory reads). Returns True if it took.
        On a shell where this is blocked, run it yourself before dumping."""
        self.sh("setenforce 0")
        return self.sh("getenforce").strip() == "Permissive"

    def set_enforcing(self):
        self.sh("setenforce 1")

    def push_memread(self):
        if not os.path.exists(MEMREAD_LOCAL):
            raise FileNotFoundError("build memread first: tools/insightmap/build.sh")
        self.push(MEMREAD_LOCAL, MEMREAD_DEVICE)
        self.sh(f"chmod 755 {MEMREAD_DEVICE}")

    # ---- the dumpsys view: counts, uuids, anchor (no point/descriptor values)
    def map_info(self) -> dict:
        d = self.sh("dumpsys tracking 2>/dev/null", timeout=25)
        info: dict = {"raw_len": len(d)}
        m = re.search(r"MapPoint (\d+).*?Descriptors (\d+)", d)
        if m:
            info["map_points"] = int(m.group(1))
            info["descriptors"] = int(m.group(2))
        m = re.search(r"topNodeUid ([0-9a-f-]+)", d)
        if m:
            info["map_uuid"] = m.group(1)
        info["l1_nodes"] = re.findall(r"L1 node ([0-9a-f]+) hosts", d)
        m = re.search(r"anchor ([0-9a-f-]+) ->.*?worldOrigin (\w)", d)
        if m:
            info["anchor_uuid"], info["anchor_world_origin"] = m.group(1), m.group(2)
        return info

    # ---- the persisted map: strictly better than any memory dump when present
    def mapdb_files(self) -> list[str]:
        out = self.sh("ls /vision/insideout/mapdb 2>/dev/null", timeout=20)
        return [f for f in out.split() if f.endswith(".mapdata")]

    def pull_mapdb(self, out_dir: str) -> str:
        """Pull /vision/insideout/mapdb and return the local directory.

        When trackingservice has persisted the map, this replaces the whole
        forensics path: exact points, their descriptors already paired, and the
        node poses to place them in one frame -- no root, no permissive, no
        process_vm_readv. Decode with mapdata.Map(). Empty until the map
        actually persists (see docs/insight-mapdata-format.md)."""
        os.makedirs(out_dir, exist_ok=True)
        self.pull("/vision/insideout/mapdb", out_dir, timeout=180)
        inner = os.path.join(out_dir, "mapdb")
        return inner if os.path.isdir(inner) else out_dir

    def big_arenas(self, min_mb: int = 6) -> list[tuple[int, int]]:
        """(vaddr, size) of the large libc_malloc arenas -- where the SoA lives.
        The map is reliably in the two biggest (~77 MB + ~41 MB)."""
        pid = self.pid()
        maps = self.sh(f"cat /proc/{pid}/maps", timeout=20)
        out = []
        for line in maps.splitlines():
            m = re.match(r"([0-9a-f]+)-([0-9a-f]+) (\S+) \S+ \S+ \S+\s*(.*)", line)
            if not m:
                continue
            a, b, perm, name = int(m.group(1), 16), int(m.group(2), 16), m.group(3), m.group(4)
            if "rw" in perm and "libc_malloc" in name and (b - a) >= min_mb * 1024 * 1024:
                out.append((a, b - a))
        out.sort(key=lambda x: -x[1])
        return out

    def dump_map(self, out_dir: str, arenas: int = 2) -> "MapDump":
        """Read the top `arenas` malloc arenas into files and return a MapDump.
        Needs push_memread() + permissive first."""
        os.makedirs(out_dir, exist_ok=True)
        pid = self.pid()
        regions = self.big_arenas()[:arenas]
        files = []
        for i, (addr, sz) in enumerate(regions):
            dev_f = f"/data/local/tmp/arena{i}.bin"
            self.sh(f"{MEMREAD_DEVICE} {pid} {addr:x} {sz} {dev_f}", timeout=90)
            loc = os.path.join(out_dir, f"arena{i}.bin")
            self.pull(dev_f, loc)
            files.append((loc, addr))
        return MapDump(files, self.map_info())


# ----------------------------------------------------------------- map dump
@dataclass
class MapDump:
    """A pulled set of arena files + the dumpsys metadata. All parsing is
    value-signature based (no reliance on the custom PersistingHeapDynamic
    container layout, which is why it survives), cross-checked against the
    dumpsys UUIDs/counts when available."""
    arenas: list[tuple[str, int]]           # (file, virtual base address)
    info: dict = field(default_factory=dict)
    _data: dict = field(default_factory=dict, repr=False)

    def _load(self, path: str) -> np.ndarray:
        if path not in self._data:
            self._data[path] = np.frombuffer(open(path, "rb").read(), dtype=np.uint8)
        return self._data[path]

    # ---- locate the VegaMap context by a known UUID (from dumpsys) ----
    def find_context(self, uuid_hex: str | None = None) -> list[tuple[str, int]]:
        """Return (file, offset) of every hit for the given UUID (raw 16-byte,
        big-endian, as Insight stores it). Defaults to the dumpsys map_uuid."""
        uuid_hex = uuid_hex or self.info.get("map_uuid", "").replace("-", "")
        if not uuid_hex:
            return []
        needle = bytes.fromhex(uuid_hex)
        hits = []
        for path, _ in self.arenas:
            data = self._load(path).tobytes()
            i = data.find(needle)
            while i >= 0:
                hits.append((path, i))
                i = data.find(needle, i + 1)
        return hits

    # ---- descriptors: the 32-byte binary store ----
    def descriptors(self, pop_lo: int = 96, pop_hi: int = 160,
                    window_mb: int = 1) -> np.ndarray:
        """Extract 256-bit binary descriptors from the densest descriptor-like
        region (popcount band, deduped). Best-effort: the region still carries
        some non-descriptor high-entropy noise, which for MATCHING only adds
        random ~128-Hamming pairs (it never fabricates low-Hamming matches --
        see cross_match's controls)."""
        best = None
        w = window_mb * 1024 * 1024 // 32
        for path, _ in self.arenas:
            d = self._load(path)
            n = len(d) // 32
            r = d[:n * 32].reshape(n, 32)
            pc = np.unpackbits(r, axis=1).sum(1)
            good = ((pc >= pop_lo) & (pc <= pop_hi) & r.any(1)).astype(np.int32)
            cs = np.cumsum(good)
            for s in range(0, len(good) - w, w // 2):
                c = cs[s + w] - cs[s]
                if best is None or c > best[0]:
                    best = (c, path, s, w)
        if best is None:
            return np.zeros((0, 32), np.uint8)
        _, path, s, w = best
        r = self._load(path)[: len(self._load(path)) // 32 * 32].reshape(-1, 32)[s:s + w]
        pc = np.unpackbits(r, axis=1).sum(1)
        return np.unique(r[(pc >= pop_lo) & (pc <= pop_hi) & r.any(1)], axis=0)

    # ---- map points: per-node contiguous f64 (x,y,z) blocks ----
    def point_blocks(self, min_pts: int = 8, coord_max: float = 8.0) -> list[np.ndarray]:
        """Return the per-node 3D point blocks (each an (n,3) f64 array). These
        are individually verifiable (room coords, gravity-aligned) but NOT yet
        linked to descriptors -- see the pairing note in the module docstring."""
        blocks = []
        for path, _ in self.arenas:
            d = self._load(path)
            n = len(d) // 8
            fl = np.frombuffer(d[:n * 8].tobytes(), dtype="<f8")
            # exact 0.0 is pre-allocated padding, not a coordinate (a real f64
            # coord being exactly zero is astronomically unlikely) -- excluding
            # it stops the giant zero-capacity arrays from forming one block.
            ok = np.isfinite(fl) & (np.abs(fl) < coord_max) & (fl != 0.0)
            i = 0
            while i + 3 <= n:
                if ok[i] and ok[i + 1] and ok[i + 2]:
                    j = i
                    while j + 3 <= n and ok[j] and ok[j + 1] and ok[j + 2]:
                        j += 3
                    m = (j - i) // 3
                    if m >= min_pts:
                        blk = fl[i:i + m * 3].reshape(m, 3)
                        xs, zs = blk[:, 0], blk[:, 2]
                        mag = np.median(np.abs(blk))
                        # accept only room-scale point clouds with real spread;
                        # rejects covariances (tiny mag), pose/gravity blocks
                        # (near-constant), and residual arrays. HEURISTIC -- an
                        # exact set needs the GraphNodeSoA walk (see paired()).
                        if (0.1 < mag < coord_max and
                                (xs.max() - xs.min()) > 0.3 and
                                (zs.max() - zs.min()) > 0.3):
                            blocks.append(blk)
                    i = j
                else:
                    i += 1
        return blocks

    # ---- map points, by their real signature (not the heuristic scan) ----
    def map_points(self, min_pts: int = 24, coord_max: float = 20.0,
                   max_ystd: float = 0.6) -> list[np.ndarray]:
        """The actual MapPoint position runs.

        Positions are NOT in the SoA field-base table (verified: the
        hand-checked ground-truth points occur exactly once across every arena,
        outside every table field). They are stored as contiguous f64x3 runs,
        identified by the GRAVITY SIGNATURE that no other float array has: the
        map is gravity-aligned, so vertical spread is markedly smaller than
        horizontal (Ystd < 0.7*min(Xstd, Zstd)). Validated against the
        hand-verified block: recovers it exactly, plus the other node runs."""
        out = []
        for path, _ in self.arenas:
            d = self._load(path)
            n = len(d) // 8
            fl = np.frombuffer(d[:n * 8].tobytes(), dtype="<f8")
            ok = np.isfinite(fl) & (np.abs(fl) < coord_max) & (np.abs(fl) > 1e-9)
            i = 0
            while i < n - 3:
                if not ok[i]:
                    i += 1
                    continue
                j = i
                while j < n and ok[j]:
                    j += 1
                m = (j - i) // 3
                if m >= min_pts:
                    p = fl[i:i + m * 3].reshape(m, 3)
                    xs, ys, zs = p[:, 0].std(), p[:, 1].std(), p[:, 2].std()
                    if (0.02 < ys < max_ystd and xs > 0.4 and zs > 0.4
                            and ys < 0.7 * min(xs, zs)
                            and not _is_direction_set(p)):
                        out.append(p)
                i = j
        return out

    # ---- point<->descriptor pairs: SOLVED, but not from memory ----
    def paired(self) -> tuple[np.ndarray, np.ndarray]:
        """Superseded. Use `mapdata.Map(dir).paired()` on a PERSISTED mapdb.

        This was going to need a GraphNodeSoA walk to rebuild the
        `MapPoint <- PointObservation -> Descriptor` chain out of raw heap. It
        never had to be: when trackingservice persists the map it writes each
        point's descriptors INSIDE the point record, so the pairing that is
        implicit in memory is explicit on disk. Pull with
        `Device.pull_mapdb()`, decode with `mapdata.Map()`."""
        raise NotImplementedError(
            "pairing is solved on the persisted map, not in memory: "
            "Device.pull_mapdb() then mapdata.Map(dir).paired(). "
            "See docs/insight-mapdata-format.md.")


def _is_direction_set(p: np.ndarray, unit_frac: float = 0.15) -> bool:
    """True if the run is unit vectors (bearings/normals), not positions.

    Caught by eye first: a run rendered as a clean SEMICIRCLE, and its radii sat
    at exactly 1.000. Horizontal unit vectors pass the gravity test (small Y
    spread, wide X/Z), so the gravity signature alone is not sufficient. Real
    map points have no reason to cluster on the unit sphere."""
    r = np.linalg.norm(p, axis=1)
    if np.median(r) < 1e-6:
        return True
    on_unit = ((r > 0.95) & (r < 1.05)).mean()
    return on_unit >= unit_frac


# ------------------------------------------------------------ SoA handles
# The map's arrays are NOT std::vectors -- they are
# perception::VersionedSoABase<...SoA, CompressedSlotBookkeeper> allocations
# addressed by packed handles, which is why pointer-pair scanning never finds
# them. The binary names the layout outright:
#     CompactVersionedHandleImpl<MapPointSoA, false, 21, 10>
# i.e. a 32-bit word = 21-bit slot INDEX | 10-bit VERSION (<<21). Verified in
# a live dump: dense arrays of words like 0x0020009b decode to index 155,
# version 1; version 0 words are plain indices.
HANDLE_IDX_BITS = 21
HANDLE_VER_BITS = 10

# The SoA arrays the map is built from (from the binary's type names). The
# point<->descriptor link runs MapPoint <- PointObservation -> Descriptor.
SOA_TYPES = (
    "MapPointSoA", "PointObservationSoA", "DescriptorSoA", "KeyRigSoA",
    "GraphNodeSoA", "GraphEdgeSoA", "PointResidualSoA", "ImagePatchSoA",
    "PlaceDescriptorSoA", "AnchorSoA", "KeyImageSoA", "GeoInfoSoA",
    "EnergyEdgeSoA", "MargEnergyEdgeSoA", "IMUEnergyEdgeSoA",
    "WifiMeasurementSoA",
)


def decode_handles(words: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Split packed 32-bit SoA handles into (index, version) arrays."""
    idx = words & ((1 << HANDLE_IDX_BITS) - 1)
    ver = (words >> HANDLE_IDX_BITS) & ((1 << HANDLE_VER_BITS) - 1)
    return idx, ver


def find_handle_arrays(buf: np.ndarray, capacity: int = 8100,
                       min_run: int = 16, max_version: int = 64):
    """Locate dense runs of valid packed handles in a dump buffer.

    Returns [(byte_offset, count, idx_array, ver_array)]. A run is the SoA's
    index array (e.g. observations -> map points); resolving it to values still
    needs the target array's base, which VersionedSoABase holds per field."""
    n = len(buf) // 4
    w = np.frombuffer(buf[:n * 4].tobytes(), dtype="<u4")
    idx, ver = decode_handles(w)
    ok = (idx > 0) & (idx < capacity) & (ver < max_version)
    runs = []
    i = 0
    while i < len(ok):
        if ok[i]:
            j = i
            while j < len(ok) and ok[j]:
                j += 1
            if j - i >= min_run:
                runs.append((i * 4, j - i, idx[i:j].copy(), ver[i:j].copy()))
            i = j
        else:
            i += 1
    return runs


# --------------------------------------------------------- matching / align
def hamming_nn(A: np.ndarray, B: np.ndarray, sample: int | None = None,
               seed: int = 0) -> tuple[np.ndarray, np.ndarray]:
    """For each (sampled) row of A, the min Hamming distance to B and its index.
    A,B are (n,32) uint8. Chunked to bound memory."""
    Ab = np.unpackbits(A, axis=1).astype(np.int16)
    Bb = np.unpackbits(B, axis=1).astype(np.int16)
    if sample and len(Ab) > sample:
        idx = np.random.RandomState(seed).choice(len(Ab), sample, replace=False)
        Ab = Ab[idx]
    mins = np.empty(len(Ab), np.int32)
    args = np.empty(len(Ab), np.int32)
    for i in range(0, len(Ab), 128):
        dd = (Ab[i:i + 128, None, :] != Bb[None, :, :]).sum(2)
        mins[i:i + 128] = dd.min(1)
        args[i:i + 128] = dd.argmin(1)
    return mins, args


def cross_match_report(descA: np.ndarray, descB: np.ndarray, sample: int = 3000) -> dict:
    """The validated cross-device descriptor test, with the shuffle control that
    distinguishes real correspondence from the shared-library artifact. A real
    match population sits well below the shuffle noise floor (~80+)."""
    mins, _ = hamming_nn(descA, descB, sample=sample)
    Bb = np.unpackbits(descB, axis=1)
    rng = np.random.RandomState(3)
    Bs = Bb.copy()
    for row in Bs:
        rng.shuffle(row)
    Bs = np.packbits(Bs, axis=1)
    mr, _ = hamming_nn(descA, Bs, sample=sample)
    return {
        "n_A": len(descA), "n_B": len(descB),
        "match_min": int(mins.min()), "match_p1": float(np.percentile(mins, 1)),
        "match_p5": float(np.percentile(mins, 5)), "match_median": float(np.median(mins)),
        "matches_lt_48": int((mins < 48).sum()),
        "shuffle_min": int(mr.min()), "shuffle_median": float(np.median(mr)),
        "verdict": "cross-match" if mins.min() < np.percentile(mr, 1) - 10 else "no signal",
    }


def kabsch(P: np.ndarray, Q: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Rigid transform (R, t) mapping P onto Q (both Nx3), least-squares."""
    cP, cQ = P.mean(0), Q.mean(0)
    H = (P - cP).T @ (Q - cQ)
    U, _, Vt = np.linalg.svd(H)
    D = np.eye(3)
    D[2, 2] = np.sign(np.linalg.det(Vt.T @ U.T))
    R = Vt.T @ D @ U.T
    return R, cQ - R @ cP


def align_ransac(pairs_P: np.ndarray, pairs_Q: np.ndarray, iters: int = 2000,
                 thresh: float = 0.1, seed: int = 0):
    """RANSAC rigid alignment over putative 3D correspondences (pairs_P[i] in
    frame A <-> pairs_Q[i] in frame B). Returns (R, t, inlier_mask) or None.
    Feed it descriptor-matched point pairs once paired() (or a graph walk)
    provides them."""
    n = len(pairs_P)
    if n < 3:
        return None
    rng = np.random.RandomState(seed)
    best = None
    for _ in range(iters):
        s = rng.choice(n, 3, replace=False)
        try:
            R, t = kabsch(pairs_P[s], pairs_Q[s])
        except np.linalg.LinAlgError:
            continue
        err = np.linalg.norm((pairs_P @ R.T + t) - pairs_Q, axis=1)
        inl = err < thresh
        if best is None or inl.sum() > best[2].sum():
            best = (R, t, inl)
    if best and best[2].sum() >= 4:
        R, t = kabsch(pairs_P[best[2]], pairs_Q[best[2]])  # refit on inliers
        return R, t, best[2]
    return None
