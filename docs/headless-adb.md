# Driving a Quest 1 with no display

Every command here was used on a real puck. The ones worth knowing are not the
obvious ones — they are the handful where the obvious command silently does
nothing, which on a headless device looks identical to a hardware fault.

Two rules that apply to everything below:

- **`adb root` does not survive a reboot.** Without it `setprop`, `chown`,
  `chcon`, `insmod` and any read of `/vision` fail *silently* or with an error
  that names the wrong cause. Re-root first, always.
- **Never pipe `dumpsys tracking` into anything that closes the pipe early**
  (`grep -m1`, `head`). It leaves the tracking service unavailable for seconds
  afterwards, reporting `Can't find service: tracking`. Dump to a file
  on-device, then grep the file.

---

## Getting a shell at all

```sh
adb -s <SERIAL> tcpip 5555                       # once, over USB
adb -s <SERIAL> shell setprop persist.adb.tcp.port 5555   # survives reboots
adb connect <IP>:5555
adb -s <IP>:5555 root && adb connect <IP>:5555   # re-root after every boot
adb -s <IP>:5555 shell id -u                     # must print 0
```

`persist.adb.tcp.port` is what makes adbd listen on 5555 from boot with no
`adb tcpip` at all — the volatile `service.adb.tcp.port` is checked first but
is not set here.

Finding a puck that has moved:

```sh
adb -s <IP>:5555 shell 'ip -4 addr show wlan0' | grep -oE 'inet [0-9.]+'
adb -s <IP>:5555 shell 'dumpsys wifi | grep -m1 -i "current SSID"'
```

### A puck that has never joined wifi

`wpa_cli` alone does not work — the framework's `WifiConfigManager` owns
supplicant config and reconciles the change away within seconds. Android 10 has
no `cmd wifi connect-network`. The route that works is transplanting
`WifiConfigStore.xml` from a puck that is already joined:

```sh
adb -s <JOINED> pull /data/misc/wifi/WifiConfigStore.xml .
# edit: give this puck its OWN RandomizedMacAddress, set
# MacRandomizationSetting=0 and NumAssociation=0
adb -s <NEW> push WifiConfigStore.xml /data/local/tmp/wcs.xml
adb -s <NEW> shell 'cp /data/local/tmp/wcs.xml /data/misc/wifi/WifiConfigStore.xml
                    chown system:system /data/misc/wifi/WifiConfigStore.xml
                    chmod 600 /data/misc/wifi/WifiConfigStore.xml'
adb -s <NEW> reboot        # the framework only reads it at boot
```

**Copy *over* the existing file rather than replacing it** — that preserves the
`u:object_r:wifi_data_file:s0` context, which a fresh push does not carry.

**Never reuse the donor's MAC.** Two pucks with one MAC is horrible to debug and
breaks MAC-keyed DHCP leases. Derive one from the serial so it is deterministic
and unique.

---

## Keeping a headset awake with nobody wearing it

A body puck is never worn, and a headset that believes it is off a head powers
down before anything can start.

```sh
setprop persist.oculus.forceHeadsetOn 1      # survives reboot
setprop debug.oculus.forceHeadsetOn 1        # volatile — cleared by a reboot
setprop persist.ovr.disable.sensorproxy true
setprop persist.oculus.guardian_disable 1
settings put system screen_off_timeout 86400000
settings put global wifi_sleep_policy 2
am broadcast -a com.oculus.vrpowermanager.prox_close
svc power stayon true
```

Use the `persist.*` forms: init restores them every boot, and the volatile
`debug.oculus.forceHeadsetOn` is cleared by exactly the event where it is
needed.

Checking whether any of it worked:

```sh
dumpsys power | grep -m1 mWakefulness=       # want Awake
dumpsys power | grep -m1 "Display Power"     # want state=ON
dumpsys battery | grep -E "level:|AC powered"
```

> **`persist.ovr.disable.sensorproxy true` disables the proximity path**, which
> is what stops a body puck sleeping — and also means `prox_close` and
> `input keyevent KEYCODE_WAKEUP` have nothing to act on. On a puck configured
> this way, **physically covering the proximity sensor** is the reliable way to
> wake it.

---

## The guardian package: the single most expensive switch

```sh
pm disable-user --user 0 com.oculus.guardian   # tracker streams, displays DARK
pm enable com.oculus.guardian                  # displays come up, tracker emits NOTHING
pm list packages -d | grep guardian            # is it disabled?
```

**Disable it before the tracker app first starts.** The property
`persist.oculus.guardian_disable` is *not* sufficient. With the package enabled
the tracker app looks completely healthy — running, window-focused, correct
config, Insight at `6DOF Valid: Yes` — and emits nothing at all, while the
displays show passthrough instead of going dark.

It has to be re-enabled for the two things that need a rendered display: minting
a map, and using any on-headset UI. Treat it as a bracket, not a toggle, and
disable it again afterwards or the puck stops streaming.

### Minting a map headlessly

No VR UI needed; the guardian service takes JSON commands.

```sh
pm enable com.oculus.guardian
setprop persist.oculus.guardian_json_cmds_user_build 1
am start-foreground-service \
    com.oculus.guardian/com.oculus.vrguardianservice.VrGuardianService
stop trackingservice; start trackingservice        # guardian must start FIRST
am broadcast -a com.oculus.vrguardianservice.JsonCmdUserBroadcast \
    -p com.oculus.guardian \
    --es cmd '{"automation":{"guardian":{"force_stationary":true}}}'
```

Three things that each silently do nothing when wrong: the extra key must be
`cmd` (a wrong one logs `Cmd: null`), the JSON must be wrapped in
`{"automation":{"guardian":{…}}}` (a bare `{"guardian":…}` parses and never
reaches the handler), and guardian must be started *before* trackingservice or
anchor work fails with `SlamAnchorRuntimeIpcClient: InitClientInternal failed!`.

---

## Tracking and the sensor stack

```sh
# ALWAYS dump to a file first
adb -s <IP>:5555 shell 'dumpsys tracking > /data/local/tmp/t.txt 2>/dev/null
                        grep -m1 "Tracking Level" /data/local/tmp/t.txt
                        grep "Vega Map Context" /data/local/tmp/t.txt'

stop trackingservice; start trackingservice
stop vendor.oculus.sensors-hal-1-0; start vendor.oculus.sensors-hal-1-0
getprop init.svc.trackingservice
getprop init.svc.vendor.oculus.sensors-hal-1-0
```

`Time: -0.00` with `0DOF` means the engine has never produced a sample — that is
a camera-pipeline problem, not a "cannot see enough features" problem. Confirm
with:

```sh
grep -c "No Camera samples have been received" /data/local/tmp/t.txt
logcat -d -t 2000 | grep -iE "MontereyCameraProvider|TIMESTAMPCHECKER"
```

`Frame marked invalid by frame time stamper` means frames are arriving and being
rejected — on a **panel-free** puck that is the missing synthetic-TE module, not
a broken camera.

### Panel-free pucks: the synthetic TE module

```sh
insmod /data/local/tmp/seperationanxiety.ko
lsmod | grep seperationanxiety
grep 'msmgpio  10' /proc/interrupts        # non-zero count = it took the TE line
stop trackingservice; stop vendor.oculus.sensors-hal-1-0
start vendor.oculus.sensors-hal-1-0; start trackingservice
```

**Order matters.** The HAL establishes camera sync once, early; if it comes up
with no TE every frame is discarded and restarting it later is the only cure. So
the module goes in *first*, then the HAL is restarted.

`/data/local/tmp` survives a reboot, so the `.ko` only needs placing once — what
does not survive is it being *loaded*.

---

## Apps, without a screen

```sh
adb -s <IP>:5555 install -r app.apk
am start -n <pkg>/.MainActivity
am force-stop <pkg>
pm list packages | grep -i <name>
dumpsys package <pkg> | grep -m1 versionName
appops set <pkg> SYSTEM_ALERT_WINDOW allow      # BAL exemption for boot-start
am force-stop com.oculus.os.vrusb               # clears the USB dialog that steals focus
```

Appops are written to disk **lazily** — a reboot within a few seconds of
granting silently loses the grant. Wait ~45 s.

The tracker app reads its config from a plain file:

```sh
/sdcard/Android/data/com.mapperlocalizer.questtracker/files/config.txt
```

### Driving an on-headset UI over adb

This is the one that is genuinely non-obvious, and it makes headless operation
of *any* 2D app possible.

```sh
uiautomator dump /data/local/tmp/ui.xml         # read the whole view hierarchy
dumpsys display | grep -E "Display [0-9]+:|VrDesktopDisplay"
dumpsys window | grep -iE "<pkg>|displayId"     # which display is the app on?
input -d <displayId> tap <x> <y>                # NOTE THE -d
```

**A 2D app on a Quest renders on a virtual display**, typically
`VrDesktopDisplay` (400x640), not display 0. `uiautomator dump` reads it fine —
so you can see every button and its bounds — but a plain `input tap` goes to the
default display and lands nowhere. The symptom is a dump that shows the UI
perfectly and taps that do nothing at all.

Get the display id from `dumpsys window`, pass it as `-d`, and the coordinates
from `uiautomator` are then correct as-is.

Parsing the dump for tappable targets:

```sh
grep -oE 'text="[^"]+"[^>]*bounds="\[[0-9]+,[0-9]+\]\[[0-9]+,[0-9]+\]"' ui.xml
```

---

## Boot slots and fastboot

These pucks are **A/B**: `boot_a` and `boot_b`, no plain `boot`.

```sh
getprop ro.boot.slot_suffix                  # which slot is running
getprop ro.boot.flash.locked                 # 0 = unlocked
getprop ro.boot.verifiedbootstate            # orange = unlocked

# dump a slot for backup, from the device itself
dd if=/dev/block/bootdevice/by-name/boot_b of=/data/local/tmp/boot_b.img bs=1M

adb reboot bootloader                        # NOT key combos
fastboot devices
fastboot getvar current-slot
fastboot flash boot_b <image>
fastboot set_active b
fastboot reboot
```

> **`boot_a` carries the vulnerable image that provides bootloader unlock. Never
> write to it.** It is the recovery path: any failed experiment in `boot_b` is
> undone with `fastboot set_active a`.

A `Power`-only boot lands in `msc` and key combos will not reach fastboot, so
use `adb reboot bootloader`.

### After a Magisk install

Magisk changes how root works, which matters because every tool here assumes
`adb root`:

```sh
/sbin/magisk -V          # versionCode
/sbin/magisk -v          # e.g. 30.7:MAGISK:R
ls /sbin/ | grep magisk  # magisk, magisk32, magiskinit, magiskpolicy
su -c id                 # request-based root, needs granting
id -u                    # adb root may now return 2000, not 0
```

---

## Reading things that need root

```sh
ls /vision/insideout/mapdb                   # 700 system:system — root only
```

After `adb push` into `/vision`, the file carries the wrong SELinux label and
trackingservice silently cannot read it:

```sh
chown system:system /vision/insideout/mapdb/*.mapdata
chmod 600 /vision/insideout/mapdb/*.mapdata
chcon u:object_r:vision_file:s0 /vision/insideout/mapdb/*.mapdata
ls -Z /vision/insideout/mapdb/*.mapdata      # verify the label stuck
```

Toybox `cp -a` does **not** preserve the SELinux label, so a file restored from
an on-device backup needs the `chcon` re-run too.
