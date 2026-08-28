# Insight Map Loader

Full-body tracking from retired Quest 1 headsets, worn as body pucks.

The name is the mechanism: it loads one Insight map onto several headsets. A Quest 1
already contains a competent SLAM system, and headsets holding the same map track in the
same coordinate frame — so this moves that map from one headset to the others and feeds
their poses to SteamVR as generic trackers. No lighthouses, no marker boards, no
calibration ritual.

Components:
* `android/q1tracker` - on-puck OpenXR app, streams pose as MPT1 over UDP
* `desktop/insight-map-loader-core` - host service: ingest, bridge watchdog, fleet control, map jobs
* `desktop/insight-map-loader-gui` - control surface (egui): fleet status, map sharing, role assignment
* `desktop/steamvr_driver` - OpenVR driver exposing up to 11 generic trackers

Tools:
* `tools/q1bringup.sh` - take one rooted headset from bare to streaming, or repair it
* `tools/q1sep.py` - ground-truth check: hold two pucks together, measure the disagreement
* `tools/q1resid.py` - split that disagreement into frame yaw, mount geometry and noise
* `tools/insightmap/` - decode, match, visualise and self-test Insight's SLAM map

## Installing

There is no installer; everything is built from source. See
[CONTRIBUTING.md](CONTRIBUTING.md) for prerequisites and build steps, then
[INSTALL.md](INSTALL.md) to set it up, then [docs/puck-bringup.md](docs/puck-bringup.md)
for each headset.

You will need **rooted Quest 1 headsets**. Rooting is out of scope here; without root
none of this works.

## Building & Contributing

For information on building and contributing to the codebase, see
[CONTRIBUTING.md](CONTRIBUTING.md).

Documentation is in [docs/](docs/README.md) - how it works, and how it was found out.
[FINDINGS.md](FINDINGS.md) records what was tried and failed, which is probably the most
useful file here if you are doing similar reverse engineering.

## Measured

On two Quest 1 pucks held physically together:

* Colocation error: **3.3 cm horizontal median**, with an identity transform and nothing
  host-side in the path
* Previous best, using a solved transform: 9.6 cm
* Map decode verified against live `dumpsys`: same root uuid, node set and point counts

The 3.3 cm figure is the honest one. It is the only measurement with real ground truth,
because two co-located pucks cannot lie to each other.

## Limitations

**Verified on exactly two headsets, on one network, in one room.** Everything above is
reproducible here and none of it is reproducible elsewhere until someone else tries.
A report from other hardware is the most valuable contribution this project can receive.

* The map is per-space. A puck must physically be in the mapped room to relocalize into
  it, and a disconnected space needs its own map.
* One calibration remains. The tracker app reports an OpenXR LOCAL frame, so a
  LOCAL→world bridge is solved per session. It goes stale on a tracker restart and needs
  a still moment to re-solve.
* Same hardware only. The map embeds the originating device's camera calibration. Proven
  between two Quest 1s, untested across models.
* Insight prunes its own map. Point counts are not monotonic - two pucks were observed
  dropping from 1269 points each to 835 and 860 without leaving the room - so two pucks
  diverge in content even with no new territory.
* The map-to-map alignment fallback is not trustworthy on real maps. Its solver is exact
  against synthetic ground truth and a map matched against itself returns exact identity,
  but the node-pair self-test currently recovers identity on only 7 of 15 pairs. An
  earlier record of 12/15 could not be reproduced. Colocation does not depend on this
  path; do not build on it without re-checking.
* Quest 1 is EOL, and the headsets here are deliberately kept off Meta's servers. An OTA
  would change the tracking library all of this depends on.

## License clarification

**Insight Map Loader is distributed under the GNU General Public License v3.0
([LICENSE]). Third-party components and their licences are recorded in [THIRD_PARTY],
notably the vendored `openvr_driver.h`, which is BSD-3-Clause.**

**This project contains no Meta code.** It interoperates with software already present on
a device you own, using interfaces recovered by observation. Device libraries, where a
build needs them, are read from your own headset and are never redistributed here. The
same applies to anything you produce with it:

* A persisted Insight map is a 3-D scan of the room it was made in. Do not commit one,
  do not attach one to an issue, and do not ask a reporter to send you theirs.
* Do not commit anything pulled off a Quest. Meta's libraries are theirs.

## Contributions

Any contribution submitted for inclusion in this repository will be licensed under the
GPL-3.0 ([LICENSE]), without any additional terms or conditions.

You also certify that the code you have used is compatible with that licence or is
authored by you. Note that GPL-3.0 is deliberately restrictive: permissively licensed
projects generally cannot absorb code from here, so check the direction of travel before
importing code either way.

[LICENSE]: LICENSE
[THIRD_PARTY]: THIRD_PARTY.md
