# Quest 1 -- calibration and IMU

What a SLAM frontend needs besides the images, and where it lives. Companion to
[quest1-hal.md](quest1-hal.md), which covers the camera path.

## Calibration

Factory calibration is plain JSON on `/persist`, world readable. No HAL call
needed -- `ICameraProvider::getCalibrationData` exists but is redundant.

```
/persist/calibration/camera_calibration_v2.json   4x camera, the one to use
/persist/calibration/camera_calibration.json      older, smaller
/persist/calibration/imu_calibration.json
/persist/calibration/mag_calibration.json
/persist/calibration/online/<id>                  runtime-refined, rewritten live
```

`tools/pull_calibration.sh` fetches them. They carry the headset's serial number
and are per device, so they are deliberately not checked in.

### Cameras

Four entries, each `OV7251`, global shutter, 640x480:

```json
"Projection":  { "Model": "PinholeSymmetric",
                 "Coefficients": [191.888, 316.714, 247.140] },     // f, cx, cy
"Distortion":  { "Model": "Fisheye62", "Coefficients": [ ...8... ] },
"DeviceFromCamera": [ ...16... ]                                    // 4x4 row major
```

`PinholeSymmetric` is a single focal length for both axes. `Fisheye62` is the
6-radial + 2-tangential fisheye model (k1..k6, p1, p2).

Extrinsics translations, i.e. where each camera sits relative to the device
origin, in metres:

| camera | x | y | z |
|---|---|---|---|
| 0 | -0.05583 | -0.03940 | -0.07042 |
| 1 | -0.07430 | +0.03226 | -0.06992 |
| 2 | +0.05573 | -0.03949 | -0.07027 |
| 3 | +0.07341 | +0.03240 | -0.06966 |

So 0/2 form a ~111 mm horizontal stereo pair and 1/3 a ~147 mm one.

### IMU

```json
"DeviceFromImu":  [ ...16... ],          // identity rotation, t = (0.040475, -0.033575, -0.067834)
"Gyroscope":     { "Model": "Linear",
                   "RectificationMatrix": [ ...9... ],
                   "Offset": { "Model": "Constant", "ConstantOffset": [ ...3... ] } },
"Accelerometer": { "Model": "Linear",
                   "RectificationMatrix": [ ...9... ],
                   "Offset": { "Model": "LinearTemperatureDependence",
                               "OffsetAtZeroDegC": [ ...3... ],
                               "OffsetTemperatureCoefficient": [ ...3... ] } }
```

Both rectification matrices are near a signed permutation of the identity, so
the IMU axes are swapped and flipped relative to the device frame -- do not skip
applying them. The accelerometer offset is temperature dependent, and the sample
stream carries the die temperature, so the correction is directly applicable.

## The IMU stream

`vendor.oculus.hardware.sensors@1.0::IImu`, three methods:

| tx | method |
|---|---|
| 1 | `getProperties() -> (Result, MotionSensorProperties)` |
| 2 | `prepareStream(MQDescriptor<ImuData, UNSYNC>, ISensorClient, FmqConfig) -> Result` |
| 3 | `streamControl(ISensorClient, StreamCommand)` |

**This FMQ works**, which is worth stating plainly because the camera one does
not. The difference is visible in the signature: `ICameraStream::prepareStream`
has a reply callback and hands back a queue of the HAL's own that it then never
writes; `IImu::prepareStream` has no callback, returns `Result` directly, and the
HAL writes into the descriptor you pass it. Registration is otherwise identical
-- serve an `ISensorClient`, pass an ashmem-backed queue and a shared EventFlag
with non-zero notification masks, then `streamControl(START)`.

Measured: **6003 records over 6.01 s = 1000 Hz, zero gaps over 2 ms.**

`ImuData` is 64 bytes:

```
+0x00 u64    timestamp, ns, sensor clock
+0x08 u64    timestamp, ns, a second time domain
+0x10 u64    timestamp, ns, a third
+0x18 u64    -1 (constant)
+0x20 f32    temperature, degrees C          (~46 C, agrees with the thermal zones)
+0x24 f32x3  accelerometer, m/s^2            (magnitude 9.88 at rest)
+0x30 f32x3  gyroscope, rad/s
+0x3c f32    unused
```

Values are raw -- apply the rectification and offsets from
`imu_calibration.json` before use.

Two things that cost time and are easy to get wrong:

* **libfmq read/write pointers are byte offsets, not record counts.** Indexing
  the ring by the raw counter reads the wrong slot and mostly returns zeros.
* **The writer stalls if the reader never advances the read pointer.** It ran
  for about a second -- the queue depth -- and then froze with the timestamps
  repeating. Publish `*readPtr` as you consume and wake the read-notification
  bit; with that it runs indefinitely at a flat 1 kHz.

`out/q1imu` (source `tools/q1imu.cpp`) does all of the above and prints the
decoded stream.

## Time domains, and aligning cameras to the IMU

### What the three IMU timestamps are

Measured by reading the newest record and the host clocks at the same instant:

| field | relative to `CLOCK_BOOTTIME` | what it is |
|---|---|---|
| `+0x00` | -6.0739 s, stable | the syncboss MCU clock |
| `+0x08` | -1.8 to -2.9 ms | sample instant, sensor side |
| `+0x10` | **-0.25 to -0.98 ms** | effectively `CLOCK_BOOTTIME` |

`CLOCK_BOOTTIME` and `CLOCK_MONOTONIC` differ by ~100 ns on this device, so
either works. **`+0x10` is directly usable as a host timestamp**, and the sub-
millisecond lag is our own read latency, not the sensor's.

`+0x00` is the interesting one: a monotonic MCU clock offset from boot by a
fixed ~6.074 s. That is the domain the cameras are synced in --
`CameraMetrics` counts 87668 sync pulses against 87647 headset framesets, one
pulse per frameset.

### What the cameras give you, and what they do not

Nothing. Row 0 is 640 bytes but only the first ~96 are populated and the rest is
zeroed; scanning every offset and alignment for a u64 within 10% of
`CLOCK_MONOTONIC`, `BOOTTIME`, `REALTIME` or their microsecond forms finds no
match. **Camera frames carry a counter, not a time.**

The counter advances **1 per sensor frame at 60 Hz** and is shared across
cameras, so in principle
`t_frame = epoch + counter x 16.67 ms` in the syncboss domain, convertible to
`BOOTTIME` with the ~6.074 s offset above. The epoch is the missing piece.

### What did not work, and why

Correlating the frame counter against the syncboss clock by latching a camera
frame and reading the newest IMU sample gave a slope of 4.9 ms/count (203 Hz)
and then -5.2 ms/count (-193 Hz), with residuals larger than the whole sampled
span. Both fits are meaningless. Two reasons, both worth knowing before
repeating the exercise:

* **The counter has to come from one camera.** Each camera's cursor advances
  independently and they can sit a frame apart, so taking the counter from
  whichever camera happens to be tagged makes it jump between them.
* **A camera may tag only one exposure class.** Observed live: cam2 tagging all
  16 buffers with consecutive counters (124..148) while cam0 tagged 9 of 16,
  all even (130, 132, ... 148) -- i.e. only the long-exposure frames, with the
  short frames in between still consuming a counter value. Pinning a reference
  camera is necessary but not sufficient; it also has to be one that is
  currently tagging every frame.

So a valid measurement needs a reference camera verified to be tagging
consecutively (check with `q1diag ring` first), samples taken only when its
counter actually changes, and a fit against the *lower envelope* of the
residual rather than the mean, since latch latency is one-sided -- never early,
sometimes late.

### How Insight actually does it: the syncboss MCU stream

The sensors HAL (pid 742) owns four char devices from the `oculus_syncboss` SPI
driver: `/dev/syncboss0`, `syncboss_stream0`, `syncboss_control0`,
`syncboss_powerstate0`. `syncboss_stream0` is the timestamped sensor feed from
the MCU, and **it is readable by a second reader** -- opening it while the HAL
holds it works and delivers data.

Records are self-framing: `01 03 00 <type> 00 <len>` followed by `len` bytes.
Measured over a 6 s capture, 6799 records, no resyncs:

| type | bytes | rate | payload |
|---|---|---|---|
| `0x50` | 42 | 999.4 Hz | u64 ts_us, then accel (g) x3, gyro x3, temperature -- the IMU |
| `0x51` | 28 | 30.06 Hz | u64 ts_us, 3 floats ~1e-4 -- magnetometer |
| `0x55` | 18 | 71.8 Hz | u64 ts_us, u32 +1/record -- display vsync (72 Hz panel) |
| `0xe0` | 20 | **30.00 Hz** | u8 subtype, u64 ts_us, u32 counter, u8 `counter % 16` |
| `0xd9` | 39 | bursts | ids, occasional |
| `0x8f` | 29 | occasional | -- |

Timestamps are microseconds in the syncboss domain, the same clock the IMU
carries at `+0x00`. Since every IMU record carries *both* that clock and
`CLOCK_BOOTTIME` at `+0x10`, the two domains can be tied together continuously
without needing `getTimeTranslation` at all.

**`0xe0` is the camera frame event.** Its trailing byte cycles 0..15 and is
exactly `counter % 16` -- and the camera rings are 16 buffers deep. Watching it
live, the index walks 0,1,...,15,0,... in order at a dead-flat 33333 us
interval. So the MCU reports, per frame, a hardware timestamp and the ring slot
the frame is being written into. That is the frame timestamp, and it carries
none of the arrival jitter that timing on the host does.

#### The 30 Hz is not a factor-of-two mystery

`0xe0` runs at 30 Hz against what looked like 60 fps of frames. It is not
covering half of them -- **30 Hz is the true rate of a single stream.** The
HAL's own counters, sampled 20 s apart:

| counter | rate |
|---|---|
| `headset_frameset_count` | 30.15 Hz |
| `hand_frameset_count` | 30.10 Hz |
| `syncpulse_count` | 30.10 Hz |
| `controller_frameset_count` | 0 (no controllers powered) |

So HEADSET is 30 fps, HAND is 30 fps, and they interleave into the **one shared
buffer pool** all three FrameTypes hand back. The 60 Hz we measure walking the
ring is those two streams together, not one stream at 60. One sync pulse per
HEADSET frameset, 1:1.

That also explains the exposure interleave, which had looked like a quirk: the
long exposure is HEADSET (environment tracking) and the short one is HAND, which
wants a short exposure. They alternate because they are different streams
sharing a sensor, not because one stream dithers its exposure.

The practical consequence is worth being clear about: **`Exposure::LongOnly` at
30 fps is not half rate, it is the native SLAM frame rate.** Taking `Any` at
60 fps does not get you more environment-tracking frames, it interleaves the
hand-tracking ones.

A candidate for a similar 30 Hz record belonging to the HAND stream was ruled
out: `0x51` alternates almost perfectly with `0xe0`, but the two run at
33264 us and 33333 us respectively and drift past each other by ~66 us per
cycle, so they are independent sources. `0x51` is the magnetometer on its own
oscillator; the alternation is two near-equal rates sliding, not a lock.

#### The frame-to-record mapping

Correlating *timestamps* was the wrong approach and drowned in read latency.
Both sides expose a monotonic counter, so correlate counter to counter and the
latency stops mattering:

```
unwrapped_frame_tag = 2 * sb_counter + K
```

Measured over 60 events spanning several counter wraps: median -767874, full
range -767876..-767874, 49/60 exactly on the median. No drift. The +-2 outliers
are the ring poller sampling between updates, not the relationship moving.

The factor of two is the two streams sharing the pool: the tag advances +1 per
pool write at 60 Hz while `0xe0` counts HEADSET frames at 30 Hz. `K` is constant
for a session and has to be established once by correlation, because it depends
on when each side started counting.

So, for a frame whose tag counter is `T`, its capture time is the `ts_us` of the
`0xe0` record with `sb_counter == (T - K) / 2`, in the syncboss domain, and every
IMU record carries both that clock and `CLOCK_BOOTTIME` so it converts straight
to the host timeline. That is a hardware timestamp per frame with none of the
arrival jitter host-side timing has.

`out/q1map` (source `tools/q1map.cpp`) does the correlation and prints the
residual.

Two corrections to earlier readings of this data, both of which cost time:

* **The `0xe0` trailing byte is not a ring slot.** It is exactly
  `counter % 16`, so it carries no information beyond the counter. It looked
  like a slot index because it cycles 0..15 and the rings happen to be 16 deep.
  The pool is actually allocated from a free list -- observed write order 11, 6,
  13, 0, 3, 5, 12, 14, 15, 2, ... -- so there is no index to follow, and
  indexing the ring by it reads essentially arbitrary buffers.
* **The frame tag counter is 8 bit, not 16.** Byte 88 is always zero and byte 89
  takes all 256 values, verified by sweeping. Reading it as a big-endian u16 at
  +88 gives the right number but implies a range it does not have, and any
  comparison with `<` or `>` silently stops working after 256 frames. Compare as
  `(int8_t)(a - b)`.

#### Also worth knowing: the pool is written in pairs

Watching writes land, the interval alternates ~11 ms then ~22 ms rather than a
steady 16.7 ms. The two streams are captured close together within each 33.3 ms
cycle -- HEADSET then HAND about 11 ms later -- and then the sensors idle for
the rest of the cycle. Anything that assumes evenly spaced 60 Hz writes will be
wrong about frame timing by up to 5 ms.

### The older idea, kept for context### The older idea, kept for context

`IExternalTimingProvider` also exists in the HAL, with an
`ExternalTimestampData` FMQ shaped exactly like the IMU one that works
(`prepareStream` returning `Result` directly, no reply callback). It is **not
registered** on this device -- `lshal` lists only ICameraProvider,
IControllerProvider, IIad, IImu, IMag and IPowerstate -- so it cannot be reached
over HIDL here, but it is the mechanism the design intends.

For VIO in the meantime: a *constant* unknown camera-IMU offset is something
VINS-class estimators solve for online. Jitter is what hurts, and deriving frame
times from the counter rather than from arrival time removes it.

---

# Insight's pose shared memory -- the fast path, and its gate

`dumpsys tracking` yields the head pose at ~20 Hz through a text dump. The real
consumers (vrshell, systemdriver, mrsystemservice, shellenv, guardian -- the
processes dumpsys lists under "Head Tracker Memory") instead **map a shared
memory region** and read pose at render rate with no binder call per sample.
That path is now fully mapped.

## The contract

Recovered from `libossdk.oculus.so`'s AIDL-generated proxy with `llvm-nm -DC`
and `llvm-objdump`, not guessed. Note the binder interface descriptors are
**String16**, so plain `strings` misses them -- use `strings -e l`.

```
service name  "tracking"
descriptor    "oculus.internal.tracking.ITrackingService"
  tx 1  getSharedMemoryFileDescriptor(ITrackingServiceClient, SharedMemoryRequest, out PFD)
  tx 2  getTrackingSocketFileDescriptor(out PFD)
  tx 3  unregisterClient(ITrackingServiceClient)

callback      "oculus.internal.tracking.ITrackingServiceClient"
  tx 1  getTrackingModeFlags(out int)      -- service asks the client what it wants

SharedMemoryRequest::writeToParcel  writes exactly ONE int32 (the tracker selector)
SharedMemoryRequest::valid()        is `type < 8`  -> selectors 0..7
Parcel::writeParcelable calls writeToParcel directly: no null marker, no length.
Reply is binder::Status then a bare fd (Java: readException(); readFileDescriptor()).
```

The eight selectors line up with the trackers `dumpsys tracking` lists: Head,
Body, Controller, Eye, Face, Hand, Orthofit, Anchor. The service logs the name
it resolved -- `D TrackingService: getSharedMemory: <pid>, AnchorTracker` -- so
the mapping can be read straight out of logcat per request.

## Two gates, in order

**1. The caller must be a package.** As root the service refuses before doing
anything:

```
E TrackingService: No packages for calling uid 0, pid 7242
```

It resolves the *calling uid's package name* first, and uid 0 has none. Same
shape as the anchor API: run from an installed app, not `app_process`.

**2. The caller must hold VR focus.** From a real app (`com.q1.anchor`,
uid 10074) the request gets through to the actual check and fails with a clear
reason:

```
SecurityException: requested shared memory region when not registered/focused
```

`trackingservice` carries `FocusType::HeadTracking` / `FocusType::InputTracking`
and a `VrFocusManager`. `getTrackingSocketFileDescriptor` (tx 2) is gated the
same way -- it returns a null fd. This is a deliberate privacy boundary: head
pose goes to the focused immersive app, not to background processes.

**Focus cannot be asserted from an ordinary app.** `oculus.internal.IVrFocusService`
(service `vrfocus`) exposes `setAppState(int,int)` (tx 6), `getTopActivity`
(tx 8), `unregisterVrFocusListener` (tx 3), `getClientFocusStatus`,
`registerVrFocusListener`, `getImmersiveApp`, `getForegroundApps` -- but every
call from our app returns a bare `SecurityException`. The focus service is
system-privileged.

## Where that leaves the fast path

The only remaining legitimate route is to **genuinely be the focused immersive
VR app**: give the puck app a VR Activity, launch it to the foreground, let
vrshell grant it focus, and then the shared memory opens. For a tracker puck
that is arguably the correct end state anyway -- the puck runs nothing else --
but it is a real piece of work (VR manifest, activity, keeping it foregrounded)
and untested.

Until then, `dumpsys tracking` remains a working, ungated ~20 Hz pose source:

```bash
adb shell "dumpsys tracking | grep -A4 '  Hmd:'"
#   Pose: (rot=( qx, qy, qz, qw), trans=( x, y, z))   -- verified bit-stable when static
```

Tools: `tools/q1mapjava` `PoseTool` (root/app_process probe, hits gate 1) and
`tools/q1anchorapp` `PoseReceiver` (`probe` / `dump` / `watch` / `socket` /
`focus` -- runs in-app, reaches gate 2).

## Attempting the fast path with a VR activity -- result

The one route left was to genuinely hold VR focus, so `tools/q1anchorapp` gained
a `VrActivity` declaring exactly the two markers the Oculus system services look
for (both found referenced in `oculus-system-services.jar`):

```xml
<meta-data android:name="com.samsung.android.vr.application.mode" android:value="vr_only"/>
<intent-filter>
  <action android:name="android.intent.action.MAIN"/>
  <category android:name="android.intent.category.LAUNCHER"/>
  <category android:name="com.oculus.intent.category.VR"/>
</intent-filter>
```

**It half-worked, and the half that worked is worth keeping.** Launching it
(after `am force-stop com.oculus.os.vrusb` -- the MTP alert activity squats in
the foreground whenever USB is attached and blocks `am start`) makes our app the
resumed activity:

```
mResumedActivity: ActivityRecord{... com.q1.anchor/.VrActivity t13}
```

and the service now resolves us properly, clearing gate 1:

```
D TrackingService: getSharedMemory: com.q1.anchor, AnchorTracker     <- package, not "uid 0"
```

But the memory is still refused: `not registered/focused`, over 15 attempts
across 15 s. **Being the resumed *Android* activity is not the same as being the
*immersive VR* app.** VR focus is granted by vrshell to an app that actually
enters VR mode (`vrapi_EnterVrMode` -- EGL context, compositor session, frame
submission); a 2D activity with the VR manifest markers never gets it.

### Why the existing clients get it

Every process in dumpsys's "Head Tracker Memory" list is either a system
component or a real VR app. Decisively, `mrsystemservice` is a **non-rendering
daemon running as uid `system`**:

```
system  852  mrsystemservice
system  25990  trackingservice
```

So the gate is effectively **uid `system` OR a genuinely focused immersive VR
app**. There is no debug-property bypass in the binary.

### The three ways in, and where they stand

| route | status |
|---|---|
| run as uid `system` (a `/system/priv-app` install) | needs `/system` writable, i.e. verity off via fastboot -- **off-limits for this project** |
| genuine VR-mode entry via VrApi | possible but a real native VR app (EGL + vrapi + render loop); **untried** |
| `dumpsys tracking` at ~20 Hz | **works today, ungated** -- and bit-stable when static |

For the Insight-hybrid the ~20 Hz path is enough to build and prove the
cross-puck alignment, because the alignment transform only needs to update at
place-recognition rate; the high-rate per-puck pose is what would eventually
want the shared memory.

## Frame timestamps: read time vs the hardware clock

`q1record` stamps a frameset when `Camera::next()` returns. That is *read* time
and includes HAL detection plus copy latency, so it is not when the shutter
fired. Measured against the sensor's own frame counter over a 30 s recording:

```
true jitter: std 2.8-3.2 ms, p95 ~5-6 ms, occasional |45| ms outliers
```

At 30 fps that is a large fraction of a frame period, and visual-inertial
fusion is sensitive to camera-IMU timing.

**The fix needs no new device work.** Every frameset already carries the
sensor's frame counter, and that counter ticks at **exactly 60.00 Hz**
(measured 16.6665 ms/tick over 1800 ticks, on both pucks independently). So the
counter is itself a clean hardware time base. `tools/q1retime.py` fits

```
t = a * counter + b        (least squares, outliers rejected)
```

over a recording and regenerates every frame timestamp from it, renaming the
PNGs and rewriting the EuRoC `data.csv`. Jitter goes to zero; timestamps become
exact multiples of 16.667 ms. The original read-time csv is kept alongside as
`frames.csv.readtime`.

What this does **not** fix: `b` absorbs the mean read latency, so a constant
unknown offset to the IMU clock remains. That is far less damaging than jitter
(a VIO time-offset estimator can absorb a constant), but if it ever matters the
syncboss `0xe0` camera-frame events carry true hardware capture timestamps and
the IMU records carry (sensor, boottime) pairs to convert them -- see the
syncboss section above.

Worth stating plainly: retiming was done on the hypothesis that it would fix
ORB-SLAM3's tracking failure. **It did not** -- that turned out to be an IMU
initialisation gate. The retiming is still correct and worth keeping for any
VIO, but it was not the blocker.
