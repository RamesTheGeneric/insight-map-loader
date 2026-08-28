# Loading a kernel module at boot

A panel-free puck has no display TE signal, so the frame timestamper rejects
every camera frame and tracking never initialises. The synthetic-TE kernel
module fixes that, but only if it is loaded **before the sensors HAL** — the HAL
establishes camera sync once, early, and if it comes up with no TE the only cure
is restarting it afterwards.

`insmod` from the host works, but does not survive a power-cycle, so every puck
needs a hand before it tracks. Magisk's `post-fs-data` stage runs early enough
and solves it permanently. Getting there took four separate discoveries, each of
which fails **silently**, so work through this in order and check each gate.

Command-level detail for everything here is in [headless-adb.md](headless-adb.md).

## What you need first

- The headset on the OS the module was built against. A `.ko` is tied to one
  exact kernel: `uname -r` **and** the sha1 of `/proc/version`, because
  `CONFIG_MODVERSIONS` is on and there is no force-load. A different build of the
  same release fails with `disagrees about version of symbol module_layout`.
- An unlocked bootloader (`ro.boot.flash.locked=0`, `verifiedbootstate=orange`).
- `adb root` working. This stays true afterwards — **Magisk is not what gives you
  root here**, `ro.debuggable=1` is. Magisk buys you `post-fs-data`, nothing else.

## 1. Pick the slot, and check which one you are protecting

These are A/B devices. If one slot holds an older image you want to keep — an
image that provides bootloader unlock, say — **it is not always `boot_a`.** Read
the OS patch level out of each boot header rather than assuming:

```sh
adb shell 'dd if=/dev/block/bootdevice/by-name/boot_a bs=1M' | md5sum
head -c 48 boot_x.img | xxd -s 44 -l 4 -p     # os_version + os_patch_level
```

The low 11 bits are the patch level: `((year-2000) << 4) | month`. So
`0x14000187` → `0x187` = 391 → `391>>4 = 24`, `391&0xF = 7` → **2024-07**, and
the upper bits give `10.0.0`. An old patch level marks the image worth keeping.

Patch the slot the device **currently boots**, so the other slot keeps its old
image untouched and stays a working fallback. Pull a copy of both to the host
before starting.

**Never reach for a sideload to fill a slot you want to keep.** An OTA writes the
**inactive** slot and then makes it active — so protecting a slot means making it
the *active* one first, not the other way round.

## 2. Patch the boot image — the `LEGACYSAR` gate

The kernel command line contains `skip_initramfs`:

```
skip_initramfs  root=/dev/dm-0  dm="system none ro,0 1 android-verity /dev/sdaN"
```

The kernel therefore ignores the boot ramdisk, and the ramdisk is where
`magiskinit` lives. Magisk's answer is to hexpatch the kernel, but
`boot_patch.sh` only does it when told the device is legacy system-as-root, and
**that defaults to off**:

```sh
adb shell 'dd if=/dev/block/bootdevice/by-name/boot_X of=/data/local/tmp/stock.img bs=1M'
adb shell 'cd /data/adb/magisk && LEGACYSAR=true KEEPVERITY=false \
           KEEPFORCEENCRYPT=false sh ./boot_patch.sh /data/local/tmp/stock.img'
```

**Gate — this line must appear:**

```
Patch @ 0x01DF95B8 [736B69705F696E697472616D667300] -> [77616E745F696E697472616D667300]
                    s k i p _ i n i t r a m f s        w a n t _ i n i t r a m f s
```

Without it you get an image that boots perfectly, keeps `adb root`, and contains
no Magisk at all. Nothing reports an error. Flash `new-boot.img` to the slot
chosen above and reboot.

**Gate — after the reboot:**

```sh
mount | grep " /sbin "     # want: magisk on /sbin type tmpfs
pidof magiskd
```

AVB1 signing is not an obstacle; an unlocked bootloader accepts the resigned
image. The patch is deterministic, so one patched image serves every headset of
the same model and build — verify by md5 rather than rebuilding per device.

## 3. Transplant the Magisk environment

`magiskinit` creates `/data/adb/{magisk,modules,post-fs-data.d,service.d}` but
leaves `/data/adb/magisk` **empty**. Those binaries normally arrive with the
Magisk app, which a headset with no usable UI cannot run. Until they are there,
modules never execute:

```
* Initializing Magisk environment
* Magisk environment incomplete, abort      # every boot, not just the first
$ magisk --install-module foo.zip
Incomplete Magisk install                   # rc=1, no further explanation
```

Copy them from a headset that has them, preserving ownership and label:

```sh
for f in magisk magisk32 magiskboot magiskinit magiskpolicy busybox \
         init-ld stub.apk util_functions.sh addon.d.sh boot_patch.sh; do
  adb -s <DONOR> pull /data/adb/magisk/$f .
done
adb -s <DONOR> pull /data/adb/magisk/chromeos chromeos
adb -s <NEW> push . /data/local/tmp/magiskenv/
adb -s <NEW> shell 'cp -r /data/local/tmp/magiskenv/. /data/adb/magisk/
                    chown -R root:root /data/adb/magisk
                    chmod -R 0755 /data/adb/magisk
                    restorecon -R /data/adb/magisk'
```

Skip any `kernel`, `kernel_dtb`, `ramdisk.cpio`, `new-boot.img` or
`stock_boot.img` — those are patching leftovers, not part of the environment.

**Then reboot before installing anything.** The completeness check runs against
what the daemon found at boot.

**Gate:** `/cache/magisk.log` shows `Running module service scripts` with no
`abort`.

## 4. Install the module

```sh
adb push SeperationAnxiety.zip /data/local/tmp/
adb shell 'magisk --install-module /data/local/tmp/SeperationAnxiety.zip'
```

A well-formed module checks the kernel at install time and refuses a mismatch
rather than installing something that will silently do nothing:

```
- checking kernel build
  release : 4.4.205-perf+
  build   : 5cd7637e06c507e7ef4b8f45b12b02b5c2df9979
- kernel matches, installing
```

Files land in `/data/adb/modules_update/<id>/` with an `update` marker in
`/data/adb/modules/<id>/`. That is normal two-phase installation — **reboot** and
Magisk moves them into place and runs them.

## Verifying it actually worked

Take a baseline *before* the reboot so the result is unambiguous. On a
panel-free puck with no module loaded:

```
gpio10: 162: 0 0 0 0 0 0 0 0  msmgpio 10 Edge syncboss0     ← zero interrupts
Tracking Level: 0DOF (PT=0, PV=0, OT=0, OV=0)  Valid: No
Time: -0.00                                    ← engine never produced a sample
```

After the reboot, with nothing run from the host:

```sh
cat /data/adb/modules/<id>/sa.log
grep "msmgpio  10" /proc/interrupts     # sample twice: ~70 Hz
grep -c seperationanxiety /proc/modules
dumpsys tracking > /data/local/tmp/t.txt; grep -m1 "Tracking Level" /data/local/tmp/t.txt
```

A working result reaches `6DOF ... Valid: Yes` roughly 40–55 s into a cold boot.

Note the module may refuse a manual `insmod` with **`Device or resource busy`**
while a panel is attached — it will not fight a driver already holding the line.
That is not a failure: `post-fs-data` runs before the display stack claims the
GPIO, so the boot-time load succeeds where the manual one cannot.

## When it goes wrong

| symptom | cause |
|---|---|
| Boots fine, no `/sbin/magisk`, no `magiskd` | `LEGACYSAR` was not set — no `Patch @` line |
| `Magisk environment incomplete, abort` | `/data/adb/magisk` empty — transplant it |
| `Incomplete Magisk install`, rc=1 | same, and it needs a reboot after the transplant |
| `disagrees about version of symbol module_layout` | `.ko` built against a different kernel build |
| `insmod: Device or resource busy` | a panel driver already owns the GPIO |
| No USB at all, no adb, no fastboot | see below |

**A puck that vanishes** is usually flat, not bricked. `lsusb | grep 2833`:
`0186` is normal adb, **`0083` is mass-storage/recovery**. A battery level read
between reboots is not trustworthy — one read 92% while cycling. A USB hub will
not charge these; use a real charger and leave it off until full.

Do not power-cycle repeatedly while it sits in msc: the bootloader can self-heal
by writing a golden stock boot into the working slot, silently undoing the flash
and making it look like the patch failed. Re-hash the partition before drawing
any conclusion.

**A device showing `Your device is corrupt. It cannot be trusted and will not
boot`** waits indefinitely for a power press, which rules out unattended
power-on. It is worth confirming the claim is even true — compare the system
partition against a headset that boots clean:

```sh
adb shell 'dd if=/dev/block/bootdevice/by-name/system_X bs=4M' | md5sum
```

If they match, nothing is corrupt and the trigger is
`androidboot.veritymode=disabled` coming from per-device state. `adb
enable-verity` cannot help on a `user` build — it answers `enable-verity only
works for userdebug builds`.
