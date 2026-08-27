#!/usr/bin/env python3
"""q1resid -- decompose the last centimetres of colocation error.

Carry two pucks HELD RIGIDLY TOGETHER around the room. Because they are rigid,
their two trajectories differ by exactly one 4-DoF transform, and fitting it
separates the things a separation *distance* cannot:

  yaw        residual rotation between the two world frames. THE error term.
             Costs r*sin(yaw) of position, so it grows with distance from the
             map origin and is invisible standing at it.
  translation the mount offset -- two headsets cannot occupy the same space, so
             their tracking origins are genuinely centimetres apart. Real
             distance, not error, and no alignment removes it.
  residual   what neither explains: tracking noise plus the 1 cm dumpsys
             position quantisation. The floor.

Why not just regress separation against radius: the mount offset is fixed in the
PUCKS' frame and rotates with them, while the yaw error is perpendicular to the
radius from the map origin. Taking a magnitude discards the direction that tells
them apart, so the two add or cancel depending on heading -- measured on a real
walk, that produced 1 cm and 19 cm at the SAME radius, in episodes (lag-1
autocorrelation 0.60), and a radius fit explaining 7% of the variance. Fitting
the transform uses the direction and is not under-constrained.

    ./q1resid.py --seconds 120
    ./q1resid.py 192.168.1.10 192.168.1.11 --seconds 120

Hold ONE fixed relative orientation throughout, and cover a real spread of
positions -- the yaw term needs a lever arm to be visible at all.
"""
import argparse
import json
import math
import os
import re
import statistics
import subprocess
import sys
import time

import numpy as np

# Max movement between consecutive reads for a sample to count as stationary.
# Below this the 75 ms A->B sampling gap contributes under a centimetre.
STILL_M = 0.02

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "insightmap"))
from matchmap import yaw_kabsch, yaw_deg  # noqa: E402


def pose(ip):
    """Insight WORLD position, or None when not tracking at 6DoF."""
    # Never pipe `dumpsys tracking` into anything that closes the pipe early --
    # it leaves the tracking service unavailable for seconds.
    subprocess.run(["adb", "-s", f"{ip}:5555", "shell",
                    "dumpsys tracking > /data/local/tmp/qres.txt 2>/dev/null"],
                   capture_output=True, timeout=30)
    o = subprocess.run(["adb", "-s", f"{ip}:5555", "shell",
                        "grep -A4 '  Hmd:' /data/local/tmp/qres.txt"],
                       capture_output=True, timeout=30).stdout.decode()
    if "6DOF" not in o or "Valid: Yes" not in o:
        return None
    m = re.search(r"trans=\(([^)]*)\)", o)
    return [float(x) for x in m.group(1).split(",")] if m else None


def fit(A, B):
    """Solve A ~ R*B + t, 4-DoF, plus per-sample residuals."""
    R, t = yaw_kabsch(A, B)
    pred = B @ R.T + t
    err = np.linalg.norm(A - pred, axis=1)
    return R, t, err


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pucks", nargs="*")
    ap.add_argument("--config", default="insight-map-loader.json")
    ap.add_argument("--seconds", type=float, default=120.0)
    ap.add_argument("--out", default="resid.json")
    args = ap.parse_args()

    ips = args.pucks
    if not ips:
        if not os.path.exists(args.config):
            sys.exit(f"no {args.config}; pass two puck IPs")
        ips = [p["ip"] for p in json.load(open(args.config)).get("pucks", [])]
    if len(ips) < 2:
        sys.exit("need two pucks")
    a_ip, b_ip = ips[0], ips[1]

    print(f"hold {a_ip} and {b_ip} together in ONE fixed relative orientation.")
    print(f"STAND STILL at a spot, let it sample, then MOVE to a new spot and stand")
    print(f"still again. Ten-odd spots over {args.seconds:.0f}s, spread near AND far from")
    print("where the map was made. Samples are only taken while both pucks are still,")
    print("because the two are read 75 ms apart and walking smears that into the result.\n")

    # The two pucks are read SEQUENTIALLY over adb, ~75 ms apart. While moving,
    # that gap alone contributes 4-8 cm at walking pace -- comparable to the
    # whole effect being measured, and it silently inflates both the separation
    # and the residual. So only samples taken while BOTH pucks are stationary
    # are kept: the yaw term needs spread in POSITION, not continuous motion.
    A, B = [], []
    prev_a = prev_b = None
    t_end = time.time() + args.seconds
    while time.time() < t_end:
        a, b = pose(a_ip), pose(b_ip)
        if a is None or b is None:
            print("  (a puck is not at 6DoF)")
            time.sleep(0.3)
            continue
        if prev_a is not None:
            moved = max(math.dist(a, prev_a), math.dist(b, prev_b))
            if moved > STILL_M:
                print(f"    moving ({moved*100:.0f} cm) — hold still to sample")
                prev_a, prev_b = a, b
                time.sleep(0.3)
                continue
            A.append(a)
            B.append(b)
            r = math.hypot(a[0], a[2])
            print(f"  n={len(A):3d}  r={r:5.2f} m  sep={math.dist(a, b)*100:5.1f} cm  (still)")
        prev_a, prev_b = a, b
        time.sleep(0.3)

    if len(A) < 20:
        sys.exit("too few samples to fit a transform")

    A, B = np.array(A), np.array(B)
    R, t, err = fit(A, B)
    yaw = yaw_deg(R)
    radii = np.hypot(A[:, 0], A[:, 2])

    json.dump({"A": A.tolist(), "B": B.tolist(),
               "yaw_deg": yaw, "t": t.tolist(),
               "residual_cm": (err * 100).tolist()}, open(args.out, "w"))

    print(f"\n{len(A)} samples, radius {radii.min():.2f}-{radii.max():.2f} m")
    print(f"raw separation: median {np.median(np.linalg.norm(A-B, axis=1))*100:.1f} cm")
    print()
    print(f"  FRAME YAW      {yaw:+.3f} deg")
    print(f"                 -> costs {abs(math.sin(math.radians(yaw)))*100:.1f} cm per metre "
          f"of radius; {abs(math.sin(math.radians(yaw)))*radii.max()*100:.1f} cm at your furthest")
    print(f"  MOUNT OFFSET   ({t[0]*100:+.1f}, {t[1]*100:+.1f}, {t[2]*100:+.1f}) cm  "
          f"|horizontal| = {math.hypot(t[0], t[2])*100:.1f} cm")
    print(f"                 -> physical, not error. Not removable.")
    print(f"  RESIDUAL       median {np.median(err)*100:.1f} cm, "
          f"p90 {np.percentile(err, 90)*100:.1f} cm")
    print(f"                 -> tracking noise + 1 cm dumpsys quantisation. The floor.")

    # Quote the uncertainty, not a goodness-of-fit percentage: yaw is inferred
    # from a lever arm, so the noise floor divided by the mean radius (and by
    # sqrt(n)) IS the precision, and it is the number that says whether a yaw
    # this small is distinguishable from zero at all.
    lever = float(np.mean(radii))
    sigma = float(np.median(err))
    yaw_sd = math.degrees(math.atan(sigma / lever)) / math.sqrt(len(A)) if lever > 0.1 else float("nan")
    print(f"\n  yaw precision  +/- {yaw_sd:.3f} deg "
          f"(noise {sigma*100:.1f} cm over a {lever:.2f} m mean lever arm, n={len(A)})")
    if abs(yaw) < 2 * yaw_sd:
        print(f"  NOTE: {abs(yaw):.3f} deg is within 2 sigma of zero — not resolved.")
    if radii.max() - radii.min() < 1.5:
        print("  NOT ENOUGH RADIUS SPREAD — walk further out and re-run; the yaw")
        print("  term has no lever arm here and the number above is not meaningful.")
    elif abs(yaw) < 0.2:
        print(f"  => frames agree to {abs(yaw):.2f} deg. What is left is mount offset")
        print("     and noise; a yaw correction would buy nothing.")
    else:
        print(f"  => {abs(yaw):.2f} deg of real frame yaw. Worth correcting: it is the")
        print("     only term here that grows with distance.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
