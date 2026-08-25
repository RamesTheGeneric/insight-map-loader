#!/bin/bash
# Host-side controller for a fleet of Quest 1 tracker pucks.
#
# Each puck listens on wireless adb (port 5555, persistent via
# persist.adb.tcp.port). Root is available on every boot because the unlock
# set androidboot.adb.rootable=1, but adbd still starts as `shell` -- so each
# puck needs a one-time `adb root` after it boots. This tool does that dance
# for the whole fleet, keeps the camera server running, and lets you run a
# command across every puck at once. Nothing runs on the headsets themselves;
# this is all host side.
#
# Puck list, in priority order:
#   1. $Q1_PUCKS            space/comma separated IPs, e.g. "192.168.1.10 192.168.1.13"
#   2. tools/pucks.list     one IP (or ip:port) per line, # comments allowed
#   3. the single default IP below
#
# Commands:
#   up                 connect + root + prox-override every puck
#   status             one-line health per puck (reachable/uid/serial/serve/fps)
#   deploy             push out/q1serve + out/q1diag to every puck
#   serve [flags]      deploy, then (re)start q1serve on every puck, detached
#   stop               stop q1serve on every puck
#   run "<cmd>"        run a shell command on every puck (as whatever uid it is)
#   identify [secs]    blink each puck's LED a distinct colour to tell them apart
#   blink <ip> [color] [secs]   blink one puck's LED (red/green/blue/yellow/cyan/magenta/white)
#   gui [--port N]     launch the web dashboard (tools/q1gui.py)
#   watch [secs]       daemon: keep the fleet up (and, with --serve, streaming)
#                      re-checking every <secs> (default 15). Add --serve to
#                      (re)start q1serve whenever a puck is found not running.
#
# Examples:
#   ./tools/q1fleet.sh up
#   ./tools/q1fleet.sh serve --exposure any --q 75
#   ./tools/q1fleet.sh status
#   ./tools/q1fleet.sh run "getprop ro.serialno"
#   ./tools/q1fleet.sh watch --serve 20
set -u
cd "$(dirname "$(readlink -f "$0")")/.."   # repo root

DEFAULT_IP=192.168.1.10
PORT=5555
DST=/data/nativetest64/vendor/ovrcam
SERVE_LOG=/data/local/tmp/q1serve.log

say() { echo "$@" >&2; }

# ---- puck list -------------------------------------------------------------
pucks() {
  local raw=""
  if [ -n "${Q1_PUCKS:-}" ]; then
    raw="$Q1_PUCKS"
  elif [ -f tools/pucks.list ]; then
    raw="$(grep -vE '^\s*(#|$)' tools/pucks.list)"
  else
    raw="$DEFAULT_IP"
  fi
  # normalise to ip:port, one per line
  echo "$raw" | tr ', ' '\n\n' | grep -vE '^$' | while read -r p; do
    case "$p" in *:*) echo "$p" ;; *) echo "$p:$PORT" ;; esac
  done
}

# ---- per-puck primitives ---------------------------------------------------
# All take a target "ip:port" as $1.

_uid()   { ANDROID_SERIAL="$1" adb shell id -u 2>/dev/null | tr -d '\r'; }
_sh()    { local t="$1"; shift; ANDROID_SERIAL="$t" adb shell "$@"; }
_ip()    { echo "${1%%:*}"; }

# Connect, and if adbd came up as shell, re-root it. Returns 0 iff root.
ensure_root() {
  local t="$1"
  adb connect "$t" >/dev/null 2>&1 || true
  local u; u="$(_uid "$t")"
  [ -z "$u" ] && { sleep 1; adb connect "$t" >/dev/null 2>&1 || true; u="$(_uid "$t")"; }
  [ -z "$u" ] && return 1
  if [ "$u" != "0" ]; then
    ANDROID_SERIAL="$t" adb root >/dev/null 2>&1 || true
    local i
    for i in $(seq 1 15); do
      sleep 1
      adb connect "$t" >/dev/null 2>&1 || true
      u="$(_uid "$t")"
      [ "$u" = "0" ] && break
    done
  fi
  [ "$u" = "0" ]
}

# Keep the puck awake off-head (a reboot clears this).
apply_prox() {
  _sh "$1" "am broadcast -a com.oculus.vrpowermanager.prox_close; \
            setprop debug.oculus.forceHeadsetOn 1" >/dev/null 2>&1 || true
}

serve_pid() { _sh "$1" "pidof q1serve 2>/dev/null" | tr -d '\r'; }

# curl the on-device MJPEG server directly (no adb forward needed).
serve_stats() {
  curl -s --max-time 2 "http://$(_ip "$1"):8080/stats" 2>/dev/null
}

start_serve() {
  local t="$1"; shift
  _sh "$t" "mkdir -p $DST; pkill -9 q1serve" >/dev/null 2>&1 || true
  # detached: setsid + full fd redirection so adb shell returns immediately
  _sh "$t" "setsid $DST/q1serve $* >$SERVE_LOG 2>&1 </dev/null &" >/dev/null 2>&1 || true
}

deploy_one() {
  local t="$1"
  ANDROID_SERIAL="$t" adb push out/q1serve out/q1diag /data/local/tmp/ >/dev/null 2>&1 || return 1
  # A running binary cannot be overwritten in place ("Text file busy"), so stop
  # anything using it first. Re-deploying over a live server is the normal case.
  _sh "$t" "pkill -9 q1serve; pkill -9 q1diag" >/dev/null 2>&1 || true
  _sh "$t" "mkdir -p $DST && cp /data/local/tmp/q1serve /data/local/tmp/q1diag $DST/ && \
            chmod 755 $DST/q1serve $DST/q1diag" >/dev/null 2>&1
}

# ---- commands --------------------------------------------------------------
cmd_up() {
  local t
  for t in $(pucks); do
    if ensure_root "$t"; then
      apply_prox "$t"
      say "  $t  root  $(_sh "$t" getprop ro.serialno | tr -d '\r')"
    else
      say "  $t  UNREACHABLE"
    fi
  done
}

cmd_status() {
  printf "%-22s %-6s %-16s %-8s %s\n" PUCK STATE SERIAL Q1SERVE FPS
  local t
  for t in $(pucks); do
    adb connect "$t" >/dev/null 2>&1 || true
    local u; u="$(_uid "$t")"
    if [ -z "$u" ]; then
      printf "%-22s %-6s %-16s %-8s %s\n" "$t" "down" "-" "-" "-"
      continue
    fi
    local state; [ "$u" = "0" ] && state="root" || state="shell"
    local ser; ser="$(_sh "$t" getprop ro.serialno | tr -d '\r')"
    local pid; pid="$(serve_pid "$t")"
    local sv fps; sv="$(serve_stats "$t")"
    if [ -n "$sv" ]; then
      fps="$(echo "$sv" | grep -oE '"fps":[0-9.]+' | cut -d: -f2)"
    else
      fps="-"
    fi
    printf "%-22s %-6s %-16s %-8s %s\n" "$t" "$state" "$ser" "${pid:--}" "$fps"
  done
}

cmd_deploy() {
  [ -f out/q1serve ] || { say "out/q1serve missing -- run ./build_q1.sh first"; return 1; }
  local t
  for t in $(pucks); do
    ensure_root "$t" || { say "  $t  UNREACHABLE"; continue; }
    if deploy_one "$t"; then say "  $t  deployed"; else say "  $t  deploy FAILED"; fi
  done
}

cmd_serve() {
  [ -f out/q1serve ] || { say "out/q1serve missing -- run ./build_q1.sh first"; return 1; }
  local t
  for t in $(pucks); do
    ensure_root "$t" || { say "  $t  UNREACHABLE"; continue; }
    apply_prox "$t"
    deploy_one "$t" || { say "  $t  deploy FAILED"; continue; }
    start_serve "$t" "$@"
    sleep 2
    local pid; pid="$(serve_pid "$t")"
    if [ -n "$pid" ]; then
      say "  $t  serving (pid $pid)  ->  http://$(_ip "$t"):8080/"
    else
      say "  $t  q1serve did NOT start (see $SERVE_LOG on device)"
    fi
  done
}

cmd_stop() {
  local t
  for t in $(pucks); do
    ensure_root "$t" >/dev/null 2>&1 || { say "  $t  UNREACHABLE"; continue; }
    _sh "$t" "pkill -9 q1serve" >/dev/null 2>&1 || true
    say "  $t  stopped"
  done
}

cmd_run() {
  local cmd="$1"; local t
  for t in $(pucks); do
    adb connect "$t" >/dev/null 2>&1 || true
    if [ -z "$(_uid "$t")" ]; then say "== $t == UNREACHABLE"; continue; fi
    say "== $t =="
    _sh "$t" "$cmd"
  done
}

cmd_identify() {
  local secs="${1:-6}"
  local colors="red green blue yellow cyan magenta white"
  local i=0 t
  say "identifying pucks (each a different colour, ${secs}s):"
  for t in $(pucks); do
    local col; col=$(echo $colors | cut -d' ' -f$((i+1)))
    [ -n "$col" ] || col="white"
    if ensure_root "$t"; then          # LED sysfs writes need root
      say "  $(_ip "$t")  ->  $col"
      ./tools/q1blink.sh "$t" "$col" "$secs" >/dev/null 2>&1 &
    else
      say "  $(_ip "$t")  ->  UNREACHABLE"
    fi
    i=$((i+1))
  done
  wait
  say "done"
}

cmd_watch() {
  local do_serve=0 interval=15
  local serve_flags=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --serve) do_serve=1 ;;
      [0-9]*)  interval="$1" ;;
      *)       serve_flags="$serve_flags $1" ;;
    esac
    shift
  done
  say "watching $(pucks | tr '\n' ' ')  every ${interval}s  serve=$do_serve"
  say "(ctrl-c to stop)"
  while true; do
    local t
    for t in $(pucks); do
      if ensure_root "$t"; then
        apply_prox "$t"
        if [ "$do_serve" = "1" ] && [ -z "$(serve_pid "$t")" ]; then
          deploy_one "$t" && start_serve "$t" $serve_flags
          say "$(date +%H:%M:%S)  $t  (re)started q1serve"
        fi
      else
        say "$(date +%H:%M:%S)  $t  unreachable"
      fi
    done
    sleep "$interval"
  done
}

# ---- dispatch --------------------------------------------------------------
MODE="${1:-status}"; shift || true
case "$MODE" in
  up)      cmd_up ;;
  status)  cmd_status ;;
  deploy)  cmd_deploy ;;
  serve)   cmd_serve "$@" ;;
  stop)    cmd_stop ;;
  run)     [ $# -ge 1 ] || { say "usage: q1fleet.sh run \"<cmd>\""; exit 2; }; cmd_run "$1" ;;
  identify) cmd_identify "$@" ;;
  blink)   [ $# -ge 1 ] || { say "usage: q1fleet.sh blink <ip> [color] [secs]"; exit 2; }; exec ./tools/q1blink.sh "$@" ;;
  gui)     exec python3 tools/q1gui.py "$@" ;;
  watch)   cmd_watch "$@" ;;
  *)       say "unknown command: $MODE"; sed -n '2,40p' "$0" >&2; exit 2 ;;
esac
