# Bringing up a new tracker puck

From a freshly-rooted Quest 1 to a puck streaming into SteamVR in the fleet's
shared tracking frame. Rooting itself is out of scope — see
`memory/quest1-root-model.md`; this starts the moment `adb root` works.

Budget ~30 minutes, most of it waiting on the appop flush in step 4.

**Steps 1–6 are automated**, and the script verifies each rather than assuming
it worked:

```sh
./tools/q1bringup.sh <ip> --usb <serial> --apk <q1tracker.apk> --device <0-10>
./tools/q1bringup.sh <ip>          # re-check or repair an existing puck
```

It is idempotent, and it stops at the first thing that would make a later step
fail silently. Steps 7–10 need the room and the GUI, so it ends by listing
them. The rest of this file is why each step exists.

> **Never disable the DNS blackhole of Meta's domains.** It is what keeps these
> EOL headsets from pulling an OTA that would change `libtrackingengines.so`,
> whose frozen format the whole map pipeline depends on. Several system UIs
> (the dashboard, the store) are permanently broken as a result. That is the
> intended trade; route around them.

---

## 1. Wireless adb

Quest 1 is Android 10, so this is the classic `adb tcpip` route — no pairing
code, no "Wireless debugging" toggle. Over USB:

```sh
adb -s <SERIAL> tcpip 5555
adb -s <SERIAL> shell "ip -4 addr show wlan0" | grep -oE 'inet [0-9.]+'
adb connect <IP>:5555
```

**Pin the IP to the MAC in your router** (dd-wrt: Services → DHCP Server →
Static Leases). It is a DHCP lease and it *will* move otherwise, and every
tool here addresses pucks by IP. Details and the current fleet table:
`docs/wireless-adb.md`.

## 2. Root over wifi

```sh
adb -s <IP>:5555 root && adb connect <IP>:5555
adb -s <IP>:5555 shell id -u        # must print 0
```

**Root does not survive a reboot.** Re-run this after every one, or
`setprop`/`chown`/`chcon` fail *silently* and everything downstream looks
broken for the wrong reason. `tools/q1connect.sh` does the retry loop.

## 3. Install the tracker app

Build and install `android/q1tracker` (package
`com.mapperlocalizer.questtracker`). It is an OpenXR app that streams MPT1 pose
packets to the host.

```sh
adb -s <IP>:5555 install -r <q1tracker.apk>
```

## 4. Provision for off-head, unattended operation

A body puck is never worn, and a headset that believes it is off a head powers
down before anything can start.

```sh
./desktop/target/release/insight-map-loader provision      # all pucks in insight-map-loader.json
```

or by hand:

```sh
adb -s <IP>:5555 shell '
  appops set com.mapperlocalizer.questtracker SYSTEM_ALERT_WINDOW allow
  setprop persist.oculus.guardian_disable 1
  setprop persist.oculus.forceHeadsetOn 1
  setprop persist.ovr.disable.sensorproxy true
  settings put system screen_off_timeout 86400000
  settings put global wifi_sleep_policy 2'
```

These are `persist.*` deliberately, so init restores them every boot — the
volatile `debug.oculus.forceHeadsetOn` is cleared by a reboot, which is exactly
when it is needed. **Wait ~45 s before rebooting**: the appop has to reach disk,
and `provision` blocks for that reason.

## 5. Disable the guardian PACKAGE

```sh
adb -s <IP>:5555 shell pm disable-user --user 0 com.oculus.guardian
```

**Do this before the tracker app first starts, and do not skip it.** The
property in step 4 is *not* sufficient. With the package enabled, the tracker
app looks completely healthy — running, window-focused, correct config, Insight
at `6DOF Valid: Yes` — and **emits nothing at all**. An enabled package also
drives the displays to passthrough instead of dark.

The package must be *re-enabled* to create a map (step 7), which is why that is
a bracket rather than a toggle.

## 6. Add it to the fleet

Edit `insight-map-loader.json`:

```json
{ "ip": "192.168.1.13", "device": 2 }
```

`device` is the **SteamVR role id** — see the table in step 9. It must be
**unique**; two pucks sharing one id fight at packet rate and the tracker
flickers between two bodies. `Config::load` now refuses that outright.

(The separate `"role": "hip"|"ankle"` field is the legacy *alignment* role,
meaning "owns the shared frame". Colocation makes it inert. Do not confuse the
two — they collide on the word hip/waist.)

Then in the GUI press **⟳ Launch trackers**, which writes each puck's
`config.txt` (host, port, device) and starts the app.

## 7. Give it the map

This is what puts the puck in the *same tracking frame* as the others. Two
cases:

**The fleet already has a map** (usual). Start the GUI, pick a source puck,
press **⇄ Share map**. The new puck must be physically in that space and able
to see mapped territory, or it will load the map and fail to relocalize into
it. Its existing map is archived on-device and to `~/insight-map-loader-backups/` first.

**A brand-new space, nobody has a map.** Wear the puck, reach 6DOF, and press
**✚ Create map** on its card. It brackets the guardian package around the
creation and verifies the pose stream resumes afterwards.

Confirm: every puck should report the **same** `topNodeUid`, marked
`(persistent)`.

```sh
./desktop/target/release/insight-map-loader mapdb
```

Full detail, including doing it by hand: `docs/insight-map-lifecycle.md`.

## 8. Bridge

MPT1 streams the tracker app's OpenXR **LOCAL** frame, so something must map it
to the Insight world frame. Hold the pucks **still** and press **⌖ Bridge now**.

Needed again after anything that resets a frame: relaunching trackers, sharing
a map, restarting `trackingservice`, or a reboot. A stale bridge is not subtle —
a day-old one showed a correctly colocated puck rotated 180°.

## 9. Assign its SteamVR role

Pick from the dropdown on the puck's card. Applied live: it rewrites
`insight-map-loader.json`, updates the running service and reconfigures the puck.

| id | role | | id | role |
|---|---|---|---|---|
| 0 | Waist / hip | | 6 | Left elbow |
| 1 | Left foot | | 7 | Right elbow |
| 2 | Right foot | | 8 | Left shoulder |
| 3 | Chest | | 9 | Right shoulder |
| 4 | Left knee | | 10 | Camera *(MR capture, not a body joint)* |
| 5 | Right knee | | | |

Ids are **append-only**: SteamVR keys pairings, role bindings and room
calibration off the serial each id maps to, so renumbering makes it forget
every tracker you have set up.

Use **⚑** to identify which physical headset is which — it flashes the role
colour `id + 1` times. The count is the reliable part; a tri-colour LED only
has seven distinguishable colours and there are eleven roles.

## 10. Verify

- **Fleet banner green**: "all N pucks on shared map ####".
- **Slot live** for the new role, not stale.
- **Physically**: hold two pucks together and run `tools/q1sep.py`. Expect
  ~3 cm horizontal. This is the only measurement with real ground truth — two
  co-located pucks cannot lie to each other.

---

## Troubleshooting

| symptom | cause |
|---|---|
| tracker app healthy but nothing streams | guardian **package** enabled — step 5, then restart the app |
| displays show passthrough instead of dark | same |
| `setprop`/`chcon` silently do nothing | lost root to a reboot — step 2 |
| puck absent from the GUI entirely | not in `insight-map-loader.json`, or a duplicate `device` id |
| tracker rotated ~180° | stale bridge — step 8 |
| puck loads the map but never relocalizes | it cannot *see* mapped territory: wrong room, blank wall, too dark |
| puck sleeps when set down | `persist.oculus.forceHeadsetOn` missing — step 4 |
| role change appears not to apply | restart the GUI; check `insight-map-loader.json` actually changed |

## What this puck now has

Config on the device: proximity defeated, guardian package disabled, screen and
wifi sleep off, boot-start granted. On the host: an entry in `insight-map-loader.json`, a
role, a bridge, and a copy of the fleet's map.

The only per-session step is the bridge. Everything else survives a reboot
except **adb root**, which does not.
