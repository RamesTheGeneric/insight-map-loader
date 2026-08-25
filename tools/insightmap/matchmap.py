#!/usr/bin/env python3
"""matchmap -- descriptor correspondences and rigid alignment between two maps.

Consumes what `mapdata` decodes: 3D points each carrying their own 256-bit
descriptors. Produces a rigid transform placing map B in map A's frame.

Two details matter and both were learned by getting them wrong first:

1. **A point has MANY descriptors** (one per observation) and they are near
   duplicates. A textbook Lowe ratio test then compares a descriptor against
   another view of the SAME point, so the ratio is ~1 and every match is
   thrown away. The ratio must be taken against the best match belonging to a
   DIFFERENT point. `match_points()` does that.
2. **Matching is at descriptor level, deciding is at point level.** Individual
   observations match noisily, so votes are accumulated per (point_a, point_b)
   pair and the pair is judged on its best distance plus its vote count.

Alignment is 6-DoF by default (Kabsch in RANSAC). Gravity-constrained 4-DoF
(yaw + translation) is available via `yaw_only=True` for the cross-device case,
where both Insight world frames are gravity-aligned and roll/pitch is forbidden.
"""
import numpy as np

# popcount LUT: Hamming distance over uint8 rows without unpacking to bits.
_POP = np.unpackbits(np.arange(256, dtype=np.uint8)[:, None], axis=1).sum(1).astype(np.int32)

MAX_DESC_PER_POINT = 8


def flatten(node_pts: np.ndarray, per_point_desc: list) -> tuple[np.ndarray, np.ndarray]:
    """(descriptors Mx32, point index M) capped at MAX_DESC_PER_POINT per point.

    The cap is for cost, and it is spread across the observation list rather
    than taking the first k, because consecutive observations come from nearby
    keyrigs and would be the most redundant possible choice."""
    D, idx = [], []
    for i, ds in enumerate(per_point_desc):
        if len(ds) > MAX_DESC_PER_POINT:
            sel = np.linspace(0, len(ds) - 1, MAX_DESC_PER_POINT).astype(int)
            ds = [ds[j] for j in sel]
        for d in ds:
            D.append(d)
            idx.append(i)
    if not D:
        return np.zeros((0, 32), np.uint8), np.zeros(0, int)
    return np.array(D, dtype=np.uint8), np.array(idx)


def hamming(A: np.ndarray, B: np.ndarray, chunk: int = 256):
    """AxB Hamming distances, chunked so the XOR intermediate stays bounded."""
    out = np.empty((len(A), len(B)), dtype=np.int32)
    for i in range(0, len(A), chunk):
        blk = A[i:i + chunk]
        out[i:i + chunk] = _POP[np.bitwise_xor(blk[:, None, :], B[None, :, :])].sum(2)
    return out


def match_points(dA, iA, dB, iB, ratio: float = 0.9, max_dist: int = 110,
                 min_votes: int = 1):
    """Descriptor matching -> point correspondences.

    Returns (pairs Kx2 point indices, best distance per pair, votes per pair).
    The ratio test is taken against the best match on a DIFFERENT point (see
    module docstring); with per-point descriptor sets it is otherwise inert."""
    if not len(dA) or not len(dB):
        return np.zeros((0, 2), int), np.zeros(0), np.zeros(0, int)
    d = hamming(dA, dB)
    best = d.argmin(1)
    best_d = d[np.arange(len(dA)), best]
    best_pt = iB[best]
    # best distance to any descriptor NOT on the winning point
    masked = np.where(iB[None, :] == best_pt[:, None], np.iinfo(np.int32).max, d)
    second_d = masked.min(1)
    ok = (best_d <= max_dist) & (best_d < ratio * second_d)

    votes: dict[tuple[int, int], list] = {}
    for a, b, dist in zip(iA[ok], best_pt[ok], best_d[ok]):
        k = (int(a), int(b))
        v = votes.get(k)
        if v is None:
            votes[k] = [1, int(dist)]
        else:
            v[0] += 1
            v[1] = min(v[1], int(dist))
    if not votes:
        return np.zeros((0, 2), int), np.zeros(0), np.zeros(0, int)
    pairs = np.array([k for k in votes])
    cnt = np.array([votes[tuple(k)][0] for k in pairs])
    dist = np.array([votes[tuple(k)][1] for k in pairs])
    keep = cnt >= min_votes
    return pairs[keep], dist[keep], cnt[keep]


# ------------------------------------------------------------------ solvers
def kabsch(P: np.ndarray, Q: np.ndarray):
    """R,t minimising |P - (R Q + t)|."""
    cp, cq = P.mean(0), Q.mean(0)
    U, _, Vt = np.linalg.svd((Q - cq).T @ (P - cp))
    D = np.diag([1.0, 1.0, np.sign(np.linalg.det(U @ Vt))])
    R = U @ D @ Vt
    return R, cp - R @ cq


def yaw_kabsch(P: np.ndarray, Q: np.ndarray):
    """Gravity-constrained: rotation about +Y only, plus translation.

    Closed form -- the yaw that best aligns the horizontal components is the
    argument of sum(conj(q_xz) * p_xz) in the complex plane."""
    cp, cq = P.mean(0), Q.mean(0)
    p, q = P - cp, Q - cq
    zp = p[:, 0] + 1j * p[:, 2]
    zq = q[:, 0] + 1j * q[:, 2]
    th = np.angle(np.sum(np.conj(zq) * zp))
    c, s = np.cos(th), np.sin(th)
    R = np.array([[c, 0, s], [0, 1, 0], [-s, 0, c]])
    return R, cp - R @ cq


def ransac(P, Q, thr=0.15, iters=4000, yaw_only=False, seed=0, weights=None):
    """Robust fit of Q -> P. Returns (n_inliers, R, t, median_residual, mask).

    Minimal set is 2 points for yaw-only (4 DoF) and 3 for full 6-DoF."""
    solve = yaw_kabsch if yaw_only else kabsch
    k = 2 if yaw_only else 3
    n = len(P)
    best = (0, None, None, None, np.zeros(n, bool))
    if n < k + 1:
        return best
    rng = np.random.RandomState(seed)
    p = None if weights is None else weights / weights.sum()
    for _ in range(iters):
        s = rng.choice(n, k, replace=False, p=p)
        try:
            R, t = solve(P[s], Q[s])
        except np.linalg.LinAlgError:
            continue
        e = np.linalg.norm(Q @ R.T + t - P, axis=1)
        m = e < thr
        if m.sum() > best[0]:
            R2, t2 = solve(P[m], Q[m])
            e2 = np.linalg.norm(Q @ R2.T + t2 - P, axis=1)
            m2 = e2 < thr
            if m2.sum() >= m.sum():
                best = (int(m2.sum()), R2, t2, float(np.median(e2[m2])), m2)
            else:
                best = (int(m.sum()), R, t, float(np.median(e[m])), m)
    return best


def rotation_report(R: np.ndarray) -> dict:
    """Angle, axis, and how far the axis tilts off vertical -- the number that
    says whether a solved rotation respects gravity."""
    ang = np.degrees(np.arccos(np.clip((np.trace(R) - 1) / 2, -1, 1)))
    ax = np.array([R[2, 1] - R[1, 2], R[0, 2] - R[2, 0], R[1, 0] - R[0, 1]])
    n = np.linalg.norm(ax)
    ax = ax / n if n > 1e-9 else np.array([0.0, 1.0, 0.0])
    return {"angle_deg": float(ang), "axis": ax,
            "axis_tilt_from_Y_deg": float(np.degrees(np.arccos(np.clip(abs(ax[1]), -1, 1))))}


def yaw_deg(R: np.ndarray) -> float:
    """Yaw about +Y, in degrees."""
    return float(np.degrees(np.arctan2(R[0, 2], R[0, 0])))


def align(ptsA, descA, ptsB, descB, thr=0.15, yaw_only=False, **kw):
    """Full pipeline: two per-point descriptor sets -> transform placing B in A.

    ptsX: Nx3, descX: list of per-point lists of 32-byte descriptors."""
    dA, iA = flatten(ptsA, descA)
    dB, iB = flatten(ptsB, descB)
    pairs, dist, votes = match_points(dA, iA, dB, iB, **kw)
    if len(pairs) < 4:
        return {"ok": False, "reason": "insufficient matches", "n_matches": len(pairs)}
    P, Q = ptsA[pairs[:, 0]], ptsB[pairs[:, 1]]
    inl, R, t, res, mask = ransac(P, Q, thr=thr, yaw_only=yaw_only,
                                  weights=votes.astype(float))
    if R is None:
        return {"ok": False, "reason": "no model", "n_matches": len(pairs)}
    out = {"ok": inl >= 6, "n_matches": len(pairs), "inliers": inl,
           "residual_m": res, "R": R, "t": t, "pairs": pairs, "mask": mask}
    out.update(rotation_report(R))
    if not out["ok"]:
        out["reason"] = "too few inliers"
    return out


# Consensus gate. Inlier COUNT cannot separate a real solve from a spurious
# one here -- on real node pairs the good and the hopeless both sit around
# 4-13% inliers. Stability can: re-solving from different RANSAC seeds, a real
# overlap lands on the same transform every time while a spurious one wanders.
# Measured on .108's 15 node pairs: every true pair agreed to within 7.4 deg of
# yaw, and the one pair with no true correspondences spread over 350 deg.
YAW_SPREAD_MAX_DEG = 10.0
T_SPREAD_MAX_M = 1.0
MIN_INLIERS = 6


def align_stable(ptsA, descA, ptsB, descB, thr=0.25, yaw_only=True,
                 seeds: int = 5, **kw):
    """`align` plus the seed-consensus overlap gate.

    Returns the same dict with `ok`, plus `yaw_spread_deg` / `t_spread_m`. When
    `ok` is False the caller must HOLD its previous transform rather than use
    this one -- a wrong alignment is worse than a stale one."""
    dA, iA = flatten(ptsA, descA)
    dB, iB = flatten(ptsB, descB)
    pairs, dist, votes = match_points(dA, iA, dB, iB, **kw)
    if len(pairs) < MIN_INLIERS:
        return {"ok": False, "reason": "insufficient matches", "n_matches": len(pairs)}
    P, Q = ptsA[pairs[:, 0]], ptsB[pairs[:, 1]]
    w = votes.astype(float)
    sols = []
    for s in range(seeds):
        inl, R, t, res, mask = ransac(P, Q, thr=thr, yaw_only=yaw_only, seed=s, weights=w)
        if R is not None:
            sols.append((inl, R, t, res, mask))
    if not sols:
        return {"ok": False, "reason": "no model", "n_matches": len(pairs)}
    yaws = np.array([yaw_deg(s[1]) for s in sols])
    ts = np.array([s[2] for s in sols])
    # circular spread, so a pair straddling +-180 is not called unstable
    yspread = float(np.degrees(np.ptp(np.unwrap(np.radians(yaws)))))
    tspread = float(np.linalg.norm(ts.max(0) - ts.min(0)))
    inl, R, t, res, mask = max(sols, key=lambda s: s[0])
    out = {"n_matches": len(pairs), "inliers": inl, "residual_m": res,
           "R": R, "t": t, "pairs": pairs, "mask": mask,
           "yaw_deg": yaw_deg(R), "yaw_spread_deg": yspread, "t_spread_m": tspread,
           "inlier_frac": inl / len(pairs)}
    out.update(rotation_report(R))
    out["ok"] = (inl >= MIN_INLIERS and yspread <= YAW_SPREAD_MAX_DEG
                 and tspread <= T_SPREAD_MAX_M)
    if not out["ok"]:
        out["reason"] = ("unstable across seeds" if inl >= MIN_INLIERS
                         else "too few inliers")
    return out


def bundle(node, root_frame: bool = True, max_range: float = 8.0):
    """A `mapdata.NodeMap` -> (points Nx3, per-point descriptor lists), the
    input shape `align`/`align_stable` expect."""
    P = node.points(root_frame=root_frame, max_range=max_range)
    keep = node._keep(max_range)
    desc = [[np.frombuffer(bytes(o[1]), dtype=np.uint8)
             for o in p.get(3, []) if len(o.get(1, b"")) == 32]
            for p in (q for q, k in zip(node._pts, keep) if k)]
    return P, desc
