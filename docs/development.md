# Development

Building the pieces, iterating on them, and — the part that matters most here —
**working on this without a fleet of rooted Quest 1s in front of you.**

[INSTALL.md](../INSTALL.md) covers building it once to *use* it. This covers
building it repeatedly to *change* it.

## The four components

| | lines | language | rebuild cost |
|---|---|---|---|
| `android/q1tracker/` | ~1,000 C++ + 3 Java | C++17 / Java 17 | ~30 s, plus an install |
| `desktop/insight-map-loader-core/` | ~3,600 | Rust 2021 | 2–10 s |
| `desktop/insight-map-loader-gui/` | ~700 | Rust 2021 (egui) | 2–10 s |
| `desktop/steamvr_driver/` | ~500 | C++17 | ~3 s, plus a SteamVR restart |

They are joined by exactly one thing: **the MPT1 packet**, 68 bytes over UDP.
Nothing else crosses a component boundary, which is why the pieces can be worked
on separately and why the wire format is the one thing you cannot casually
change (see [The contract](#the-contract-mpt1) below).

## The Rust side

```sh
cargo build --release --manifest-path desktop/Cargo.toml   # both binaries
cargo test  --manifest-path desktop/Cargo.toml             # 26 unit tests, ~0.2 s
```

Toolchain floor is **Rust 1.75**, edition 2021, set in `desktop/Cargo.toml`.
There are no external services, no async runtime, and no database — the whole
thing is threads, sockets and `std::process::Command`.

**While iterating, do not use `run-gui.sh`.** It launches the existing binary if
one is present and never checks whether your sources are newer, so you will run
old code and not be told. Use:

```sh
cargo run --release -p insight-map-loader-gui
```

which rebuilds every time and no-ops in about half a second when nothing
changed. `run-gui.sh` is for launching, not for developing.

### Where things live

| module | what it owns |
|---|---|
| `mpt1.rs` | the wire format and the role enum. The contract. |
| `ingest.rs` | the UDP socket, one slot per role, liveness (`Live` / `Stale` / `Absent`) |
| `transform.rs` | 4-DoF (yaw + translation) maths |
| `bridge.rs` | solving a puck's OpenXR-LOCAL → Insight-world transform |
| `config.rs` | the site config, role assignment, building the per-puck transforms |
| `fleet.rs` | everything that shells out to `adb` |
| `jobs.rs` | the serialized job queue for long fleet operations |
| `aggregate.rs` | applying transforms and re-emitting |
| `service.rs` | the loop that ties it together, plus event detection |

`lib.rs` is UI-free deliberately: the GUI and the CLI both sit on top of it, and
neither should be able to change what "where is this puck" means.

### Tests

Test names are written as sentences describing the behaviour, because the name
is what you read when it breaks:

```
config::transform_tests::a_puck_without_a_bridge_is_omitted
service::tests::carried_apart_does_not_flag
bridge::tests::too_few_pairs_is_refused
ingest::tests::junk_is_counted_not_fatal
```

Follow that. `carried_apart_does_not_flag` tells you what the drift monitor is
supposed to tolerate; `does_not_flag_carried_apart_case_2` would not.

The suite covers the parts with no hardware in them — wire format, transform
algebra, config editing, job ordering, drift logic. The parts that talk to a
headset are not unit-tested and cannot usefully be; they are verified against
real pucks and the results recorded in the docs.

### Formatting and lints — read this before running `cargo fmt`

**The tree is not rustfmt-formatted.** `cargo fmt --all -- --check` currently
reports diffs in essentially every file, and `cargo clippy` reports about
fifteen warnings (mostly `map_or` simplifications).

So do **not** run `cargo fmt` across the tree. It would produce a thousand-line
diff of pure noise, bury whatever you actually changed, and make every future
`git blame` point at the reformat instead of at the reasoning. Match the style
of the code around your change instead. If a wholesale reformat is ever wanted,
it belongs in its own commit that changes nothing else.

New code should be clippy-clean; the existing warnings are not your problem.

## The Android app

```sh
cd android/q1tracker
./gradlew :app:assembleDebug
adb -s <ip>:5555 install -r app/build/outputs/apk/debug/app-debug.apk
```

If Gradle cannot find the SDK, create `android/q1tracker/local.properties`
containing `sdk.dir=/path/to/Android/Sdk`. It is gitignored — it is a path on
your machine, not a project setting.

Specifics worth knowing before you change the build file:

- **`arm64-v8a` only.** Nothing else is worth building; the Quest 1 is arm64.
- **`minSdk 24`, `targetSdk 32`, `compileSdk 34`.** The low target is
  deliberate — this is a sideloaded app on an EOL Android 10 device, and newer
  targets add background-execution restrictions that cost us the boot-start
  behaviour and buy nothing.
- **The OpenXR loader comes from a prefab dependency**
  (`org.khronos.openxr:openxr_loader_for_android:1.1.43`), not a vendored blob.
  `buildFeatures { prefab = true }` is what makes that work.
- **JDK 17.** Newer JDKs will fight the Android Gradle Plugin.

The app is mostly `quest_tracker.cpp` — an OpenXR session that reads a pose each
frame and sends it as MPT1. The Java side is a thin activity plus a
`BootReceiver` so the app starts after a reboot without anyone launching it.

Reinstalling does **not** restart a running app. Force-stop it first, or use
`insight-map-loader up`, which does the whole launch dance including the
proximity-sensor override.

## The SteamVR driver

```sh
cd desktop/steamvr_driver
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
```

Two things make this pleasant to iterate on:

- **It links nothing from OpenVR.** SteamVR loads a driver as a shared library
  exporting `HmdDriverFactory`; only the *header* is needed to compile, and that
  header is vendored (`vendor/openvr/headers/openvr_driver.h`, BSD-3). There is
  no OpenVR SDK to build or install.
- **`vrpathreg` points SteamVR at your build tree**, and the build stages a
  complete installable driver at `build/mapper`. So **rebuilding is
  reinstalling** — you only register once, ever.

You do have to restart SteamVR to load a new build. Watch its log for:

```
mapper: init ok, listening on udp/5181
```

If you don't see it, SteamVR did not load your driver at all, and nothing
downstream will make sense.

## The Python tools

No packaging, no virtualenv required, no dependencies beyond **numpy**. They are
scripts, run from their own directory so the sibling imports resolve:

```sh
cd tools/insightmap
./selftest_align.py /tmp/mapdb108
```

`tools/insightmap/README.md` documents the module layout and the two things the
matcher does differently from a textbook one.

---

## Working without hardware

This is the section that decides whether you can contribute at all without two
rooted headsets. A surprising amount is reachable.

**The full Rust test suite** needs nothing: `cargo test`, 26 tests, 0.2 seconds.
Wire format, transform algebra, config editing, job ordering and drift detection
are all covered there.

**Drive the SteamVR driver with synthetic poses.** No pucks, no host service:

```sh
python3 desktop/steamvr_driver/tools/send_test_tracker.py --device 6
```

That emits real MPT1 packets and moves one tracker in a slow circle. Exactly one
tracker should appear, as Left Elbow, **and nothing else** — if a dozen appear,
SteamVR loaded an older build of the driver. This exercises the entire
socket → device → SteamVR pose path.

**Watch the ingest side.** The core ships an example that binds the port and
prints one line per slot per second — state, pose, rate, age:

```sh
cargo run --manifest-path desktop/Cargo.toml --example listen -- 5180
```

Point `send_test_tracker.py` at it (`--port 5180`) and you have both halves of
the host pipeline running with no headset involved. Rate is *counted* there
rather than assumed, because "the tracker is running" and "poses are arriving at
the rate you think" are different claims that look identical from outside.

**Work on the map pipeline from a saved mapdb.** A pulled `mapdb` directory is
just files; decode, matching, alignment and both visualisers run entirely
offline. `selftest_align.py` is the strong one — a single puck's own L1 nodes are
independent observations of one room whose correct relative transform is *known*
to be the identity, so the whole chain can be scored without any external ground
truth.

⚠️ **A mapdb is a 3-D scan of someone's home.** Never commit one, never attach
one to an issue, and never ask a reporter to send you theirs. `*.mapdata` is
gitignored for this reason.

**What genuinely needs hardware:** anything in `fleet.rs`, the bring-up script,
the bridge solver against real poses, and every colocation measurement.

---

## The contract: MPT1

68 bytes, little-endian, packed, magic `0x3154504D` (`'MPT1'`). The layout is
stated in three places that **must agree**:

| | |
|---|---|
| `desktop/insight-map-loader-core/src/mpt1.rs` | the Rust producer/consumer |
| `desktop/steamvr_driver/src/mapper_protocol.h` | the C++ consumer |
| `desktop/steamvr_driver/tools/send_test_tracker.py` | the synthetic producer |

Change one and you must change all three. There is no shared header generating
them; the duplication is deliberate (the driver deliberately has no build
dependency on the Rust crate) and the tests assert the layout on the Rust side.

**Role ids are APPEND ONLY. Never renumber, never rename.** SteamVR keys device
pairings, role bindings and room calibration off the serial each id maps to, so
renumbering silently rebinds a user's existing trackers to the wrong body parts.
There are currently 11 roles (0 = Waist … 10 = Camera), and
`MAPPER_DEV_COUNT` is `static_assert`ed against the driver's table.

## Rules for anything that touches a puck

Both were learned the hard way and are honoured throughout `fleet.rs`,
`q1bringup.sh` and `q1sep.py`. Breaking either produces failures that look like
something else entirely.

1. **Never pipe `dumpsys tracking` into anything that closes the pipe early** —
   `grep -m1`, `head`. It leaves the tracking service unavailable for *seconds*
   afterwards, reporting `Can't find service: tracking`, and you will blame the
   wrong thing. Dump to a file on-device, then grep the file.
2. **Batch device queries into one shell round trip.** Each `adb shell` costs
   50–150 ms. A status pass that looks instant in code takes a visible second
   when it is eight round trips.

Two more that bite:

- **Every adb call needs a timeout.** `std::process::Command` has no deadline,
  and one hung adb wedges its caller forever — a launch-style `adb shell` was
  measured stuck for over an hour, which silently stopped a whole polling
  thread while every puck still looked healthy. `fleet.rs` wraps every call in
  `timeout` for this reason.
- **`adb push` into `/vision` needs `chcon u:object_r:vision_file:s0`
  afterwards.** A pushed file carries the wrong SELinux label and
  trackingservice simply cannot read it — no error, just a map that does not
  load.

## Debugging, in order of usefulness

| symptom | look at |
|---|---|
| a puck streams nothing | `insight-map-loader status` — tracking level, tracker up, guardian off |
| poses arrive but are wrong | `cargo run --example listen`, then compare against `dumpsys` |
| trackers wrong or duplicated in SteamVR | the driver's own log line, then `send_test_tracker.py` |
| pucks disagree in space | `tools/q1sep.py` — the only check with real ground truth |
| a map looks wrong | `visualize3d.py`, and `selftest_align.py` on the same mapdb |

The failure modes in this project are mostly **silent**: a puck healthy in every
indicator that emits nothing, a stale bridge that produces a confident wrong
pose, a map that loads and relocalizes into the wrong place. Prefer a check that
*measures* something over one that reports a boolean, and when you add a status
signal, make sure it can actually go false.
