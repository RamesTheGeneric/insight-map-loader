# Wireless adb on the Quest 1

The Quest 1 (`monterey`) is **Android 10 / API 29**, so this is the classic
`adb tcpip` route -- *not* the Android 11+ `adb pair` / mDNS flow. There is no
pairing code and no Developer-options "Wireless debugging" toggle to find.

Verified working end to end: the HAL camera path, the frame rings and the MJPEG
server all run over wifi with no measurable penalty versus USB (numbers below).

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

`adb root` and any `setenforce` are **not** persistent. Symptom is
`cp: /data/nativetest64/vendor/ovrcam/q1serve: Permission denied` out of
`deploy_q1.sh`. Fix:

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
  `u:r:su:s0`, and the whole camera pipeline runs with SELinux still
  **Enforcing**. Earlier sessions had run `setenforce 0`; it turns out to be
  unnecessary, so leave enforcing on.
- The **proximity override is also cleared** by a reboot. Re-apply it, or just
  use `deploy_q1.sh`, which does it for you:
  ```bash
  adb shell "am broadcast -a com.oculus.vrpowermanager.prox_close"
  adb shell "setprop debug.oculus.forceHeadsetOn 1"
  ```

## Using it with the existing scripts

`deploy_q1.sh`, `deploy_run.sh` and friends all call bare `adb`. With USB *and*
wifi both attached that fails with `adb: more than one device/emulator`. No
script changes are needed -- adb honours `ANDROID_SERIAL`:

```bash
export ANDROID_SERIAL=192.168.1.10:5555
./deploy_q1.sh ring
./deploy_q1.sh serve
```

`deploy_q1.sh serve` sets up `adb forward tcp:8080`, which works over wifi too,
so `http://localhost:8080/` keeps working. But over wifi you can also skip the
forward entirely and hit the device directly:

```
http://192.168.1.10:8080/
```

Handy when the headset is being worn or is on a stand across the room, which is
the whole point for the tracker-puck work.

## Performance: wifi vs USB

The Quest 1's USB is 2.0 (~40 MB/s ceiling), and 5 GHz wifi lands in the same
place, so there is essentially nothing to lose.

Bulk transfer, 32 MB `dd` through `adb shell`:

| transport | throughput |
|---|---|
| USB | 33.8 MB/s |
| wifi (5 GHz) | 30.5 MB/s |

MJPEG server, 4-camera 1280x960 mosaic, q=75, direct to the device IP with no
adb forward:

| mode | served fps | capture fps | jpeg | skipped | torn |
|---|---|---|---|---|---|
| default (long exposure only) | 29.6 | 29.99 | 155 KB | 414* | 0 |
| `--exposure any` | **57.2** | 56.92 | 167 KB | **0** | 0 |

\* those skips are from the client attach/detach at the edges of the sample,
not steady state.

57.2 fps x 167 KB = **9.6 MB/s (~76 Mbps)** sustained with **zero skipped and
zero torn frames**. Wireless keeps up with the sensors completely.

### One gotcha that looks like a wifi problem and is not

A first measurement showed **2.5 fps**, which looks damning for wifi. It was
not. `/stats` told the real story:

```json
{"fps":5.76,"capture_fps":30.02,"encode_ms":16.25,"skipped":1939,"clients":2}
```

Capture was at full rate and encode was fine; the served rate had collapsed.
The cause was a **stale client**: a `curl` that had hit its `--max-time` was
still counted in `clients`, and the server was stalling on the dead socket.
Killing and restarting `q1serve` restored 29.6 fps immediately.

So: when the stream looks slow, read `/stats` before blaming the network.
`capture_fps` vs `fps` separates "the sensors/HAL are struggling" from "the
delivery path is struggling", and `clients` will show a leaked connection.

## Quick reference

```bash
# connect (after a host reboot, or first thing in a session)
adb connect 192.168.1.10:5555
export ANDROID_SERIAL=192.168.1.10:5555

# after a *device* reboot, additionally:
adb root && sleep 3 && adb connect 192.168.1.10:5555

# sanity
adb shell id            # want uid=0(root), context u:r:su:s0
curl -s http://192.168.1.10:8080/stats

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

So instead of fighting for root-at-boot, use the helper:

```bash
eval "$(./tools/q1connect.sh)"          # default IP
eval "$(./tools/q1connect.sh 192.168.1.13)"
eval "$(./tools/q1connect.sh --usb)"    # discover IP from a USB-attached puck
```

It connects, re-roots if adbd came back as shell, reapplies the proximity
override, and prints the `export ANDROID_SERIAL=...` line to eval. That is the
whole post-reboot recovery for a puck.

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

# Fleet control from the host: `tools/q1fleet.sh`

Since root-at-boot is deliberately host-driven (above), the whole fleet is
managed from one host-side script. Nothing runs on the headsets except the
camera server. `q1fleet.sh` wraps the per-boot `adb root` + prox dance for
every puck and gives one place to start, watch and query them.

The puck list comes from, in order: the `$Q1_PUCKS` env var (space/comma
separated), `tools/pucks.list` (one IP per line, gitignored because it is
site-specific, like the calibration), or the single default IP baked into the
script.

```bash
# who is up, and are they streaming?
./tools/q1fleet.sh status
# PUCK                 STATE  SERIAL          Q1SERVE  FPS
# 192.168.1.10:5555   root   <SERIAL>  3772     62.75
# 192.168.1.13:5555   down   -               -        -

./tools/q1fleet.sh up                          # connect + root + prox, all pucks
./tools/q1fleet.sh serve --exposure any --q 75 # deploy + start q1serve on each
./tools/q1fleet.sh stop                        # stop the servers
./tools/q1fleet.sh run "getprop ro.serialno"   # run a command on every puck
```

`serve` starts `q1serve` **detached** on each puck (`setsid`, fds redirected to
`/data/local/tmp/q1serve.log`), so the host `adb shell` returns immediately and
each puck keeps streaming on its own IP at `:8080`. `status` reads each puck's
`/stats` directly over the LAN, so `FPS` is live (it reads 0 with no client
attached -- the encoder only runs on demand -- and jumps to ~60 once something
is pulling the stream).

For an unattended rig, `watch` is the closest thing to "autonomous pucks"
without touching the device: it re-checks every puck on an interval, re-roots
any that rebooted back to shell, and with `--serve` restarts `q1serve` on any
puck found not running.

```bash
./tools/q1fleet.sh watch --serve 20    # keep the fleet rooted + streaming, every 20s
```

Run that on the same host that consumes the tracking data and a puck that is
power-cycled comes back to a streaming state a few seconds later with no manual
step -- the host notices it, re-roots it, and restarts the server. That is the
practical substitute for an on-device boot service, and it needs no changes to
the read-only system partition.

## Bringing a new puck online

Done for real with puck 2 (`<SERIAL>`); the wifi step had a wrinkle worth
recording.

1. Once over USB, enable the persistent wireless port:
   ```bash
   adb -s <serial> shell setprop persist.adb.tcp.port 5555
   ```
   (The `androidboot.adb.rootable=1` fastboot flag from the unlock guide is
   assumed already set, so `adb root` works on every boot.)
2. **Get it onto wifi** -- see below if it has never joined.
3. Give it a static DHCP lease on the router so its IP is stable.
4. Add the IP to `tools/pucks.list`.
5. `./tools/q1fleet.sh up` should show it as `root`.

### A puck that has never joined the network

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
> `RandomizedMacAddress`. Puck 1's is `36:86:55:bc:c0:88` while it actually
> uses its factory MAC, so randomization is configured but inactive on this
> build. Copy it and two pucks carry the same MAC -- a horrible thing to
> debug, and it breaks MAC-keyed static DHCP leases if the feature ever turns
> on. Give each puck a unique locally-administered MAC (deriving it from the
> serial makes it deterministic) and set `MacRandomizationSetting` to `0` so
> the factory MAC is used. Also reset `NumAssociation` to `0`.

Only those three fields should differ from the donor file.

## GUI: `tools/q1gui.py`

A local web dashboard over the same logic, for when a terminal table is not
enough. The pucks already serve MJPEG over HTTP, so the browser can embed each
puck's live 4-camera mosaic directly next to its controls -- no extra plumbing,
and no native GUI toolkit fighting to render four video streams.

```bash
./tools/q1fleet.sh gui              # or: python3 tools/q1gui.py
# q1 fleet GUI -> http://127.0.0.1:8090/
```

Stdlib only -- no pip install. `--port N` to move it, `--host 0.0.0.0` to reach
it from another machine (read the security note above first; that exposes puck
control to the LAN).

One card per puck, refreshed every 3 s:

- status dot (green root / amber shell / red down), serial, `ip:port`, pid
- the **live mosaic** embedded straight from `http://<puck>:8080/stream`
- the full `/stats` row -- served fps, capture fps, encode ms, KB/frame,
  skipped, torn, clients, exposure
- per-puck **Root / Serve / Stop / Reboot**, an "Open" link to the raw stream,
  and a shell box that runs a command on that puck and shows the output
- header controls to Serve/Stop/Root the **whole fleet** at once, with the
  `q1serve` flags in an editable field

### A bug worth recording

The obvious implementation -- re-render `grid.innerHTML` on every poll --
is wrong here, and quietly so. Replacing the HTML destroys and recreates every
`<img>`, which **restarts each MJPEG stream every 3 seconds** and leaks a
client connection on the puck each time. That is precisely the stale-client
condition that collapsed the served rate to 2.5 fps earlier in this document.

So the dashboard rebuilds a card only when its *structure* changes (reachable /
root-vs-shell / serving / pid / serial) and otherwise patches the stat numbers
in place. Verified with a DOM stub driving the real render function:

```
steady-state polls -> same <img> element: true
steady-state polls -> same card element : true
fps cell patched in place               : true
serve->stop rebuilds the card           : true
```

and against the live puck, `clients` held flat across repeated polls.

### API

The backend is a plain JSON API, so it is scriptable too:

```bash
curl -s localhost:8090/api/pucks | python3 -m json.tool
curl -s -X POST localhost:8090/api/action -H 'Content-Type: application/json' \
     -d '{"op":"serve","flags":"--exposure any --q 75"}'
```

`op` is one of `up`, `serve`, `stop`, `deploy`, `reboot`, `run` (with `cmd`).
Omit `targets` to hit the whole fleet, or pass a list of `ip:port`. Unknown ops,
empty `run` commands and malformed JSON are rejected with 400.

## Two pucks at once

First real fleet test, both streaming `--exposure any --q 75` concurrently to
separate clients over the same AP:

| puck | served fps | capture fps | jpeg | skipped | torn |
|---|---|---|---|---|---|
| 1 `.108` | 49.6 | 59.1 | 193 KB | 0 | 0 |
| 2 `.132` | 59.9 | 60.1 | 35 KB | 0 | 0 |

**Zero skipped and zero torn on both**, roughly 94 Mbps combined. Wifi is not
the constraint for two pucks, and there is headroom for the third.

The large difference in `jpeg_bytes` is not a fault: puck 2 was sitting in a
dark spot. Snapshots from both confirmed all four cameras working on each --
a near-black scene simply compresses to 35 KB where a lit room takes 193 KB.
When a puck's frames look suspiciously small, pull `/snapshot` and look before
assuming the capture path is broken.

### `Text file busy` on re-deploy

Re-serving over an already-running server failed on both pucks:

```
192.168.1.10:5555  deploy FAILED
192.168.1.11:5555  deploy FAILED
```

`deploy_one` copied the new binary into place *before* anything stopped the
old one, and Linux will not let a running executable be overwritten:

```
cp: /data/nativetest64/vendor/ovrcam/q1serve: Text file busy
```

It only ever worked before because nothing was running yet. Both `q1fleet.sh`
and the GUI backend now `pkill` the binaries before the copy.
