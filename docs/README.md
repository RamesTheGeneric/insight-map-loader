# Documentation

## Start here

| | |
|---|---|
| [puck-bringup.md](puck-bringup.md) | **Setting up a headset**, from freshly-rooted to streaming. Ten steps, each with the reason it exists, and a troubleshooting table keyed by symptom. |
| [insight-map-lifecycle.md](insight-map-lifecycle.md) | **Create / share / extend / read** the shared map — the operational reference for colocation. |

## Reference

| | |
|---|---|
| [insight-mapdata-format.md](insight-mapdata-format.md) | The persisted map format: fbthrift compact with a float extension, the record kinds, and the (azimuth, elevation, inverse-depth) point encoding. Includes how each convention was pinned. |
| [insight-map-and-anchors.md](insight-map-and-anchors.md) | The colocation binder API, transaction codes, signatures, and where each route is walled. |
| [insight-slam-internals.md](insight-slam-internals.md) | What Insight is doing underneath: VIPER/VegaMapper, keyrigs, L1 nodes, descriptors. |
| [headless-adb.md](headless-adb.md) | **Driving a puck with no display**: the Meta-specific adb, `pm`/`am`/`setprop` incantations, and the ones where the obvious command silently does nothing. |
| [wireless-adb.md](wireless-adb.md) | Getting adb over wifi on an Android 10 headset — no pairing code, no toggle. |
| [quest1-sensors.md](quest1-sensors.md) | Device-level notes: factory calibration, the IMU stream, aligning time domains, and Insight's pose shared memory — including the gate that keeps that fast path shut. |

## Also

- [../FINDINGS.md](../FINDINGS.md) — what was tried and failed, and the silent
  failure modes. Probably the most useful file here if you are doing similar
  reverse engineering.
- [../README.md](../README.md) — setup for the headsets, the driver and the GUI,
  plus how to work on the code and what is useful to contribute. Everything you
  need to get running is there; these documents are the reasoning behind it.

These documents describe **observed behaviour of a device you own**. Where a
format or interface is written down, it is a description recovered by
observation, not copied implementation.
