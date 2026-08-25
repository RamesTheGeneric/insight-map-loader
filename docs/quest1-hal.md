# Quest 1 (`monterey`) -- HAL reverse engineering

Android 10 / API 29, adbd already root (`u:r:su:s0`). SELinux is Enforcing but
the su domain is unconstrained, so nothing here needs `setenforce 0`.

`./deploy_q1.sh serve`, then open **http://localhost:8080/** (the script sets up
the USB `adb forward` for you). `/stream` is the raw MJPEG, `/snapshot` a single
JPEG, `/stats` a JSON counter feed.

The sensors run at 60 fps, alternating two exposures (see below). Measured at
1280x960 over USB:

| | fps | p50 | p95 |
|---|---|---|---|
| both exposures (`--exposure any`) | 56 | 17.2 ms | 28.1 ms |
| long exposure only (`--exposure long`, the default) | 30 | 33.1 ms | 44.5 ms |

The encoder is not the limit either way -- it does 1280x960 in 10.9 ms on four
threads, against a 16.6 ms frame budget.

## How Insight gets its feed, and how it knows what it is looking at

`lshal debug vendor.oculus.hardware.sensors@1.0::ICameraProvider/default` dumps
the HAL's whole state, and it answers most of this directly.

**There are three streams, and `FrameType` selects between them.** Confirmed by
launching with `--ft N` and watching where our client shows up (the
`human-readable-identifier` in the dump is exactly the `SensorClientInfo.name`
we return from `getSensorClientInfo`):

| FrameType | Stream | our client appears as |
|---|---|---|
| **2** | **HEADSET** | started |
| 3 | CONTROLLER | stopped |
| 4 | HAND | started |

**HEADSET is the environment-tracking (SLAM) stream**, and FrameType 2 is what
`q1serve` uses. Meta's own clients on pid 853 (`trackingservice`) are named
`IOT`, `Obj`, `ExpCtl` on HEADSET, `Hand`/`Obj`/`ExpCtl` on HAND, and `Cnst` on
CONTROLLER. The same dump carries `CameraMetrics`:

```
headset_frameset_count: 87647     syncpulse_count: 87668
hand_frameset_count:    81456     invalid_frame_tag: 17
controller_frameset_count: 6150
```

All three streams are views on **one shared buffer pool** -- the dmabufs handed
back for FrameType 2, 3 and 4 are the same, and each ring advances at the same
60 Hz. So subscribing to HEADSET does not by itself get you only HEADSET frames.

**Per-frame metadata is embedded in row 0 of the image.** That `invalid_frame_tag`
counter and the service's `syncboss_camera_set_frame_tag_mode` /
`syncboss_camera_decode_metadata` exports point at it: each frame carries a
header in its first row, visible as a dashed line along the top edge of every
camera. Big endian, with a single-byte field at offset 14 that shifts everything
after it:

```
[0..2]   magic 00 00 01
[3..5]   a repeated MARKER byte -- NOT a constant. Firmware varies it with
         tracking state and it differs per device; seen as f0, and as 40/50
         (long) vs 20/30 (short). Validate the header on the dimensions below,
         never on this byte.
[6..7]   exposure -- the real long/short discriminator, in sensor units.
         e.g. long 250 / short 189 on one puck, 279 / 2 on another.
[15..16] width  = 640     <- use these two as the header's magic
[17..18] height = 480
[29..30] tag = 0x1111
[31..34] frame type: 2 = HEADSET, 3 = CONTROLLER, 4 = HAND
         (NOTE: both exposure classes arrive on type 2 -- streamType does
          not separate long from short; classify by exposure.)
[88..89] frame counter, identical across all four cameras
```

**Watch out:** an earlier parser hardcoded bytes [3..5] as `f0 f0 f0`. The
firmware later emitted `40/50/20/30` there instead, so every frame was rejected
as untagged and exposure classification silently fell back to frame *brightness*
-- which is fine for a static scene but fragile for a moving puck through varied
lighting (auto-exposure can converge the two classes' brightness while their
exposure *settings* stay distinct). Fixed by validating on the 640x480
dimensions instead and reading the exposure value directly.

This is how the real consumer knows what each frame is. It is *not* the FMQ:
`prepareStream` returns a genuine queue (1 x 984 bytes, its own ashmem and
EventFlag) and our registration completes -- the HAL calls back into our
`ISensorClient` -- but the write counter never leaves 0. Sweeping
`StreamCommand` 0-3, supplying correct EventFlag notification masks, and waking
the HAL's own flag all changed nothing. `q1serve fmq` reproduces it.

**The tag is not universal, and it is not even stable.** `q1serve ring` prints
every buffer's counter per camera. Two observations minutes apart:

```
cam0: 16/16 valid    cam1: 8/16    cam2: 16/16    cam3: 0/16
cam0:  8/16 valid    cam1: 0/16    cam2: 16/16    cam3: 0/16
```

So which cameras tag, and how many of their frames, changes while the headset is
running -- presumably `syncboss_camera_set_frame_tag_mode` following whatever
tracking features are active. cam2 has tagged every frame every time we looked;
nothing else can be relied on. That makes the content-based fallback
load-bearing rather than an edge case, and it is why `q1serve` classifies each
camera independently instead of trusting one.

## What differs from the Quest 2 HAL

The Quest 1 ships an older revision of the same `.hal`, so the two are *not*
wire-compatible. `ISensorClient` is the only interface that is identical, which
is lucky because it is the one we have to implement.

| | Quest 2 | Quest 1 |
|---|---|---|
| `getStream` | `(string system, string purpose)` | `(FrameType)` -- one enum; 2, 3 and 4 all resolve |
| `sizeof(FrameSet)` | 1056 | **984** (0x3d8) |
| `SensorClientInfo` | 40 bytes | **16** -- a bare `hidl_string` |
| `prepareStream` returns | `MQDescriptor<FrameSetRecycleInfo>` | `MQDescriptor<FrameSet>` |
| buffer pool | `vec<hidl_handle>`, 5+5 | `vec<ImageBufferHandle>` = `{hidl_handle meta; hidl_handle image}`, **16 deep** |
| HAL lib | `/odm/lib64/` | `/vendor/lib64/` |

`ICameraProvider` transaction codes are 1–9 in declaration order (getProperties,
getCalibrationData, getStream, get/setUtilityFrequency, get/setChannels,
setFrameRate, getRawImageMode); `ICameraStream` 1–12 (getMetadata,
writeSessionOcalData, prepareStream, getConfiguration, streamControl,
start/stopOverridingExposureSettings, setExposureGain, setPhaseOffset,
getFrameRate, setCameraSyncMode, setResolution).

Each buffer's `metadata` handle is a 64-byte ashmem block holding a *static*
gralloc descriptor -- buffer index, 640x481, stride 640, size 0x4c000 -- not a
per-frame header. The `image` handle is a 311296-byte dmabuf.

Five symbols our binary needs are Android 11+ additions missing on API 29
(`getRequestingSid`, `getMinSchedulerPolicy`, `BnHwBase::checkSubclass`,
`return_status::onValueRetrieval`, `atrace_get_enabled_tags`); `q1_stubs.cpp`
supplies them and explains why each is inert here.

## Keep the headset awake

`deploy_q1.sh` broadcasts `com.oculus.vrpowermanager.prox_close` and sets
`debug.oculus.forceHeadsetOn 1`. This matters for more than convenience: asleep,
the cameras fall to a dim near-static image, auto-exposure stops adapting, and
frame detection (below) has almost no signal. Undo with
`am broadcast -a com.oculus.vrpowermanager.prox_open`.

## Picking the newest frame

Three candidate signals, only one of which survives contact:

* **The FMQ is dead.** `prepareStream` returns a real queue -- 1 x 984 bytes,
  its own ashmem and EventFlag -- and our registration genuinely completes: the
  HAL calls back into our `ISensorClient`, and `lshal debug` lists us as a
  started client. But it is never written. It is a 1-element
  `kUnsynchronizedWrite` queue, i.e. a mailbox (writer overwrites in place,
  reader takes the latest), so trusting our grantor offsets is not good enough
 -- the honest test is to checksum the whole 4096-byte page, and it is all
  zeros and stays all zeros. Sweeping `StreamCommand` 0-3, supplying correct
  EventFlag notification masks and waking the HAL's own flag changed nothing.
  `q1serve fmq` reproduces all of it. The Quest 2 code never used the FMQ
  either.
* **The per-buffer metadata handle is static** -- a gralloc descriptor (index,
  640x481, stride, size), no counter. Rows 481-483 are zeroed.
* **The row-0 frame header does carry an exact counter**, identical across all
  four cameras -- but only where the camera tags its frames, and which cameras
  do that varies while the device runs (see above). So it cannot serve as the
  frame clock for all four.

So recency still has to be recovered from content. `q1serve` compares a full
640-byte row per buffer between polls. These are non-coherent dmabuf mappings,
so a buffer nobody touched still reads a few bytes different as cache lines
refill, but a real rewrite replaces the whole row. Measured over 3200 polls of
all 16 buffers: 55631 reads differed in 0-31 bytes, 20 in 32-63, and real
rewrites land at 64+. Anything from 64 to 384 gave exactly 60.0 fps, so the
threshold (256) sits in a wide flat region rather than on a knife edge.

Two things this got wrong on the way, both of which showed up as a stream
running *above* the 60 fps sensor rate -- a useful smell, since you cannot
publish more frames than the sensor produces:

* publishing when any *single* camera advanced. The four are hardware-synced but
  their DMA completions land apart, so that emitted up to four mosaics per
  sensor frame, three-quarters of each stale.
* treating any content change as a new frame, which rode the cache noise to
  ~75 fps of torn output.

The HAL also does not fill every slot -- one buffer per ring is never written --
so the cursor takes the newest settled buffer rather than walking strictly to
index+1, which dead-ends on the unused slot.

## Not tearing the frames

Two things had to be right before the image was clean.

**Row 0 is metadata, not image.** It renders as a dashed line of encoded bytes
along the top edge of every camera. `kCTop` starts the image at source row 1, so
the header never reaches the encoder.

**The buffer has to be finished, and stay finished.** Two separate problems:

* *Completion.* The settle probe originally read row 240. Frames land
  top-to-bottom, so a mid-frame probe going quiet only proves the top half
  arrived -- the bottom could still be in flight, and the lower part of the image
  tore. Moving the probe to the bottom fixed that but made a poor detector (dark,
  low contrast region: 22 fps with a 103 ms p95). So there are now two probes: a
  full row at 240 for detection, 160 sampled bytes of row 478 for completion.
  A buffer ticks when either moves and is only eligible once both are quiet.
* *The write-during-read race.* A buffer could pass the settled test and then
  have the next frame written into it while we were still copying its 300 KB
  out, giving a new top and an old bottom. The cursor now stands one frame back
  (the second newest settled buffer), which puts a full 16.6 ms between the
  writer and us for one frame of extra latency. `mosaicIntact()` re-reads two
  source rows after the copy as a backstop and drops the frame if they moved;
  `/stats` reports that as `torn`, which now stays at 0.

Largest row-to-row mean jump inside a quadrant, over 80 frame-cameras: median
2.6, p90 4.3. A tear used to show up as a step of 40+.

## Interleaved exposures (flicker)

The HAL alternates two exposure levels frame by frame. This is two *streams*
sharing the sensors, not one stream dithering: the long exposure is HEADSET
(environment tracking) at 30 fps and the short one is HAND at 30 fps, confirmed
against the HAL's own frameset counters.
With no controllers powered on the short frames look like a plain darker copy of
the same scene (5% RMSE after normalising), which is why this reads as a 30 Hz
flicker rather than as an obviously different feed.

`Options::exposure` picks how to handle it. The library classifies against the
two exposure *levels* rather than a ratio of a running maximum; a ratio test
drifts as auto-exposure moves and let about one frame in ten of the wrong class
through.

| `Exposure` | `q1serve --exposure` | fps | output means over 16 frames |
|---|---|---|---|
| `LongOnly` *(default)* | `long` | 30 | `274` x16, dead flat |
| `Any` | `any` | 56 | `274 227 274 228 ...` |
| `ShortOnly` | `short` | 30 | the hand-tracking class |

30 fps is not a compromise here: it is the native rate of the HEADSET stream.
The 60 Hz seen walking the ring is HEADSET and HAND interleaved into one shared
pool -- see [quest1-sensors.md](quest1-sensors.md#the-30-hz-is-not-a-factor-of-two-mystery).

Three things had to line up before `LongOnly` was actually flat:

* **Classify per camera.** Each runs its own levels (cam0 alternates 737/342
  while cam3 does 593/316) and each camera's cursor advances independently, so
  they can land a frame apart. Classifying on one camera and publishing all four
  put the odd quadrant on the wrong side of the interleave.
* **Every camera has to agree** before a frameset is published. This is what
  takes the rate from 33 fps (leaking short frames) to a clean 30.
* **The untagged cameras need a fallback.** Tagging is partial and varies at
  runtime, so any camera may be untagged at any moment; those cluster on frame
  brightness instead, which the two exposures make cleanly bimodal.

`Any` stays approximate. `FrameInfo::gainToLong` gives the factor to bring a
short frame up to the long one, but scaling cannot undo highlight clipping, and
on an untagged camera the ratio is estimated from scene brightness rather than
read from the sensor. Use `LongOnly` if you want photometric consistency, which
a SLAM frontend does.

Forcing a single exposure would remove the interleave entirely.
`startOverridingExposureSettings` succeeds (exception=0) but does not change the
schedule on its own; it would need `setExposureGain`, whose `ExposureGainSettings`
layout is not reversed yet. It would also take the setting away from Meta's
tracking, so it stays opt-in behind `--override-exposure`.

## The encoder

`fastjpeg.h`, a self-contained baseline grayscale JPEG encoder. The Quest 2 one
called `cosf()` once per (coefficient, sample) pair -- 1024 transcendental calls
per 8x8 block, ~20M per frame -- which was the framerate ceiling. This uses the
AAN float DCT with the descale folded into the quantization reciprocals, a
64-bit bit writer, `__builtin_clz` for magnitude categories, and splits the image
into per-thread bands separated by restart markers. 44.8 dB PSNR at q=75.

On device at 1280x960 q=70: **10.9 ms** with 4 threads, 16.2 ms with 1.

## libjpeg-turbo

Built and wired in -- `./build_libjpeg_turbo.sh`, then `--enc turbo`. The
`libjpeg.so` in the repo root is an Android 34 build and will not load on API 29;
this cross-builds the vendored source for arm64/API 29 with full NEON SIMD.

It is **not** faster here: 16.18 ms/frame against `fastjpeg`'s 16.20 ms
single-threaded, i.e. a dead heat, and it loses to the 4-thread configuration
(10.9 ms). Grayscale is why -- there is no color conversion or chroma subsampling
for its SIMD to accelerate, and both encoders end up bound on scalar Huffman
coding. It is kept as a cross-check and a fallback; `fastjpeg` stays the default
because it threads.

## Reproducing the findings

Every claim above has a mode behind it. `./deploy_q1.sh <mode>`, or run
`q1diag` with no arguments to list them:

| mode | what it shows |
|---|---|
| `streams` | every FrameType's stream and frame rate |
| `ring` | each buffer's frame counter -- which cameras are tagging right now |
| `tag` | decodes the row-0 header live: exposure, dimensions, type, counter |
| `rows` | raw bytes of rows 0-3, i.e. the header itself |
| `expo` | mean luma per frameset, per camera -- the exposure interleave |
| `pub` | the exposure of each frameset, classified long or short |
| `rate` | delivered frameset rate |
| `noise` | histogram of per-poll row differences (the detection threshold) |
| `meta` | the per-buffer gralloc descriptor |
| `fmq` | the FrameSet queue, its masks, and the mailbox page checksum |
| `config` | `getConfiguration` -- exposure and gain limits as doubles |
| `pair` | one frame of each exposure class, written as PGM |

`--stream N` selects the FrameType (2 = HEADSET, the default; 3 = CONTROLLER,
4 = HAND).

Useful alongside it, and the source of the stream names and client list:

```bash
adb shell lshal debug vendor.oculus.hardware.sensors@1.0::ICameraProvider/default
```

## Open questions

* **Why the HAL never writes the FrameSet queue.** Everything about the
  registration looks correct from the outside. This is the one designed channel
  we could not open, and it is what would carry per-frame metadata for *all*
  cameras rather than only the tagged ones.
* **Turning frame tagging on for every camera, and keeping it on.** The service
  exports `syncboss_camera_set_frame_tag_mode`; it is not reachable over HIDL,
  but if every camera could be made to tag every frame, all the content-based
  machinery here disappears -- exact recency and exact exposure on all four,
  and `Exposure::Any` becomes exact too, giving flicker-free 60 fps instead of a
  30 fps half-rate.
* **Forcing a single exposure.** `startOverridingExposureSettings` returns
  success but does not change the schedule on its own; it needs
  `setExposureGain`, whose `ExposureGainSettings` layout is not reversed yet.
* **Calibration.** `dumpsys sensorservice` has no camera block on this device.
  `ICameraProvider::getCalibrationData(uint32) -> hidl_handle` (tx=2) is the
  likely path, alongside `/vendor/lib64/libcalibrationstore.so`.

## Building

Needs the NDK at `ndk/` (see the main README), and a Quest 1 on adb the first
time -- the link stubs in `vndk_q1/` are the device's own platform libraries, so
`build_q1.sh` pulls them rather than checking them in. libjpeg-turbo is
cross-built from the vendored source for the same reason:

```bash
./build_libjpeg_turbo.sh    # -> build_jt/libturbojpeg.a  (arm64, API 29, NEON)
./build_q1.sh               # -> out/libq1cam.a, out/q1serve, out/q1diag
./deploy_q1.sh serve        # push, keep awake, adb forward, run
```

## Layout

```
q1cam/include/q1cam/q1cam.h   the public API
q1cam/src/hal.h  hal.cpp      HAL session: HIDL glue, ring tracking, frame tags
q1cam/src/q1cam.cpp           capture thread, staging, tear rejection
q1cam/src/stubs.cpp           API-29 link stubs
q1syms.h                      mangled vendor Bp* names (generated)
tools/gen_q1syms.py           regenerates q1syms.h from the device lib
halroot_q1/  gen_q1/          Quest 1 .hal and its hidl-gen output
apps/q1serve.cpp              MJPEG server, an example consumer
apps/turbo_enc.h              libjpeg-turbo wrapper
fastjpeg.h                    threaded baseline JPEG encoder
tools/q1diag.cpp              the diagnostics above
tools/bringup/                the original probes, kept as history
build_q1.sh  deploy_q1.sh     cross-compile; push and run
```

## The displays are load-bearing for tracking (measured 2026-08-22)

Disconnecting a Quest 1's display panels — an obvious-looking way to make a
lighter, cooler, longer-lasting body puck — **kills Insight tracking outright**.
Not degraded: `Tracking Level: 0DOF (PT=0, PV=0, OT=0, OV=0)`, `Valid: No`,
`Time: -0.00`, and an all-zero quaternion `(0,0,0,0)` that is not even a valid
rotation. Tracking never starts at all.

The `tracking` log buffer names the mechanism:

```
MontereyCameraProvider: Frame marked invalid by frame time stamper
SensorService: Frame associated with stale frame sync: sync seqId: -1, frame seqId: N
SensorService: Frame is either too old or is arriving before frame sync
```

`sync seqId: -1` — there is no frame-sync reference, so every camera frame is
rejected as untimestampable.

A controlled comparison isolates what actually died. Both pucks, same 2 s
`q1record`:

| puck | cameras | IMU |
|---|---|---|
| .132, displays **removed**  | 26.5 fps | **0 samples, 0 Hz** |
| .108, displays **attached** | 28.4 fps | 2023 samples, **1012 Hz** |

**The cameras are fine and the frame-sync is gone. The IMU row is NOT what it
looks like** -- see the sync-pulse section below, which was written later and
supersedes the obvious reading of this table. The 0 Hz above is measured at the
CONSUMER, and the driver-level stream is demonstrably alive at the same moment
(1 kHz SPI poll loop, miscfifo filling). The leading explanation is that the
samples flow off the MCU but the sensor HAL never publishes them because none
carries a valid frame-sync association, which would make this 0 Hz a
CONSEQUENCE of the sync failure rather than evidence the IMU itself is dead.
Not resolved: distinguishing IMU packets from status packets in that FIFO
requires parsing the stream's packet types, which has not been done. Do not
cite this row as "the IMU is dead".

**Two false leads, recorded so they are not chased again.** The kernel log on
the display-less puck shows `oculus_syncboss: Bad magic number detected:
0xcacacaca` / `SPI transaction rejected` right after VR shell start, which
looks like a smoking gun. It is not: the *working* puck logs the same message
24 times in a normal session. It is a benign recurring artifact on both.
Likewise `/dev/syncboss_stream0` reads 0 bytes on both pucks — the sensor HAL
holds it exclusively ("miscfifo ... opened by
vendor.oculus.hardware.sensors@1.0-service ... is full"), so a second reader
proves nothing.

What IS established: the syncboss MCU (an nRF52 on SPI, `oculusnrf`) comes up
and completes real SPI round-trips early in boot even with the panels gone --
it reads the proximity calibration and later "Turning on cameras" succeeds --
yet no IMU sample reaches the tracking consumer.

(An earlier version of this paragraph said "the MCU is alive but never enters
its streaming state". That is FALSE and the device's own dmesg disproves it:
the sensor HAL opens the handle at t=4.27, "Starting stream" logs at t=4.34,
and the miscfifo is full by t=5.96. The MCU enters streaming and produces data.
What never starts is the ~72 Hz sync pulse.)

**Where the gate actually is.** Every userspace layer is byte-identical
between a working puck and a display-less one: `oculus_syncboss` binds to
`spi12.0` with the same sysfs attributes, SurfaceFlinger has a Display 0,
`dumpsys sensorservice` lists the same single sensor (the tracking IMU never
appears in the Android sensor framework at all on either puck -- Insight takes
it through the private syncboss path). The ONLY difference in the entire system
is the kernel command line, written by the bootloader after it probes DSI:

    working:      mdss_mdp.panel=1:dsi:0:qcom,mdss_dsi_sdc_lightman_video:...:panelid:00004000
                  androidboot.panel_type=1TJ350
    no panels:    mdss_mdp.panel=0:dsi:0::panelid:00000000
                  (androidboot.panel_type absent entirely)

`ro.boot.panel_type` is a symptom, not the gate -- nothing under /vendor/bin,
/vendor/bin/hw or trackingservice contains the string, so overriding the
property with resetprop changes nothing.

**The syncboss does NOT take a vsync wire from the panel.** An earlier draft of
this document claimed MDSS's vsync pulse train drives the syncboss's sync
domain; that was inferred from the panel=0/IMU=0 correlation and is wrong as
stated. The syncboss device-tree node declares exactly three signals --

    oculus,syncboss-timesync  -> msmgpio 10   (IRQ 162, edge, "syncboss0")
    oculus,syncboss-reset     -> msmgpio 126
    oculus,syncboss-wakeup    -> msmgpio 119  (IRQ 271, edge, "syncboss0")

plus SPI, the prox flag, and the IMU/mag core supplies. There is no display,
panel, TE, or vsync reference anywhere in the node. The AP-visible sync is a
single line that the MCU drives INTO the AP.

That line ticks at ~73 Hz on a working puck (219 edges in 3 s) and **has fired
exactly 0 times since boot** on the display-less one. So the MCU does not
free-run an autonomous heartbeat: the pulse train requires something the
missing panel provides.

**The IMU stream itself is NOT that GPIO, and it IS running without panels.**
The driver polls the MCU over SPI on a timer; from the display-less puck's own
dmesg:

    [ 4.270] SyncBoss handle opened (vendor.oculus.h)    <- the sensor HAL
    [ 4.340] Starting stream ... Trans. Period : 1000 us <- 1 kHz SPI poll loop
    [ 5.956] miscfifo syncboss_stream0 ... is full       <- data IS being produced
    [17.136] Turning on cameras

The FIFO filling means the MCU is answering those 1 kHz transfers with data.
So the earlier "IMU 0 Hz" finding describes what reaches the tracking consumer,
not the driver-level stream, which is alive. What is missing is specifically the
~72 Hz sync pulse -- exactly what SensorService names when it logs
`sync seqId: -1`, and why every camera frame then fails the time stamper.

**Mere vsync availability is not the gate.** With no panel, hwcomposer still
runs a software vsync: `Vsync 1631: 2 x 13.93 ms (71.83 Hz)` appears in logcat
on the display-less puck. A 72 Hz vsync exists and the sync pulse still never
starts, so the HAL is not simply waiting for "a vsync rate" to exist.

Two readings remain, and software cannot separate them from the host side:
  (a) the MCU derives frame sync from a board-level display signal that the
      AP's device tree does not describe (it describes AP-side pins only);
  (b) the sensor HAL programs the MCU's sync period only when it finds a real
      panel config, and panel=0 leaves it unprogrammed.

UNVERIFIED, do not repeat as fact: whether a puck can run with panels connected
but dark. The working puck was measured with `panel_power_on = 1`, backlight
127, display ON. Blanking the panel while watching the GPIO 10 edge rate is the
test; it was not run, to avoid disturbing the only working puck before the
third-puck acceptance test.

**Only the LEFT panel is required, and the bootloader is NOT the gate.**
Measured with `tools/q1syncprobe.sh` and `tools/q1paneldark.sh`:

| config | cmdline | panel_type | GPIO 10 | seqId -1 | Insight |
|---|---|---|---|---|---|
| both panels    | `panel=1 ... panelid:00004000` | `1TJ350` | 72 Hz | 0 | **6DOF Valid** |
| **left only**  | `panel=0 ... panelid:00000000` | *(empty)* | 72 Hz | 0 | **6DOF Valid** |
| right only     | `panel=0 ... panelid:00000000` | *(empty)* | 0 Hz | 392/392 | 0DOF Invalid |
| none           | `panel=0 ... panelid:00000000` | *(empty)* | 0 Hz | all | 0DOF Invalid |

Read the middle two rows together: they are IDENTICAL in every software-visible
display indicator -- same cmdline, same empty `panel_type`, same
`panel_power_on = 1`, same backlight, same ~71.8 Hz vsync -- and yet one
tracks at 6DoF and the other is stone dead. The only difference is which
physical panel is plugged in.

Three consequences:

1. **The bootloader's panel detection is not the gate.** `panel=0` appears in a
   WORKING configuration. (Detection needs both panels because the config is
   `split_dsi`; one panel is not a valid split config. That is unrelated to
   tracking.) An earlier revision of this document claimed detection keys on
   DSI0 and that this explained the failure -- wrong on both counts.
2. **No software panel config gates it either**, since software cannot tell
   left-only from right-only. Reading (b) is effectively dead.
3. **The dependency is physical/electrical, specific to the LEFT side.**
   Reading (a) survives.

**The panel does not need to be lit.** Backlight driven to 0 with
`q1paneldark.sh`: GPIO 10 held 72 Hz, Insight held 6DOF Valid. Pucks can run
their one panel dark.

**Refined hypothesis -- the 72 Hz is generated by the MCU, gated on a static
signal.** With `panel=0` the DSI driver never probes or drives a panel, so the
left panel is not being clocked and cannot be emitting TE pulses -- yet GPIO 10
ticks at exactly 72 Hz. So the MCU is generating that itself and merely
*enabling* it based on something about the left side. Two candidates worth a
multimeter, both cheap to check with the puck apart:

  - a panel-present / ID line on the left connector that the nRF52 senses. If
    so the override is a RESISTOR, not a 72 Hz PWM, and no panel is needed.
  - the left display flex physically carrying syncboss or camera-sync
    conductors alongside DSI, so that removing it opens an unrelated circuit.
    Compact HMD flexes routinely carry more than one subsystem.

Either way the useful next step is continuity/level probing between the LEFT
display connector and the nRF52, comparing left-connected vs not.

**SOLVED: yes, it can be overridden, entirely in software.** The
SeperationAnxiety Magisk module (Juspertinry, v1.0) ships a kernel module that
generates a synthetic display TE pulse on the timesync GPIO. Measured on a puck
with BOTH panels physically removed:

| | cmdline | GPIO 10 | seqId -1 | Insight |
|---|---|---|---|---|
| no panels            | `panel=0` | 0 Hz  | 370/370 | 0DOF Invalid |
| no panels + module   | `panel=0` | **72 Hz** | **0** | **6DOF Valid** |

    insmod /data/local/tmp/seperationanxiety.ko
    # dmesg: driving TE on gpio 10 @ 0x3d0a000 at 13888888 ns (cfg 0x1 -> 0x201)

It reconfigures the TLMM pad from input to output (`cfg 0x1 -> 0x201`) and
drives it at 13888888 ns = 72 Hz. Parameters `gpio`, `period_ns`, `pulse_ns`
and `tile` are all tunable at insmod time.

**This corrects three conclusions recorded above.**

1. The line is a **display TE**, driven by the panel. The earlier reading that
   the nRF52 generates the 72 Hz itself and merely gates it on panel presence
   was wrong -- if the MCU were the source, driving the pad from the AP could
   not have restored anything.
2. "Not established, and not cheaply" was wrong. It is one kernel module and
   no hardware.
3. The objection that injecting a pulse could not work -- because the sync
   seqId rides in the SPI stream and bare edges would leave it at -1 -- was
   also wrong. seqId is derived from the edges themselves: with the synthetic
   pulse running, the `seqId: -1` count is exactly 0.

**No HAL restart is needed.** The module's own `post-fs-data.sh` loads early
because "if the HAL comes up with no TE, every frame is discarded and tracking
never initialises -- restarting it later is the only cure". That is not what
happens here: loading the module at t=66 s, long after the HAL came up at
t=4.3 s and spent a minute logging `seqId: -1`, restored 6DOF on its own with
no restart of `trackingservice` or `vendor.oculus.sensors-hal-1-0`. So a plain
host-driven `insmod` after boot is sufficient, which fits this fleet's model
(nothing on-device survives a reboot as root; the host drives everything) and
avoids needing Magisk at all -- `adb root` plus `insmod` is the whole
installation. Magisk itself never worked here: the APK installs but there is no
patched boot image, so there is no `magisk` binary and no `su`.

The module is built against ONE kernel and checks both `uname -r` and the
sha1 of `/proc/version` before loading, because this kernel has
CONFIG_MODVERSIONS. Both pucks match: `4.4.205-perf+`,
`5cd7637e06c507e7ef4b8f45b12b02b5c2df9979`.

Consequences for the puck design:

- **Do not strip displays to save weight/power.** The panels can presumably be
  left dark, but they must remain electrically connected.
- Our own capture path is unaffected in isolation (`q1record` still pulled 324
  PGMs at 28 fps), so raw camera work survives — but anything needing pose
  does not, which is everything the tracker and the on-puck verifier do.
- `q1verify` correctly reports `no-input` on such a puck: it refuses to score
  without a valid 6DoF pose rather than inventing one.
