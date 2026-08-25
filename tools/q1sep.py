#!/usr/bin/env python3
"""Ground-truth colocation check: put two pucks together, measure the disagreement.

Physically stack or touch two pucks, then run this. If they share a map they
share a coordinate frame, so their reported positions should differ only by the
physical offset between the two headsets' Insight origins.

Why this is worth having: it is the only measurement in the system with real
ground truth. Every other signal -- matching residual, inlier count, "same root
uuid" -- scores the map against itself, so a map that is internally consistent
but wrong still scores well. Two co-located pucks cannot lie to each other.

**No transform is applied.** That is the point: with a shared map the transform
IS identity, and applying anything would measure the correction rather than the
colocation. The reading therefore also tests the premise -- if these numbers are
large, the pucks are not really sharing a frame.

    ./tools/q1sep.py                          # pucks from insight-map-loader.json
    ./tools/q1sep.py 192.168.1.10 192.168.1.11
    ./tools/q1sep.py --samples 20

Interpreting it: expect a few centimetres horizontally. A steady vertical offset
is normal and is mount geometry, not error -- two headsets held together have
their origins a headset-thickness apart. Drift would not hold still.
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


def dumpsys_pose(ip):
    """Insight WORLD position, or None if the puck is not tracking at 6DoF."""
    # Never pipe `dumpsys tracking` into something that closes the pipe early
    # (grep -m1, head): that leaves the tracking service unavailable for
    # seconds. Dump to a file on-device, then grep the file.
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


def map_root(ip):
    """The puck's map context: (short root uuid, persistent?)."""
    o = subprocess.run(["adb", "-s", f"{ip}:5555", "shell",
                        "grep -m1 'Vega Map Context' /data/local/tmp/qsep.txt"],
                       capture_output=True, timeout=30).stdout.decode()
    m = re.search(r"topNodeUid ([0-9a-f-]+)", o)
    return (m.group(1)[:8] if m else ""), "(persistent)" in o


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pucks", nargs="*", help="IPs (default: read the config)")
    ap.add_argument("--config", default="insight-map-loader.json")
    ap.add_argument("--samples", type=int, default=8)
    args = ap.parse_args()

    ips = args.pucks
    if not ips:
        if not os.path.exists(args.config):
            sys.exit(f"no {args.config}; pass puck IPs on the command line")
        ips = [p["ip"] for p in json.load(open(args.config)).get("pucks", [])]
    if len(ips) < 2:
        sys.exit("need at least two pucks to measure a separation")

    # Sharing a map is the precondition. Without it the numbers below are
    # meaningless rather than merely bad, so say so instead of reporting them.
    roots = {}
    for ip in ips:
        dumpsys_pose(ip)                       # refreshes the on-device dump
        roots[ip] = map_root(ip)
        r, persistent = roots[ip]
        print(f"  {ip:<16} map {r or 'none':<9} "
              f"{'persistent' if persistent else 'TRANSIENT'}")
    distinct = {r for r, _ in roots.values() if r}
    if len(distinct) != 1 or not all(p for _, p in roots.values()):
        print("\nthese pucks are NOT on one shared persistent map — "
              "share a map first, or this measures nothing")

    print(f"\nsampling {args.samples}x, no transform applied (identity)")
    horiz, full = [], []
    for i in range(args.samples):
        pos = {ip: dumpsys_pose(ip) for ip in ips}
        if any(p is None for p in pos.values()):
            print(f"  {i}: a puck is not tracking at 6DoF")
            time.sleep(0.5)
            continue
        a, b = pos[ips[0]], pos[ips[1]]
        d = [a[k] - b[k] for k in range(3)]
        h = math.hypot(d[0], d[2])
        t = math.dist(a, b)
        horiz.append(h)
        full.append(t)
        print(f"  {i}: horiz {h:.3f} m  3D {t:.3f} m   "
              f"d=({d[0]:+.3f},{d[1]:+.3f},{d[2]:+.3f})")
        time.sleep(0.3)

    if not horiz:
        sys.exit("no valid samples")
    print(f"\nhorizontal: median {statistics.median(horiz):.3f} m  "
          f"min {min(horiz):.3f}  max {max(horiz):.3f}")
    print(f"3D        : median {statistics.median(full):.3f} m")
    print("\nA steady vertical offset is mount geometry, not error. The "
          "horizontal median is the colocation figure.")


if __name__ == "__main__":
    sys.exit(main())
