# Mapper SteamVR driver

An OpenVR driver that publishes the body trackers (waist + both feet) as
`TrackedDeviceClass_GenericTracker` devices with FBT role hints. It does **no
estimation** — each tracker computes its own filtered pose on-device (per
CLAUDE.md §0, per-tracker mono-inertial) and streams it here over UDP; the driver
relays those poses to SteamVR with velocity and a negative `poseTimeOffset` so
the compositor extrapolates through the remaining latency.

The driver links nothing from OpenVR — it only needs `openvr_driver.h` (vendored
under `vendor/openvr/headers/`) and exports `HmdDriverFactory`. There is no
SDK build step.

## Layout

```
steamvr_driver/
  src/driver_mapper.cpp     driver + UDP receive thread
  src/mapper_protocol.h     the 68-byte MPT1 pose packet (shared wire contract)
  mapper/                   the installable driver tree (manifest + resources)
  tools/send_test_tracker.py  synthetic pose sender for bring-up
  CMakeLists.txt
  vendor/openvr/headers/openvr_driver.h   (vendored; tracked in-repo)
```

## Where it runs

The driver is a `vrserver` plugin: it runs **inside SteamVR on the machine
hosting SteamVR — the Windows PC**. Source can be edited anywhere (it built clean
on macOS as a syntax check), but the shippable `driver_mapper.dll` must be built
with **MSVC** and loaded by SteamVR on Windows. Do not ship a MinGW build; the
C++ vtable ABI SteamVR expects is MSVC's.

## Build on Windows (MSVC)

Prereqs: Visual Studio 2019/2022 with the C++ workload, and CMake (bundled with
VS or standalone).

```bat
cd steamvr_driver
cmake -S . -B build -G "Visual Studio 17 2022" -A x64
cmake --build build --config Release
```

This produces `build\mapper\` — the complete installable tree:

```
build\mapper\
  driver.vrdrivermanifest
  resources\settings\default.vrsettings
  bin\win64\driver_mapper.dll
```

## Install into SteamVR

Register the staged tree with SteamVR's path tool (no admin needed):

```bat
"C:\Program Files (x86)\Steam\steamapps\common\SteamVR\bin\win64\vrpathreg.exe" adddriver "%CD%\build\mapper"
```

(Adjust the Steam path if yours differs.) Verify:

```bat
vrpathreg.exe show
```

Alternatively copy `build\mapper` into
`...\SteamVR\drivers\mapper` — `vrpathreg` is cleaner because it survives SteamVR
updates and is easy to remove (`vrpathreg removedriver "<path>"`).

Restart SteamVR after registering.

## Test without the SLAM pipeline

With SteamVR running, feed the driver synthetic poses from any machine that can
reach the PC (or locally):

```bash
python3 tools/send_test_tracker.py --host <PC-IP> --port 5180
```

A waist tracker should appear in the SteamVR status window and orbit slowly ~1 m
in front of the origin. `--feet` adds two foot trackers; `--static` holds still.

Check `driver_mapper` shows up in the SteamVR web console
(`Settings ▸ Developer ▸ Web Console`) — look for the `[mapper]` log lines
(`init ok, listening on udp/5180`, `activated mapper_waist as TrackerRole_Waist`).

## Feeding it real poses

The producer sends one `MapperPosePacket` per tracker per update to UDP
`udp_port` (default **5180**) on the PC. Poses must already be in the **SteamVR
world frame** (y-up, metres, -z forward): the map→universe SE(3) alignment is
applied upstream, not in the driver.

For bring-up on the Mac, `tools/hip_filter_live.py --mpt1` emits the MPT1 packet
directly (velocity from the ESKF state, angular velocity from the bias-corrected
gyro rotated to world). Point it at the PC:

```bash
python3 tools/hip_filter_live.py --cam-imu cam_imu_hip.json \
    --mpt1 --device 0 --out <PC-IP>:5180
```

`--device` selects waist(0)/left_foot(1)/right_foot(2); run one filter per
tracker. Without `--mpt1` the filter keeps emitting the legacy 28-byte pose on
udp/5160 for the scene app. The packet contract lives in `src/mapper_protocol.h`
so both ends stay in sync.

Note the poses are in the **map frame** at this stage; align map→SteamVR-universe
with OpenVR-SpaceCalibrator (continuous mode with a headset-mounted reference is
the plan). SpaceCalibrator applies a rigid SE(3), so it also absorbs the
axis-convention difference between the gravity-aligned map frame and SteamVR's
y-up frame — no manual axis swap needed, as long as both frames are right-handed
and metric.

## Settings

`resources/settings/default.vrsettings` (override in SteamVR's
`steamvr.vrsettings` under the `driver_mapper` section):

| key | default | meaning |
|---|---|---|
| `enable` | `true` | load the driver |
| `udp_port` | `5180` | pose input port |
| `stale_timeout_s` | `0.5` | mark a tracker not-tracking after this silence |
| `pipeline_latency_s` | `0.025` | extra constant added to the pose age for `poseTimeOffset` extrapolation |

## FBT roles

The driver pre-assigns roles (`TrackerRole_Waist` / `LeftFoot` / `RightFoot`) by
writing SteamVR's `trackers` section itself, and sets each device's
`ControllerType` to `vive_tracker_waist`/`_left_foot`/`_right_foot` so apps like
VRChat pick up the role with no per-app setup. If a role doesn't stick, assign it
manually in SteamVR ▸ **Manage Trackers**.
