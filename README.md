# Insight Map Loader

**Full-body tracking from retired Quest 1 headsets, worn as body pucks.**

The name is the mechanism: it **loads one Insight map onto several headsets**.
That is the whole trick — a Quest 1 already contains a competent SLAM system,
and headsets holding the same map track in the same coordinate frame. This
project moves that map from one headset to the others and feeds their poses to
SteamVR as generic trackers.

No lighthouses, no marker boards, no calibration ritual. A Quest 1 is a
four-camera inside-out tracker with an IMU that Meta no longer supports; there
are a lot of them, and they are cheap.

---

## How it works

The load-bearing idea: **two headsets that load the same Insight map track in
the same coordinate frame.** Not approximately — natively, because the map's
root node *is* the frame.

```
  puck (Quest 1)                   host                        SteamVR
  ┌───────────────┐  MPT1/UDP  ┌──────────────────┐  MPT1  ┌────────────────┐
  │ Insight SLAM  │───────────▶│ insight-map-loader    │───────▶│ driver_mapper  │
  │ q1tracker app │  pose 100Hz│ ingest + bridge  │udp/5181│ GenericTrackers│
  └───────────────┘            └──────────────────┘        └────────────────┘
         ▲                              │
         │ shared map (mapdb)           │ adb: status, map share, roles
         └──────────────────────────────┘
```

Every puck runs an OpenXR app that streams its pose. The host aggregates them
and re-emits into a SteamVR driver. Because the pucks share a map, **no
inter-puck transform is solved or stored** — the historical failure mode of this
kind of system, where a stored calibration goes stale the moment the tracker
relocalizes.

## Measured

On two Quest 1 pucks, held physically together:

| | |
|---|---|
| colocation error | **3.3 cm horizontal median** (8 samples, identity transform, nothing host-side in the path) |
| previous best, with a solved transform | 9.6 cm |
| map decode | verified against live `dumpsys`: same root uuid, node set and point counts |
| alignment pipeline (fallback path) | identity recovered on 12/15 node pairs at 0.33–1.79° / 6–18 cm, **0 wrong transforms** |

The 3.3 cm figure is the honest one: it is the only measurement with real ground
truth, because two co-located pucks cannot lie to each other.

## What is here

| | |
|---|---|
| `android/q1tracker/` | the on-puck OpenXR app — streams pose as MPT1 over UDP |
| `desktop/insight-map-loader-core/` | host service: ingest, bridge watchdog, fleet control, map jobs |
| `desktop/insight-map-loader-gui/` | the control surface (egui) — fleet status, map sharing, roles |
| `desktop/steamvr_driver/` | OpenVR driver exposing up to 11 generic trackers |
| `tools/insightmap/` | decode, match, visualise and self-test Insight's SLAM map |
| `tools/q1bringup.sh` | take one rooted headset from bare to streaming, or repair it |
| `tools/q1sep.py` | the ground-truth check — hold two pucks together, measure the disagreement |
| `docs/` | how it works and how it was found out — see `docs/README.md` |

Start with **[INSTALL.md](INSTALL.md)**, then **[docs/puck-bringup.md](docs/puck-bringup.md)**
for each headset.

Working on the code instead? **[docs/development.md](docs/development.md)** for
the build-and-iterate loop (including how much runs with no headsets at all),
and **[CONTRIBUTING.md](CONTRIBUTING.md)** for what is useful to send.

## Requirements

- **Rooted Quest 1s.** Rooting is out of scope here; without root none of this
  works.
- A Linux host (developed on CachyOS; the SteamVR driver builds on Windows too).
- SteamVR.
- Toolchains: JDK 17 + Android SDK/NDK, Rust ≥ 1.75, CMake ≥ 3.15, Python 3 +
  numpy, `adb`.

## Honest limits

- **Verified on exactly two headsets, on one network, in one room.** Everything
  in the results table is reproducible here; none of it is reproducible
  elsewhere until someone else tries.
- **The map is per-space.** A puck must be physically in the mapped room to
  relocalize into it. Disconnected spaces need their own map.
- **One calibration remains**: the tracker app reports an OpenXR LOCAL frame, so
  a LOCAL→world bridge is still solved per session. It goes stale on restarts
  and needs a still moment to re-solve.
- **Same hardware only.** The map embeds the originating device's camera
  calibration. Proven between two Quest 1s, untested across models.
- Quest 1 is EOL and the headsets here are kept off Meta's servers deliberately;
  an OTA would change the tracking library this all depends on.

## Licence

GPL-3.0. See [LICENSE](LICENSE) and [THIRD_PARTY.md](THIRD_PARTY.md).

This project contains **no Meta code**. It interoperates with the software
already on a device you own, using interfaces recovered by observation. Device
libraries, where a build needs them, are read from your own headset and are
never redistributed here.
