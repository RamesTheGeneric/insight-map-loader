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

### When the shell never starts

Before any on-headset UI can be driven, `com.oculus.vrshell` has to get past
startup. The symptom of it not doing so is a log that repeats forever at ~10 ms
intervals:

```
[SEO] ShellApp: BlockingLaunch: Awaiting User Identity.
```

VrShell is blocking on an identity that `com.oculus.socialplatform` supplies, and
socialplatform is dying on launch. Confirm which package, and why:

```sh
logcat -d -t 3000 > /data/local/tmp/lc.txt
grep -A4 "FATAL EXCEPTION" /data/local/tmp/lc.txt
grep -iE "ShellApp|User Identity" /data/local/tmp/lc.txt | tail
dumpsys window | grep -E "mCurrentFocus|mFocusedApp"
```

The cause seen here was a **system app that had auto-updated past the OS**:

```
java.lang.NoSuchMethodError: No direct method <init>(Ljava/lang/String;F)V
  in class Lcom/oculus/os/AnalyticsEvent; (declaration ... appears in
  /system/framework/com.oculus.os.platform.jar)
```

The APK in `/data/app` was calling a framework method this build does not have.
`pkgFlags=[ SYSTEM ... UPDATED_SYSTEM_APP ]` is the tell — compare against a
headset whose shell works, which will show the factory version under
`/system/app`:

```sh
dumpsys package com.oculus.socialplatform | grep -E "versionName=|codePath=|pkgFlags="
```

Removing the update reverts to the `/system` copy, which by construction matches
the framework:

```sh
pm uninstall com.oculus.socialplatform          # NO --user flag
```

**`pm uninstall --user 0` is the wrong command** and looks like it worked
(`Success`): it uninstalls the package *for that user* and leaves the update in
place, so the shell now has no socialplatform at all. Recover with
`cmd package install-existing com.oculus.socialplatform`, then run the plain
`pm uninstall`. The `codePath` flipping back to `/system/app/…` is the
confirmation.

A residual crash loop in `com.oculus.horizon` may remain, restarting every ~3 s:

```
Unable to start service com.facebook.rti.push.service.FbnsService ...
  java.lang.RuntimeException: Tokenbinding not implemented for legacy auth
```

It does not block the shell. It also cannot be switched off the usual ways —
both `pm disable-user` and `pm suspend` refuse with `Cannot disable a protected
package` (suspend fails quietly, reporting `new suspended state: false`). It
only appears on a headset that carries a signed-in account.

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

### Patching a boot image with Magisk

The kernel command line on these headsets contains **`skip_initramfs`**:

```sh
tr ' ' '\n' < /proc/cmdline | grep -E "initramfs|^root=|^dm="
# skip_initramfs   root=/dev/dm-0   dm="system none ro,0 1 android-verity /dev/sda7"
```

That tells the kernel to ignore the boot ramdisk and mount system directly —
and the ramdisk is where `magiskinit` lives. Magisk's answer is to hexpatch the
kernel, `skip_initramfs` → `want_initramfs`, but `boot_patch.sh` only does that
when told the device is legacy system-as-root. **It defaults to off**, so a
bare invocation produces an image that boots perfectly with no Magisk in it:

```sh
adb root
adb shell 'dd if=/dev/block/bootdevice/by-name/boot_b of=/data/local/tmp/stock.img bs=1M'
adb shell 'cd /data/adb/magisk && LEGACYSAR=true KEEPVERITY=false \
           KEEPFORCEENCRYPT=false sh ./boot_patch.sh /data/local/tmp/stock.img'
```

**Check for this line in the output — it is the whole difference:**

```
Patch @ 0x01DF95B8 [736B69705F696E697472616D667300] -> [77616E745F696E697472616D667300]
                    s k i p _ i n i t r a m f s        w a n t _ i n i t r a m f s
```

No `Patch @` line means Magisk will be silently absent after the flash: the
device boots, `adb root` still works, and nothing whatsoever indicates that the
patch did not take. Then flash `new-boot.img` to the **inactive-of-slot-a**
partition (`fastboot flash boot_b`) and confirm afterwards with
`mount | grep " /sbin "` — a magisk tmpfs there is the proof.

AVB1 signing is not an obstacle. These images report `AVB1_SIGNED` and there is
no separate `vbmeta` partition, but an unlocked bootloader
(`verifiedbootstate=orange`) boots the resigned image without complaint.

### After a Magisk install

```sh
/sbin/magisk -V          # versionCode
/sbin/magisk -v          # e.g. 30.7:MAGISK:R
ls -l /sbin/magisk* /sbin/su   # su is a symlink -> ./magisk
mount | grep " /sbin "   # magisk on /sbin type tmpfs — if absent, it isn't really up
pidof magiskd
cat /cache/magisk.log    # rewritten each time the daemon starts
```

**`adb root` keeps working.** The patched image still has `ro.debuggable=1`, so
`adb root` gives a real uid 0 (`context=u:r:su:s0`) exactly as before — it just
has to be re-issued after every boot, same as always. A shell that reports
uid 2000 has not lost root to Magisk; it has simply not been rooted yet.

### Completing Magisk's setup with no UI

The Magisk app's post-install has two halves. The first — populating
`/data/adb/magisk` with `busybox`, `magiskboot`, `magiskpolicy`, `stock_boot.img`
— is already done by the install itself:

```sh
ls /data/adb/magisk       # needs root; expect ~12 files incl. busybox, stock_boot.img
```

The second half is **granting root**, and that is the part that needs the app.
With no grant recorded, `su` sits in the default *query* policy: magiskd asks the
manager app to raise a prompt, and on a headset with no working shell that prompt
never appears — so `su -c id` **hangs forever** rather than failing.

Grant it from the database instead (`policy=2` is allow):

```sh
/sbin/magisk --sqlite "REPLACE INTO policies (uid,policy,until,logging,notification) VALUES(2000,2,0,1,0)"
/sbin/magisk --sqlite "REPLACE INTO settings (key,value) VALUES(\"root_access\",3)"
/sbin/magisk --sqlite "SELECT * FROM policies"
su -c id                 # now returns instantly, context=u:r:magisk:s0
```

uid 2000 is `shell`. `root_access=3` is APPS_AND_ADB and is the default anyway,
but it is absent from a fresh database, so setting it removes a variable.

Two traps:

- **`--sqlite` talks to the daemon**, so it needs root itself. Bootstrap with
  `adb root`, not with `su`.
- **Do not run `SELECT sql FROM sqlite_master`.** It kills magiskd — the client
  reports `failed to fill whole buffer` and every later call gets
  `Cannot connect to daemon: Connection refused`. Nothing restarts it
  automatically; `/sbin/magisk --daemon` brings it back.

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
