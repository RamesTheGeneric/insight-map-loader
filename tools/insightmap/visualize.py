#!/usr/bin/env python3
"""visualize -- inspect Insight map point clouds and their alignment.

Renders one or two extracted maps to a self-contained HTML page with three
orthographic views (top XZ, front XY, side ZY) plus stats. Two maps are drawn
in contrasting colours so overlap -- i.e. ALIGNMENT -- is visually obvious:
aligned maps show the two clouds sitting on the same structure; misaligned ones
show two offset/rotated copies of the room.

  # one map (a directory pulled from /vision/insideout/mapdb)
  ./visualize.py --dump /tmp/mapdb132 --out map132.html

  # two maps, B optionally transformed by a solved 4-DoF T (yaw_deg + tx ty tz)
  ./visualize.py --dump /tmp/mapdb132 --dump2 /tmp/mapdb108 \
                 --yaw 12.5 --t 0.3 0 -1.2 --out align.html

For an orbitable 3D view of the same data, use `visualize3d.py`; this one is
the flat-projection view, which is better for reading metric offsets.

Points are exact: decoded from the persisted map, already resolved into the
map's root frame. Each node is drawn in its own colour, so overlapping clouds
from two pucks mean they share a frame.
"""
import argparse
import glob
import html
import math
import os

import numpy as np



def load_points(dump_dir: str) -> list[np.ndarray]:
    """Per-node point clouds from a pulled mapdb, in the map's root frame."""
    if not glob.glob(os.path.join(dump_dir, "nd_*_2.mapdata")):
        raise SystemExit(f"no nd_*.mapdata in {dump_dir} — pull a mapdb first")
    import mapdata as md
    return [n.points() for n in md.Map(dump_dir).nodes]


def apply_4dof(P: np.ndarray, yaw_deg: float, t) -> np.ndarray:
    c, s = math.cos(math.radians(yaw_deg)), math.sin(math.radians(yaw_deg))
    R = np.array([[c, 0, s], [0, 1, 0], [-s, 0, c]])
    return P @ R.T + np.asarray(t, float)


def svg_view(sets, ia, ib, w, h, pad, title, labels, colors):
    """One orthographic projection: sets = list of (Nx3, color)."""
    allp = np.vstack([p for p, _ in sets]) if sets else np.zeros((0, 3))
    if len(allp) == 0:
        return f"<svg width='{w}' height='{h}'></svg>"
    lo = allp[:, [ia, ib]].min(0)
    hi = allp[:, [ia, ib]].max(0)
    span = np.maximum(hi - lo, 1e-6)
    sc = min((w - 2 * pad) / span[0], (h - 2 * pad) / span[1])

    def xy(p):
        x = pad + (p[ia] - lo[0]) * sc
        y = h - pad - (p[ib] - lo[1]) * sc      # flip: +up
        return x, y

    parts = [f"<svg width='{w}' height='{h}' style='background:#0f1115'>"]
    # grid every metre
    g0, g1 = math.floor(lo[0]), math.ceil(hi[0])
    for gx in range(g0, g1 + 1):
        x = pad + (gx - lo[0]) * sc
        parts.append(f"<line x1='{x:.1f}' y1='{pad}' x2='{x:.1f}' y2='{h-pad}' stroke='#22262e'/>")
    g0, g1 = math.floor(lo[1]), math.ceil(hi[1])
    for gy in range(g0, g1 + 1):
        y = h - pad - (gy - lo[1]) * sc
        parts.append(f"<line x1='{pad}' y1='{y:.1f}' x2='{w-pad}' y2='{y:.1f}' stroke='#22262e'/>")
    for P, col in sets:
        for p in P:
            x, y = xy(p)
            parts.append(f"<circle cx='{x:.1f}' cy='{y:.1f}' r='1.6' fill='{col}' fill-opacity='0.75'/>")
    parts.append(f"<text x='{pad}' y='18' fill='#9aa4b2' font-family='monospace' font-size='13'>{html.escape(title)}</text>")
    parts.append(f"<text x='{pad}' y='{h-6}' fill='#5b6472' font-family='monospace' font-size='11'>1 m grid</text>")
    parts.append("</svg>")
    return "".join(parts)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dump", required=True, help="a pulled mapdb directory")
    ap.add_argument("--dump2", help="a second mapdb, drawn over the first")
    ap.add_argument("--yaw", type=float, default=0.0, help="apply yaw (deg) to map B")
    ap.add_argument("--t", type=float, nargs=3, default=[0, 0, 0], help="apply translation to map B")
    ap.add_argument("--out", default="insight_map.html")
    args = ap.parse_args()

    A = load_points(args.dump)
    PA = np.vstack(A) if A else np.zeros((0, 3))
    sets_all = [(PA, "#4fc3f7")]
    labels = [os.path.basename(args.dump.rstrip("/"))]
    stats = [f"A: {len(A)} blocks, {len(PA)} pts"]

    PB = None
    if args.dump2:
        B = load_points(args.dump2)
        PB = np.vstack(B) if B else np.zeros((0, 3))
        if args.yaw or any(args.t):
            PB = apply_4dof(PB, args.yaw, args.t)
            stats.append(f"B transformed: yaw {args.yaw:+.2f}°, t {tuple(args.t)}")
        sets_all.append((PB, "#ff8a65"))
        labels.append(os.path.basename(args.dump2.rstrip("/")))
        stats.append(f"B: {len(B)} blocks, {len(PB)} pts")
        # overlap metric: for a sample of B, distance to nearest A point
        if len(PA) and len(PB):
            s = PB[np.random.RandomState(0).choice(len(PB), min(2000, len(PB)), replace=False)]
            d = np.sqrt(((s[:, None, :] - PA[None, :, :]) ** 2).sum(2)).min(1) if len(PA) < 4000 else None
            if d is not None:
                stats.append(f"B→A nearest-point: median {np.median(d):.2f} m, "
                             f"p10 {np.percentile(d,10):.2f} m, &lt;0.2 m: {100*(d<0.2).mean():.1f}%")

    views = [("top  (X→ Z↑)", 0, 2), ("front (X→ Y↑)", 0, 1), ("side (Z→ Y↑)", 2, 1)]
    svgs = "".join(f"<div class='v'>{svg_view(sets_all, ia, ib, 560, 420, 30, t, labels, None)}</div>"
                   for t, ia, ib in views)
    legend = "".join(
        f"<span class='k'><i style='background:{c}'></i>{html.escape(l)}</span>"
        for (_, c), l in zip(sets_all, labels))
    doc = f"""<!doctype html><meta charset=utf-8>
<title>Insight map{'s' if PB is not None else ''}</title>
<style>
 body{{background:#0b0d10;color:#c9d1d9;font-family:system-ui,sans-serif;margin:24px}}
 h1{{font-size:18px;font-weight:600;margin:0 0 4px}}
 .sub{{color:#7d8590;font-size:13px;margin-bottom:16px}}
 .views{{display:flex;flex-wrap:wrap;gap:16px}}
 .v{{border:1px solid #21262d;border-radius:8px;overflow:hidden}}
 .k{{margin-right:14px;font-size:13px}} .k i{{display:inline-block;width:10px;height:10px;
   border-radius:50%;margin-right:6px;vertical-align:middle}}
 ul{{color:#9aa4b2;font-size:13px;line-height:1.6}} code{{color:#79c0ff}}
</style>
<h1>Insight map point clouds</h1>
<div class=sub>{legend}</div>
<div class=views>{svgs}</div>
<ul>{"".join(f"<li>{s}</li>" for s in stats)}
<li>Aligned maps overlap on the same structure; misaligned ones look like two
offset or rotated copies of the room.</li>
<li>Points are decoded from the persisted map, in its root frame.</li></ul>
"""
    open(args.out, "w").write(doc)
    print(f"wrote {args.out}")
    for s in stats:
        print("  " + s.replace("&lt;", "<"))


if __name__ == "__main__":
    raise SystemExit(main())
