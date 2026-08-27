# Mapper Quest Tracker

Publishes a Quest headset's own 6DoF pose as an **MPT1 tracker**, so the
SteamVR driver exposes it and OpenVR-SpaceCalibrator can align the Quest's
tracking space with the SteamVR universe.

Uses the **supported** path — the headset's inside-out pose via OpenXR. No
camera access and no root: Quest 1 never exposed raw tracking frames, but its
own pose is a first-class API. The session renders nothing (zero layers
submitted each frame), which is exactly right for a headset being used as a
tracking puck rather than a display.

Optionally, with a **UVC camera bolted on** (USB host mode is available on
Quest 1 — see below), it also becomes a **known-pose mapping camera**: each
frame is tagged on-device with the headset pose at that frame's own capture
instant and streamed as **MPF1** posed frames, so the host can build a
drift-free map by triangulation from known poses (CLAUDE.md §2.1) with no SLAM
on the device. This is additive — a headset with no camera is unchanged.

## Build

```bash
cd android/questtracker
./gradlew :app:assembleDebug
```

The OpenXR loader comes from Khronos' official Maven AAR
(`org.khronos.openxr:openxr_loader_for_android`, prefab package `OpenXR`) — no
license-gated Meta SDK needed. The runtime itself lives on the headset.

## Install and configure

```bash
adb install -r app/build/outputs/apk/debug/app-debug.apk

# target: the SteamVR PC running the mapper driver, plus an optional mirror to
# the dashboard so the live 3D view shows the Quest too
cat > /tmp/config.txt <<CFG
host=<your PC's LAN IP>
port=5180
device=0
mirror=<host>:5181
CFG
adb push /tmp/config.txt \
  /sdcard/Android/data/com.mapperlocalizer.questtracker/files/config.txt

adb shell am start -n com.mapperlocalizer.questtracker/android.app.NativeActivity
```

`device` is the MPT1 device id (0 waist / 1 left_foot / 2 right_foot) — it
decides which tracker the Quest appears as in SteamVR.

## Known-pose mapping camera (optional)

With a UVC module on the USB-C port (via an OTG adapter; a powered hub is wise
since both ends draw), add to `config.txt`:

```
cam=1
cam_host=<host>            # only used by the camera track, not shipped here
cam_port=5171              # MPF1 posed-frame TCP (distinct from MPT1's 5180)
cam_mjpeg=1
cam_w=640
cam_h=480
cam_fps=30
```

Two headless prerequisites on this lens-less headset:

1. **CAMERA permission.** Android refuses USB access to video-class devices
   without it, and there is no dialog to tap here — grant it over adb:

   ```bash
   adb shell pm grant com.mapperlocalizer.questtracker android.permission.CAMERA
   ```

2. **USB permission.** Handled automatically: attaching the camera re-launches
   the app through the `USB_DEVICE_ATTACHED` intent-filter, which grants access
   with no dialog. (The port is dual-role — `android.hardware.usb.host` is
   present — but OTG is untested electrically; a hub that supplies its own power
   is the safe first try.)

On the host:

```bash
python3 tools/posed_frame_receiver.py --out room_scan --port 5171
```

This writes `room_scan/frames/*.jpg` and `room_scan/poses.csv` (the headset pose
per frame). The **headset→camera hand-eye extrinsic** (CLAUDE.md §7 stage 2) is
still owed before those poses can be used to triangulate — the stream carries
the headset pose, not the camera pose.

**Status:** builds clean (native UVC + libusb + Java USB shim; MPF1 verified
against the receiver with synthetic packets). **Not yet run with a real camera**
— needs the OTG adapter + module, and cannot be tested while the port is
adb-tethered (same port).

## Headset preparation (both matter)

1. **Defeat the proximity sensor.** Otherwise the Quest never promotes the
   session past state 1 (IDLE) and no poses are produced. Either tape over the
   sensor inside the headset, or — verified working, and much easier for a
   device used as a puck — tell the power manager the headset is worn:

   ```bash
   adb shell am broadcast -a com.oculus.vrpowermanager.prox_close
   ```

   This does not persist across a reboot; re-send it after each boot (or use
   tape). `com.oculus.vrpowermanager.automation_disable` restores normal
   behaviour.

   Plugging the USB cable in pops a USB/MTP dialog that becomes the top
   activity, and `am start` then refuses with *"Activity not started because
   the current activity is being kept for the user"* — confusing, because the
   app neither starts nor crashes. With no lenses it cannot be dismissed by
   looking at it, so kill it:

   ```bash
   adb shell am force-stop com.oculus.os.vrusb
   ```
2. **Disable the guardian/boundary.** The app deliberately uses the `LOCAL`
   reference space rather than `STAGE` precisely because `STAGE` depends on a
   configured boundary. `LOCAL`'s origin is the headset pose at session start —
   arbitrary, which does not matter: resolving an arbitrary origin against the
   SteamVR universe is exactly what SpaceCalibrator does.

## Coordinate frames

OpenXR and OpenVR share a convention — right-handed, y-up, -z-forward — so the
axes pass through unchanged. Only the quaternion component order differs
(OpenXR `(x,y,z,w)` vs MPT1's `(w,x,y,z)`), which the sender handles.

## Status

**Verified on hardware 2026-08-02** (Quest 1 `vr_monterey`, runtime v50, Android
10 / SDK 29). All four bring-up gates pass:

1. `adb devices` sees the headset — developer mode still works on this EOL
   device.
2. `OpenXR ready` in `adb logcat -s QuestTracker`.
3. Session reaches state 5 (FOCUSED) once the proximity sensor is defeated, and
   holds it through handling and motion.
4. MPT1 arrives at **75 Hz**, `valid=1`, zero malformed packets, no dropouts.
   Stationary noise floor **0.2–0.6 mm** peak-to-peak per second.

### Two Quest-1-specific gotchas, both fixed here

Neither is obvious from the failure mode, so they are worth keeping in mind if
this is ever ported to another frozen runtime:

- **`apiVersion` must be pinned to OpenXR 1.0.** The Khronos loader AAR ships
  1.1 headers, so `XR_CURRENT_API_VERSION` requests 1.1 and the v50 runtime
  rejects it with `XR_ERROR_API_VERSION_UNSUPPORTED` (-4). Nothing here needs
  1.1.
- **`com.oculus.supportedDevices` must list only `quest`.** Listing newer names
  is not harmless: v50 parses the list during `xrCreateInstance` and
  hard-aborts inside `libvrapiimpl.so` with
  `Unknown device to support: quest3` — a SIGABRT, not an error return.

The VrApi fallback was never needed: `xrInitializeLoaderKHR` is present on v50.

## Bluetooth transport (optional)

MPT1 poses can additionally go over Bluetooth RFCOMM (SPP). Set `bt=1` in
`config.txt` and grant the runtime permission once — this headset has no dialog
you can tap:

```bash
adb shell pm grant com.mapperlocalizer.questtracker android.permission.BLUETOOTH_CONNECT
```

The app then advertises an SPP service named `MapperMPT1`. Pair the headset from
the PC's Bluetooth settings, find the **outgoing** COM port it creates, and run
the bridge:

```bash
python a Bluetooth bridge (not shipped; the tracker streams over UDP) --port COM7
```

The bridge forwards each 68-byte frame to `127.0.0.1:5180`, which is where the
driver already listens — so **the driver needs no changes** and can be restarted
independently. UDP keeps running at the same time, so the two transports can be
compared rather than swapped blind.

**Timing caveat, stated plainly:** Bluetooth adds more latency *and more jitter*
than Wi-Fi. The driver's clock-offset estimator ages poses from the producer's
own `t_ns`, which absorbs the constant component — but jitter becomes
extrapolation error, and pose timing is what this pipeline is most sensitive to.
Expect robust, not smoother than UDP.
