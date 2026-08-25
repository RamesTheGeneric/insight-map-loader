#!/usr/bin/env python3
"""Ground-truth alignment check: put two pucks together, measure the disagreement.

Physically stack or touch the pucks, then run this. Their reported map-frame
positions should then differ only by the physical offset between the two
headsets' Insight origins (~0.1-0.2 m). Anything substantially larger is
alignment error, and this prints it as a single number.

Why this is worth having: it is the only measurement in the system with real
ground truth. Every other quality signal -- solver residual, inlier count, the
on-puck verifier's median -- scores a transform against the MAP, so a map that
is itself distorted scores well while the pucks disagree with each other. Two
co-located pucks cannot lie to you.

It also localises the fault. The math here reads each puck's Insight WORLD pose
from dumpsys and applies only T_map_world, bypassing the bridge and the MPT1
stream entirely (same convention as Frame4Dof::apply_point). So:

    disagreement here      -> the fault is in T_map_world (localization)
    agreement here but not
    in the GUI             -> the fault is downstream: bridge or MPT1

    ./tools/q1sep.py                      # all pucks in transforms.json
    ./tools/q1sep.py --transforms X.json
"""
import argparse
import itertools
import json
import math
import re
import subprocess


def dumpsys_pose(ip):
    """Insight world position, or None if the puck is not tracking at 6DoF."""
    # Never pipe dumpsys into something that closes the pipe early -- it can
    # leave the tracking service unavailable for seconds. Dump, then grep.
    subprocess.run(["adb", "-s", f"{ip}:5555", "shell",
                    "dumpsys tracking > /data/local/tmp/qsep.txt 2>/dev/null"],
                   capture_output=True, timeout=30)
    o = subprocess.run(["adb", "-s", f"{ip}:5555", "shell",
                        "grep -A4 '  Hmd:' /data/local/tmp/qsep.txt"],
                       capture_output=True, timeout=30).stdout.decode()
    if "6DOF" not in o or "Valid: Yes" not in o:
        return None
    m = re.search(r"trans=\(([^)]*)\)", o)
    return [float(x) for x in m.group(1).split(",")] if m else None


def apply_4dof(p, yaw_deg, t):
    """Frame4Dof::apply_point -- yaw about the gravity-aligned Y, then translate."""
    y = math.radians(yaw_deg)
    c, s = math.cos(y), math.sin(y)
    return [c * p[0] + s * p[2] + t[0], p[1] + t[1], -s * p[0] + c * p[2] + t[2]]


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--transforms", default="transforms.json")
    ap.add_argument("--expect", type=float, default=0.2,
                    help="physical offset between the two Insight origins (m)")
    args = ap.parse_args()

    T = json.load(open(args.transforms))
    import time
    now = time.time()

    # The whole measurement rests on this and the tool cannot check it. Two
    # pucks resting 0.5 m apart on a desk produce exactly the same reading as
    # two touching pucks misaligned by 0.5 m, and a stable number across runs
    # is NOT evidence of co-location -- it just means nothing moved.
    print("ASSUMES the pucks are physically touching RIGHT NOW.")
    print("If they are not, this number is their real separation, not an error.\n")

    pos = {}
    for ip, tr in T.items():
        w = dumpsys_pose(ip)
        age = (now - tr["unix_time"]) / 60.0
        if w is None:
            print(f"  {ip}: NOT TRACKING (6DoF invalid) — skipped")
            continue
        pos[ip] = apply_4dof(w, tr["yaw_deg"], tr["t"])
        print(f"  {ip}: world={[round(x, 3) for x in w]} -> "
              f"map={[round(x, 3) for x in pos[ip]]}   "
              f"(transform {age:.0f} min old, residual {tr['residual_deg']:.2f}°)")

    if len(pos) < 2:
        print("\nneed two tracking pucks to compare")
        return 1

    print()
    worst = 0.0
    for (ia, a), (ib, b) in itertools.combinations(pos.items(), 2):
        d = math.dist(a, b)
        horiz = math.dist([a[0], a[2]], [b[0], b[2]])
        vert = abs(a[1] - b[1])
        worst = max(worst, d)
        print(f"  {ia} <-> {ib}: {d:.3f} m   (horizontal {horiz:.3f}, vertical {vert:.3f})")
        # 4-DoF cannot tilt, and Y is gravity-aligned, so a large VERTICAL
        # error means something other than the yaw/translation solve is wrong.
        if vert > 0.15:
            print(f"      ^ vertical error is large — 4-DoF alignment cannot cause "
                  f"this; suspect the pose source, not the transform")

    print()
    if worst <= args.expect:
        print(f"  OK — within the {args.expect} m physical offset of two co-located headsets.")
    else:
        print(f"  MISALIGNED by {worst - args.expect:.2f} m beyond the expected "
              f"{args.expect} m. Walk both pucks and localize again;")
        print("  if it persists after a fresh localize on BOTH, the map itself is suspect.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
