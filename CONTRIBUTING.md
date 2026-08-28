# Contributing to Insight Map Loader

This document describes essential knowledge required to contribute to Insight Map Loader.

Before anything else, one constraint shapes everything here: this has only ever run on
two rooted Quest 1 headsets, on one network, in one room. **A report from other hardware
is worth more than a feature.** See [What is useful](#what-is-useful) below.

### Prerequisites

- [Git](https://git-scm.com/downloads)
- [Rust v1.75+](https://rustup.rs) (host service and GUI)
- [JDK 17](https://adoptium.net/temurin/releases/) and the Android SDK + NDK (puck app)
- [CMake v3.15+](https://cmake.org/download/) and a C++17 compiler (SteamVR driver)
- Python 3 with `numpy` (map tools)
- `adb`, from `android-tools`
- **Rooted Quest 1 headsets.** Rooting is out of scope here; without root none of this works.

## Cloning the code

```bash
git clone https://github.com/<you>/insight-map-loader.git
cd insight-map-loader
```

Everything below is run from the repository root. The service resolves
`insight-map-loader.json`, `bridge.json` and `tools/` relative to it, so running from a
subdirectory will fail to find its own config.

## Building the code

### Host service and GUI (Rust)

The workspace manifest is at `desktop/Cargo.toml`, not the repository root, so builds
need `--manifest-path`.

- To build both binaries, run `cargo build --release --manifest-path desktop/Cargo.toml`.
  The results are `desktop/target/release/insight-map-loader` (CLI) and
  `insight-map-loader-gui`.
- To run the GUI while developing, run
  `cargo run --release --manifest-path desktop/Cargo.toml -p insight-map-loader-gui`.

(Note: `./run-gui.sh` also launches the GUI, but only builds when the binary is *missing*,
so it will happily run stale code after an edit. Use `cargo run` while iterating.)

The GUI *is* the service while it is open. Do not also run `insight-map-loader run` —
they will fight over the listen port, and the loser tells you.

### Puck app (Android)

```bash
cd android/q1tracker
./gradlew :app:assembleDebug
```

The result is at `app/build/outputs/apk/debug/app-debug.apk`, which is the path
`tools/q1bringup.sh --apk` expects.

- If Gradle cannot find the SDK, create `android/q1tracker/local.properties` containing
  `sdk.dir=/path/to/Android/Sdk`. It is gitignored: it is a path on your machine, not a
  project setting.
- The app targets `arm64-v8a` only, `minSdk 24` and `targetSdk 32`. The low target is
  deliberate — this is a sideloaded app on an EOL Android 10 device, and newer targets add
  background-execution restrictions that cost the boot-start behaviour and buy nothing.
- Reinstalling does not restart a running app. Force-stop it first, or use
  `insight-map-loader up`, which does the whole launch dance including the
  proximity-sensor override.

### SteamVR driver (C++)

```bash
cd desktop/steamvr_driver
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
```

- Nothing from OpenVR is linked; only its header is needed, and that header is vendored.
  There is no SDK to install.
- Register it once with
  `<SteamVR>/bin/linux64/vrpathreg adddriver "$PWD/build/mapper"`. This points SteamVR at
  your build tree, so **rebuilding is reinstalling** — but you must restart SteamVR to
  load a new build.
- On startup SteamVR should log `mapper: init ok, listening on udp/5181`. If it does not,
  your driver was not loaded and nothing downstream will make sense.

### Map tools (Python)

No packaging and no virtualenv required; they are scripts. Run them from their own
directory so the sibling imports resolve:

```bash
cd tools/insightmap
./selftest_align.py /tmp/mapdb108
```

## Code style

### Rust

**Do not run `cargo fmt` across the tree.** It is not currently rustfmt-formatted, so a
sweep produces a thousand-line diff that buries your change and redirects every future
`git blame` at the reformat. Match the style of the code around you. A deliberate
whole-tree reformat is fine as its own commit that changes nothing else.

New code should be clippy-clean; the existing warnings are not your problem.

Name tests as sentences describing the behaviour, because the name is what someone reads
when it breaks:

```
config::transform_tests::a_puck_without_a_bridge_is_omitted
service::tests::carried_apart_does_not_flag
bridge::tests::too_few_pairs_is_refused
```

### Comments

Comments here explain *why*, and record what was learned. This is intentional and heavier
than usual: most of the tricky code exists because of a specific discovered behaviour of
an undocumented system, and without the reason it reads as arbitrary and gets "simplified"
back into a bug. If you fix something subtle, leave the finding behind.

## Testing your changes

```bash
cargo test --manifest-path desktop/Cargo.toml
```

34 tests, about a fifth of a second. They must stay green. Add one if what you changed can
be tested without a headset — wire format, transform maths, config handling, job ordering
and drift logic all can.

If you changed something only a headset can exercise, say so plainly in the pull request
and say what you ran it against. "Untested on hardware" is a fine thing to write; a claim
that it works when you did not check is not.

## Running without hardware

More is reachable without pucks than you would expect:

- The full test suite needs nothing.
- `python3 desktop/steamvr_driver/tools/send_test_tracker.py --device 6` emits synthetic
  MPT1 and exercises the whole socket → device → SteamVR path. Exactly one tracker should
  appear, as Left Elbow, and nothing else. If a dozen appear, SteamVR loaded an older build.
- `cargo run --manifest-path desktop/Cargo.toml --example listen -- 5180` prints what is
  arriving, one line per source per second. Point the test sender at it and both halves of
  the host pipeline run with no headset involved.
- The map pipeline runs entirely offline against a pulled `mapdb` directory. `selftest_align.py`
  is the sharpest check: one puck's own L1 nodes are independent observations of one room
  whose correct relative transform is *known* to be the identity.

What genuinely needs hardware: anything in `fleet.rs`, the bring-up script, the bridge
solver against real poses, and every colocation measurement.

## What is useful

Welcome:

- Reports and fixes from other hardware, other rooms, other hosts. Include the headset
  build number (`getprop ro.build.version.incremental`), what
  `insight-map-loader status` printed, and the tracking level and map context line from
  `dumpsys tracking`.
- Making silent failures loud. This project's characteristic bug is a component that looks
  healthy and does nothing.
- Decoder and matcher work in `tools/insightmap/`, which is testable offline.
- Documentation corrections, especially where a doc claims something the code no longer does.

Probably not:

- Hand-applied offsets or manual calibration steps. If two pucks disagree, the fix is to
  find out why, not to add a correction the user has to tune. A whole alignment subsystem
  was deleted for this reason.
- Support for headsets nobody in the discussion owns.
- Reformatting sweeps (see [Code style](#code-style)).
- Rooting instructions, which are deliberately out of scope.

If you are planning something large, open an issue first — not for permission, but so you
find out whether it was already tried and failed. [FINDINGS.md](FINDINGS.md) is the record
of that and is worth reading before you start.

### The bar for a change

This project has retracted claims before: memory-extraction results that beat a
badly-chosen control, and a "sparsity floor" that was an artefact. The habits that came
out of that:

- **Use a matched control.** An unmatched one made a meaningless cluster score perfectly.
  If you claim a result beats chance, make chance work as hard as your result does.
- **State what your check cannot see.** Internal-consistency checks are cheap and mislead
  in both directions. `tools/q1sep.py` is the standard here, because two co-located pucks
  cannot lie to each other.
- **A refusal beats a confident wrong answer.** `align_stable()` returns `None` when its
  RANSAC seeds disagree, because a wrong transform is worse than none. Prefer that shape.

## Things that must never be committed

`.gitignore` covers these, but it has failed before, so check what you are staging.

- `*.mapdata` or any `mapdb` directory. A persisted Insight map is a 3-D scan of a home.
- `resid.json`, `align*.json`, `*.pgm` — measurement outputs containing puck trajectories.
- Device serials, WiFi MACs, LAN addresses.
- `insight-map-loader.json` and `bridge.json`, which are site-specific and name your network.
- APKs, keystores, build output.
- **Anything pulled off a Quest.** The device libraries are Meta's, and this project ships
  no Meta code at all. Keep it that way.

## Load-bearing invariants

Breaking one of these produces failures that look like something else entirely.

- **MPT1 role ids are append-only.** Never renumber, never rename. SteamVR keys pairings
  and room calibration off them, so renumbering silently rebinds a user's trackers to the
  wrong body parts.
- **The 68-byte packet layout is stated in three files that must agree**: `mpt1.rs`,
  `mapper_protocol.h` and `send_test_tracker.py`.
- **The byte at offset 4 means different things at each end.** From a puck it is that
  puck's identity; to the driver it is the SteamVR role. The host maps one to the other.
- **Never pipe `dumpsys tracking` into anything that closes the pipe early** (`grep -m1`,
  `head`). It leaves the tracking service unavailable for seconds afterwards, reporting
  `Can't find service: tracking`, and you will blame the wrong thing. Dump to a file
  on-device, then grep the file.
- **Batch device queries into one shell round trip.** Each `adb shell` costs 50-150 ms.
- **Every adb call needs a timeout.** One hung adb wedges its caller forever; a
  launch-style `adb shell` was measured stuck for over an hour.
- **`adb push` into `/vision` needs `chcon u:object_r:vision_file:s0` afterwards.** Without
  it trackingservice simply cannot read the file — no error, just a map that does not load.

## Code Licensing

Insight Map Loader uses the GPL-3.0 licence. Be sure that any code you reference, or
dependencies you add, are compatible with it. Note that this cuts both ways: permissively
licensed projects generally cannot absorb code from here, so check the direction of travel
before importing in either direction.

The vendored `openvr_driver.h` is BSD-3-Clause and is recorded in
[THIRD_PARTY.md](THIRD_PARTY.md). Add anything else you vendor to that file.

## Use of AI

Disclose it. If you used an AI tool to produce a contribution, say so in the pull request
and say what it did.

The bar for the code itself does not change: you are responsible for understanding what
you submit, the tests must pass, and any hardware claim must be one you actually checked.
An unverified claim is the problem, whoever or whatever wrote it.
