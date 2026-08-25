#!/usr/bin/env python3
"""selftest_align -- validate the map matcher against known ground truth.

Cross-device alignment is hard to check: two pucks give two maps and no truth
to compare against. But ONE puck's map already contains the test. Its L1 nodes
are independent observations of the same room, captured at different times from
different viewpoints -- exactly the cross-device situation -- and record kind 3
places every node in the root frame. So once both nodes are put in the root
frame, **the correct answer for every node pair is the identity**, and any
error the pipeline makes shows up as a departure from it.

That covers the whole chain at once: the (azimuth, elevation, inverse-depth)
decode, the node pose composition, descriptor matching, and the robust solve.

    ./selftest_align.py /tmp/mapdb108

Test 1 (matcher):  a node matched against ITSELF must return exact identity.
Test 2 (pipeline): every node PAIR must return identity, or be honestly
                   rejected as insufficient overlap.
"""
import argparse
import itertools
import sys

import numpy as np

import mapdata as md
import matchmap as mm

# Tolerances: what the pipeline actually achieves on .108, with headroom.
YAW_TOL_DEG = 5.0
T_TOL_M = 0.35


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mapdb")
    ap.add_argument("--thr", type=float, default=0.25, help="RANSAC inlier threshold (m)")
    ap.add_argument("--full-6dof", action="store_true",
                    help="solve 6-DoF instead of gravity-constrained 4-DoF")
    args = ap.parse_args()

    m = md.Map(args.mapdb)
    if len(m.nodes) < 2:
        print(f"need >=2 nodes, found {len(m.nodes)} — has this puck persisted a real map?")
        return 1
    B = {n.node: mm.bundle(n) for n in m.nodes}
    print(f"{len(m.nodes)} nodes, {sum(len(n) for n in m.nodes)} points\n")

    # ---- Test 1: the matcher itself
    node = max(m.nodes, key=len)
    P, D = mm.bundle(node, root_frame=False)
    r = mm.align(P, D, P, D)
    exact = (r["pairs"][:, 0] == r["pairs"][:, 1]).mean() if len(r["pairs"]) else 0.0
    t1 = (r["inliers"] == len(P) and r["angle_deg"] < 1e-6 and exact == 1.0)
    print(f"TEST 1 self-match {node.node[:8]}: {r['inliers']}/{len(P)} inliers, "
          f"angle {r['angle_deg']:.4f} deg, |t| {np.linalg.norm(r['t']):.4f} m, "
          f"correct pairs {100*exact:.1f}%  -> {'PASS' if t1 else 'FAIL'}\n")

    # ---- Test 2: every node pair, ground truth = identity
    print("TEST 2 pairwise, ground truth = identity")
    print(f"{'pair':<21} {'match':>6} {'inl':>4} {'%':>5} {'res':>6} "
          f"{'yaw':>7} {'|t|':>6} {'spread':>7}  verdict")
    npass = nrej = nfail = 0
    for a, b in itertools.combinations(list(B), 2):
        (Pa, Da), (Pb, Db) = B[a], B[b]
        r = mm.align_stable(Pa, Da, Pb, Db, thr=args.thr, yaw_only=not args.full_6dof)
        if not r.get("ok"):
            print(f"{a[:8]}<-{b[:8]}      {r.get('n_matches',0):6d} "
                  f"{r.get('inliers',0):4} {'':5} {'':6} {'':7} {'':6} "
                  f"{r.get('yaw_spread_deg',float('nan')):7.1f}  rejected: {r.get('reason')}")
            nrej += 1
            continue
        tn = float(np.linalg.norm(r["t"]))
        good = abs(r["yaw_deg"]) <= YAW_TOL_DEG and tn <= T_TOL_M
        print(f"{a[:8]}<-{b[:8]}      {r['n_matches']:6d} {r['inliers']:4d} "
              f"{100*r['inlier_frac']:5.1f} {r['residual_m']:6.3f} "
              f"{r['yaw_deg']:7.2f} {tn:6.3f} {r['yaw_spread_deg']:7.2f}  "
              f"{'PASS' if good else 'FAIL (should be identity)'}")
        npass += good
        nfail += not good

    print(f"\nidentity recovered {npass}, honestly rejected {nrej}, WRONG {nfail}")
    ok = t1 and nfail == 0 and npass >= 2
    print("SELFTEST", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
