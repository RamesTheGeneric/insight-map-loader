#!/usr/bin/env python3
"""mapdata -- decode Insight's PERSISTED map files (/vision/insideout/mapdb).

This bypasses the whole memory-forensics track. When trackingservice persists a
map it writes Thrift Compact records, one file per record:

    nd_<node-uuid>_<K>.mapdata   node data, K = record kind (1,2,3,4,6,9,11,12)
    ad_<anchor-uuid>.mapdata     anchor data

The dialect is *Facebook* Thrift compact, i.e. Apache compact plus wire type
13 = FLOAT (4 bytes, big-endian). That extension is what makes the files look
undecodable to a stock Thrift reader: poses are float32, not double.

Hand-verified on nd_..._3.mapdata (50 bytes):
    19 f3 10 <16 byte uuid>   field 1: list<i8> = the node UUID
    19 7d <28 bytes>          field 1: list<float> x7 = quat(xyzw) + t(xyz)
    00                        stop
the quaternion reads unit-norm to 1e-6, which is what pinned type 13 = float
and big-endian byte order.

Usage:
    ./mapdata.py <mapdb-dir>              # summarize every record
    ./mapdata.py <mapdb-dir> --kind 2 --tree --max-depth 4
"""
import argparse
import glob
import os
import struct
import uuid as _uuid

import numpy as np

# fbthrift compact wire types.
STOP, BOOL_T, BOOL_F, I8, I16, I32, I64, DOUBLE, BINARY, LIST, SET, MAP, STRUCT, FLOAT = range(14)

TYPE_NAME = {
    BOOL_T: "bool", BOOL_F: "bool", I8: "i8", I16: "i16", I32: "i32",
    I64: "i64", DOUBLE: "double", BINARY: "binary", LIST: "list",
    SET: "set", MAP: "map", STRUCT: "struct", FLOAT: "float",
}


class Reader:
    """Thrift compact reader. Returns plain Python values; a struct becomes a
    dict {field_id: value}, so no .thrift schema is needed to walk the tree."""

    def __init__(self, buf: bytes):
        self.b = buf
        self.i = 0

    # ---- primitives
    def u8(self) -> int:
        v = self.b[self.i]
        self.i += 1
        return v

    def varint(self) -> int:
        v = shift = 0
        while True:
            byte = self.u8()
            v |= (byte & 0x7F) << shift
            if not byte & 0x80:
                return v
            shift += 7
            if shift > 63:
                raise ValueError("varint too long")

    def zigzag(self) -> int:
        n = self.varint()
        return (n >> 1) ^ -(n & 1)

    def binary(self) -> bytes:
        n = self.varint()
        v = self.b[self.i:self.i + n]
        self.i += n
        return v

    def value(self, t: int):
        if t in (BOOL_T, BOOL_F):
            return t == BOOL_T
        if t == I8:
            return self.u8()
        if t in (I16, I32, I64):
            return self.zigzag()
        if t == DOUBLE:
            v = struct.unpack_from("<d", self.b, self.i)[0]
            self.i += 8
            return v
        if t == FLOAT:                      # fb extension, BIG-endian
            v = struct.unpack_from(">f", self.b, self.i)[0]
            self.i += 4
            return v
        if t == BINARY:
            return self.binary()
        if t in (LIST, SET):
            return self.list()
        if t == MAP:
            return self.map()
        if t == STRUCT:
            return self.struct()
        raise ValueError(f"bad wire type {t} at {self.i}")

    def list(self) -> list:
        h = self.u8()
        n, et = h >> 4, h & 0x0F
        if n == 15:
            n = self.varint()
        # Fast paths: these dominate the file and matter for 1 MB records.
        if et == I8:
            v = self.b[self.i:self.i + n]
            self.i += n
            return v                        # bytes: uuids, descriptors
        if et == FLOAT:
            v = np.frombuffer(self.b, dtype=">f4", count=n, offset=self.i)
            self.i += 4 * n
            return v.astype(np.float32)
        if et == DOUBLE:
            v = np.frombuffer(self.b, dtype="<f8", count=n, offset=self.i)
            self.i += 8 * n
            return v.copy()
        return [self.value(et) for _ in range(n)]

    def map(self) -> dict:
        n = self.varint()
        if n == 0:
            return {}
        h = self.u8()
        kt, vt = h >> 4, h & 0x0F
        out = {}
        for _ in range(n):
            k = self.value(kt)
            out[k if not isinstance(k, (bytes, bytearray)) else bytes(k)] = self.value(vt)
        return out

    def struct(self) -> dict:
        out, fid = {}, 0
        while True:
            h = self.u8()
            t = h & 0x0F
            if t == STOP:
                return out
            delta = h >> 4
            fid = fid + delta if delta else self.zigzag()
            out[fid] = self.value(t)


def load(path: str) -> dict:
    """Decode one .mapdata file. The files are a bare sequence of fields (an
    unwrapped struct body), so read them exactly like a struct."""
    with open(path, "rb") as f:
        return Reader(f.read()).struct()


def as_uuid(b) -> str:
    return str(_uuid.UUID(bytes=bytes(b))) if b is not None and len(b) == 16 else repr(b)


# --------------------------------------------------------------- map decode
# Record kinds, by observation of a live mapdb:
#   1  graph: node uuid, parent, neighbour poses, gravity, keyrigs, IMU states
#   2  MAP POINTS: positions + descriptors + observations + image patches
#   3  T_root<-node: {1: root uuid, 2: pose}
#   4  (empty lists), 6/11  root-node bookkeeping, 9  sibling poses, 12 (empty)
KIND_GRAPH, KIND_POINTS, KIND_POSE = 1, 2, 3

# Point struct fields inside record kind 2, field 3.
PT_POS, PT_DESC, PT_SCORE, PT_INFO = 1, 3, 4, 7


def quat_to_R(q) -> np.ndarray:
    """Rotation from a stored pose quaternion. Order is (x, y, z, w) -- pinned
    empirically: the alternative (w,x,y,z) reads 4x worse on the cross-node
    overlap test below."""
    x, y, z, w = [float(v) for v in q[:4]]
    n = (x * x + y * y + z * z + w * w) ** 0.5
    if n == 0:
        return np.eye(3)
    x, y, z, w = x / n, y / n, z / n, w / n
    return np.array([
        [1 - 2 * (y * y + z * z), 2 * (x * y - z * w), 2 * (x * z + y * w)],
        [2 * (x * y + z * w), 1 - 2 * (x * x + z * z), 2 * (y * z - x * w)],
        [2 * (x * z - y * w), 2 * (y * z + x * w), 1 - 2 * (x * x + y * y)],
    ])


def to_cartesian(a: np.ndarray) -> np.ndarray:
    """Stored map-point coordinates -> Cartesian metres in the node frame.

    Storage is (azimuth, elevation, INVERSE DEPTH), not xyz -- the giveaway is
    that column 0 saturates at exactly +-pi on every node while columns 1 and 2
    never do. Frame is Y-up with azimuth measured from +Z toward +X.

    All three choices (inverse depth, quaternion order, pose direction) were
    fixed by one empirical test with a sharp optimum: transform all nodes into
    the root frame and measure cross-node nearest-neighbour overlap. This
    convention gives 0.28 m median / 26% within 0.15 m; every one of the 47
    alternatives tried lands at 1.2-2.3 m and under 3%."""
    az, el, inv_d = a[:, 0], a[:, 1], a[:, 2]
    r = 1.0 / np.clip(inv_d, 1e-3, None)
    ce = np.cos(el)
    return np.stack([r * ce * np.sin(az), r * np.sin(el), r * ce * np.cos(az)], 1)


class NodeMap:
    """One L1 node's decoded map: points, descriptors and its root-frame pose."""

    def __init__(self, mapdb: str, node: str):
        self.node = node
        self.mapdb = mapdb
        self._pts = load(os.path.join(mapdb, f"nd_{node}_{KIND_POINTS}.mapdata"))[3]
        pose_rec = load(os.path.join(mapdb, f"nd_{node}_{KIND_POSE}.mapdata"))
        self.root = as_uuid(pose_rec[1])
        self.pose = np.asarray(pose_rec[2], float)          # quat xyzw + t

    def __len__(self) -> int:
        return len(self._pts)

    def _keep(self, max_range: float) -> np.ndarray:
        """Drop points at (near-)infinite depth. Inverse depth goes to zero for
        a bearing that never triangulated, which blows the position up to
        hundreds of metres; such points carry no translation information. On a
        real map this discards well under 1% (7 of 1427 at the 8 m default)."""
        inv = np.array([p[PT_POS][2] for p in self._pts], dtype=np.float64)
        return inv > 1.0 / max_range

    def points(self, root_frame: bool = True, max_range: float = 8.0) -> np.ndarray:
        """Nx3 metres. In the ROOT frame by default, so nodes are comparable —
        the node-local frames must never simply be stacked."""
        a = np.array([p[PT_POS] for p in self._pts], dtype=np.float64)
        P = to_cartesian(a)[self._keep(max_range)]
        if not root_frame:
            return P
        return P @ quat_to_R(self.pose[:4]).T + self.pose[4:7]

    def paired(self, root_frame: bool = True,
               max_range: float = 8.0) -> tuple[np.ndarray, np.ndarray]:
        """(points Nx3, descriptors Nx32) index-aligned — one row per
        OBSERVATION, so a point seen k times contributes k rows sharing its
        position. This is what the memory-forensics track could never get: the
        descriptors are stored inside the point struct, so the pairing is free.
        """
        P = self.points(root_frame, max_range)
        keep = self._keep(max_range)
        pts, desc = [], []
        for xyz, p in zip(P, (p for p, k in zip(self._pts, keep) if k)):
            for obs in p.get(PT_DESC, []):
                d = obs.get(1)
                if d is not None and len(d) == 32:
                    pts.append(xyz)
                    desc.append(np.frombuffer(bytes(d), dtype=np.uint8))
        if not pts:
            return np.zeros((0, 3)), np.zeros((0, 32), np.uint8)
        return np.array(pts), np.array(desc)


class Map:
    """A whole mapdb directory: every L1 node, resolved into the root frame."""

    def __init__(self, mapdb: str):
        self.mapdb = mapdb
        seen = []
        for path in sorted(glob.glob(os.path.join(mapdb, f"nd_*_{KIND_POINTS}.mapdata"))):
            node = os.path.basename(path)[3:].rsplit("_", 1)[0]
            if os.path.exists(os.path.join(mapdb, f"nd_{node}_{KIND_POSE}.mapdata")):
                seen.append(node)
        self.nodes = [NodeMap(mapdb, n) for n in seen]

    def points(self, max_range: float = 8.0) -> np.ndarray:
        Ps = [n.points(max_range=max_range) for n in self.nodes]
        return np.vstack(Ps) if Ps else np.zeros((0, 3))

    def paired(self, max_range: float = 8.0) -> tuple[np.ndarray, np.ndarray]:
        got = [n.paired(max_range=max_range) for n in self.nodes]
        got = [g for g in got if len(g[0])]
        if not got:
            return np.zeros((0, 3)), np.zeros((0, 32), np.uint8)
        return np.vstack([g[0] for g in got]), np.vstack([g[1] for g in got])


def kind_of(path: str) -> int | None:
    stem = os.path.basename(path).rsplit(".", 1)[0]
    tail = stem.rsplit("_", 1)[-1]
    return int(tail) if tail.isdigit() else None


def records(mapdb: str) -> list[tuple[str, int | None, str]]:
    """[(path, kind, node_or_anchor_uuid)] for every file in a mapdb dir."""
    out = []
    for p in sorted(glob.glob(os.path.join(mapdb, "*.mapdata"))):
        stem = os.path.basename(p).rsplit(".", 1)[0]
        body = stem.split("_", 1)[1]
        uid = body.rsplit("_", 1)[0] if kind_of(p) is not None else body
        out.append((p, kind_of(p), uid))
    return out


# ------------------------------------------------------------------ display
def describe(v, depth: int, max_depth: int) -> str:
    if isinstance(v, (bytes, bytearray)):
        if len(v) == 16:
            return f"uuid({as_uuid(v)})"
        return f"bytes[{len(v)}] {bytes(v[:12]).hex()}{'..' if len(v) > 12 else ''}"
    if isinstance(v, np.ndarray):
        s = f"{v.dtype}[{len(v)}]"
        if len(v) <= 8:
            s += " " + np.array2string(v, precision=4, suppress_small=True)
        else:
            s += f" min {v.min():.3f} max {v.max():.3f}"
        return s
    if isinstance(v, dict):
        if depth >= max_depth:
            return f"{{{len(v)} fields}}"
        inner = ", ".join(f"{k}={describe(x, depth + 1, max_depth)}" for k, x in v.items())
        return "{" + inner + "}"
    if isinstance(v, list):
        if depth >= max_depth:
            return f"list[{len(v)}]"
        head = ", ".join(describe(x, depth + 1, max_depth) for x in v[:3])
        return f"list[{len(v)}]({head}{', ...' if len(v) > 3 else ''})"
    if isinstance(v, float):
        return f"{v:.5f}"
    return str(v)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mapdb", help="directory of pulled .mapdata files")
    ap.add_argument("--kind", type=int, help="only this record kind")
    ap.add_argument("--node", help="only this node/anchor uuid prefix")
    ap.add_argument("--tree", action="store_true", help="print the decoded tree")
    ap.add_argument("--max-depth", type=int, default=3)
    args = ap.parse_args()

    for path, kind, uid in records(args.mapdb):
        if args.kind is not None and kind != args.kind:
            continue
        if args.node and not uid.startswith(args.node):
            continue
        size = os.path.getsize(path)
        try:
            rec = load(path)
            status = f"fields {sorted(rec)}"
        except Exception as e:                       # noqa: BLE001 - report, keep going
            rec, status = None, f"DECODE FAILED: {e}"
        print(f"{os.path.basename(path):<58} {size:>9} B  kind={kind}  {status}")
        if args.tree and rec is not None:
            for fid, v in rec.items():
                print(f"    {fid}: {describe(v, 1, args.max_depth)}")


if __name__ == "__main__":
    raise SystemExit(main())
