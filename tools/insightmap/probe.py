#!/usr/bin/env python3
"""probe -- dump one or two pucks' Insight maps and summarize them.

  # one puck: enable mapping, dump, summarize (needs adb root + permissive)
  ./probe.py --dev 192.168.1.11 --out /tmp/imap_132

  # cross-device descriptor test (the validated experiment)
  ./probe.py --dev 192.168.1.11 --out /tmp/imap_132 \
             --dev2 192.168.1.10 --out2 /tmp/imap_108 --cross

SELinux must be permissive on each puck for the memory reads. This tries
`setenforce 0`; if your shell blocks it, run it yourself first:
    adb -s <ip>:5555 shell setenforce 0
"""
import argparse
import json

import insightmap as im


def prep_and_dump(ip: str, out: str, enable: bool) -> im.MapDump:
    dev = im.Device(ip)
    if enable:
        dev.enable_mapping()
    if not dev.set_permissive():
        print(f"  WARNING {ip}: not permissive -- run `adb -s {dev.serial} "
              f"shell setenforce 0` yourself, then re-run without --enable")
    dev.push_memread()
    info = dev.map_info()
    print(f"  {ip}: {info.get('map_points','?')} points, "
          f"{info.get('descriptors','?')} descriptors, "
          f"map_uuid={info.get('map_uuid','?')[:8]}")
    dump = dev.dump_map(out)
    return dump


def summarize(dump: im.MapDump, label: str):
    ctx = dump.find_context()
    descs = dump.descriptors()
    blocks = dump.point_blocks()
    npts = sum(len(b) for b in blocks)
    print(f"[{label}] context hits={len(ctx)}  descriptors extracted={len(descs)}  "
          f"point-blocks={len(blocks)} (~{npts} pts)")
    if blocks:
        b = max(blocks, key=len)
        print(f"    sample point block ({len(b)} pts): "
              f"{[tuple(round(x,3) for x in b[i]) for i in range(min(3,len(b)))]}")
    return descs


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dev", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--dev2")
    ap.add_argument("--out2")
    ap.add_argument("--enable", action="store_true",
                    help="arm map_db+keypoint_cache and restart tracking (resets frame)")
    ap.add_argument("--cross", action="store_true", help="run the cross-device descriptor test")
    args = ap.parse_args()

    dA = prep_and_dump(args.dev, args.out, args.enable)
    descA = summarize(dA, args.dev)

    if args.dev2:
        dB = prep_and_dump(args.dev2, args.out2, args.enable)
        descB = summarize(dB, args.dev2)
        if args.cross:
            print("\n=== cross-device descriptor test ===")
            print(json.dumps(im.cross_match_report(descA, descB), indent=2))


if __name__ == "__main__":
    raise SystemExit(main())
