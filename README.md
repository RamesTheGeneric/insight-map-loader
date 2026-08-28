# Insight Map Loader

**Full-body tracking from retired Quest 1 headsets, worn as body pucks.**

The name is the mechanism: it **loads one Insight map onto several headsets**. That is
the whole trick. A Quest 1 already contains a competent SLAM system, and headsets
holding the same map track in the same tracking space. This project moves that map from
one headset to the others and feeds their poses to SteamVR as generic trackers.

---

## Contents

- [Measured](#measured) · [Requirements](#requirements) · [What is here](#what-is-here)
- **Setup:** [1. Build](#1-build-everything) · [2. SteamVR driver](#2-set-up-the-steamvr-driver) · [3. Each headset](#3-set-up-each-headset) · [4. GUI](#4-run-the-gui)
- [Daily use](#daily-use) · [Troubleshooting](#troubleshooting)
- [Working on the code](#working-on-the-code) · [Contributing](#contributing) · [Honest limits](#honest-limits) · [Licence](#licence)

## Requirements

- **Rooted Quest 1s.** Rooting is out of scope here; without root none of this works.
- A Linux host (developed on CachyOS; the SteamVR driver builds on Windows too).
- SteamVR.
- Toolchains: JDK 17 + Android SDK/NDK, Rust ≥ 1.75, CMake ≥ 3.15, Python 3 + `numpy`,
  and `adb` from `android-tools`.

## What is here

| | |
|---|---|
| `android/q1tracker/` | the on-puck OpenXR app — streams pose as MPT1 over UDP |
| `desktop/insight-map-loader-core/` | host service: ingest, bridge watchdog, fleet control, map jobs |
| `desktop/insight-map-loader-gui/` | the control surface (egui) — fleet status, map sharing, roles |
| `desktop/steamvr_driver/` | OpenVR driver exposing up to 11 generic trackers |
| `tools/q1bringup.sh` | take one rooted headset from bare to streaming, or repair it |
| `tools/q1sep.py` | ground-truth check — hold two pucks together, measure the disagreement |
| `tools/q1resid.py` | split that disagreement into frame yaw, mount geometry and noise |
| `tools/insightmap/` | decode, match, visualise and self-test Insight's SLAM map |
| `docs/` | the reverse-engineering record — how it works and how it was found out |

---

# Setup

Do these in order. Steps 1 and 2 are once per PC; step 3 is once per headset.

## 1. Build everything

```sh
git clone <your-fork> insight-map-loader
cd insight-map-loader
```

Everything below runs **from the repository root** — the service resolves
`insight-map-loader.json`, `bridge.json` and `tools/` relative to it.

**Host service and GUI.** The workspace manifest is at `desktop/Cargo.toml`, not the
repo root, so builds need `--manifest-path`:

```sh
cargo build --release --manifest-path desktop/Cargo.toml
```

That produces `desktop/target/release/insight-map-loader` (CLI) and
`insight-map-loader-gui`.

**The puck app:**

```sh
cd android/q1tracker && ./gradlew :app:assembleDebug && cd ../..
# -> android/q1tracker/app/build/outputs/apk/debug/app-debug.apk
```

Keep that path; `q1bringup.sh --apk` wants it. If Gradle cannot find the SDK, create
`android/q1tracker/local.properties` containing `sdk.dir=/path/to/Android/Sdk` (it is
gitignored — it is a path on your machine, not a project setting).

**Your site config:**

```sh
cp desktop/insight-map-loader.example.json insight-map-loader.json
```

Edit `host` to **your PC's LAN IP**. The pucks stream to it, so `127.0.0.1` will not do.
Puck entries can be left empty for now.

## 2. Set up the SteamVR driver

```sh
cd desktop/steamvr_driver
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
```

Nothing from OpenVR is linked — only its header is needed, and that header is vendored,
so there is no SDK to install.

Register it **once**. This points SteamVR at your build tree, so rebuilding *is*
reinstalling:

```sh
~/.steam/steam/steamapps/common/SteamVR/bin/linux64/vrpathreg \
    adddriver "$PWD/build/mapper"
```

Restart SteamVR and check its log for:

```
mapper: init ok, listening on udp/5181
```

If that line is missing, your driver was not loaded and nothing downstream will make
sense. Verify with no headsets involved:

```sh
python3 desktop/steamvr_driver/tools/send_test_tracker.py --device 6
```

Exactly one tracker should appear, as **Left Elbow**, and nothing else. If a dozen
appear, SteamVR is running an older build of the driver.

It listens on udp/5181 because 5180 belongs to the puck→host ingest.

## 3. Set up each headset

`tools/q1bringup.sh` does all of this and verifies each step rather than assuming:

```sh
./tools/q1bringup.sh <ip> --usb <SERIAL> \
    --apk android/q1tracker/app/build/outputs/apk/debug/app-debug.apk --device 0
```

What it is doing, and why each part matters:

**a. Wireless adb.** Once, over USB:

```sh
adb -s <SERIAL> tcpip 5555
adb -s <SERIAL> shell setprop persist.adb.tcp.port 5555   # survives reboots
adb connect <IP>:5555
```

Pin the IP to the MAC in your router. It is a DHCP lease and it *will* move, and every
tool here addresses pucks by IP.

**b. Root over wifi.**

```sh
adb -s <IP>:5555 root && adb connect <IP>:5555
adb -s <IP>:5555 shell id -u        # must print 0
```

**Root does not survive a reboot.** Re-run after every one, or `setprop`/`chown`/`chcon`
fail *silently* and everything downstream looks broken for the wrong reason.

**c. Install the tracker app.**

```sh
adb -s <IP>:5555 install -r android/q1tracker/app/build/outputs/apk/debug/app-debug.apk
```

**d. Provision for off-head operation.** A body puck is never worn, and a headset that
believes it is off a head powers down before anything can start:

```sh
./desktop/target/release/insight-map-loader provision
```

This also assigns each puck a **stable id**, which is what makes its SteamVR role
reassignable later without touching the device. It costs one tracker restart per puck,
once, and is safe to re-run.

Under the hood these are `persist.*` properties so init restores them every boot — the
volatile `debug.oculus.forceHeadsetOn` is cleared by a reboot, which is exactly when it
is needed. **Wait ~45 s before rebooting**: the appop has to reach disk, which is why
`provision` blocks.

**e. Disable the guardian PACKAGE.**

```sh
adb -s <IP>:5555 shell pm disable-user --user 0 com.oculus.guardian
```

**Do this before the tracker app first starts, and do not skip it.** The property in (d)
is *not* sufficient. With the package enabled the tracker app looks completely healthy —
running, window-focused, correct config, Insight at `6DOF Valid: Yes` — and **emits
nothing at all**. It also drives the displays to passthrough instead of dark. The package
is temporarily re-enabled when creating a map, which is why that is a bracket rather than
a toggle.

**f. Add it to `insight-map-loader.json`** with an `ip` and a `device` (its SteamVR
role, 0–10). `provision` fills in the `id`.

**g. Give it the map.** This is what puts the puck in the *same tracking frame* as the
others.

- **The fleet already has a map** (usual): start the GUI, pick a source puck, press
  **⇄ Share map**. The new puck must be physically in that space and able to see mapped
  territory. Its existing map is archived on-device and to `~/insight-map-loader-backups/`
  first.
- **A brand-new space, nobody has a map**: wear the puck, reach 6DOF, and press
  **✚ Create map** on its card.

Confirm every puck reports the **same** `topNodeUid`, marked `(persistent)`:

```sh
./desktop/target/release/insight-map-loader mapdb
```

## 4. Run the GUI

```sh
./run-gui.sh
```

or, while changing code, the form that always rebuilds:

```sh
cargo run --release --manifest-path desktop/Cargo.toml -p insight-map-loader-gui
```

The GUI **is** the service while it is open. Do not also run `insight-map-loader run` —
they will fight over the listen port, and the loser tells you.

From the GUI: **⟳ Launch trackers**, then give the fleet a shared map, then hold the
pucks still and press **⌖ Bridge now**. The fleet banner turns green when every puck is
on the same map.

---

## Daily use

| | |
|---|---|
| `insight-map-loader status` | per puck: Insight level, battery, tracker up, VPN trap |
| `insight-map-loader mapdb` | map size, age and root uuid per puck — the colocation check |
| `insight-map-loader up` | connect, configure and launch the trackers |
| `insight-map-loader bridge` | manual override for the bridge |
| `./tools/q1sep.py` | hold two pucks together; expect a few centimetres |

**⌖ Bridge now** is needed again after anything that resets a frame: relaunching
trackers, sharing a map, restarting `trackingservice`, or a reboot. A stale bridge is not
subtle — a day-old one showed a correctly colocated puck rotated 180°. The watchdog
re-solves on its own at the next still moment; the button is an override.

**⟲ Re-sync** copies a map onto pucks *already reporting that root*. Pucks sharing a map
keep mapping independently, so their content diverges while their identity does not;
plain **⇄ Share map** skips them for that reason, and Re-sync is how you get everyone
back onto one known-good copy.

One manual step remains and is expected: the shared frame and the HMD's universe are
different spaces, so run OpenVR-SpaceCalibrator (or the game's own calibration) once per
play session — the same step every mixed-tracking FBT setup does.

## Troubleshooting

The failure modes here are mostly **silent** — a puck healthy in every indicator that
emits nothing. The two that catch everyone:

- **The guardian package must be disabled before the tracker app starts** (step 3e).
- **`adb root` does not survive a reboot** (step 3b).

| symptom | look at |
|---|---|
| puck streams nothing | `insight-map-loader status` — tracking level, tracker up, guardian off |
| poses arrive but are wrong | hold still, **⌖ Bridge now**; check `bridge.json` is fresh |
| tracker missing or duplicated in SteamVR | the driver's own log line, then `send_test_tracker.py` |
| pucks disagree in space | `tools/q1sep.py` — the only check with real ground truth |
| `unclaimed puck id N` | that puck is streaming an id no config entry claims; add it |

A symptom-keyed table with more depth is in
[docs/puck-bringup.md](docs/puck-bringup.md#troubleshooting).

## Working on the code

```sh
cargo test --manifest-path desktop/Cargo.toml     # 34 tests, must stay green
```

**Do not run `cargo fmt` across the tree.** It is not currently rustfmt-formatted, so a
sweep produces a thousand-line diff that buries your change and redirects every future
`git blame` at the reformat. Match the style around you.

**Comments explain *why*, and record what was learned.** Most of the tricky code exists
because of a specific discovered behaviour of an undocumented system; without the reason
it reads as arbitrary and gets "simplified" back into a bug.

**Much of this runs with no headsets at all:**

- the full test suite needs nothing
- `python3 desktop/steamvr_driver/tools/send_test_tracker.py --device 6` exercises the
  whole socket → device → SteamVR path
- `cargo run --manifest-path desktop/Cargo.toml --example listen -- 5180` prints what is
  arriving, one line per source per second
- the map pipeline runs offline against a pulled `mapdb`; `selftest_align.py` scores it
  against known ground truth

**Load-bearing invariants.** Breaking one produces failures that look like something
else:

- **MPT1 role ids are APPEND ONLY.** SteamVR keys pairings and room calibration off them,
  so renumbering silently rebinds a user's trackers to the wrong body parts.
- **The 68-byte packet layout is stated in three files that must agree**: `mpt1.rs`,
  `mapper_protocol.h`, `send_test_tracker.py`.
- **The byte at offset 4 means different things at each end** — from a puck it is that
  puck's identity, to the driver it is the role. The host maps one to the other.
- **Never pipe `dumpsys tracking` into anything that closes the pipe early** (`grep -m1`,
  `head`). It leaves the tracking service unavailable for seconds, reporting `Can't find
  service: tracking`, and you will blame the wrong thing. Dump to a file on-device, then
  grep the file.
- **Batch device queries into one shell round trip** — each `adb shell` costs 50–150 ms.
- **Every adb call needs a timeout.** One hung adb wedges its caller forever; a
  launch-style `adb shell` was measured stuck for over an hour.
- **`adb push` into `/vision` needs `chcon u:object_r:vision_file:s0` afterwards**, or
  trackingservice silently cannot read the file.


## Licence

GPL-3.0. See [LICENSE](LICENSE) and [THIRD_PARTY.md](THIRD_PARTY.md) for the vendored
`openvr_driver.h` (BSD-3). Contributions ship under the same licence.

This project contains **no Meta code**. It interoperates with software already on a device
you own, using interfaces recovered by observation. Device libraries, where a build needs
them, are read from your own headset and are never redistributed here.


I vibecoded this entire thing incase you coulden't tell lol
