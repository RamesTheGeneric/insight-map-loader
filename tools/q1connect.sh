#!/bin/bash
# Connect to a Quest 1 over wireless adb and restore root.
#
#   ./tools/q1connect.sh                 # use the default IP below
#   ./tools/q1connect.sh 192.168.1.13   # another puck
#   ./tools/q1connect.sh --usb           # find the IP via a USB-attached device
#
# Root is NOT persistent across a device reboot (adbd comes back as shell), so
# this does the connect -> adb root -> reconnect dance every time. Port 5555 is
# persistent via persist.adb.tcp.port, so no `adb tcpip` is needed.
#
# Prints the export line to use:  eval "$(./tools/q1connect.sh)"
set -e

DEFAULT_IP=192.168.1.10
PORT=5555

say() { echo "$@" >&2; }

if [ "$1" = "--usb" ]; then
  USB=$(adb devices | awk '/\tdevice$/ && $1 !~ /:/ {print $1; exit}')
  [ -n "$USB" ] || { say "no USB device attached"; exit 1; }
  IP=$(ANDROID_SERIAL=$USB adb shell "ip -4 addr show wlan0" 2>/dev/null \
       | grep -oE 'inet [0-9.]+' | cut -d' ' -f2 | tr -d '\r')
  [ -n "$IP" ] || { say "device $USB has no wlan0 address"; exit 1; }
  say "found $USB at $IP"
else
  IP="${1:-$DEFAULT_IP}"
fi

case "$IP" in *:*) TARGET="$IP" ;; *) TARGET="$IP:$PORT" ;; esac

adb connect "$TARGET" >/dev/null 2>&1 || true
sleep 1
if ! ANDROID_SERIAL=$TARGET adb shell true >/dev/null 2>&1; then
  say "cannot reach $TARGET"
  say "  - is the device awake and on wifi?"
  say "  - first time on this device, do it once over USB:"
  say "      adb -s <serial> tcpip $PORT"
  say "      adb -s <serial> shell setprop persist.adb.tcp.port $PORT"
  exit 1
fi

UID_NOW=$(ANDROID_SERIAL=$TARGET adb shell id -u 2>/dev/null | tr -d '\r')
if [ "$UID_NOW" != "0" ]; then
  say "adbd is uid $UID_NOW; restarting as root"
  ANDROID_SERIAL=$TARGET adb root >/dev/null 2>&1 || true
  for i in $(seq 1 15); do
    sleep 1
    adb connect "$TARGET" >/dev/null 2>&1 || true
    UID_NOW=$(ANDROID_SERIAL=$TARGET adb shell id -u 2>/dev/null | tr -d '\r')
    [ "$UID_NOW" = "0" ] && break
  done
fi

if [ "$UID_NOW" != "0" ]; then
  say "connected to $TARGET but could not get root (uid=$UID_NOW)"
  exit 1
fi

# A reboot also clears the proximity override, so put it back.
ANDROID_SERIAL=$TARGET adb shell \
  "am broadcast -a com.oculus.vrpowermanager.prox_close; setprop debug.oculus.forceHeadsetOn 1" \
  >/dev/null 2>&1 || true

say "$TARGET  root  $(ANDROID_SERIAL=$TARGET adb shell getprop ro.serialno | tr -d '\r')"
echo "export ANDROID_SERIAL=$TARGET"
