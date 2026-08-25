# Wireless adb on the Quest 1

The Quest 1 (`monterey`) is **Android 10 / API 29**, so this is the classic
`adb tcpip` route -- *not* the Android 11+ `adb pair` / mDNS flow. There is no
pairing code and no Developer-options "Wireless debugging" toggle to find.

Verified working end to end: every puck in this project is reached only over
wifi, with no measurable penalty versus USB (numbers below). That matters here
more than usual — a puck strapped to an ankle cannot have a cable in it.

## The devices

One further puck to come. Each needs the setup below once over USB, and each
wants its own static DHCP lease.

| puck | serial | ip (DHCP) | wlan0 MAC |
|---|---|---|---|
| 1 | `<SERIAL>` | `192.168.1.10` | `<MAC>` |
| 2 | `<SERIAL>` | `192.168.1.11` | `<MAC>` |
| 3 | -- | -- | -- |

AP for all of them: `<YOUR-SSID>`, 5500 MHz, **open** (`key_mgmt=NONE`), adb on
port `5555`.

The IP is a DHCP lease and **will** move eventually. Pin it to the MAC in the
router (dd-wrt: Services -> DHCP Server -> Static Leases) or re-read it with:

```bash
adb -s <SERIAL> shell "ip -4 addr show wlan0" | grep -oE 'inet [0-9.]+'
```

## Setup

Once, over USB:

```bash
adb -s <SERIAL> tcpip 5555
adb connect 192.168.1.10:5555
```

To make it survive reboots, so USB is never needed again:

```bash
adb shell setprop persist.adb.tcp.port 5555
```

`adbd` on this build reads `service.adb.tcp.port` first and falls back to
`persist.adb.tcp.port` -- both strings are present in `/system/bin/adbd`. With
the persistent one set, adbd listens on 5555 from boot with no `adb tcpip`
command at all. **Verified across a real reboot**: after `adb reboot`, port
5555 was open (`grep :15B3 /proc/net/tcp6`) and `adb connect` succeeded
directly.

To undo: `adb shell setprop persist.adb.tcp.port -1` (then reboot, or
`adb usb`).

## What does *not* survive a reboot

This bit the first post-reboot run, so it is worth stating plainly. After a
cold boot the device comes back as:

```
uid=2000(shell) ... context=u:r:shell:s0
Enforcing
```

`adb root` and any `setenforce` are **not** persistent. The symptom is a
`Permission denied` out of the first thing that tries to write outside
`/data/local/tmp`. Fix:

```bash
adb root          # restarts adbd as root
sleep 3
adb connect 192.168.1.10:5555   # reconnect; adbd restarted
```

`adb root` works fine *over the wireless transport* -- adbd restarts and comes
straight back on 5555 because of the persistent port property. You do not have
to go back to USB for it.

Two further things learned here:

- **SELinux permissive is not required.** With `adb root` the shell lands in
  `u:r:su:s0`, and everything this project does runs with SELinux still
  **Enforcing**. Earlier sessions had run `setenforce 0`; it turns out to be
  unnecessary, so leave enforcing on. (Files pushed into `/vision` still need
  `chcon u:object_r:vision_file:s0` — that is a label, not a mode.)
- The **proximity override is also cleared** by a reboot. Re-apply it, or let
  `tools/q1bringup.sh` do it:
  ```bash
  adb shell "am broadcast -a com.oculus.vrpowermanager.prox_close"
  adb shell "setprop debug.oculus.forceHeadsetOn 1"
  ```

## Using it with scripts that call bare `adb`

With USB *and* wifi both attached, any script calling bare `adb` fails with
`adb: more than one device/emulator`. No script changes are needed -- adb
honours `ANDROID_SERIAL`:

```bash
export ANDROID_SERIAL=192.168.1.10:5555
```

The tools in this repo take the puck's IP explicitly instead (`tools/q1bringup.sh
<ip>`, `Device("192.168.1.10")`), so they are unaffected either way. Over wifi
you can also reach a service on the puck directly rather than through
`adb forward` -- handy when the headset is being worn or is on a stand across
the room, which is the whole point for the tracker-puck work.

## Performance: wifi vs USB

The Quest 1's USB is 2.0 (~40 MB/s ceiling), and 5 GHz wifi lands in the same
place, so there is essentially nothing to lose.

Bulk transfer, 32 MB `dd` through `adb shell`:

| transport | throughput |
|---|---|
| USB | 33.8 MB/s |
| wifi (5 GHz) | 30.5 MB/s |

That is the number that decides whether pulling a map is practical: a mapdb is
single-digit megabytes, so a pull or a seed is a couple of seconds either way.

The pose stream itself is nowhere near this — MPT1 is 68 bytes per packet at
~72 Hz per puck, about 40 kbit/s each. Wifi capacity has never been the
constraint; the AP's *latency* under load is the thing to watch, since every
packet is a pose that ages.

## Quick reference

```bash
# connect (after a host reboot, or first thing in a session)
adb connect 192.168.1.10:5555
export ANDROID_SERIAL=192.168.1.10:5555

# after a *device* reboot, additionally:
adb root && sleep 3 && adb connect 192.168.1.10:5555

# sanity
adb shell id            # want uid=0(root), context u:r:su:s0

# drop the wireless transport
adb disconnect 192.168.1.10:5555
```

## Security note

`ro.adb.secure` is `0` on this build, so adbd does **not** do the RSA
authorization handshake -- there is no "Allow USB debugging?" prompt and no
key check. With 5555 listening persistently, anyone on the LAN who can reach
the headset gets a shell, and a root shell after any `adb root` this boot.

That is fine on a trusted home network and it is what makes the puck workflow
practical. Do not take these headsets onto a network you do not control with
`persist.adb.tcp.port` set.

---

# What root adb can and cannot manage

## Where the root actually comes from

This matters more than it looks. There is **no Magisk** on this device --
`/data/adb` does not exist and there is no `su` binary. Root is purely
`adb root` against a debuggable build:

```
ro.debuggable = 1      ro.secure = 1      ro.build.type = user
ro.boot.flash.locked = 0     verifiedbootstate = orange     veritymode = enforcing
```

Consequences:

- Anything we run as root has to be **launched from adb**. There is no
  on-device `su` for an app or a boot script to call.
- `/` is mounted **read-only** (`/dev/root ... ext4 (ro,...)`) with dm-verity
  **enforcing**, so `/system` and `/vendor` cannot be modified in place, even
  as root.
- The bootloader **is** unlocked, so verity *could* be turned off by flashing
  vbmeta with `--disable-verity --disable-verification`. Not done.

## Persistent adb: the *capability* persists, the rooted daemon does not

There are two different things people mean by "persistent root", and only one
of them is true here. Keeping them separate avoids a wrong conclusion.

| | persists a reboot? |
|---|---|
| listening on 5555 (`persist.adb.tcp.port`) | **yes**, verified |
| `adb root` is *permitted* (always succeeds) | **yes**, by fastboot flags |
| adbd already *running* as root at boot | **no**, verified |
| SELinux permissive | no (and not needed -- see above) |
| proximity override | no |

**The fastboot flags make `adb root` a permanent capability.** The unlock
guide's one-time commands --

```
fastboot oem set-appended-cmdline androidboot.adb.rootable=1
fastboot oem set-enable-adb-on-retail 1
```

-- append `androidboot.adb.rootable=1` to the kernel cmdline, which shows up as
`ro.boot.adb.rootable=1`. Oculus's adbd is patched to read that property (it is
right there in `strings /system/bin/adbd` alongside `service.adb.root` and
`ro.secure`), and it is what guarantees `adb root` will succeed on every boot.
**Do not touch these** -- they are what makes any of this work.

**But that flag permits root; it does not pre-grant it.** adbd still starts
each boot as `uid=2000(shell)` and only re-execs as root when
`service.adb.root=1`, which `adb root` sets. Verified directly: after a cold
boot with no `adb root` issued, `getprop service.adb.root` is empty and `id`
is shell on both USB and wifi. `service.adb.root` is a non-persistent
`service.*` property and nothing sets it at boot -- no init trigger does:

```bash
grep -rln 'setprop service.adb' /system/etc/init/ /vendor/etc/init/ /init.rc
# (no matches)
```

So the per-boot sequence is, by design, "adbd is up and rootable -> issue
`adb root` once -> it succeeds because of the fastboot flag." That one command
is the whole cost. Making adbd come up *already* rooted would mean setting
`service.adb.root=1` from an init `.rc`, which lives on a read-only
verity-enforced partition -- i.e. disabling verity and remounting. Not worth
it: `adb root` over wifi restarts adbd and it comes straight back on 5555.

So instead of fighting for root-at-boot, let the host do it:

```bash
./tools/q1bringup.sh 192.168.1.10
```

It connects, re-roots if adbd came back as shell, reapplies the proximity
override, and carries on through the rest of the per-boot setup. That is the
whole post-reboot recovery for a puck, and the GUI runs the same steps for the
pucks it knows about.

## Wifi: yes, fully, but not through `cmd wifi`

Android 10's `cmd wifi` has **no** `connect-network` / `add-network` --
those arrived in Android 11. The full Android 10 command set is only rssi
polling, hi-perf/low-latency toggles, network-suggestion approvals, softap
channel and country code. Nothing that joins a network.

What *does* work is talking to wpa_supplicant directly. `/vendor/bin/wpa_cli`
ships on the device and root can reach the control socket:

```bash
W="/vendor/bin/wpa_cli -p /data/vendor/wifi/wpa/sockets -i wlan0"
adb shell "$W status"          # ssid, bssid, freq, ip_address, wpa_state
adb shell "$W scan" && sleep 3
adb shell "$W scan_results"    # bssid / freq / signal / flags / ssid
adb shell "$W list_networks"
```

Adding a network is verified working, and does **not** disturb the live
connection:

```bash
NID=$(adb shell "$W add_network" | tail -1 | tr -d '\r')
adb shell "$W set_network $NID ssid '\"MySSID\"'"
adb shell "$W set_network $NID psk '\"MyPassword\"'"     # or: key_mgmt NONE
adb shell "$W select_network $NID"                        # switches over
adb shell "$W remove_network $NID"                        # undo
```

Caveat worth knowing: the framework's `WifiConfigManager` owns supplicant
config and reconciles it. A network added this way is live immediately but may
not survive a wifi restart or reboot. The durable store is
`/data/misc/wifi/WifiConfigStore.xml` (root-readable, one `<Network>` block per
SSID); edit that for a network that should come back after a reboot.

`svc wifi enable|disable` also works for plain on/off.

Note `p2p0` exists and is up, so **Wi-Fi Direct is available** -- potentially
interesting for puck-to-puck map transfer without an AP in the loop. Untested.

## Other device management that works

All of the ordinary root-adb surface is there and useful for running several
pucks headless:

- `settings get|put global|secure|system ...` -- e.g. sleep and stay-awake
  behaviour
- `pm list|disable-user|enable ...` -- 105 packages installed, none currently
  disabled; trimming Oculus services could matter for a body-worn puck's
  thermals and battery
- `svc power|wifi|data`, `am`, `input`, `dumpsys`, full `/data` access
- `service call <name> <code>` -- how the Insight colocation API was reached
  (see [insight-map-and-anchors.md](insight-map-and-anchors.md))
- `adb reboot bootloader` -- fastboot is reachable, bootloader unlocked

## Security, restated

Combining what we now know: `ro.adb.secure=0` means no key handshake, 5555 is
open from boot, and the AP these are on is **open** (`key_mgmt=NONE`, no
passphrase). Anyone within radio range gets a shell on the headset, and root
after any `adb root` this boot.

That is a deliberate trade for a lab setup. Worth revisiting before these
pucks are worn anywhere that is not a controlled network.

---

# Bringing a new puck online

Since root-at-boot is deliberately host-driven (above), a puck that is
power-cycled comes back as plain shell and the host has to re-root it. The GUI
does that for the pucks it knows about; `tools/q1bringup.sh <ip>` does it for
one puck from the command line, along with the rest of the per-boot setup.

The wifi step had a wrinkle worth recording, found for real on puck 2.

1. Once over USB, enable the persistent wireless port:
   ```bash
   adb -s <serial> shell setprop persist.adb.tcp.port 5555
   ```
   (The `androidboot.adb.rootable=1` fastboot flag from the unlock guide is
   assumed already set, so `adb root` works on every boot.)
2. **Get it onto wifi** -- see below if it has never joined.
3. Give it a static DHCP lease on the router so its IP is stable.
4. Add the puck to `insight-map-loader.json` and assign it a role.
5. `./tools/q1bringup.sh <ip>` should take it all the way to 6DoF.

## A puck that has never joined the network

Puck 2 arrived with `wpa_state=DISCONNECTED`, an empty `list_networks`, and no
IP -- wifi enabled but never configured. Two things were learned.

**`wpa_cli` alone does not work.** Adding and selecting the network at the
supplicant level associates for a moment and is then reconciled away by the
framework's `WifiConfigManager`, which owns supplicant config:

```
select_network 0 -> OK
... 8s later ...
wpa_state=DISCONNECTED        # framework dropped the unknown network
```

So the framework has to own the network. On Android 10 there is no
`cmd wifi connect-network` to ask it nicely, which leaves the config store.

**Transplant `WifiConfigStore.xml` from a puck that is already joined**, then
reboot so the framework reads it at boot:

```bash
adb -s <joined> pull /data/misc/wifi/WifiConfigStore.xml .
# edit (see the MAC warning below), then:
adb -s <new> push WifiConfigStore.xml /data/local/tmp/wcs.xml
adb -s <new> shell "cp /data/local/tmp/wcs.xml /data/misc/wifi/WifiConfigStore.xml"
adb -s <new> shell "chown system:system /data/misc/wifi/WifiConfigStore.xml"
adb -s <new> shell "chmod 600 /data/misc/wifi/WifiConfigStore.xml"
adb -s <new> reboot
```

Copying over the existing file keeps the `u:object_r:wifi_data_file:s0`
context. It survived the shutdown untouched -- `system_server` did not rewrite
it on the way down -- and the puck came up joined with a DHCP lease.

> **Do not copy the file verbatim.** It contains a per-network
> `RandomizedMacAddress`, but the puck actually associates using its factory
> MAC -- so randomization is configured but inactive on this
> build. Copy it and two pucks carry the same MAC -- a horrible thing to
> debug, and it breaks MAC-keyed static DHCP leases if the feature ever turns
> on. Give each puck a unique locally-administered MAC (deriving it from the
> serial makes it deterministic) and set `MacRandomizationSetting` to `0` so
> the factory MAC is used. Also reset `NumAssociation` to `0`.

Only those three fields should differ from the donor file.
