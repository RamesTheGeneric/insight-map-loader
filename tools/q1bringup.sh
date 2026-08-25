#!/bin/bash
# Bring a freshly-rooted Quest 1 up as a tracker puck.
#
# Automates everything on the DEVICE side of docs/puck-bringup.md: wifi adb,
# root, the tracker app, the off-head provisioning, and the guardian package.
# What it cannot do is the part that needs the room -- giving the puck the
# fleet's map, bridging and picking a role -- so it ends by telling you exactly
# what is left.
#
# Every step VERIFIES rather than assumes. Most of these failures are silent:
# a puck with the guardian package enabled looks perfectly healthy and emits
# nothing, and without root `setprop` fails without saying so.
#
# Idempotent -- safe to re-run on a puck that is already part way up.
#
# Usage:
#   ./tools/q1bringup.sh <ip> [options]
#
#     --apk <path>     install/update the tracker APK first
#     --device <0-10>  SteamVR role id; also adds the puck to q2slam.json
#     --config <file>  config to edit          (default q2slam.json)
#     --usb <serial>   enable wifi adb over USB first, for a puck not yet on wifi
#     --no-wait        skip the 45 s appop flush (then do NOT reboot straight away)
#
# Examples:
#   ./tools/q1bringup.sh 192.168.1.12 --usb <SERIAL> --apk out/q1tracker.apk --device 2
#   ./tools/q1bringup.sh 192.168.1.12          # re-check / repair an existing puck
set -u
cd "$(dirname "$(readlink -f "$0")")/.."   # repo root

TRACKER_PKG=com.mapperlocalizer.questtracker
GUARDIAN_PKG=com.oculus.guardian

IP=""; APK=""; DEVICE=""; CONFIG=q2slam.json; USB=""; WAIT=1
FAILED=0

say()  { printf '%s\n' "$*" >&2; }
ok()   { printf '  \033[32m✔\033[0m %s\n' "$*" >&2; }
bad()  { printf '  \033[31m✖\033[0m %s\n' "$*" >&2; FAILED=1; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*" >&2; }
step() { printf '\n\033[1m%s\033[0m\n' "$*" >&2; }

sh_() { timeout 30 adb -s "$IP:5555" shell "$@" 2>/dev/null | tr -d '\r'; }

usage() { sed -n '2,27p' "$0" | sed 's/^# \?//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --apk)     APK="${2:?}"; shift 2 ;;
    --device)  DEVICE="${2:?}"; shift 2 ;;
    --config)  CONFIG="${2:?}"; shift 2 ;;
    --usb)     USB="${2:?}"; shift 2 ;;
    --no-wait) WAIT=0; shift ;;
    -h|--help) usage 0 ;;
    -*)        say "unknown option: $1"; usage 2 ;;
    *)         [ -z "$IP" ] && IP="$1" || { say "unexpected argument: $1"; usage 2; }; shift ;;
  esac
done
[ -n "$IP" ] || usage 2
command -v adb >/dev/null || { say "adb not found in PATH"; exit 2; }

if [ -n "$DEVICE" ] && { [ "$DEVICE" -lt 0 ] 2>/dev/null || [ "$DEVICE" -gt 10 ] 2>/dev/null; }; then
  say "--device must be 0-10 (see the role table in docs/puck-bringup.md)"; exit 2
fi

say "bringing up $IP"

# ---------------------------------------------------------------- 1. wifi adb
step "1. wireless adb"
if [ -n "$USB" ]; then
  # Quest 1 is Android 10: the classic tcpip route, no pairing code.
  adb -s "$USB" tcpip 5555 >/dev/null 2>&1
  sleep 2
  ok "enabled tcpip on $USB"
fi
adb connect "$IP:5555" >/dev/null 2>&1
if [ -n "$(sh_ 'echo up')" ]; then
  ok "reachable at $IP:5555"
else
  bad "cannot reach $IP:5555 -- is it on wifi? try --usb <serial>"
  exit 1
fi

# ------------------------------------------------------------------- 2. root
step "2. root"
# Root does NOT survive a reboot, and without it setprop/chown/chcon fail
# SILENTLY -- so this is checked, not assumed, and re-run on every bring-up.
if [ "$(sh_ id -u)" != "0" ]; then
  adb -s "$IP:5555" root >/dev/null 2>&1
  sleep 3
  adb connect "$IP:5555" >/dev/null 2>&1
  sleep 1
fi
if [ "$(sh_ id -u)" = "0" ]; then
  ok "adb root (serial $(sh_ getprop ro.serialno))"
else
  bad "no root -- every later step would fail silently; stopping"
  exit 1
fi

# ------------------------------------------------------------- 3. tracker app
step "3. tracker app"
if [ -n "$APK" ]; then
  if [ -f "$APK" ]; then
    if timeout 180 adb -s "$IP:5555" install -r "$APK" >/dev/null 2>&1; then
      ok "installed $APK"
    else
      bad "install failed: $APK"
    fi
  else
    bad "no such file: $APK"
  fi
fi
if [ -n "$(sh_ "pm path $TRACKER_PKG")" ]; then
  ok "$TRACKER_PKG present"
else
  bad "$TRACKER_PKG not installed -- pass --apk <path>"
fi

# -------------------------------------------------------------- 4. provision
step "4. provision for off-head, unattended use"
# persist.* on purpose: init restores them every boot. The volatile
# debug.oculus.forceHeadsetOn is cleared by a reboot, which is exactly when it
# is needed.
sh_ "appops set $TRACKER_PKG SYSTEM_ALERT_WINDOW allow" >/dev/null
sh_ "setprop persist.oculus.guardian_disable 1
     setprop persist.oculus.forceHeadsetOn 1
     setprop persist.ovr.disable.sensorproxy true
     settings put system screen_off_timeout 86400000
     settings put global wifi_sleep_policy 2" >/dev/null

[ "$(sh_ getprop persist.oculus.guardian_disable)" = "1" ] \
  && ok "guardian_disable=1" || bad "guardian_disable did not stick"
[ "$(sh_ getprop persist.oculus.forceHeadsetOn)" = "1" ] \
  && ok "forceHeadsetOn=1 (survives reboot)" || bad "forceHeadsetOn did not stick"

if [ "$WAIT" = "1" ]; then
  # The appop must reach disk; rebooting inside this window loses it.
  say "  waiting 45 s for the appop to flush (--no-wait to skip)"
  sleep 45
fi
case "$(sh_ "appops get $TRACKER_PKG SYSTEM_ALERT_WINDOW")" in
  *allow*) ok "SYSTEM_ALERT_WINDOW allowed (boot-start)" ;;
  *)       warn "SYSTEM_ALERT_WINDOW not confirmed -- re-run without --no-wait" ;;
esac

# ------------------------------------------------------- 5. guardian package
step "5. guardian package"
# THE step people miss. The property above is NOT sufficient: with the package
# enabled the tracker app runs, takes focus, has the right config, reports
# 6DOF Valid: Yes -- and emits nothing at all. It also drives the displays to
# passthrough instead of dark. Must be disabled BEFORE the app first starts.
sh_ "am force-stop $GUARDIAN_PKG; pm disable-user --user 0 $GUARDIAN_PKG" >/dev/null
if [ "$(sh_ "pm list packages -d" | grep -c "$GUARDIAN_PKG")" = "1" ]; then
  ok "$GUARDIAN_PKG disabled"
else
  bad "$GUARDIAN_PKG still enabled -- the puck will not stream poses"
fi

# ------------------------------------------------------------ 6. fleet config
step "6. fleet config"
if [ -n "$DEVICE" ]; then
  if [ -f "$CONFIG" ]; then
    python3 - "$CONFIG" "$IP" "$DEVICE" <<'PY'
import json, sys
path, ip, dev = sys.argv[1], sys.argv[2], int(sys.argv[3])
cfg = json.load(open(path))
pucks = cfg.setdefault("pucks", [])
clash = next((p for p in pucks if p.get("ip") != ip and p.get("device") == dev), None)
if clash:
    # Two pucks on one id fight at packet rate and the tracker flickers
    # between two bodies, with nothing reporting a problem.
    print(f"  \033[31m✖\033[0m device {dev} already belongs to {clash['ip']}", file=sys.stderr)
    sys.exit(1)
existing = next((p for p in pucks if p.get("ip") == ip), None)
if existing:
    was = existing.get("device")
    existing["device"] = dev
    msg = f"updated {ip}: device {was} -> {dev}" if was != dev else f"{ip} already device {dev}"
else:
    pucks.append({"ip": ip, "device": dev})
    msg = f"added {ip} as device {dev}"
json.dump(cfg, open(path, "w"), indent=2)
open(path, "a").write("\n")
print(f"  \033[32m✔\033[0m {msg}", file=sys.stderr)
PY
    [ $? -eq 0 ] || FAILED=1
  else
    bad "no $CONFIG -- copy desktop/q2slam.example.json and set your host IP"
  fi
else
  warn "no --device given; add it to $CONFIG yourself (role table in docs/puck-bringup.md)"
fi

# ----------------------------------------------------------------- 7. verify
step "7. state"
# NEVER pipe `dumpsys tracking` into something that closes the pipe early
# (grep -m1, head): it leaves the tracking service unavailable for seconds
# with "Can't find service: tracking". Dump to a file on-device, grep the
# file -- the same rule fleet.rs states at the top. One round trip.
STATE=$(sh_ 'dumpsys tracking > /data/local/tmp/q1bu.txt 2>/dev/null
             grep -m1 "Tracking Level" /data/local/tmp/q1bu.txt
             echo "@@"
             grep -m1 "Vega Map Context" /data/local/tmp/q1bu.txt
             echo "@@"
             dumpsys battery | grep -m1 level:')
TRACK=$(printf '%s' "$STATE" | sed -n '1,/@@/p' | grep -oE '[0-9]DOF' || true)
MAPROOT=$(printf '%s' "$STATE" | sed -n '/@@/,/@@/p' | grep "Vega Map Context" || true)
BATT=$(printf '%s' "$STATE" | grep -oE 'level: [0-9]+' | grep -oE '[0-9]+' || true)
say "  tracking : ${TRACK:-none}"
say "  battery  : ${BATT:-?}%"
case "$MAPROOT" in
  *persistent*) ok "has a persistent map: $(echo "$MAPROOT" | grep -oE 'topNodeUid [0-9a-f]{8}')" ;;
  *transient*)  warn "map context is TRANSIENT -- it will never be written; needs step 8" ;;
  *)            warn "no map context yet (needs to be awake and tracking)" ;;
esac

# ------------------------------------------------------------------- summary
step "what is left (needs the room, and the GUI)"
cat >&2 <<TXT
  8. give it the map      ./run-gui.sh  ->  ⇄ Share map    (or ✚ Create map for a new space)
                          the puck must be IN that space and able to see mapped territory
  9. bridge               hold the pucks still  ->  ⌖ Bridge now
 10. assign its role      the dropdown on its card;  ⚑ identifies it by flash count

  check:  ./desktop/target/release/q2slam mapdb     # every puck on the same root
          tools/q1sep.py                            # ~3 cm with two pucks held together

  docs/puck-bringup.md has the reasoning for each step.
TXT

if [ "$FAILED" = "1" ]; then
  say ""
  say "one or more steps FAILED -- see the ✖ lines above"
  exit 1
fi
say ""
say "device-side bring-up complete for $IP"
