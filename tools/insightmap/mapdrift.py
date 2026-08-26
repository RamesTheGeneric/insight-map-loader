#!/usr/bin/env python3
"""mapdrift -- how far two pucks' maps have drifted apart, and how much to trust it.

Two pucks seeded from one map do NOT stay identical. Each keeps mapping into its
own copy, and Insight also *prunes*: a map observed here went 1269 -> 835 points
over a few hours without the puck leaving the room. So divergence accumulates
in both directions, and the question a correction loop has to answer is not
"are they different" (always yes) but "by how much, and is that measurable".

This measures it, map against map, with no device involved beyond the pull. Both
maps carry Insight's own deep descriptors, so matching its features against its
features needs no model of ours -- which is exactly why this works where
matching live camera frames to a map does not.

    ./mapdrift.py <mapdbA> <mapdbB>
    ./mapdrift.py <mapdbA> <mapdbA>      # control: must print exact identity

Read the output in this order:

  inliers      the confidence signal. Measured against injected transforms it
               holds at 97-100% out to 2 deg and collapses to 28% by 5 deg, so
               a low fraction means "offset too large to trust", not "no offset".
  yaw          trustworthy. Recovered EXACTLY (0.000 deg error) at every offset
               tested, even where the inlier fraction had already collapsed.
  translation  advisory only. Recovery error runs ~2-8 cm in the 0.5-2 deg
               regime and 25 cm by 5 deg, so a translation under ~10 cm is not
               distinguishable from zero. Do not act on small ones.
"""
import math
import sys
import time

import numpy as np

import mapdata as md
import matchmap as mm


def regroup(pts: np.ndarray, desc: np.ndarray):
    """(observation points, observation descriptors) -> (unique points, [[desc], ...]).

    `Map.paired()` emits one row per OBSERVATION so a point seen k times appears
    k times, while `align_stable` wants unique points plus their descriptor
    lists. Rounding before uniquing only collapses true duplicates: within one
    map a landmark resolves to bit-identical coordinates every time.
    """
    key = np.round(pts, 6)
    uniq, inv = np.unique(key, axis=0, return_inverse=True)
    per = [[] for _ in range(len(uniq))]
    for j, i in enumerate(inv):
        per[i].append(desc[j])
    return uniq, per


def drift(a_dir: str, b_dir: str) -> dict:
    """Solve the transform placing map B in map A's frame."""
    pa, da = regroup(*md.Map(a_dir).paired())
    pb, db = regroup(*md.Map(b_dir).paired())
    r = mm.align_stable(pa, da, pb, db, yaw_only=True)
    r["n_points_a"], r["n_points_b"] = len(pa), len(pb)
    return r


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    t0 = time.time()
    r = drift(sys.argv[1], sys.argv[2])
    dt = time.time() - t0

    print(f"points   A {r['n_points_a']}   B {r['n_points_b']}")
    if not r.get("ok"):
        print(f"REFUSED: {r.get('reason')}  "
              f"matches={r.get('n_matches')} inliers={r.get('inliers')}")
        print("  -> hold the previous correction; a wrong one is worse than a stale one.")
        return 1

    t = r["t"]
    horiz = math.hypot(t[0], t[2])
    print(f"inliers      {r['inliers']}/{r['n_matches']} ({100*r['inlier_frac']:.1f}%)"
          f"   {'trustworthy' if r['inlier_frac'] > 0.5 else 'LOW — offset may exceed range'}")
    print(f"yaw          {r['yaw_deg']:+.3f} deg      (exact to 0.000 deg in test)")
    print(f"translation  {horiz:.3f} m horizontal  "
          f"({'below the ~0.10 m noise floor — ignore' if horiz < 0.10 else 'above the noise floor'})")
    print(f"residual     {r['residual_m']:.3f} m")
    print(f"stability    yaw spread {r['yaw_spread_deg']:.2f} deg, "
          f"t spread {r['t_spread_m']:.3f} m")
    print(f"compute      {dt:.1f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
