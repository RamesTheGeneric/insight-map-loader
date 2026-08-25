#!/usr/bin/env python3
"""
send_test_tracker.py -- emit synthetic MPT1 pose packets to the SteamVR driver.

Bring-up aid: proves the driver path (socket -> device -> SteamVR pose) works
before the real per-tracker SLAM feeds it. Moves the waist tracker in a slow
horizontal circle ~1 m in front of the play-space origin so you can watch it in
the SteamVR status window / room view.

Wire format must match steamvr_driver/src/mapper_protocol.h exactly:
  <IBBH Q 7f 3f 3f  (little-endian, 68 bytes)
  magic, device, valid, reserved, t_ns, pose[7], vel[3], angvel[3]

Poses are in the SteamVR world frame (y-up, metres). Run this ON or reachable
from the PC that hosts SteamVR; point --host at that PC.

  python3 send_test_tracker.py --host 127.0.0.1 --port 5180
"""
import argparse, math, socket, struct, time

MAGIC = 0x3154504D  # 'MPT1' little-endian
PKT = struct.Struct("<IBBHQ7f3f3f")
assert PKT.size == 68, PKT.size

DEV_WAIST, DEV_LFOOT, DEV_RFOOT = 0, 1, 2

# Role ids, matching MapperDeviceId in steamvr_driver/src/mapper_protocol.h and
# Device in q2slam-core/src/mpt1.rs. APPEND ONLY -- SteamVR keys pairings off
# the serial each id maps to.
ROLES = {
    0: "waist", 1: "left_foot", 2: "right_foot", 3: "chest",
    4: "left_knee", 5: "right_knee", 6: "left_elbow", 7: "right_elbow",
    8: "left_shoulder", 9: "right_shoulder", 10: "camera",
}


def pack(device, valid, t_ns, pose, vel, angvel):
    return PKT.pack(MAGIC, device, 1 if valid else 0, 0, t_ns,
                    *pose, *vel, *angvel)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=5180)
    ap.add_argument("--hz", type=float, default=100.0)
    ap.add_argument("--radius", type=float, default=0.3)
    ap.add_argument("--height", type=float, default=1.0, help="waist height (m)")
    ap.add_argument("--static", action="store_true", help="don't move; hold pose")
    ap.add_argument("--feet", action="store_true",
                    help="also send two foot trackers on the floor")
    ap.add_argument("--device", type=int, default=DEV_WAIST,
                    help="role id to drive instead of the waist (0-10). Use this to\n"
                         "check a role appears in SteamVR AND that no others do.")
    args = ap.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    dst = (args.host, args.port)
    dt = 1.0 / args.hz
    role = ROLES.get(args.device, f"UNKNOWN({args.device})")
    print(f"sending MPT1 -> {args.host}:{args.port} at {args.hz:g} Hz "
          f"({'static' if args.static else 'circle'})  device={args.device} [{role}]"
          f"  Ctrl-C to stop")
    if args.device not in ROLES:
        print("  NOTE: the driver drops ids outside 0-10, so nothing will appear.")

    t0 = time.time()
    n = 0
    while True:
        t = 0.0 if args.static else (time.time() - t0)
        w = 2.0 * math.pi * 0.1  # 0.1 Hz orbit
        ang = w * t
        # circle in the x-z plane (SteamVR floor), facing along motion.
        x = args.radius * math.cos(ang)
        z = -1.0 + args.radius * math.sin(ang)
        y = args.height
        # yaw quaternion about +y so the model turns with the motion.
        half = 0.5 * ang
        q = (math.cos(half), 0.0, math.sin(half), 0.0)  # w,x,y,z
        vx = -args.radius * w * math.sin(ang)
        vz = args.radius * w * math.cos(ang)
        vel = (vx, 0.0, vz)
        angvel = (0.0, w, 0.0)
        t_ns = time.monotonic_ns()

        sock.sendto(pack(args.device, True, t_ns, (x, y, z, *q), vel, angvel), dst)
        if args.feet:
            sock.sendto(pack(DEV_LFOOT, True, t_ns,
                             (x - 0.15, 0.05, z, 1, 0, 0, 0), (0, 0, 0), (0, 0, 0)), dst)
            sock.sendto(pack(DEV_RFOOT, True, t_ns,
                             (x + 0.15, 0.05, z, 1, 0, 0, 0), (0, 0, 0), (0, 0, 0)), dst)
        n += 1
        if n % int(args.hz) == 0:
            print(f"  t={t:6.1f}s  waist=({x:+.2f},{y:+.2f},{z:+.2f})")
        time.sleep(dt)


if __name__ == "__main__":
    main()
